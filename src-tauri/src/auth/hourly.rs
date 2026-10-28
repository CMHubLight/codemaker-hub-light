//! `/vega/user_hourly_cost` 响应解析（账号云端小时费用）。
//!
//! 字段为 snake_case；同时接受前端旧快照可能出现的 camelCase 别名。
//! `cost` / `tokens` 表示当前小时增量，`total_*` / `app_*` 表示累计值。

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HourlyCost {
    #[serde(default)]
    pub start_time: Option<String>,
    #[serde(default)]
    pub end_time: Option<String>,
    #[serde(default)]
    pub records: Vec<HourlyCostRecord>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct HourlyCostRecord {
    #[serde(default, alias = "snapshotHour")]
    pub snapshot_hour: String,
    #[serde(default)]
    pub cost: f64,
    #[serde(default)]
    pub tokens: Option<i64>,
    #[serde(default, alias = "totalCost")]
    pub total_cost: Option<f64>,
    #[serde(default, alias = "totalTokens")]
    pub total_tokens: Option<i64>,
    #[serde(default, alias = "appTokens")]
    pub app_tokens: BTreeMap<String, i64>,
    #[serde(default, alias = "appCosts")]
    pub app_costs: BTreeMap<String, f64>,
    #[serde(default, alias = "syncedAt")]
    pub synced_at: Option<String>,
}

/// 解析响应；服务端业务错误（HTTP 200 + `error`）返回 `Err`。
pub fn parse_hourly_cost(value: &Value) -> Result<HourlyCost, String> {
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        return Err(error.trim().to_string());
    }
    serde_json::from_value(value.clone()).map_err(|err| format!("无法解析小时费用响应：{err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_measured_snake_case_response() {
        let value = serde_json::json!({
            "start_time": "2026-10-20 00:00:00",
            "end_time": "2026-10-20 20:00:00",
            "total": 2,
            "user": "test_user",
            "records": [
                {
                    "snapshot_hour": "2026-10-20 00:00:00",
                    "tokens": 0,
                    "cost": 0.0,
                    "total_tokens": 154717654,
                    "total_cost": 18.56,
                    "app_tokens": {"_codemaker_cli": 154704264},
                    "app_costs": {"_codemaker_cli": 18.3},
                    "synced_at": "2026-10-20 00:02:06"
                }
            ]
        });
        let parsed = parse_hourly_cost(&value).expect("应解析成功");
        assert_eq!(parsed.records.len(), 1);
        let record = &parsed.records[0];
        assert_eq!(record.snapshot_hour, "2026-10-20 00:00:00");
        assert_eq!(record.cost, 0.0);
        assert_eq!(record.total_tokens, Some(154_717_654));
        assert_eq!(record.app_tokens.get("_codemaker_cli"), Some(&154_704_264));
    }

    #[test]
    fn accepts_camel_case_aliases() {
        let value = serde_json::json!({
            "records": [{
                "snapshotHour": "2026-10-20 01:00:00",
                "cost": 1.25,
                "totalCost": 19.81,
                "appTokens": {"_codemaker_cli": 1000},
                "appCosts": {"_codemaker_cli": 1.25},
                "syncedAt": "2026-10-20 01:02:00"
            }]
        });
        let parsed = parse_hourly_cost(&value).expect("应解析成功");
        assert_eq!(parsed.records[0].snapshot_hour, "2026-10-20 01:00:00");
        assert_eq!(parsed.records[0].total_cost, Some(19.81));
        assert_eq!(
            parsed.records[0].synced_at.as_deref(),
            Some("2026-10-20 01:02:00")
        );
    }

    #[test]
    fn rejects_business_error_body() {
        let value = serde_json::json!({"error": "查询时间范围不能超过 24 小时"});
        let error = parse_hourly_cost(&value).expect_err("业务错误应失败");
        assert!(error.contains("24 小时"));
    }

    #[test]
    fn empty_records_are_valid() {
        let parsed =
            parse_hourly_cost(&serde_json::json!({"records": []})).expect("空记录是合法响应");
        assert!(parsed.records.is_empty());
    }
}
