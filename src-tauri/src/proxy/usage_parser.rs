//! usage / cost 解析（P3）：token 四元组归一与 `aigw.cost.total_credit` 换算。
//!
//! 归一规则见 docs/usage.md；费用换算见 §7.2 与
//! docs/gateway-contract.md（`total_credit / 100`）。

use serde::Serialize;

use crate::proxy::forward::ApiFormat;

/// token 四元组（归一后）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    /// 未命中缓存的输入 token。
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
}

impl TokenUsage {
    pub fn total(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }

    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    /// SSE 跨事件合并：Anthropic 的 `message_start` 只有输入，`message_delta`
    /// 才有最终输出；同一字段在后续事件里通常只会变大，因此取最大值。
    pub fn merge(&mut self, other: TokenUsage) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(other.cache_write_tokens);
    }
}

/// 费用来源，与数据库 `cost_source` 对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// 上游返回的 `aigw.cost.total_credit`。
    Upstream,
    /// 无上游费用时按 `model_prices` 估算。
    Estimated,
    /// 无法得到费用。
    None,
}

impl CostSource {
    /// 数据库 `cost_source` 文本。
    pub fn as_str(self) -> &'static str {
        match self {
            CostSource::Upstream => "upstream",
            CostSource::Estimated => "estimated",
            CostSource::None => "none",
        }
    }
}

/// 单请求费用。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CostInfo {
    pub total_credit: Option<f64>,
    pub cost_yuan: f64,
    pub cost_source: CostSource,
    pub currency: &'static str,
    pub trace_id: Option<String>,
}

impl CostInfo {
    /// 没有上游费用（未拿到 `aigw.cost`）时的零值。
    ///
    /// “空费用”的形状只在这里定义一次，避免 routes / tee 各写一份而漂移。
    pub fn none() -> Self {
        Self {
            total_credit: None,
            cost_yuan: 0.0,
            cost_source: CostSource::None,
            currency: "CNY",
            trace_id: None,
        }
    }
}

/// `total_credit` → 人民币元；保留 8 位小数避免浮点噪声。
pub fn credit_to_yuan(total_credit: f64, credit_to_yuan_ratio: f64) -> f64 {
    if !total_credit.is_finite() || !credit_to_yuan_ratio.is_finite() {
        return 0.0;
    }
    let value = total_credit * credit_to_yuan_ratio;
    (value * 100_000_000.0).round() / 100_000_000.0
}

/// 从上游响应 JSON 中提取 `aigw.cost.total_credit`（兼容包裹在 `usage` 下、字符串数字）。
pub fn extract_total_credit(payload: &serde_json::Value) -> Option<f64> {
    payload
        .pointer("/aigw/cost/total_credit")
        .and_then(as_f64_lenient)
        .or_else(|| {
            payload
                .pointer("/usage/aigw/cost/total_credit")
                .and_then(as_f64_lenient)
        })
}

fn as_f64_lenient(value: &serde_json::Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<f64>().ok())
        })
        .filter(|number| number.is_finite())
}

fn as_i64_lenient(value: &serde_json::Value) -> i64 {
    value
        .as_i64()
        .or_else(|| value.as_u64().map(|number| number as i64))
        .or_else(|| value.as_f64().map(|number| number as i64))
        .or_else(|| {
            value
                .as_str()
                .and_then(|text| text.trim().parse::<i64>().ok())
        })
        .unwrap_or(0)
}

fn get_i64(payload: &serde_json::Value, key: &str) -> i64 {
    payload.get(key).map(as_i64_lenient).unwrap_or(0)
}

/// OpenAI 系的 `prompt_tokens` 包含缓存命中部分；归一后 input 只保留未命中部分。
fn split_cached_input(prompt_tokens: i64, cache_read_tokens: i64) -> i64 {
    (prompt_tokens - cache_read_tokens).max(0)
}

/// 从上游响应 JSON 中提取 `aigw.trace_id`。
pub fn extract_trace_id(payload: &serde_json::Value) -> Option<String> {
    payload
        .pointer("/aigw/trace_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            payload
                .pointer("/usage/aigw/trace_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
}

/// 解析 usage 四元组。
///
/// 支持三种形态：
/// - OpenAI Chat：末帧 `usage`（`prompt_tokens` 含缓存命中）；
/// - Anthropic：`message_start.message.usage` 与 `message_delta.usage`；
/// - Responses：整包 `usage` 或 `response.completed.response.usage`。
///
/// 返回 `None` 表示当前事件没有 usage；调用方应按事件逐个解析并 `merge`。
pub fn parse_usage(format: ApiFormat, payload: &serde_json::Value) -> Option<TokenUsage> {
    match format {
        ApiFormat::AnthropicMessages => {
            let usage = payload
                .pointer("/message/usage")
                .or_else(|| payload.get("usage"))?;
            Some(TokenUsage {
                input_tokens: get_i64(usage, "input_tokens"),
                output_tokens: get_i64(usage, "output_tokens"),
                cache_read_tokens: get_i64(usage, "cache_read_input_tokens"),
                cache_write_tokens: get_i64(usage, "cache_creation_input_tokens"),
            })
        }
        ApiFormat::OpenAiChat => {
            let usage = payload.get("usage")?;
            let details = usage.get("prompt_tokens_details");
            let cache_read = details
                .and_then(|details| details.get("cached_tokens"))
                .map(as_i64_lenient)
                .unwrap_or_else(|| get_i64(usage, "prompt_cache_hit_tokens"));
            let cache_write = details
                .and_then(|details| details.get("cache_write_tokens"))
                .map(as_i64_lenient)
                .unwrap_or(0);
            Some(TokenUsage {
                input_tokens: split_cached_input(get_i64(usage, "prompt_tokens"), cache_read),
                output_tokens: get_i64(usage, "completion_tokens"),
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            })
        }
        ApiFormat::OpenAiResponses => {
            let usage = payload
                .pointer("/response/usage")
                .or_else(|| payload.get("usage"))?;
            let details = usage.get("input_tokens_details");
            let cache_read = details
                .and_then(|details| details.get("cached_tokens"))
                .map(as_i64_lenient)
                .unwrap_or(0);
            let cache_write = details
                .and_then(|details| details.get("cache_write_tokens"))
                .map(as_i64_lenient)
                .unwrap_or(0);
            Some(TokenUsage {
                input_tokens: split_cached_input(get_i64(usage, "input_tokens"), cache_read),
                output_tokens: get_i64(usage, "output_tokens"),
                cache_read_tokens: cache_read,
                cache_write_tokens: cache_write,
            })
        }
        ApiFormat::Unknown => None,
    }
}

/// 解析费用：有上游费用用 upstream，否则返回 None 来源（估算在 P3 接 model_prices）。
pub fn parse_cost(payload: &serde_json::Value, credit_to_yuan_ratio: f64) -> CostInfo {
    match extract_total_credit(payload) {
        Some(credit) => CostInfo {
            total_credit: Some(credit),
            cost_yuan: credit_to_yuan(credit, credit_to_yuan_ratio),
            cost_source: CostSource::Upstream,
            currency: "CNY",
            trace_id: extract_trace_id(payload),
        },
        None => CostInfo {
            total_credit: None,
            cost_yuan: 0.0,
            cost_source: CostSource::None,
            currency: "CNY",
            trace_id: extract_trace_id(payload),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credit_conversion_matches_contract_samples() {
        // docs/gateway-contract.md 的两组样例。
        assert!((credit_to_yuan(2.843_328, 0.01) - 0.028_433_28).abs() < 1e-9);
        assert!((credit_to_yuan(0.811_536, 0.01) - 0.008_115_36).abs() < 1e-9);
    }

    #[test]
    fn extracts_upstream_cost_and_trace() {
        let payload = json!({
            "aigw": { "cost": { "total_credit": 2.843328 }, "trace_id": "trace-1" }
        });
        assert_eq!(extract_total_credit(&payload), Some(2.843_328));
        assert_eq!(extract_trace_id(&payload).as_deref(), Some("trace-1"));

        let cost = parse_cost(&payload, 0.01);
        assert_eq!(cost.cost_source, CostSource::Upstream);
        assert!((cost.cost_yuan - 0.028_433_28).abs() < 1e-9);
    }

    #[test]
    fn missing_cost_is_none_source() {
        let cost = parse_cost(&json!({}), 0.01);
        assert_eq!(cost.cost_source, CostSource::None);
        assert_eq!(cost.cost_yuan, 0.0);
    }

    #[test]
    fn token_total_sums_four_buckets() {
        let usage = TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 3,
            cache_write_tokens: 4,
        };
        assert_eq!(usage.total(), 10);
        assert!(!usage.is_zero());
    }

    #[test]
    fn parses_openai_chat_final_chunk() {
        let payload = json!({
            "usage": {
                "prompt_tokens": 450528,
                "completion_tokens": 494,
                "total_tokens": 451022,
                "prompt_tokens_details": { "cached_tokens": 447232 },
                "prompt_cache_hit_tokens": 447232,
                "prompt_cache_miss_tokens": 3296
            }
        });
        let usage = parse_usage(ApiFormat::OpenAiChat, &payload).expect("应解析出 usage");
        assert_eq!(usage.input_tokens, 3296);
        assert_eq!(usage.output_tokens, 494);
        assert_eq!(usage.cache_read_tokens, 447232);
        assert_eq!(usage.cache_write_tokens, 0);
        assert_eq!(usage.total(), 451022);
    }

    #[test]
    fn parses_anthropic_start_and_delta_then_merges() {
        let start = json!({
            "type": "message_start",
            "message": {
                "usage": {
                    "input_tokens": 1110,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 384
                }
            }
        });
        let delta = json!({
            "type": "message_delta",
            "usage": {
                "input_tokens": 1110,
                "output_tokens": 735,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 384
            }
        });
        let mut usage = parse_usage(ApiFormat::AnthropicMessages, &start).expect("start");
        usage.merge(parse_usage(ApiFormat::AnthropicMessages, &delta).expect("delta"));
        assert_eq!(usage.input_tokens, 1110);
        assert_eq!(usage.output_tokens, 735);
        assert_eq!(usage.cache_read_tokens, 384);
        assert_eq!(usage.cache_write_tokens, 0);
    }

    #[test]
    fn parses_responses_usage_and_cached_input() {
        let payload = json!({
            "usage": {
                "input_tokens": 7,
                "output_tokens": 13,
                "total_tokens": 20,
                "input_tokens_details": { "cached_tokens": 5, "cache_write_tokens": 2 }
            }
        });
        let usage = parse_usage(ApiFormat::OpenAiResponses, &payload).expect("应解析出 usage");
        assert_eq!(usage.input_tokens, 2);
        assert_eq!(usage.output_tokens, 13);
        assert_eq!(usage.cache_read_tokens, 5);
        assert_eq!(usage.cache_write_tokens, 2);
    }

    #[test]
    fn parses_responses_completed_event() {
        let payload = json!({
            "type": "response.completed",
            "response": {
                "usage": { "input_tokens": 7, "output_tokens": 13 }
            }
        });
        let usage = parse_usage(ApiFormat::OpenAiResponses, &payload).expect("应解析出 usage");
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 13);
    }

    #[test]
    fn accepts_string_total_credit_from_upstream() {
        // 实际响应里 total_credit 是字符串。
        let payload = json!({
            "aigw": {
                "trace_id": "3cd3f19d6d67489f:f4abdf2195a690ba:3cd3f19d6d67489f:0",
                "cost": { "total_credit": "2.843328" }
            }
        });
        let cost = parse_cost(&payload, 0.01);
        assert_eq!(cost.cost_source, CostSource::Upstream);
        assert_eq!(cost.total_credit, Some(2.843_328));
        assert!((cost.cost_yuan - 0.028_433_28).abs() < 1e-9);
    }

    #[test]
    fn missing_usage_returns_none() {
        assert!(parse_usage(ApiFormat::OpenAiChat, &json!({})).is_none());
        assert!(parse_usage(ApiFormat::AnthropicMessages, &json!({})).is_none());
        assert!(parse_usage(ApiFormat::Unknown, &json!({"usage": {}})).is_none());
    }
}
