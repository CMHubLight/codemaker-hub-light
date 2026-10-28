//! 仓储层：只做读写，不含业务规则、不含行解码（见 `db::rows`）、不拼 SQL 文本
//! （见 `db::sql`）。
//!
//! 覆盖认证 profile、额度快照、用量日聚合与请求明细的读写。

use chrono::{SecondsFormat, Utc};
use rusqlite::{params, params_from_iter, OptionalExtension};

use crate::db::{rows::FromRow, sql, DbError};

// 对外暴露的行类型与查询条件：调用方一直通过 `db::repo::*` 使用，保持路径稳定。
pub use super::rows::{
    AuthProfileRow, ModelPriceRow, QuotaSnapshotRow, RequestDetailRow, RequestRow, UsageDayRow,
    UsageDimRow, UsageExportRow, UsageTotalsRow,
};
pub use super::sql::{Cursor, RequestQuery, UsageDimension};

/// 统一 RFC3339（UTC、秒精度）时间戳。
pub fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// 当前 schema 版本；空库返回 0。
pub fn schema_version(conn: &rusqlite::Connection) -> Result<i64, DbError> {
    let version: Option<i64> = conn
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .optional()?
        .flatten();
    Ok(version.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// 账号档案
// ---------------------------------------------------------------------------

/// 当前激活的账号档案。
pub fn active_profile(conn: &rusqlite::Connection) -> Result<Option<AuthProfileRow>, DbError> {
    let sql = format!(
        "SELECT {} FROM auth_profiles \
         WHERE is_active = 1 ORDER BY updated_at DESC LIMIT 1",
        sql::AUTH_PROFILE_COLUMNS
    );
    Ok(conn
        .query_row(&sql, [], AuthProfileRow::from_row)
        .optional()?)
}

/// 写入账号档案的参数（避免长参数列表）。
pub struct ProfileUpsert<'a> {
    pub user: &'a str,
    pub display_name: Option<&'a str>,
    pub dept: Option<&'a str>,
    pub server_url: &'a str,
    pub auth_user: Option<&'a str>,
    pub models_json: Option<&'a str>,
    pub is_active: bool,
    pub issued_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub validated_at: Option<i64>,
}

/// 写入/更新账号档案（按 user + server_url 去重），返回 profile id。
pub fn upsert_profile(
    conn: &rusqlite::Connection,
    profile: &ProfileUpsert<'_>,
) -> Result<i64, DbError> {
    let now = now_rfc3339();
    conn.execute(
        "INSERT INTO auth_profiles \
         (user, display_name, dept, server_url, auth_user, is_active, issued_at, expires_at, \
          validated_at, models_json, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11) \
         ON CONFLICT(user, server_url) DO UPDATE SET \
           display_name = excluded.display_name, \
           dept = excluded.dept, \
           is_active = excluded.is_active, \
           issued_at = excluded.issued_at, \
           expires_at = excluded.expires_at, \
           validated_at = excluded.validated_at, \
           models_json = excluded.models_json, \
           updated_at = excluded.updated_at",
        params![
            profile.user,
            profile.display_name,
            profile.dept,
            profile.server_url,
            profile.auth_user,
            if profile.is_active { 1_i64 } else { 0_i64 },
            profile.issued_at,
            profile.expires_at,
            profile.validated_at,
            profile.models_json,
            now
        ],
    )?;
    let id: i64 = conn.query_row(
        "SELECT id FROM auth_profiles WHERE user = ?1 AND server_url = ?2",
        params![profile.user, profile.server_url],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// 登出时把所有档案标记为非激活（保留历史，不清敏感数据——凭据根本不在 DB）。
pub fn deactivate_profiles(conn: &rusqlite::Connection) -> Result<(), DbError> {
    conn.execute(
        "UPDATE auth_profiles SET is_active = 0, updated_at = ?1 WHERE is_active = 1",
        params![now_rfc3339()],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 额度快照
// ---------------------------------------------------------------------------

/// 最近一次成功（status = 'ok'）的额度快照；失败不覆盖旧值。
///
/// `id DESC` 是必须的第二排序键：`fetched_at` 只到秒，同一秒内写入的多条快照
/// 否则"最新"不确定。
pub fn latest_quota_snapshot(
    conn: &rusqlite::Connection,
    quota_type: &str,
) -> Result<Option<QuotaSnapshotRow>, DbError> {
    let sql = format!(
        "SELECT {} FROM quota_snapshots \
         WHERE quota_type = ?1 AND status = 'ok' ORDER BY fetched_at DESC, id DESC LIMIT 1",
        sql::QUOTA_SNAPSHOT_COLUMNS
    );
    Ok(conn
        .query_row(&sql, params![quota_type], QuotaSnapshotRow::from_row)
        .optional()?)
}

/// 追加一条额度快照（P1 拉取额度后写入）。
pub struct NewQuotaSnapshot<'a> {
    pub profile_id: Option<i64>,
    pub quota_type: &'a str,
    pub status: &'a str,
    pub http_status: Option<i64>,
    pub monthly_quota: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percentage: Option<f64>,
    pub reset_day: Option<i64>,
    pub period_start: Option<&'a str>,
    pub reset_at: Option<&'a str>,
    pub timezone: Option<&'a str>,
    pub is_active: Option<bool>,
    pub raw_json: Option<&'a str>,
}

pub fn insert_quota_snapshot(
    conn: &rusqlite::Connection,
    snapshot: &NewQuotaSnapshot<'_>,
) -> Result<i64, DbError> {
    conn.execute(
        "INSERT INTO quota_snapshots \
         (profile_id, quota_type, status, http_status, monthly_quota, used, remaining, percentage, \
          reset_day, period_start, reset_at, timezone, is_active, raw_json, fetched_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            snapshot.profile_id,
            snapshot.quota_type,
            snapshot.status,
            snapshot.http_status,
            snapshot.monthly_quota,
            snapshot.used,
            snapshot.remaining,
            snapshot.percentage,
            snapshot.reset_day,
            snapshot.period_start,
            snapshot.reset_at,
            snapshot.timezone,
            snapshot
                .is_active
                .map(|value| if value { 1_i64 } else { 0_i64 }),
            snapshot.raw_json,
            now_rfc3339()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 每个 `quota_type` 保留最近 `keep` 条快照，返回删除行数。
pub fn trim_quota_snapshots(conn: &rusqlite::Connection, keep: usize) -> Result<usize, DbError> {
    let quota_types: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT quota_type FROM quota_snapshots")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut deleted = 0usize;
    let keep = keep.max(1);
    for quota_type in quota_types {
        // 保留边界：第 keep 新的那条的 id；不足 keep 条时子查询为 NULL → 不删。
        let boundary: Option<i64> = conn
            .query_row(
                "SELECT id FROM quota_snapshots WHERE quota_type = ?1 \
                 ORDER BY id DESC LIMIT 1 OFFSET ?2",
                params![quota_type, keep as i64 - 1],
                |row| row.get(0),
            )
            .optional()?;
        let Some(boundary) = boundary else {
            continue;
        };
        deleted += conn.execute(
            "DELETE FROM quota_snapshots WHERE quota_type = ?1 AND id < ?2",
            params![quota_type, boundary],
        )?;
    }
    Ok(deleted)
}

// ---------------------------------------------------------------------------
// 用量聚合（读 usage_daily）
// ---------------------------------------------------------------------------

/// 区间总量汇总。
pub fn usage_totals(
    conn: &rusqlite::Connection,
    from_day: &str,
    to_day: &str,
) -> Result<UsageTotalsRow, DbError> {
    Ok(conn.query_row(
        &sql::usage_totals(),
        params![from_day, to_day],
        UsageTotalsRow::from_row,
    )?)
}

/// 按日汇总。
pub fn usage_by_day(
    conn: &rusqlite::Connection,
    from_day: &str,
    to_day: &str,
) -> Result<Vec<UsageDayRow>, DbError> {
    let mut stmt = conn.prepare(&sql::usage_by_day())?;
    let rows = stmt.query_map(params![from_day, to_day], UsageDayRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 按维度汇总（列名来自白名单枚举）。
pub fn usage_by_dimension(
    conn: &rusqlite::Connection,
    dimension: UsageDimension,
    from_day: &str,
    to_day: &str,
) -> Result<Vec<UsageDimRow>, DbError> {
    let mut stmt = conn.prepare(&sql::usage_by_dimension(dimension))?;
    let rows = stmt.query_map(params![from_day, to_day], UsageDimRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 逐行流式导出（日 / 协议 / 模型 / 客户端）。
///
/// 不把结果集收进 `Vec`：导出区间由用户任选，旧实现会把所有行读进内存再拼成
/// 一个大 `String`（三份拷贝）。`max_rows` 是硬上限，返回 `(写入行数, 是否被截断)`。
pub fn export_rows_stream(
    conn: &rusqlite::Connection,
    from_day: &str,
    to_day: &str,
    max_rows: usize,
    mut sink: impl FnMut(UsageExportRow) -> Result<(), DbError>,
) -> Result<(usize, bool), DbError> {
    let mut stmt = conn.prepare(&sql::usage_export())?;
    let mut rows = stmt.query(params![from_day, to_day])?;
    let mut written = 0usize;
    while let Some(row) = rows.next()? {
        if written == max_rows {
            return Ok((written, true));
        }
        sink(UsageExportRow::from_row(row)?)?;
        written += 1;
    }
    Ok((written, false))
}

// ---------------------------------------------------------------------------
// 请求明细（读 proxy_session_requests + proxy_request_costs）
// ---------------------------------------------------------------------------

/// 满足条件的请求数。
pub fn count_requests(
    conn: &rusqlite::Connection,
    query: &RequestQuery<'_>,
) -> Result<i64, DbError> {
    let (sql, params) = sql::count_requests(query);
    Ok(conn.query_row(&sql, params_from_iter(params), |row| row.get(0))?)
}

/// 分页请求明细（按本地日 + 开始时间倒序；有游标走 keyset，否则退回 OFFSET）。
pub fn list_requests(
    conn: &rusqlite::Connection,
    query: &RequestQuery<'_>,
) -> Result<Vec<RequestRow>, DbError> {
    let (sql, params) = sql::list_requests(query);
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(params), RequestRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// 单请求详情（含正文与费用明细；正文默认未记录时为 None）。
pub fn request_detail(
    conn: &rusqlite::Connection,
    request_id: i64,
) -> Result<Option<RequestDetailRow>, DbError> {
    Ok(conn
        .query_row(
            &sql::request_detail(),
            params![request_id],
            RequestDetailRow::from_row,
        )
        .optional()?)
}

// ---------------------------------------------------------------------------
// 模型价格
// ---------------------------------------------------------------------------

#[allow(dead_code)] // P3 使用
pub fn list_model_prices(conn: &rusqlite::Connection) -> Result<Vec<ModelPriceRow>, DbError> {
    let sql = format!(
        "SELECT {} FROM model_prices ORDER BY model ASC",
        sql::MODEL_PRICE_COLUMNS
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], ModelPriceRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{run_migrations, Db};
    use rusqlite::types::Value;

    fn test_db() -> Db {
        let db = Db::open_in_memory().expect("内存库应可打开");
        run_migrations(&db).expect("迁移应成功");
        db
    }

    /// `EXPLAIN QUERY PLAN` 的每一行（第 4 列是说明文本）。
    fn query_plan(conn: &rusqlite::Connection, sql: &str, params: Vec<Value>) -> String {
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .expect("EXPLAIN QUERY PLAN 应可准备");
        let rows = stmt
            .query_map(params_from_iter(params), |row| row.get::<_, String>(3))
            .expect("查询计划应可读");
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .expect("查询计划应可收集")
            .join("\n")
    }

    fn seed_request(conn: &rusqlite::Connection, day_local: &str, status_kind: &str, model: &str) {
        conn.execute(
            "INSERT INTO proxy_session_requests \
               (method, request_path, api_format, model, started_at, day_local, status_kind) \
             VALUES ('POST', '/v1/messages', 'anthropic', ?1, ?2, ?3, ?4)",
            params![
                model,
                format!("{day_local}T01:00:00Z"),
                day_local,
                status_kind
            ],
        )
        .expect("插入请求行应成功");
    }

    fn list_query() -> RequestQuery<'static> {
        RequestQuery {
            from_day: "2026-09-01",
            to_day: "2026-10-31",
            limit: 50,
            ..Default::default()
        }
    }

    /// 列表必须走 keyset 主索引做区间扫描：不得全表扫描，也不得建临时排序 B 树。
    #[test]
    fn list_requests_plan_uses_keyset_index() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        for day in ["2026-10-01", "2026-10-02", "2026-10-03"] {
            seed_request(&conn, day, "success", "claude-sonnet-4");
        }
        let (sql, params) = sql::list_requests(&list_query());
        let plan = query_plan(&conn, &sql, params);
        assert!(
            plan.contains("ix_psr_day_started_id"),
            "列表应命中 keyset 索引，实际计划：{plan}"
        );
        assert!(
            !plan.contains("SCAN proxy_session_requests"),
            "列表不得全表扫描：{plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "排序必须由索引满足，不能建临时 B 树：{plan}"
        );
    }

    /// keyset 分页也必须是索引区间扫描：游标谓词若写成不可推导的形式，
    /// 计划会退化成"从索引头扫到游标位置"，翻到深页时和 OFFSET 一样慢。
    #[test]
    fn keyset_plan_uses_index_range() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        for day in ["2026-10-01", "2026-10-02", "2026-10-03"] {
            seed_request(&conn, day, "success", "claude-sonnet-4");
        }
        let cursor = Cursor {
            day_local: "2026-10-02".to_string(),
            started_at: "2026-10-02T01:00:00Z".to_string(),
            id: 2,
        };
        let query = RequestQuery {
            after: Some(&cursor),
            ..list_query()
        };
        let (sql, params) = sql::list_requests(&query);
        let plan = query_plan(&conn, &sql, params);
        assert!(
            plan.contains("ix_psr_day_started_id"),
            "游标分页应命中 keyset 索引，实际计划：{plan}"
        );
        assert!(
            !plan.contains("SCAN proxy_session_requests"),
            "游标分页不得全表扫描：{plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "游标分页不得建临时排序 B 树：{plan}"
        );
    }

    /// 状态筛选必须命中 (status_kind, day_local, ...) 复合索引，而不是逐行比较。
    #[test]
    fn status_filter_plan_uses_composite_index() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        for day in ["2026-10-01", "2026-10-02", "2026-10-03"] {
            seed_request(&conn, day, "success", "claude-sonnet-4");
            seed_request(&conn, day, "error", "claude-sonnet-4");
        }
        let query = RequestQuery {
            status: Some("error"),
            ..list_query()
        };
        let (sql, params) = sql::list_requests(&query);
        let plan = query_plan(&conn, &sql, params);
        assert!(
            plan.contains("ix_psr_status_day_started"),
            "状态筛选应命中复合索引，实际计划：{plan}"
        );
        assert!(
            !plan.contains("SCAN proxy_session_requests"),
            "不得全表扫描：{plan}"
        );
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "不得建临时排序 B 树：{plan}"
        );
    }

    /// 每页的 COUNT(*) 也必须是索引区间扫描（默认筛选是"全部历史"，
    /// 否则每翻一页都要扫全表）。
    #[test]
    fn count_requests_plan_avoids_full_scan() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        seed_request(&conn, "2026-10-01", "success", "claude-sonnet-4");
        let (sql, params) = sql::count_requests(&list_query());
        let plan = query_plan(&conn, &sql, params);
        assert!(
            plan.contains("COVERING INDEX") || plan.contains("USING INDEX"),
            "计数应走索引：{plan}"
        );
        assert!(
            !plan.contains("SCAN proxy_session_requests"),
            "计数不得全表扫描：{plan}"
        );
    }

    /// 详情查询按 id 主键定位，费用走唯一索引；按列名解码，加列不会错位。
    #[test]
    fn request_detail_decodes_by_name() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        seed_request(&conn, "2026-10-20", "success", "claude-sonnet-4");
        let request_id: i64 = conn
            .query_row("SELECT id FROM proxy_session_requests", [], |row| {
                row.get(0)
            })
            .expect("读取 id 应成功");
        conn.execute(
            "INSERT INTO proxy_request_costs \
               (request_id, cost_yuan, cost_source, currency, total_credit, trace_id, created_at) \
             VALUES (?1, 0.25, 'upstream', 'CNY', 25.0, 'trace-1', '2026-10-20T00:00:00Z')",
            params![request_id],
        )
        .expect("插入费用应成功");

        let detail = request_detail(&conn, request_id)
            .expect("详情查询应成功")
            .expect("详情应存在");
        assert_eq!(detail.request.model.as_deref(), Some("claude-sonnet-4"));
        assert_eq!(detail.request.day_local, "2026-10-20");
        assert_eq!(detail.cost_total_credit, Some(25.0));
        assert_eq!(detail.cost_trace_id.as_deref(), Some("trace-1"));
        assert!(!detail.body_truncated);
        assert!(detail.request_body.is_none());
    }

    /// 旧库可能残留同一条请求的多行费用；唯一索引上可以再插一次也不会产生第二行。
    #[test]
    fn cost_rows_are_unique_per_request() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        seed_request(&conn, "2026-10-20", "success", "claude-sonnet-4");
        let request_id: i64 = conn
            .query_row("SELECT id FROM proxy_session_requests", [], |row| {
                row.get(0)
            })
            .expect("读取 id 应成功");
        let insert = "INSERT INTO proxy_request_costs (request_id, cost_yuan, created_at) \
                      VALUES (?1, 1.0, '2026-10-20T00:00:00Z')";
        conn.execute(insert, params![request_id])
            .expect("首次插入应成功");
        assert!(
            conn.execute(insert, params![request_id]).is_err(),
            "同一 request_id 的第二行费用必须被唯一索引拒绝"
        );
    }

    /// 额度快照必须能靠部分索引直接定位"最新一条成功快照"。
    #[test]
    fn latest_quota_plan_uses_partial_index() {
        let db = test_db();
        let conn = db.conn().expect("应能取连接");
        for (index, fetched_at) in ["2026-10-20T00:00:00Z", "2026-10-20T00:00:01Z"]
            .iter()
            .enumerate()
        {
            conn.execute(
                "INSERT INTO quota_snapshots \
                   (quota_type, status, monthly_quota, fetched_at) VALUES ('global', 'ok', ?1, ?2)",
                params![index as f64, fetched_at],
            )
            .expect("插入快照应成功");
        }
        let plan = query_plan(
            &conn,
            "SELECT monthly_quota AS monthly_quota FROM quota_snapshots \
             WHERE quota_type = ? AND status = 'ok' ORDER BY fetched_at DESC, id DESC LIMIT 1",
            vec![Value::Text("global".to_string())],
        );
        assert!(
            plan.contains("ix_quota_snapshots_ok_latest"),
            "额度快照应命中部分索引：{plan}"
        );
        assert!(
            !plan.contains("SCAN quota_snapshots"),
            "不得全表扫描：{plan}"
        );

        let latest = latest_quota_snapshot(&conn, "global")
            .expect("读取应成功")
            .expect("应存在快照");
        assert_eq!(latest.monthly_quota, Some(1.0), "同秒内应按 id 取最新一条");
    }
}
