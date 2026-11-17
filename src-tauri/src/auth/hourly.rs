//! `/vega/user_hourly_cost` 响应解析（账号云端小时费用）。
//!
//! 字段为 snake_case；同时接受前端旧快照可能出现的 camelCase 别名。
//! `cost` / `tokens` 表示当前小时增量，`total_*` / `app_*` 表示累计值。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// 云端字段容错：`null`、字符串数字都按"缺失/可解析值"处理，单条坏记录不拖垮整段曲线。
fn as_f64_value(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| {
        value
            .as_str()
            .and_then(|text| text.trim().parse::<f64>().ok())
    })
}

fn as_i64_value(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
}

fn de_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(Value::String(text)) => text,
        Some(Value::Number(number)) => number.to_string(),
        _ => String::new(),
    })
}

fn de_f64<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(as_f64_value).unwrap_or(0.0))
}

fn de_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(as_f64_value))
}

fn de_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(as_i64_value))
}

fn de_map_i64<'de, D>(deserializer: D) -> Result<BTreeMap<String, i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| as_i64_value(value).map(|number| (key.clone(), number)))
                .collect()
        })
        .unwrap_or_default())
}

fn de_map_f64<'de, D>(deserializer: D) -> Result<BTreeMap<String, f64>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(Value::as_object)
        .map(|object| {
            object
                .iter()
                .filter_map(|(key, value)| as_f64_value(value).map(|number| (key.clone(), number)))
                .collect()
        })
        .unwrap_or_default())
}

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
    #[serde(default, alias = "snapshotHour", deserialize_with = "de_string")]
    pub snapshot_hour: String,
    #[serde(default, deserialize_with = "de_f64")]
    pub cost: f64,
    #[serde(default, deserialize_with = "de_opt_i64")]
    pub tokens: Option<i64>,
    #[serde(default, alias = "totalCost", deserialize_with = "de_opt_f64")]
    pub total_cost: Option<f64>,
    #[serde(default, alias = "totalTokens", deserialize_with = "de_opt_i64")]
    pub total_tokens: Option<i64>,
    #[serde(default, alias = "appTokens", deserialize_with = "de_map_i64")]
    pub app_tokens: BTreeMap<String, i64>,
    #[serde(default, alias = "appCosts", deserialize_with = "de_map_f64")]
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

    /// null / 字符串数字不应让整段响应解析失败（单条记录容错）。
    #[test]
    fn tolerates_null_and_string_numbers() {
        let value = serde_json::json!({
            "records": [{
                "snapshot_hour": null,
                "cost": null,
                "total_cost": "1.25",
                "total_tokens": "154717654",
                "app_tokens": {"_codemaker_cli": "1000", "other": null},
                "app_costs": {"_codemaker_cli": "1.25"},
                "synced_at": null
            }, {
                "snapshot_hour": "2026-10-20 01:00:00",
                "cost": 2.5,
                "total_tokens": 7
            }]
        });
        let parsed = parse_hourly_cost(&value).expect("容错解析应成功");
        assert_eq!(parsed.records.len(), 2);
        assert_eq!(parsed.records[0].cost, 0.0);
        assert_eq!(parsed.records[0].total_cost, Some(1.25));
        assert_eq!(parsed.records[0].total_tokens, Some(154_717_654));
        assert_eq!(
            parsed.records[0].app_tokens.get("_codemaker_cli"),
            Some(&1000)
        );
        assert!(!parsed.records[0].app_tokens.contains_key("other"));
        assert_eq!(parsed.records[1].total_tokens, Some(7));
    }
}
