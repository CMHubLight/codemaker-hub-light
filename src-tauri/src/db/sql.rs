//! SQL 文本与查询参数构造（不含行解码）。
//!
//! 约定：
//! - 所有查询列一律 `AS <字段名>`，由 `db::rows::FromRow` 按**列名**读取，避免
//!   位置下标与 `format!` 拼出的列序耦合（历史上 `request_detail` 用 17..29
//!   硬编码下标，加一列就整段错位）。
//! - 动态 SQL 只拼接本文件内的白名单片段；任何外部字符串都只能作为参数绑定。
//! - 可选筛选**按需拼接**，不用 `(?N IS NULL OR col = ?N)`：后者会让 SQLite
//!   放弃在该列上使用索引，所有行都被逐行比较（列表查询的主要退化来源）。

use rusqlite::types::Value;

/// 账号档案列（`AS` 名与 `AuthProfileRow` 字段一致）。
pub const AUTH_PROFILE_COLUMNS: &str = "id AS id, user AS user, display_name AS display_name, \
     dept AS dept, server_url AS server_url, is_active AS is_active, issued_at AS issued_at, \
     expires_at AS expires_at, validated_at AS validated_at, models_json AS models_json";

/// 额度快照列。
pub const QUOTA_SNAPSHOT_COLUMNS: &str = "monthly_quota AS monthly_quota, used AS used, \
     remaining AS remaining, percentage AS percentage, reset_day AS reset_day, \
     period_start AS period_start, reset_at AS reset_at, timezone AS timezone, \
     is_active AS is_active, status AS status, http_status AS http_status, \
     raw_json AS raw_json, fetched_at AS fetched_at";

/// 模型价格列。
pub const MODEL_PRICE_COLUMNS: &str = "model AS model, input_per_mtok AS input_per_mtok, \
     output_per_mtok AS output_per_mtok, cache_read_per_mtok AS cache_read_per_mtok, \
     cache_write_per_mtok AS cache_write_per_mtok, currency AS currency, source AS source, \
     effective_from AS effective_from, updated_at AS updated_at";

/// CSV 导出的 `usage_daily` 维度列。
pub const USAGE_EXPORT_COLUMNS: &str = "day AS day, api_format AS api_format, model AS model, \
     client AS client, request_count AS request_count, error_count AS error_count, \
     input_tokens AS input_tokens, output_tokens AS output_tokens, \
     cache_read_tokens AS cache_read_tokens, cache_write_tokens AS cache_write_tokens, \
     cost_yuan AS cost_yuan";

/// `usage_daily` 的 8 个聚合列。
///
/// 只在这里写一次：总量 / 按日 / 按维度三处查询共用，口径不会再各自漂移。
pub const TOTALS_AGG: &str = "COALESCE(SUM(request_count), 0) AS request_count, \
     COALESCE(SUM(error_count), 0) AS error_count, \
     COALESCE(SUM(input_tokens), 0) AS input_tokens, \
     COALESCE(SUM(output_tokens), 0) AS output_tokens, \
     COALESCE(SUM(cache_read_tokens), 0) AS cache_read_tokens, \
     COALESCE(SUM(cache_write_tokens), 0) AS cache_write_tokens, \
     COALESCE(SUM(cost_yuan), 0.0) AS cost_yuan, \
     COALESCE(SUM(total_duration_ms), 0) AS total_duration_ms";

/// 请求列表的基础列（含费用 LEFT JOIN 的两列）。
pub const REQUEST_COLUMNS: &str = "r.id AS id, r.started_at AS started_at, \
     r.day_local AS day_local, r.request_path AS request_path, r.api_format AS api_format, \
     r.model AS model, r.status_code AS status_code, r.is_sse AS is_sse, \
     r.input_tokens AS input_tokens, r.output_tokens AS output_tokens, \
     r.cache_read_tokens AS cache_read_tokens, r.cache_write_tokens AS cache_write_tokens, \
     r.duration_ms AS duration_ms, r.error_kind AS error_kind, r.session_id AS session_id, \
     r.client_request_id AS client_request_id, c.cost_yuan AS cost_yuan, \
     c.cost_source AS cost_source";

/// 单请求详情追加的列（正文与费用明细）。
pub const REQUEST_DETAIL_COLUMNS: &str = "r.upstream_url AS upstream_url, \
     r.request_bytes AS request_bytes, r.response_bytes AS response_bytes, \
     r.first_byte_ms AS first_byte_ms, r.request_headers AS request_headers, \
     r.request_body AS request_body, r.response_body AS response_body, \
     r.body_truncated AS body_truncated, r.ended_at AS ended_at, \
     c.currency AS cost_currency, c.total_credit AS cost_total_credit, \
     c.trace_id AS cost_trace_id, c.detail_json AS cost_detail_json";

/// 请求列表的数据来源（费用缺行时 LEFT JOIN 给出 NULL，不影响列表）。
pub const REQUEST_FROM: &str = "FROM proxy_session_requests r \
     LEFT JOIN proxy_request_costs c ON c.request_id = r.id";

/// 请求列表的排序键。
///
/// `day_local` 是 `started_at` 的单调函数（同一天内按 started_at 递增），所以
/// "先按本地日、再按开始时间"与"直接按开始时间"是同一个全序；把 `day_local`
/// 放在最前面才能命中 `ix_psr_day_started_id`，跨日区间不必再做临时 B 树排序。
/// `id` 是最后一级 tie-break，保证 keyset 游标严格唯一。
pub const REQUEST_ORDER_BY: &str = "r.day_local DESC, r.started_at DESC, r.id DESC";

/// 用量维度（列名白名单，禁止外部拼接 SQL 标识符）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageDimension {
    ApiFormat,
    Model,
    Client,
}

impl UsageDimension {
    pub fn column(self) -> &'static str {
        match self {
            UsageDimension::ApiFormat => "api_format",
            UsageDimension::Model => "model",
            UsageDimension::Client => "client",
        }
    }
}

/// 请求列表分页游标（keyset）：上一页最后一行的排序键。
///
/// 必须把 `day_local` 一起带回，不能由 `started_at` 现场重算：`day_local` 是
/// 落库值，历史行按升级当时的机器时区回填（见 0003），重算会与库里的值不一致，
/// 导致翻页跳行或重复。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Cursor {
    pub day_local: String,
    pub started_at: String,
    pub id: i64,
}

/// 请求查询条件（日期区间 + 可选过滤 + 分页）。
#[derive(Debug, Clone)]
pub struct RequestQuery<'a> {
    /// 起始日（含，YYYY-MM-DD，本地日）。
    pub from_day: &'a str,
    /// 结束日（含，YYYY-MM-DD，本地日）。
    pub to_day: &'a str,
    /// 模型模糊匹配（`%value%`；前导通配符无法走索引，只能区间内逐行过滤）。
    pub model_like: Option<&'a str>,
    /// 协议精确匹配（anthropic|openai_chat|openai_responses）。
    pub api_format: Option<&'a str>,
    /// 状态精确匹配（success|error|cancelled|unknown）。
    pub status: Option<&'a str>,
    /// keyset 游标；`None` 表示第一页。与 `offset` 同时给时游标优先。
    pub after: Option<&'a Cursor>,
    pub limit: u32,
    pub offset: u32,
}

impl<'a> Default for RequestQuery<'a> {
    fn default() -> Self {
        Self {
            from_day: "0000-01-01",
            to_day: "9999-12-31",
            model_like: None,
            api_format: None,
            status: None,
            after: None,
            limit: 50,
            offset: 0,
        }
    }
}

/// 构造 `WHERE` 子句与按位对应的参数。
fn where_clause(query: &RequestQuery<'_>, cursor: Option<&Cursor>) -> (String, Vec<Value>) {
    let mut sql = String::from(" WHERE r.day_local >= ? AND r.day_local <= ?");
    let mut params = vec![
        Value::Text(query.from_day.to_string()),
        Value::Text(query.to_day.to_string()),
    ];
    if let Some(model) = query.model_like {
        sql.push_str(" AND r.model LIKE ?");
        params.push(Value::Text(format!("%{model}%")));
    }
    if let Some(api_format) = query.api_format {
        sql.push_str(" AND r.api_format = ?");
        params.push(Value::Text(api_format.to_string()));
    }
    if let Some(status) = query.status {
        sql.push_str(" AND r.status_kind = ?");
        params.push(Value::Text(status.to_string()));
    }
    if let Some(cursor) = cursor {
        // 行值比较（SQLite >= 3.15）：严格小于上一页最后一行的排序键。
        sql.push_str(" AND (r.day_local, r.started_at, r.id) < (?, ?, ?)");
        params.push(Value::Text(cursor.day_local.clone()));
        params.push(Value::Text(cursor.started_at.clone()));
        params.push(Value::Integer(cursor.id));
    }
    (sql, params)
}

/// 满足条件的请求总数（不含游标：游标只决定取哪一页）。
pub fn count_requests(query: &RequestQuery<'_>) -> (String, Vec<Value>) {
    let (where_sql, params) = where_clause(query, None);
    (
        format!("SELECT COUNT(*) FROM proxy_session_requests r{where_sql}"),
        params,
    )
}

/// 分页请求列表：keyset 优先，无游标时保留 OFFSET（前端 Phase 4 才切游标）。
pub fn list_requests(query: &RequestQuery<'_>) -> (String, Vec<Value>) {
    let (where_sql, mut params) = where_clause(query, query.after);
    let mut sql = format!(
        "SELECT {REQUEST_COLUMNS} {REQUEST_FROM}{where_sql} \
         ORDER BY {REQUEST_ORDER_BY} LIMIT ?"
    );
    params.push(Value::Integer(i64::from(query.limit)));
    if query.after.is_none() && query.offset > 0 {
        sql.push_str(" OFFSET ?");
        params.push(Value::Integer(i64::from(query.offset)));
    }
    (sql, params)
}

/// 单请求详情（正文默认未记录时为 NULL）。
pub fn request_detail() -> String {
    format!(
        "SELECT {REQUEST_COLUMNS}, {REQUEST_DETAIL_COLUMNS} {REQUEST_FROM} \
         WHERE r.id = ? LIMIT 1"
    )
}

/// 日区间总量汇总。
pub fn usage_totals() -> String {
    format!("SELECT {TOTALS_AGG} FROM usage_daily WHERE day >= ? AND day <= ?")
}

/// 日区间按日汇总。
pub fn usage_by_day() -> String {
    format!(
        "SELECT day AS day, {TOTALS_AGG} FROM usage_daily WHERE day >= ? AND day <= ? \
         GROUP BY day ORDER BY day ASC"
    )
}

/// 日区间按维度汇总（列名来自 [`UsageDimension`] 白名单）。
pub fn usage_by_dimension(dimension: UsageDimension) -> String {
    let column = dimension.column();
    format!(
        "SELECT {column} AS key, {TOTALS_AGG} FROM usage_daily WHERE day >= ? AND day <= ? \
         GROUP BY {column} ORDER BY cost_yuan DESC, request_count DESC"
    )
}

/// CSV 导出的原始维度行。
///
/// `ORDER BY day ASC, id ASC` 是"稳定且能走索引"的导出顺序（逐行流式写盘，不需要
/// 排序缓冲）；旧实现按 `cost_yuan DESC` 排，会为任意区间建临时 B 树。
pub fn usage_export() -> String {
    format!(
        "SELECT {USAGE_EXPORT_COLUMNS} FROM usage_daily WHERE day >= ? AND day <= ? \
         ORDER BY day ASC, id ASC"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query() -> RequestQuery<'static> {
        RequestQuery {
            from_day: "2026-10-01",
            to_day: "2026-10-31",
            ..Default::default()
        }
    }

    #[test]
    fn where_clause_only_emits_active_filters() {
        let (sql, params) = count_requests(&query());
        assert_eq!(
            sql,
            "SELECT COUNT(*) FROM proxy_session_requests r \
             WHERE r.day_local >= ? AND r.day_local <= ?"
        );
        assert_eq!(params.len(), 2, "没有筛选时只绑日期区间");

        let (sql, params) = count_requests(&RequestQuery {
            status: Some("error"),
            api_format: Some("anthropic"),
            model_like: Some("claude"),
            ..query()
        });
        assert!(sql.contains("r.model LIKE ?"));
        assert!(sql.contains("r.api_format = ?"));
        assert!(sql.contains("r.status_kind = ?"));
        assert!(!sql.contains("IS NULL OR"), "不得退化成可选过滤写法");
        assert_eq!(params.len(), 5);
        assert_eq!(params[2], Value::Text("%claude%".to_string()));
    }

    #[test]
    fn list_uses_keyset_cursor_and_skips_offset() {
        let cursor = Cursor {
            day_local: "2026-10-20".to_string(),
            started_at: "2026-10-20T10:00:00Z".to_string(),
            id: 42,
        };
        let (sql, params) = list_requests(&RequestQuery {
            after: Some(&cursor),
            offset: 100, // 游标优先，offset 必须被忽略
            limit: 50,
            ..query()
        });
        assert!(sql.contains("ORDER BY r.day_local DESC, r.started_at DESC, r.id DESC"));
        assert!(
            sql.contains("(r.day_local, r.started_at, r.id) < (?, ?, ?)"),
            "keyset 必须用行值比较：{sql}"
        );
        assert!(!sql.contains("OFFSET"), "有游标时不得再用 OFFSET");
        assert_eq!(params.len(), 2 + 3 + 1);
        assert_eq!(params[5], Value::Integer(50));

        let (sql, _) = list_requests(&RequestQuery {
            offset: 100,
            limit: 50,
            ..query()
        });
        assert!(
            sql.ends_with("LIMIT ? OFFSET ?"),
            "无游标时保留 offset：{sql}"
        );
    }

    #[test]
    fn count_ignores_cursor() {
        let cursor = Cursor {
            day_local: "2026-10-20".to_string(),
            started_at: "2026-10-20T10:00:00Z".to_string(),
            id: 42,
        };
        let (sql, params) = count_requests(&RequestQuery {
            after: Some(&cursor),
            ..query()
        });
        assert!(!sql.contains("< (?, ?, ?)"), "总数不应受游标影响");
        assert_eq!(params.len(), 2);
    }
}
