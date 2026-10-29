//! 用量统计：对外返回结构 + 聚合计算 + 只读查询。
//!
//! 字段与前端 `src/lib/types.ts` 对齐（snake_case）：
//! `UsageSummary { today, month, daily, models, recent, generated_at }`、
//! `RequestPage { total, items }`。指标口径见
//! docs/usage.md；P0 数据库为空时返回零值/空列表。

pub mod aggregate;
pub mod queries;

use std::collections::BTreeMap;

use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

use crate::db::repo::QuotaSnapshotRow;

/// keyset 游标：把上一页返回的 `next_cursor` 原样传回即可。
pub use crate::db::repo::Cursor;

/// UTC 时刻 → 机器本地日期 `YYYY-MM-DD`。
///
/// 全应用只应有这一个「本地日」定义：请求表的 `day_local`、`usage_daily.day`、
/// 以及「今天/本月」窗口都用它，避免 UTC 日与本地日混用。
pub fn local_day(time: DateTime<Utc>) -> String {
    time.with_timezone(&Local).format("%Y-%m-%d").to_string()
}

/// token 四元组 + 费用与比率（前端 `UsageTotals`）。
///
/// 比率在无请求时为 `None`，避免把“无数据”显示成 0。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageTotals {
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub cost_yuan: f64,
    pub cache_hit_rate: Option<f64>,
    pub avg_latency_ms: Option<f64>,
    pub error_rate: Option<f64>,
}

/// 按日趋势（前端 `DailyUsage`）。
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    /// YYYY-MM-DD
    pub date: String,
    pub requests: i64,
    pub tokens: i64,
    pub cost_yuan: f64,
}

/// 模型分布（前端 `ModelUsage`）。
#[derive(Debug, Clone, Serialize)]
pub struct ModelUsage {
    pub model: String,
    pub requests: i64,
    pub tokens: i64,
    pub cost_yuan: f64,
}

/// 请求列表项（前端 `RecentRequest`）。
#[derive(Debug, Clone, Serialize)]
pub struct RecentRequest {
    pub id: String,
    /// ISO 时间字符串。
    pub created_at: String,
    pub model: String,
    /// anthropic | openai_chat | openai_responses | unknown
    pub protocol: String,
    pub client: String,
    /// success | error | cancelled
    pub status: String,
    pub latency_ms: Option<i64>,
    /// 首个上游响应字节的耗时（仅流式请求有值；口径见 docs/usage.md）。
    pub first_byte_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_yuan: f64,
}

/// 概览页汇总（前端 `UsageSummary`）。
#[derive(Debug, Clone, Serialize)]
pub struct UsageSummary {
    pub today: UsageTotals,
    pub month: UsageTotals,
    /// 最近 30 天，按日期升序。
    pub daily: Vec<DailyUsage>,
    pub models: Vec<ModelUsage>,
    pub recent: Vec<RecentRequest>,
    pub generated_at: String,
}

/// 请求分页（前端 `RequestPage`）。
#[derive(Debug, Clone, Serialize)]
pub struct RequestPage {
    /// 满足筛选条件的总条数；`include_total = false` 时为 `-1`（未统计）。
    pub total: i64,
    pub items: Vec<RecentRequest>,
    /// 下一页游标：本页是满页时给出，传回 `after` 即可继续翻页。
    pub next_cursor: Option<Cursor>,
    /// 本页所用游标（首页为 `None`）：前端用游标栈回退，或继续用 `offset`。
    pub prev_cursor: Option<Cursor>,
}

/// 请求页筛选参数（前端 `RequestFilter`）。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RequestFilter {
    /// 起始时间（ISO 日期或日期时间，取前 10 位日期）。
    pub from: Option<String>,
    /// 结束时间（同上，含当天）。
    pub to: Option<String>,
    /// 模型关键字（模糊匹配）。
    pub model: Option<String>,
    /// anthropic | openai_chat | openai_responses
    pub protocol: Option<String>,
    /// 客户端关键字（请求列表暂未关联会话级 client，预留）。
    pub client: Option<String>,
    /// success | error | cancelled
    pub status: Option<String>,
    pub offset: Option<u32>,
    pub limit: Option<u32>,
    /// keyset 游标：把上一页的 `next_cursor` 原样传回；与 `offset` 同时给时游标优先。
    pub after: Option<Cursor>,
    /// 是否返回 `total`（默认 true）。翻页/无限滚动传 false 可省掉每页的
    /// `COUNT(*)`，此时 `total` 为 `-1`。
    pub include_total: Option<bool>,
}

/// 请求详情（P0 提供结构；正文默认未记录时为 None）。
#[derive(Debug, Clone, Serialize)]
pub struct RequestDetail {
    pub request: RecentRequest,
    pub upstream_url: Option<String>,
    pub request_bytes: Option<i64>,
    pub response_bytes: Option<i64>,
    pub first_byte_ms: Option<i64>,
    pub request_headers: Option<String>,
    pub request_body: Option<String>,
    pub response_body: Option<String>,
    pub body_truncated: bool,
    pub ended_at: Option<String>,
    pub cost_source: Option<String>,
    pub cost_total_credit: Option<f64>,
    pub cost_currency: Option<String>,
    pub cost_trace_id: Option<String>,
    pub cost_detail_json: Option<String>,
}

/// 月度额度（前端 `QuotaInfo`）；无快照时命令返回 `null`。
#[derive(Debug, Clone, Serialize)]
pub struct QuotaInfo {
    pub monthly_quota: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percentage: Option<f64>,
    pub reset_day: Option<i64>,
    pub reset_at: Option<String>,
    pub period_start: Option<String>,
    pub is_active: bool,
    pub usage_cost: Option<f64>,
    pub currency: String,
    pub fetched_at: String,
    // ---- 附加字段 ----
    pub status: String,
    pub timezone: Option<String>,
    // ---- 云端月度用量分布 ----
    pub total_tokens: Option<i64>,
    pub app_tokens: BTreeMap<String, i64>,
    pub app_costs: BTreeMap<String, f64>,
    pub model_tokens: BTreeMap<String, i64>,
    pub model_costs: BTreeMap<String, f64>,
}

impl QuotaInfo {
    pub fn from_snapshot(row: QuotaSnapshotRow) -> Self {
        let (total_tokens, app_tokens, app_costs, model_tokens, model_costs, usage_cost) =
            cloud_month_fields(row.raw_json.as_deref());
        Self {
            monthly_quota: row.monthly_quota,
            used: row.used,
            remaining: row.remaining,
            percentage: row.percentage,
            reset_day: row.reset_day,
            reset_at: row.reset_at,
            period_start: row.period_start,
            is_active: row.is_active.unwrap_or(false),
            usage_cost: usage_cost.or(row.used),
            currency: "CNY".to_string(),
            fetched_at: row.fetched_at,
            status: row.status,
            timezone: row.timezone,
            total_tokens,
            app_tokens,
            app_costs,
            model_tokens,
            model_costs,
        }
    }
}

/// 从额度快照原始 JSON 中提取当前月分布字段；任何结构不符都降级为空值。
type CloudMonthFields = (
    Option<i64>,
    BTreeMap<String, i64>,
    BTreeMap<String, f64>,
    BTreeMap<String, i64>,
    BTreeMap<String, f64>,
    Option<f64>,
);

fn cloud_month_fields(raw_json: Option<&str>) -> CloudMonthFields {
    let empty = || {
        (
            None,
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            BTreeMap::new(),
            None,
        )
    };
    let Some(raw) = raw_json else {
        return empty();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return empty();
    };
    let root = value
        .get("data")
        .filter(|inner| inner.is_object())
        .unwrap_or(&value);
    let Some(current) = root
        .get("current_month")
        .or_else(|| root.get("currentMonth"))
    else {
        return empty();
    };
    let total_tokens = current
        .get("total_tokens")
        .or_else(|| current.get("totalTokens"))
        .and_then(number_to_i64);
    let usage_cost = current
        .get("usage_cost")
        .or_else(|| current.get("usageCost"))
        .and_then(serde_json::Value::as_f64)
        .filter(|number| number.is_finite());
    (
        total_tokens,
        int_map(
            current
                .get("app_tokens")
                .or_else(|| current.get("appTokens")),
        ),
        cost_map(current.get("app_costs").or_else(|| current.get("appCosts"))),
        int_map(
            current
                .get("model_tokens")
                .or_else(|| current.get("modelTokens")),
        ),
        cost_map(
            current
                .get("model_costs")
                .or_else(|| current.get("modelCosts")),
        ),
        usage_cost,
    )
}

fn number_to_i64(value: &serde_json::Value) -> Option<i64> {
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|number| number.is_finite())
            .map(|number| number.round() as i64)
    })
}

fn int_map(value: Option<&serde_json::Value>) -> BTreeMap<String, i64> {
    value
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| number_to_i64(value).map(|number| (key.clone(), number)))
                .collect()
        })
        .unwrap_or_default()
}

fn cost_map(value: Option<&serde_json::Value>) -> BTreeMap<String, f64> {
    value
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value
                        .as_f64()
                        .filter(|number| number.is_finite())
                        .map(|number| (key.clone(), number))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 费用曲线的单个点。
#[derive(Debug, Clone, Serialize)]
pub struct CostTrendPoint {
    pub label: String,
    pub at: String,
    pub cost: f64,
    pub tokens: Option<i64>,
}

/// 费用曲线：24h 走云端，30d 走本机用量库。
#[derive(Debug, Clone, Serialize)]
pub struct CostTrend {
    pub range: String,
    pub source: String,
    pub unit: String,
    pub points: Vec<CostTrendPoint>,
    pub total_cost: f64,
    pub peak_cost: f64,
    pub updated_at: Option<String>,
    pub generated_at: String,
    /// 云端积分折算人民币的比例；30d 本机人民币模式不使用。
    pub credit_to_yuan: f64,
}

/// CSV 导出结果（内部）。
#[derive(Debug, Clone, Serialize)]
pub struct CsvExport {
    pub path: String,
    pub rows: usize,
    pub bytes: u64,
    /// 是否因为超过行数上限而截断（只导出到上限为止）。
    pub truncated: bool,
}

/// 导出命令返回（前端 `UsageExportResult`）。
#[derive(Debug, Clone, Serialize)]
pub struct UsageExportResult {
    pub saved: bool,
    pub path: Option<String>,
    pub message: Option<String>,
    // ---- 附加字段 ----
    pub rows: usize,
    pub bytes: u64,
    /// 行数超过上限被截断（前端应提示缩小时间范围）。
    pub truncated: bool,
}
#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(raw_json: Option<String>, used: Option<f64>) -> QuotaSnapshotRow {
        QuotaSnapshotRow {
            monthly_quota: Some(3500.04),
            used,
            remaining: Some(3442.0),
            percentage: Some(1.66),
            reset_day: Some(1),
            period_start: Some("2026-10-01".to_string()),
            reset_at: None,
            timezone: None,
            is_active: Some(true),
            status: "ok".to_string(),
            http_status: Some(200),
            raw_json,
            fetched_at: "2026-10-20T13:23:00Z".to_string(),
        }
    }

    #[test]
    fn from_snapshot_restores_cloud_month_fields() {
        let raw = include_str!("../../tests/fixtures/auth/quota_nested.json").to_string();
        let info = QuotaInfo::from_snapshot(snapshot(Some(raw), Some(58.04)));
        assert_eq!(info.total_tokens, Some(802_378_086));
        assert_eq!(info.usage_cost, Some(58.04));
        assert_eq!(info.app_tokens.len(), 1);
        assert_eq!(info.model_tokens.len(), 2);
        assert_eq!(info.model_costs.get("deepseek-flash"), Some(&55.25));
    }

    #[test]
    fn from_snapshot_without_raw_json_falls_back_to_used() {
        let info = QuotaInfo::from_snapshot(snapshot(None, Some(12.5)));
        assert_eq!(info.total_tokens, None);
        assert!(info.model_tokens.is_empty());
        assert_eq!(info.usage_cost, Some(12.5));
    }
}
