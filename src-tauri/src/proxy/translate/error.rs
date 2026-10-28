//! 上游错误 → Anthropic error body / SSE error 事件。

use serde_json::{json, Value};

use super::sse::push_event;

/// 上游错误 JSON / 非 JSON 转 Anthropic error body。
pub fn openai_error_body(body: &[u8]) -> Vec<u8> {
    match serde_json::from_slice::<Value>(body) {
        Ok(value) => error_body(&value).to_string().into_bytes(),
        Err(_) => json!({
            "type": "error",
            "error": { "type": "api_error", "message": "上游返回了无法解析的错误响应" }
        })
        .to_string()
        .into_bytes(),
    }
}

pub(super) fn error_body(value: &Value) -> Value {
    if let Some(error) = value.get("error") {
        let error_type = error
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("api_error");
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("上游返回错误");
        json!({
            "type": "error",
            "error": { "type": error_type, "message": message }
        })
    } else {
        json!({
            "type": "error",
            "error": { "type": "api_error", "message": value.to_string() }
        })
    }
}

pub(super) fn error_event_value(value: &Value) -> Vec<u8> {
    let event = error_body(value);
    let mut out = String::new();
    push_event(&mut out, "error", &event);
    out.into_bytes()
}

pub(super) fn error_event_body(error_type: &str, message: &str) -> Vec<u8> {
    let mut out = String::new();
    push_event(
        &mut out,
        "error",
        &json!({
            "type": "error",
            "error": { "type": error_type, "message": message }
        }),
    );
    out.into_bytes()
}
