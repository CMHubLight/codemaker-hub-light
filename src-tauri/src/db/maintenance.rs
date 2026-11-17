//! 数据保留期清理与额度快照剪枝。
//!
//! `retention_days` 之前只存不用（`settings.rs` 校验了 1..=3650，但没有任何消费者），
//! 请求表 / 费用表 / 额度快照全都只增不减。这里是唯一入口，接线（启动后一次 +
//! 之后每天一次）由 `lib.rs` 侧完成。
//!
//! 设计要点：
//! - **分批 DELETE**：一次删太多会长时间持写锁，把代理的落库写入饿死；每批
//!   [`DELETE_BATCH`] 行，批间自动提交。
//! - 只按**已落库的本地日**删（`day_local < cutoff`），能命中
//!   `ix_psr_day_started_id` 的索引前缀；`day_local IS NULL` 的异常行一律保留。
//! - 两张表的时间列**表示不同**，边界必须分开算：`day_local` / `day` 存本地日
//!   字符串；`proxy_sessions.last_active_at` 存 **UTC RFC3339**。旧实现把本地日
//!   字符串直接当会话边界比较，字符串比较只看得见 UTC 日期，等于把边界锚在 UTC
//!   0 点而不是本地 0 点：东八区会误删边界当天 00:00–08:00（UTC 仍是前一天）的
//!   会话，西半球则漏删边界前一晚的会话。
//! - 费用行靠 `ON DELETE CASCADE` 清理（每个连接都开了 `foreign_keys = ON`）。
//! - 结束时 `ANALYZE`：大删除会显著改变数据分布，不刷新统计的话后续查询计划
//!   会继续按旧估算选索引。

use chrono::{Duration, Local, LocalResult, NaiveDate, SecondsFormat, TimeZone, Utc};
use rusqlite::params;

use super::{blocking, repo, Db, DbError};

/// 单批删除行数。
const DELETE_BATCH: usize = 5_000;
/// 每个 `quota_type` 保留的额度快照条数。
const QUOTA_KEEP: usize = 200;
/// 失败/非成功快照额外保留天数（它们不参与"最新成功快照"查询，只用于排障）。
const QUOTA_NONOK_KEEP_DAYS: i64 = 7;

/// 一次清理的结果（诊断/日志用；字段全部是"删了多少行"）。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MaintenanceReport {
    /// 保留边界日：`< cutoff_day` 的本地日数据被删除。
    pub cutoff_day: String,
    pub retention_days: u32,
    pub requests_deleted: u64,
    pub sessions_deleted: u64,
    pub usage_days_deleted: u64,
    pub quota_snapshots_deleted: u64,
}

/// 异步入口：把整段清理挪到阻塞线程池。
pub async fn run_maintenance(db: &Db, retention_days: u32) -> Result<MaintenanceReport, DbError> {
    blocking::run(db, move |db| prune(&db, retention_days)).await
}

/// 保留窗口的两种边界表示（同一天）。
///
/// - `cutoff_day`：本地日字符串 `YYYY-MM-DD`，用于 `day_local` / `day`（这两列
///   落库的就是本地日）；
/// - `cutoff_instant`：该本地日 0 点换算成的 UTC RFC3339（秒精度），用于和
///   `last_active_at`（UTC RFC3339）比较。
///
/// 返回 `(cutoff_day, cutoff_instant)`。
fn cutoff_bounds(retention_days: u32, today: NaiveDate) -> (String, String) {
    let cutoff = today - Duration::days(i64::from(retention_days) - 1);
    (
        cutoff.format("%Y-%m-%d").to_string(),
        local_midnight_utc(cutoff),
    )
}

/// 本地日 0 点 → UTC RFC3339 秒字符串。
fn local_midnight_utc(day: NaiveDate) -> String {
    let midnight = day.and_hms_opt(0, 0, 0).expect("00:00:00 总是合法时间");
    let instant = match Local.from_local_datetime(&midnight) {
        LocalResult::Single(datetime) => datetime,
        // 夏令时回拨会让本地时钟出现两个 0 点：取**较晚**的瞬间，宁可多留一小时
        // 也不误删边界当天的会话。
        LocalResult::Ambiguous(_, latest) => latest,
        // 个别时区（如巴西旧规则）在 0 点直接跳到 1 点，本地 0 点不存在：取跳跃后
        // 第一个可表示时刻（即该日的真实起点）。
        LocalResult::None => Local
            .from_local_datetime(&(midnight + Duration::hours(1)))
            .earliest()
            .unwrap_or_else(|| Utc.from_utc_datetime(&midnight).with_timezone(&Local)),
    };
    instant
        .with_timezone(&Utc)
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 同步实现（测试直接调用，避免为验证多开一个 runtime）。
pub fn prune(db: &Db, retention_days: u32) -> Result<MaintenanceReport, DbError> {
    prune_at(db, retention_days, Local::now().date_naive())
}

/// 以 `today` 作为"今天"执行清理。
///
/// `prune` 传机器本地日；测试传固定日期，让边界断言与运行时刻无关。
fn prune_at(db: &Db, retention_days: u32, today: NaiveDate) -> Result<MaintenanceReport, DbError> {
    // 设置只允许 1..=3650；这里再钳一次，防止调用方绕过校验传入 0/超大值。
    let retention_days = retention_days.clamp(1, 3650);
    let (cutoff_day, cutoff_instant) = cutoff_bounds(retention_days, today);

    let conn = db.conn()?;
    let mut report = MaintenanceReport {
        cutoff_day,
        retention_days,
        ..Default::default()
    };

    // 1) 逐请求行（on delete cascade 会带走对应费用行）。
    loop {
        let deleted = conn.execute(
            "DELETE FROM proxy_session_requests WHERE id IN ( \
                 SELECT id FROM proxy_session_requests \
                  WHERE day_local IS NOT NULL AND day_local < ?1 LIMIT ?2)",
            params![report.cutoff_day, DELETE_BATCH as i64],
        )?;
        report.requests_deleted += deleted as u64;
        if deleted < DELETE_BATCH {
            break;
        }
    }

    // 2) 会话行（请求行已先删，剩下的都是保留期内的请求）。
    //    这里必须用 UTC 瞬间边界：`last_active_at` 是 UTC RFC3339。
    loop {
        let deleted = conn.execute(
            "DELETE FROM proxy_sessions WHERE id IN ( \
                 SELECT id FROM proxy_sessions \
                  WHERE last_active_at < ?1 LIMIT ?2)",
            params![cutoff_instant, DELETE_BATCH as i64],
        )?;
        report.sessions_deleted += deleted as u64;
        if deleted < DELETE_BATCH {
            break;
        }
    }

    // 3) 日聚合（仪表盘/趋势只按天读，同样按保留窗口裁剪）。
    loop {
        let deleted = conn.execute(
            "DELETE FROM usage_daily WHERE id IN ( \
                 SELECT id FROM usage_daily WHERE day < ?1 LIMIT ?2)",
            params![report.cutoff_day, DELETE_BATCH as i64],
        )?;
        report.usage_days_deleted += deleted as u64;
        if deleted < DELETE_BATCH {
            break;
        }
    }

    // 4) 额度快照：先删超过 7 天的非成功快照，再按"每个 quota_type 保留最近
    //    QUOTA_KEEP 条"兜底（成功快照也是只增不删的）。
    let nonok_cutoff = (Utc::now() - Duration::days(QUOTA_NONOK_KEEP_DAYS))
        .to_rfc3339_opts(SecondsFormat::Secs, true);
    let nonok_deleted = conn.execute(
        "DELETE FROM quota_snapshots WHERE status <> 'ok' AND fetched_at < ?1",
        params![nonok_cutoff],
    )?;
    let trimmed = repo::trim_quota_snapshots(&conn, QUOTA_KEEP)?;
    report.quota_snapshots_deleted = (nonok_deleted + trimmed) as u64;

    // 5) 刷新统计（大删除后旧估算会让查询计划失真）。没有任何删除时跳过：
    //    ANALYZE 本身要扫索引，没必要每天白跑一遍。
    let deleted_total = report.requests_deleted
        + report.sessions_deleted
        + report.usage_days_deleted
        + report.quota_snapshots_deleted;
    if deleted_total > 0 {
        conn.execute_batch("ANALYZE;")?;
    }

    tracing::info!(
        cutoff = %report.cutoff_day,
        requests = report.requests_deleted,
        sessions = report.sessions_deleted,
        usage_days = report.usage_days_deleted,
        quota_snapshots = report.quota_snapshots_deleted,
        "保留期清理完成"
    );
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    use crate::db::run_migrations;

    fn test_db() -> Db {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("迁移应成功");
        db
    }

    fn day(offset: i64) -> String {
        (Local::now().date_naive() + Duration::days(offset))
            .format("%Y-%m-%d")
            .to_string()
    }

    fn insert_request(db: &Db, day_local: Option<&str>) -> i64 {
        let conn = db.conn().expect("应能取连接");
        // started_at 是 NOT NULL；缺 day_local 的行仍要有可解析的开始时间。
        let started_at = format!("{}T01:00:00Z", day_local.unwrap_or("2026-10-20"));
        conn.execute(
            "INSERT INTO proxy_session_requests \
               (method, request_path, api_format, started_at, day_local, status_kind) \
             VALUES ('POST', '/v1/messages', 'anthropic', ?1, ?2, 'success')",
            params![started_at, day_local],
        )
        .expect("插入请求行应成功");
        conn.last_insert_rowid()
    }

    fn request_count(db: &Db) -> i64 {
        let conn = db.conn().expect("应能取连接");
        conn.query_row("SELECT COUNT(*) FROM proxy_session_requests", [], |row| {
            row.get(0)
        })
        .expect("计数应成功")
    }

    /// 保留期：`< cutoff` 的请求/费用/会话/日聚合被删，`== cutoff` 与 NULL 行保留；
    /// 第二次运行必须零变更（幂等）。
    ///
    /// 用固定"今天"跑 `prune_at`：会话边界要按本地 0 点换算出的 UTC 瞬间比较，
    /// 断言不能随运行时刻或机器时区漂移。
    #[test]
    fn retention_prunes_old_rows_and_keeps_boundary() {
        let db = test_db();
        let today = NaiveDate::from_ymd_opt(2026, 11, 17).expect("日期应合法");
        let retention_days = 7;
        let (cutoff, cutoff_instant) = cutoff_bounds(retention_days, today);
        assert_eq!(cutoff, "2026-11-11", "保留 7 天时边界应是 6 天前");
        let old = "2026-11-10".to_string();
        let old_request = insert_request(&db, Some(&old));
        let boundary_request = insert_request(&db, Some(&cutoff));
        let recent_request = insert_request(&db, Some("2026-11-17"));
        // 缺 day_local 的异常行：不能因为清理而被静默删除。
        let null_request = insert_request(&db, None);

        // 会话边界按 UTC 瞬间：早 1 秒的删、恰好等于边界的保留（严格小于才删）。
        let cutoff_at = DateTime::parse_from_rfc3339(&cutoff_instant)
            .expect("边界必须是合法 RFC3339")
            .with_timezone(&Utc);
        let before_cutoff =
            (cutoff_at - Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Secs, true);

        {
            let conn = db.conn().expect("应能取连接");
            for request_id in [old_request, boundary_request, recent_request] {
                conn.execute(
                    "INSERT INTO proxy_request_costs (request_id, cost_yuan, created_at) \
                     VALUES (?1, 1.0, ?2)",
                    params![request_id, cutoff_instant],
                )
                .expect("插入费用应成功");
            }
            conn.execute(
                "INSERT INTO proxy_sessions \
                   (external_session_id, api_format, first_seen_at, last_active_at, \
                    created_at, updated_at) \
                 VALUES ('old-session', 'anthropic', ?1, ?1, ?1, ?1)",
                params![before_cutoff],
            )
            .expect("插入旧会话应成功");
            conn.execute(
                "INSERT INTO proxy_sessions \
                   (external_session_id, api_format, first_seen_at, last_active_at, \
                    created_at, updated_at) \
                 VALUES ('recent-session', 'anthropic', ?1, ?1, ?1, ?1)",
                params![cutoff_instant],
            )
            .expect("插入近期会话应成功");
            for (day_local, cost) in [(old, 1.0), (cutoff.clone(), 2.0)] {
                conn.execute(
                    "INSERT INTO usage_daily \
                       (day, api_format, model, client, request_count, error_count, input_tokens, \
                        output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
                        total_duration_ms, updated_at) \
                     VALUES (?1, 'anthropic', 'claude-sonnet-4', 'claude-code', 1, 0, 1, 1, 0, 0, \
                             ?2, 10, '2026-10-01T00:00:00Z')",
                    params![day_local, cost],
                )
                .expect("插入日聚合应成功");
            }
        }
        assert_eq!(request_count(&db), 4);

        let report = prune_at(&db, retention_days, today).expect("清理应成功");
        assert_eq!(report.cutoff_day, cutoff);
        assert_eq!(report.requests_deleted, 1, "只应删掉早于边界的请求行");
        assert_eq!(report.sessions_deleted, 1, "只应删掉早于边界的会话行");
        assert_eq!(report.usage_days_deleted, 1, "只应删掉早于边界的日聚合");

        assert_eq!(request_count(&db), 3, "边界当天 + 今天 + 缺列行都应保留");
        let conn = db.conn().expect("应能取连接");
        let costs: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_request_costs", [], |row| {
                row.get(0)
            })
            .expect("统计费用应成功");
        assert_eq!(costs, 2, "被删请求的费用行应级联删除");
        let sessions: i64 = conn
            .query_row("SELECT COUNT(*) FROM proxy_sessions", [], |row| row.get(0))
            .expect("统计会话应成功");
        assert_eq!(sessions, 1, "边界瞬间的会话必须保留");
        let usage_days: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage_daily", [], |row| row.get(0))
            .expect("统计日聚合应成功");
        assert_eq!(usage_days, 1, "边界当天的日聚合必须保留");
        // NULL 行确实还在（避免"顺手删异常数据"）。
        let null_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_session_requests WHERE day_local IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("统计应成功");
        assert_eq!(null_rows, 1);
        let _ = null_request;
        drop(conn);

        let again = prune_at(&db, retention_days, today).expect("二次清理应成功");
        assert_eq!(again.requests_deleted, 0, "重复清理必须零变更");
        assert_eq!(again.sessions_deleted, 0);
        assert_eq!(again.usage_days_deleted, 0);
        assert_eq!(request_count(&db), 3);
    }

    /// `cutoff_bounds` 必须把本地日字符串与 UTC 瞬间两个边界同时给出，且 UTC
    /// 瞬间反解回本地仍是边界当天（0 点；夏令时跳跃时是该日真实起点）。
    #[test]
    fn cutoff_bounds_maps_local_midnight_to_utc() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 17).expect("日期应合法");
        let (cutoff_day, cutoff_instant) = cutoff_bounds(7, today);
        assert_eq!(cutoff_day, "2026-11-11");
        assert!(
            cutoff_instant.ends_with('Z'),
            "会话边界必须是 UTC 表示：{cutoff_instant}"
        );
        let parsed = DateTime::parse_from_rfc3339(&cutoff_instant)
            .expect("边界必须是合法 RFC3339")
            .with_timezone(&Utc);
        // 秒精度且无小数：与 `last_active_at` 的落库格式（rfc3339 秒 + Z）可比。
        assert_eq!(
            parsed.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            cutoff_instant
        );
        assert_eq!(
            parsed.with_timezone(&Local).date_naive(),
            today - Duration::days(6),
            "UTC 瞬间反解回本地必须仍是边界当天"
        );
    }

    /// 会话边界按 UTC 瞬间严格比较：cutoff 前 1 秒删、等于 cutoff 与后 1 秒保留。
    /// 固定日期计算边界，与运行时刻、机器时区无关。
    #[test]
    fn session_retention_boundary_is_utc_second_precise() {
        let db = test_db();
        let today = NaiveDate::from_ymd_opt(2026, 11, 17).expect("日期应合法");
        let retention_days = 7;
        let (_, cutoff_instant) = cutoff_bounds(retention_days, today);
        let cutoff = DateTime::parse_from_rfc3339(&cutoff_instant)
            .expect("边界必须是合法 RFC3339")
            .with_timezone(&Utc);
        let before = (cutoff - Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
        let after = (cutoff + Duration::seconds(1)).to_rfc3339_opts(SecondsFormat::Secs, true);

        {
            let conn = db.conn().expect("应能取连接");
            for (name, stamp) in [
                ("before", before.as_str()),
                ("at", cutoff_instant.as_str()),
                ("after", after.as_str()),
            ] {
                conn.execute(
                    "INSERT INTO proxy_sessions \
                       (external_session_id, api_format, first_seen_at, last_active_at, \
                        created_at, updated_at) \
                     VALUES (?1, 'anthropic', ?2, ?2, ?2, ?2)",
                    params![name, stamp],
                )
                .expect("插入会话应成功");
            }
        }

        let report = prune_at(&db, retention_days, today).expect("清理应成功");
        assert_eq!(
            report.sessions_deleted, 1,
            "只有 cutoff 前 1 秒的会话应被删除（边界是严格小于）"
        );

        let conn = db.conn().expect("应能取连接");
        let mut stmt = conn
            .prepare("SELECT external_session_id FROM proxy_sessions ORDER BY external_session_id")
            .expect("准备查询应成功");
        let remaining: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .expect("查询应成功")
            .collect::<rusqlite::Result<Vec<_>>>()
            .expect("收集行应成功");
        assert_eq!(remaining, vec!["after".to_string(), "at".to_string()]);
    }

    /// 额度快照：超过 7 天的非 ok 行被删；每个 quota_type 只保留最近 200 条。
    #[test]
    fn quota_snapshots_are_pruned_and_trimmed() {
        let db = test_db();
        {
            let conn = db.conn().expect("应能取连接");
            let stale =
                (Utc::now() - Duration::days(30)).to_rfc3339_opts(SecondsFormat::Secs, true);
            for index in 0..5 {
                conn.execute(
                    "INSERT INTO quota_snapshots \
                       (quota_type, status, fetched_at) VALUES ('global', 'error', ?1)",
                    params![stale],
                )
                .expect("插入过期非 ok 快照应成功");
                let _ = index;
            }
            let fresh = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
            for index in 0..205 {
                conn.execute(
                    "INSERT INTO quota_snapshots \
                       (quota_type, status, monthly_quota, fetched_at) \
                     VALUES ('global', 'ok', ?1, ?2)",
                    params![index as f64, fresh],
                )
                .expect("插入成功快照应成功");
            }
        }

        let report = prune(&db, 90).expect("清理应成功");
        assert_eq!(report.quota_snapshots_deleted, 10, "5 条过期 + 裁掉的 5 条");

        let conn = db.conn().expect("应能取连接");
        let total: i64 = conn
            .query_row("SELECT COUNT(*) FROM quota_snapshots", [], |row| row.get(0))
            .expect("统计应成功");
        assert_eq!(total as usize, QUOTA_KEEP, "每个 quota_type 只保留 200 条");
        let non_ok: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM quota_snapshots WHERE status <> 'ok'",
                [],
                |row| row.get(0),
            )
            .expect("统计应成功");
        assert_eq!(non_ok, 0, "过期失败快照应被删除");

        let latest = repo::latest_quota_snapshot(&conn, "global", None)
            .expect("读取最新快照应成功")
            .expect("清理后仍应有成功快照");
        assert_eq!(latest.monthly_quota, Some(204.0), "保留的是最新一条");
    }

    /// 保留天数为 1 时只保留今天。
    #[test]
    fn retention_one_day_keeps_today_only() {
        let db = test_db();
        insert_request(&db, Some(&day(-1)));
        insert_request(&db, Some(&day(0)));

        let report = prune(&db, 1).expect("清理应成功");
        assert_eq!(report.cutoff_day, day(0));
        assert_eq!(report.requests_deleted, 1);
        assert_eq!(request_count(&db), 1);
    }
}
