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
//! - 费用行靠 `ON DELETE CASCADE` 清理（每个连接都开了 `foreign_keys = ON`）。
//! - 结束时 `ANALYZE`：大删除会显著改变数据分布，不刷新统计的话后续查询计划
//!   会继续按旧估算选索引。

use chrono::{Duration, Local, SecondsFormat, Utc};
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

/// 同步实现（测试直接调用，避免为验证多开一个 runtime）。
pub fn prune(db: &Db, retention_days: u32) -> Result<MaintenanceReport, DbError> {
    // 设置只允许 1..=3650；这里再钳一次，防止调用方绕过校验传入 0/超大值。
    let retention_days = retention_days.clamp(1, 3650);
    let cutoff = Local::now().date_naive() - Duration::days(i64::from(retention_days) - 1);
    let cutoff_day = cutoff.format("%Y-%m-%d").to_string();

    let conn = db.conn()?;
    let mut report = MaintenanceReport {
        cutoff_day: cutoff_day.clone(),
        retention_days,
        ..Default::default()
    };

    // 1) 逐请求行（on delete cascade 会带走对应费用行）。
    loop {
        let deleted = conn.execute(
            "DELETE FROM proxy_session_requests WHERE id IN ( \
                 SELECT id FROM proxy_session_requests \
                  WHERE day_local IS NOT NULL AND day_local < ?1 LIMIT ?2)",
            params![cutoff_day, DELETE_BATCH as i64],
        )?;
        report.requests_deleted += deleted as u64;
        if deleted < DELETE_BATCH {
            break;
        }
    }

    // 2) 会话行（请求行已先删，剩下的都是保留期内的请求）。
    loop {
        let deleted = conn.execute(
            "DELETE FROM proxy_sessions WHERE id IN ( \
                 SELECT id FROM proxy_sessions \
                  WHERE last_active_at < ?1 LIMIT ?2)",
            params![cutoff_day, DELETE_BATCH as i64],
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
            params![cutoff_day, DELETE_BATCH as i64],
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
    #[test]
    fn retention_prunes_old_rows_and_keeps_boundary() {
        let db = test_db();
        let retention_days = 7;
        let cutoff = day(-(i64::from(retention_days) - 1));
        let old = day(-(i64::from(retention_days)));
        let old_request = insert_request(&db, Some(&old));
        let boundary_request = insert_request(&db, Some(&cutoff));
        let recent_request = insert_request(&db, Some(&day(0)));
        // 缺 day_local 的异常行：不能因为清理而被静默删除。
        let null_request = insert_request(&db, None);

        {
            let conn = db.conn().expect("应能取连接");
            for request_id in [old_request, boundary_request, recent_request] {
                conn.execute(
                    "INSERT INTO proxy_request_costs (request_id, cost_yuan, created_at) \
                     VALUES (?1, 1.0, ?2)",
                    params![request_id, format!("{cutoff}T00:00:00Z")],
                )
                .expect("插入费用应成功");
            }
            conn.execute(
                "INSERT INTO proxy_sessions \
                   (external_session_id, api_format, first_seen_at, last_active_at, \
                    created_at, updated_at) \
                 VALUES ('old-session', 'anthropic', ?1, ?1, ?1, ?1)",
                params![format!("{old}T01:00:00Z")],
            )
            .expect("插入旧会话应成功");
            conn.execute(
                "INSERT INTO proxy_sessions \
                   (external_session_id, api_format, first_seen_at, last_active_at, \
                    created_at, updated_at) \
                 VALUES ('recent-session', 'anthropic', ?1, ?1, ?1, ?1)",
                params![format!("{cutoff}T01:00:00Z")],
            )
            .expect("插入近期会话应成功");
            for (day_local, cost) in [(old.clone(), 1.0), (cutoff.clone(), 2.0)] {
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

        let report = prune(&db, retention_days).expect("清理应成功");
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
        assert_eq!(sessions, 1, "近期会话必须保留");
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

        let again = prune(&db, retention_days).expect("二次清理应成功");
        assert_eq!(again.requests_deleted, 0, "重复清理必须零变更");
        assert_eq!(again.sessions_deleted, 0);
        assert_eq!(again.usage_days_deleted, 0);
        assert_eq!(request_count(&db), 3);
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

        let latest = repo::latest_quota_snapshot(&conn, "global")
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
