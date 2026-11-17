//! Anthropic Messages ↔ OpenAI Chat 协议翻译。
//!
//! 只做协议形状转换，不改变上游路由/鉴权；`/v1/messages` 翻译后转发到
//! `/openai/v1/chat/completions`，因为上游网关不接受原生 Anthropic 反代。
//! 首版不保留 thinking / prompt caching / documents 等 Anthropic 专属能力。
//!
//! 子模块（纯移动，无逻辑改动）：
//! - `request`：Anthropic 请求 → OpenAI Chat 请求 + 输入 token 估算；
//! - `response`：OpenAI Chat 响应 → Anthropic 响应 / 合成 SSE；
//! - `stream`：OpenAI SSE → Anthropic SSE 状态机；
//! - `error`：错误体转换；`sse`：SSE 事件写出；
//! - `tests`：原 `translate.rs` 的单测。

mod error;
mod request;
mod response;
mod sse;
mod stream;

pub use error::openai_error_body;
pub use request::{anthropic_to_openai_request, estimate_input_tokens, PreparedRequest};
pub use response::{message_json_to_sse, openai_to_anthropic_response};
pub use stream::StreamTranslator;

use serde_json::Value;

/// 从 OpenAI 的 `content` 取纯文本：兼容字符串与 `[{"type":"text","text":…}]` 数组。
///
/// 数组形态常见于多模态/part 形式的网关响应；旧实现只认字符串，会把正文静默丢掉
/// （token 照记、回答为空）。
pub(super) fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_string());
    }
    let parts = content.as_array()?;
    let mut joined = String::new();
    for part in parts {
        let text = part
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| part.as_str());
        if let Some(text) = text {
            joined.push_str(text);
        }
    }
    (!joined.is_empty()).then_some(joined)
}

#[cfg(test)]
mod tests;
