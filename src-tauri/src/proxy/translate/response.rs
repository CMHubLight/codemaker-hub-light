//! OpenAI Chat 非流式响应 → Anthropic Messages 响应 / 合成 SSE。

use serde_json::{json, Value};

use crate::proxy::{
    forward::ApiFormat,
    usage_parser::{self, TokenUsage},
};

use super::{
    error::{error_body, error_event_body, error_event_value},
    sse::push_event,
};

// ---------------------------------------------------------------------------
// 非流式响应：OpenAI Chat → Anthropic Messages
// ---------------------------------------------------------------------------

/// 将 OpenAI Chat 非流式响应翻译为 Anthropic Messages 响应。
pub fn openai_to_anthropic_response(body: &[u8], request_model: &str) -> Result<Vec<u8>, String> {
    let value: Value =
        serde_json::from_slice(body).map_err(|err| format!("上游响应不是合法 JSON：{err}"))?;
    if value.get("error").is_some() {
        return Ok(error_body(&value).to_string().into_bytes());
    }
    let message = build_anthropic_message(&value, request_model);
    serde_json::to_vec(&message).map_err(|err| format!("序列化翻译响应失败：{err}"))
}

/// 流式请求但上游返回 JSON 时，合成一次性 Anthropic SSE。
pub fn message_json_to_sse(body: &[u8]) -> Vec<u8> {
    let Ok(message) = serde_json::from_slice::<Value>(body) else {
        return error_event_body("api_error", "上游返回了无法解析的 JSON");
    };
    if message.get("type").and_then(Value::as_str) == Some("error") {
        return error_event_value(&message);
    }
    let mut out = String::new();
    let id = message
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("msg_translated");
    let model = message
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let usage = message.get("usage").cloned().unwrap_or_else(|| json!({}));
    push_event(
        &mut out,
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage
            }
        }),
    );

    let blocks = message
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (index, block) in blocks.iter().enumerate() {
        let index = index as u64;
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                let text = block
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                push_event(
                    &mut out,
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": { "type": "text", "text": "" }
                    }),
                );
                push_event(
                    &mut out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": { "type": "text_delta", "text": text }
                    }),
                );
            }
            Some("tool_use") => {
                push_event(
                    &mut out,
                    "content_block_start",
                    &json!({
                        "type": "content_block_start",
                        "index": index,
                        "content_block": {
                            "type": "tool_use",
                            "id": block.get("id").and_then(Value::as_str).unwrap_or("toolu_translated"),
                            "name": block.get("name").and_then(Value::as_str).unwrap_or("tool"),
                            "input": {}
                        }
                    }),
                );
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                push_event(
                    &mut out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string())
                        }
                    }),
                );
            }
            _ => continue,
        }
        push_event(
            &mut out,
            "content_block_stop",
            &json!({ "type": "content_block_stop", "index": index }),
        );
    }

    let stop_reason = message
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("end_turn");
    push_event(
        &mut out,
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": { "stop_reason": stop_reason, "stop_sequence": null },
            "usage": usage
        }),
    );
    push_event(&mut out, "message_stop", &json!({ "type": "message_stop" }));
    out.into_bytes()
}

fn build_anthropic_message(value: &Value, request_model: &str) -> Value {
    let choice = value.get("choices").and_then(|choices| choices.get(0));
    let upstream_message = choice.and_then(|choice| choice.get("message"));
    let mut content = Vec::new();

    if let Some(text) = upstream_message
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        content.push(json!({ "type": "text", "text": text }));
    }
    if let Some(tool_calls) = upstream_message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for tool_call in tool_calls {
            let id = tool_call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4()));
            let function = tool_call.get("function").unwrap_or(&Value::Null);
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let input = function
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
                .filter(Value::is_object)
                .unwrap_or_else(|| json!({}));
            content.push(json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input
            }));
        }
    }

    let usage = usage_parser::parse_usage(ApiFormat::OpenAiChat, value).unwrap_or_default();
    let stop_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .map(stop_reason)
        .unwrap_or("end_turn");
    let model = value
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or(request_model);
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .map(|id| {
            if id.starts_with("msg_") {
                id.to_string()
            } else {
                format!("msg_{id}")
            }
        })
        .unwrap_or_else(|| format!("msg_{}", uuid::Uuid::new_v4()));

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": anthropic_usage(&usage)
    })
}

pub(super) fn anthropic_usage(usage: &TokenUsage) -> Value {
    json!({
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cache_creation_input_tokens": usage.cache_write_tokens,
        "cache_read_input_tokens": usage.cache_read_tokens
    })
}

pub(super) fn stop_reason(reason: &str) -> &'static str {
    match reason {
        "length" => "max_tokens",
        "tool_calls" | "function_call" => "tool_use",
        _ => "end_turn",
    }
}
