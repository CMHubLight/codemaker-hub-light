//! 月度额度响应解析与别名映射。
//!
//! 契约：`GET {gateway}/api/v1/user_quota/usage?quota_type=global`（docs/gateway-contract.md）。
//! 服务端字段名存在多种写法，这里统一映射到 [`crate::usage::QuotaInfo`]；
//! 原始 JSON 由调用方写入 `quota_snapshots.raw_json` 以便字段改名时排查。

use std::collections::BTreeMap;

use chrono::{SecondsFormat, Utc};
use serde_json::Value;

use crate::{
    auth::client::{bool_field, int_field, numeric_field, string_field, unwrap_data},
    db::repo::NewQuotaSnapshot,
    usage::QuotaInfo,
};

/// 解析后的额度数据（别名归一）。
#[derive(Debug, Clone, Default)]
pub struct QuotaData {
    pub monthly_quota: Option<f64>,
    pub used: Option<f64>,
    pub remaining: Option<f64>,
    pub percentage: Option<f64>,
    pub reset_day: Option<i64>,
    pub period_start: Option<String>,
    pub reset_at: Option<String>,
    pub timezone: Option<String>,
    pub is_active: Option<bool>,
    pub usage_cost: Option<f64>,
    pub total_tokens: Option<i64>,
    pub app_tokens: BTreeMap<String, i64>,
    pub app_costs: BTreeMap<String, f64>,
    pub model_tokens: BTreeMap<String, i64>,
    pub model_costs: BTreeMap<String, f64>,
    pub raw: Value,
}

impl QuotaData {
    /// 转成前端 `QuotaInfo`；`fetched_at` 使用当前时间。
    pub fn to_quota_info(&self) -> QuotaInfo {
        QuotaInfo {
            monthly_quota: self.monthly_quota,
            used: self.used,
            remaining: self.remaining,
            percentage: self.percentage,
            reset_day: self.reset_day,
            reset_at: self.reset_at.clone(),
            period_start: self.period_start.clone(),
            is_active: self.is_active.unwrap_or(false),
            usage_cost: self.usage_cost,
            currency: "CNY".to_string(),
            fetched_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
            status: "ok".to_string(),
            timezone: self.timezone.clone(),
            total_tokens: self.total_tokens,
            app_tokens: self.app_tokens.clone(),
            app_costs: self.app_costs.clone(),
            model_tokens: self.model_tokens.clone(),
            model_costs: self.model_costs.clone(),
        }
    }

    /// 转成数据库写入结构；`raw_json` 由调用方提供（所有权在外层）。
    pub fn to_snapshot<'a>(
        &'a self,
        profile_id: Option<i64>,
        raw_json: &'a str,
    ) -> NewQuotaSnapshot<'a> {
        NewQuotaSnapshot {
            profile_id,
            quota_type: "global",
            status: "ok",
            http_status: Some(200),
            monthly_quota: self.monthly_quota,
            used: self.used,
            remaining: self.remaining,
            percentage: self.percentage,
            reset_day: self.reset_day,
            period_start: self.period_start.as_deref(),
            reset_at: self.reset_at.as_deref(),
            timezone: self.timezone.as_deref(),
            is_active: self.is_active,
            raw_json: Some(raw_json),
        }
    }
}

/// 解析额度响应，兼容 `{"data": {...}}` 包裹、扁平字段与嵌套形态。
///
/// 本机真实响应（2026-10-20）没有顶层 `used/remaining`，而是：
/// ```json
/// {
///   "quota_config": {"is_active": true, "monthly_quota": 3500.04, "reset_day": 1},
///   "current_month": {"usage_cost": 58.04, "remaining": 3442.0, "percentage": 1.66,
///                     "year": 2026, "month": 10},
///   "last_month": {...}, "prev_month": {...}, "last_updated_at": "..."
/// }
/// ```
pub fn parse_quota(value: &Value) -> QuotaData {
    let data = unwrap_data(value);
    let quota_config = data.get("quota_config");
    let current = data
        .get("current_month")
        .or_else(|| data.get("currentMonth"));

    let monthly_quota = numeric_field(
        data,
        &[
            "monthly_quota",
            "monthlyQuota",
            "monthly_total",
            "monthlyTotal",
            "total",
            "quota",
        ],
    )
    .or_else(|| quota_config.and_then(|cfg| numeric_field(cfg, &["monthly_quota", "monthlyQuota"])))
    .or_else(|| current.and_then(|cur| numeric_field(cur, &["monthly_quota", "monthlyQuota"])));

    let remaining = numeric_field(
        data,
        &[
            "remaining",
            "remain",
            "remaining_quota",
            "remainingQuota",
            "left",
        ],
    )
    .or_else(|| current.and_then(|cur| numeric_field(cur, &["remaining", "remain"])));

    let usage_cost = numeric_field(data, &["usage_cost", "usageCost", "cost"]).or_else(|| {
        current.and_then(|cur| numeric_field(cur, &["usage_cost", "usageCost", "cost"]))
    });

    // 响应没有顶层 used：优先 usage_cost，其次 monthly_quota - remaining。
    let used = numeric_field(
        data,
        &["used", "usage_cost", "usageCost", "used_quota", "usedQuota"],
    )
    .or(usage_cost)
    .or_else(|| match (monthly_quota, remaining) {
        (Some(total), Some(left)) => Some((total - left).max(0.0)),
        _ => None,
    });

    let percentage = numeric_field(
        data,
        &["percentage", "percent", "used_percentage", "usedPercentage"],
    )
    .or_else(|| current.and_then(|cur| numeric_field(cur, &["percentage", "percent"])));

    let reset_day = int_field(data, &["reset_day", "resetDay"])
        .or_else(|| quota_config.and_then(|cfg| int_field(cfg, &["reset_day", "resetDay"])));

    let is_active = bool_field(data, &["is_active", "isActive", "active"])
        .or_else(|| quota_config.and_then(|cfg| bool_field(cfg, &["is_active", "isActive"])));

    let period_start = string_field(
        data,
        &["period_start", "periodStart", "start", "period_start_at"],
    )
    .or_else(|| {
        let year = current.and_then(|cur| int_field(cur, &["year"]))?;
        let month = current.and_then(|cur| int_field(cur, &["month"]))?;
        Some(format!("{year:04}-{month:02}-01"))
    });

    let total_tokens = current.and_then(|cur| int_field(cur, &["total_tokens", "totalTokens"]));
    let app_tokens = current
        .map(|cur| int_map(cur.get("app_tokens").or_else(|| cur.get("appTokens"))))
        .unwrap_or_default();
    let app_costs = current
        .map(|cur| cost_map(cur.get("app_costs").or_else(|| cur.get("appCosts"))))
        .unwrap_or_default();
    let model_tokens = current
        .map(|cur| int_map(cur.get("model_tokens").or_else(|| cur.get("modelTokens"))))
        .unwrap_or_default();
    let model_costs = current
        .map(|cur| cost_map(cur.get("model_costs").or_else(|| cur.get("modelCosts"))))
        .unwrap_or_default();

    QuotaData {
        monthly_quota,
        used,
        remaining,
        percentage,
        reset_day,
        period_start,
        reset_at: string_field(data, &["reset_at", "resetAt", "reset_time", "resetTime"]),
        timezone: string_field(data, &["timezone", "tz"]),
        is_active,
        usage_cost,
        total_tokens,
        app_tokens,
        app_costs,
        model_tokens,
        model_costs,
        raw: value.clone(),
    }
}

fn int_map(value: Option<&Value>) -> BTreeMap<String, i64> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value
                        .as_i64()
                        .or_else(|| {
                            value
                                .as_f64()
                                .filter(|number| number.is_finite())
                                .map(|number| number.round() as i64)
                        })
                        .map(|number| (key.clone(), number))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cost_map(value: Option<&Value>) -> BTreeMap<String, f64> {
    value
        .and_then(Value::as_object)
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

#[cfg(test)]
mod tests {
    use super::*;

    const QUOTA: &str = include_str!("../../tests/fixtures/auth/quota.json");
    const QUOTA_CAMEL: &str = include_str!("../../tests/fixtures/auth/quota_camel.json");
    const QUOTA_NESTED: &str = include_str!("../../tests/fixtures/auth/quota_nested.json");

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture 应为合法 JSON")
    }

    #[test]
    fn parses_snake_case_quota() {
        let data = parse_quota(&parse(QUOTA));
        assert_eq!(data.monthly_quota, Some(10_000_000.0));
        assert_eq!(data.used, Some(2_500_000.0));
        assert_eq!(data.remaining, Some(7_500_000.0));
        assert_eq!(data.percentage, Some(25.0));
        assert_eq!(data.reset_day, Some(1));
        assert_eq!(data.period_start.as_deref(), Some("2026-10-01"));
        assert_eq!(data.reset_at.as_deref(), Some("2026-11-01T00:00:00Z"));
        assert_eq!(data.timezone.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(data.is_active, Some(true));
        assert_eq!(data.usage_cost, Some(3.5));

        let info = data.to_quota_info();
        assert_eq!(info.monthly_quota, Some(10_000_000.0));
        assert!(info.is_active);
        assert_eq!(info.currency, "CNY");
        assert_eq!(info.status, "ok");
    }

    #[test]
    fn parses_camel_case_quota_with_data_wrapper() {
        let data = parse_quota(&parse(QUOTA_CAMEL));
        assert_eq!(data.monthly_quota, Some(500.0));
        assert_eq!(data.used, Some(125.0));
        assert_eq!(data.remaining, Some(375.0));
        assert_eq!(data.percentage, Some(25.0));
        assert_eq!(data.reset_day, Some(15));
        assert_eq!(data.is_active, Some(true));
        assert_eq!(data.period_start.as_deref(), Some("2026-10-15"));
        assert_eq!(data.reset_at.as_deref(), Some("2026-11-15T00:00:00Z"));
    }

    #[test]
    fn parses_measured_nested_quota_shape() {
        let data = parse_quota(&parse(QUOTA_NESTED));
        assert_eq!(data.monthly_quota, Some(3500.04));
        assert_eq!(data.used, Some(58.04));
        assert_eq!(data.remaining, Some(3442.0));
        assert_eq!(data.percentage, Some(1.66));
        assert_eq!(data.reset_day, Some(1));
        assert_eq!(data.period_start.as_deref(), Some("2026-10-01"));
        assert_eq!(data.is_active, Some(true));
        // 3500.04 - 3442.0 = 58.04，与 usage_cost 一致。
        assert_eq!(data.usage_cost, Some(58.04));
        assert_eq!(data.total_tokens, Some(802_378_086));
        assert_eq!(data.app_tokens.get("_codemaker_cli"), Some(&802_364_696));
        assert_eq!(data.model_tokens.len(), 2);
        assert_eq!(data.model_costs.get("deepseek-flash"), Some(&55.25));

        let info = data.to_quota_info();
        assert!(info.is_active);
        assert_eq!(info.currency, "CNY");
        assert_eq!(info.total_tokens, Some(802_378_086));
        assert_eq!(info.model_tokens.len(), 2);
    }

    #[test]
    fn missing_fields_stay_none_and_do_not_fake_zero() {
        let data = parse_quota(&parse(r#"{}"#));
        assert_eq!(data.monthly_quota, None);
        assert_eq!(data.used, None);
        assert_eq!(data.remaining, None);
        assert_eq!(data.is_active, None);
        let info = data.to_quota_info();
        assert!(!info.is_active);
        assert_eq!(info.total_tokens, None);
        assert!(info.model_tokens.is_empty());
    }

    #[test]
    fn snapshot_uses_raw_json_and_alias_values() {
        let data = parse_quota(&parse(QUOTA));
        let raw = serde_json::to_string(&data.raw).expect("序列化应成功");
        let snapshot = data.to_snapshot(Some(7), &raw);
        assert_eq!(snapshot.profile_id, Some(7));
        assert_eq!(snapshot.quota_type, "global");
        assert_eq!(snapshot.status, "ok");
        assert_eq!(snapshot.used, Some(2_500_000.0));
        assert!(snapshot.raw_json.is_some());
    }
}
