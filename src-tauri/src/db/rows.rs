//! 行结构体与解码：全部按**列名**读取（SQL 里一律 `AS <字段名>`）。
//!
//! 不使用 `row.get(0)` 这类位置下标：SQL 由 `db::sql` 拼装，列一增删位置就漂移
//! （历史上 `request_detail` 用 17..29 的硬编码下标对接 30 列，加一列即错位，
//! 而且错误只在运行时以"类型不匹配"暴露）。

use rusqlite::Row;

/// 行解码统一接口：`stmt.query_map(params, T::from_row)`。
pub trait FromRow: Sized {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self>;
}

// ---------------------------------------------------------------------------
// 账号档案 / 额度快照
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct AuthProfileRow {
    pub id: i64,
    pub user: String,
    pub display_name: Option<String>,
    pub dept: Option<String>,
    pub server_url: String,
    pub is_active: bool,
    pub issued_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub validated_at: Option<i64>,
    pub models_json: Option<String>,
}

impl FromRow for AuthProfileRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            user: row.get("user")?,
            display_name: row.get("display_name")?,
            dept: row.get("dept")?,
            server_url: row.get("server_url")?,
            is_active: row.get::<_, i64>("is_active")? != 0,
            issued_at: row.get("issued_at")?,
            expires_at: row.get("expires_at")?,
            validated_at: row.get("validated_at")?,
            models_json: row.get("models_json")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct QuotaSnapshotRow {
    pub monthly_quota: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percentage: Option<f64>,
    pub reset_day: Option<i64>,
    pub period_start: Option<String>,
    pub reset_at: Option<String>,
    pub timezone: Option<String>,
    pub is_active: Option<bool>,
    pub status: String,
    pub http_status: Option<i64>,
    pub raw_json: Option<String>,
    pub fetched_at: String,
}

impl FromRow for QuotaSnapshotRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            monthly_quota: row.get("monthly_quota")?,
            used: row.get("used")?,
            remaining: row.get("remaining")?,
            percentage: row.get("percentage")?,
            reset_day: row.get("reset_day")?,
            period_start: row.get("period_start")?,
            reset_at: row.get("reset_at")?,
            timezone: row.get("timezone")?,
            is_active: row
                .get::<_, Option<i64>>("is_active")?
                .map(|value| value != 0),
            status: row.get("status")?,
            http_status: row.get("http_status")?,
            raw_json: row.get("raw_json")?,
            fetched_at: row.get("fetched_at")?,
        })
    }
}

// ---------------------------------------------------------------------------
// 用量聚合
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct UsageTotalsRow {
    pub request_count: i64,
    pub error_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_yuan: f64,
    pub total_duration_ms: i64,
}

impl FromRow for UsageTotalsRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            request_count: row.get("request_count")?,
            error_count: row.get("error_count")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cache_write_tokens: row.get("cache_write_tokens")?,
            cost_yuan: row.get("cost_yuan")?,
            total_duration_ms: row.get("total_duration_ms")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct UsageDayRow {
    pub day: String,
    pub totals: UsageTotalsRow,
}

impl FromRow for UsageDayRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            day: row.get("day")?,
            totals: UsageTotalsRow::from_row(row)?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct UsageDimRow {
    pub key: String,
    pub totals: UsageTotalsRow,
}

impl FromRow for UsageDimRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            key: row.get("key")?,
            totals: UsageTotalsRow::from_row(row)?,
        })
    }
}

/// CSV 导出的一行（`usage_daily` 原始维度行）。
#[derive(Debug, Clone)]
pub struct UsageExportRow {
    pub day: String,
    pub api_format: String,
    pub model: String,
    pub client: String,
    pub request_count: i64,
    pub error_count: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub cost_yuan: f64,
}

impl FromRow for UsageExportRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            day: row.get("day")?,
            api_format: row.get("api_format")?,
            model: row.get("model")?,
            client: row.get("client")?,
            request_count: row.get("request_count")?,
            error_count: row.get("error_count")?,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cache_write_tokens: row.get("cache_write_tokens")?,
            cost_yuan: row.get("cost_yuan")?,
        })
    }
}

// ---------------------------------------------------------------------------
// 请求明细
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct RequestRow {
    pub id: i64,
    pub started_at: String,
    /// 请求开始时间所属的**本地日**（落库列），keyset 游标与日期筛选都用它。
    pub day_local: String,
    pub request_path: String,
    pub api_format: String,
    pub model: Option<String>,
    pub status_code: Option<i64>,
    pub is_sse: bool,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub duration_ms: Option<i64>,
    pub error_kind: Option<String>,
    pub session_id: Option<i64>,
    pub client_request_id: Option<String>,
    pub cost_yuan: Option<f64>,
    pub cost_source: Option<String>,
}

impl FromRow for RequestRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            started_at: row.get("started_at")?,
            day_local: row.get("day_local")?,
            request_path: row.get("request_path")?,
            api_format: row.get("api_format")?,
            model: row.get("model")?,
            status_code: row.get("status_code")?,
            is_sse: row.get::<_, i64>("is_sse")? != 0,
            input_tokens: row.get("input_tokens")?,
            output_tokens: row.get("output_tokens")?,
            cache_read_tokens: row.get("cache_read_tokens")?,
            cache_write_tokens: row.get("cache_write_tokens")?,
            duration_ms: row.get("duration_ms")?,
            error_kind: row.get("error_kind")?,
            session_id: row.get("session_id")?,
            client_request_id: row.get("client_request_id")?,
            cost_yuan: row.get("cost_yuan")?,
            cost_source: row.get("cost_source")?,
        })
    }
}

#[derive(Debug, Clone)]
pub struct RequestDetailRow {
    pub request: RequestRow,
    pub upstream_url: Option<String>,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub first_byte_ms: Option<i64>,
    pub request_headers: Option<String>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub body_truncated: bool,
    pub ended_at: Option<String>,
    pub cost_currency: Option<String>,
    pub cost_total_credit: Option<f64>,
    pub cost_trace_id: Option<String>,
    pub cost_detail_json: Option<String>,
}

impl FromRow for RequestDetailRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            request: RequestRow::from_row(row)?,
            upstream_url: row.get("upstream_url")?,
            request_bytes: row.get("request_bytes")?,
            response_bytes: row.get("response_bytes")?,
            first_byte_ms: row.get("first_byte_ms")?,
            request_headers: row.get("request_headers")?,
            request_body: row.get("request_body")?,
            response_body: row.get("response_body")?,
            body_truncated: row.get::<_, i64>("body_truncated")? != 0,
            ended_at: row.get("ended_at")?,
            cost_currency: row.get("cost_currency")?,
            cost_total_credit: row.get("cost_total_credit")?,
            cost_trace_id: row.get("cost_trace_id")?,
            cost_detail_json: row.get("cost_detail_json")?,
        })
    }
}

// ---------------------------------------------------------------------------
// 模型价格
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // P3 估算费用时使用
pub struct ModelPriceRow {
    pub model: String,
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
    pub cache_read_per_mtok: f64,
    pub cache_write_per_mtok: f64,
    pub currency: String,
    pub source: String,
    pub effective_from: Option<String>,
    pub updated_at: String,
}

impl FromRow for ModelPriceRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            model: row.get("model")?,
            input_per_mtok: row.get("input_per_mtok")?,
            output_per_mtok: row.get("output_per_mtok")?,
            cache_read_per_mtok: row.get("cache_read_per_mtok")?,
            cache_write_per_mtok: row.get("cache_write_per_mtok")?,
            currency: row.get("currency")?,
            source: row.get("source")?,
            effective_from: row.get("effective_from")?,
            updated_at: row.get("updated_at")?,
        })
    }
}
