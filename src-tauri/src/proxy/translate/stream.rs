//! OpenAI Chat SSE → Anthropic Messages SSE 增量状态机。

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::proxy::{
    forward::ApiFormat,
    sse_frames::{self, SSE_FAIL_LIMIT},
    usage_parser::{self, TokenUsage},
};

use super::{
    error::error_body,
    response::{anthropic_usage, stop_reason},
    sse::push_event,
};

// ---------------------------------------------------------------------------
// 流式响应：OpenAI SSE → Anthropic SSE
// ---------------------------------------------------------------------------

/// 增量 OpenAI Chat SSE → Anthropic Messages SSE 状态机。
pub struct StreamTranslator {
    buffer: Vec<u8>,
    started: bool,
    finished: bool,
    message_id: String,
    model: String,
    estimated_input_tokens: u64,
    next_index: u64,
    text_index: Option<u64>,
    // 先按 OpenAI index 聚合工具参数，结束时按顺序输出，避免并行工具造成
    // Anthropic content block 交错开始/停止。
    tool_blocks: HashMap<u64, ToolBlock>,
    finish_reason: Option<String>,
    usage: TokenUsage,
}

#[derive(Default)]
struct ToolBlock {
    id: String,
    name: String,
    arguments: String,
}

impl StreamTranslator {
    pub fn new(estimated_input_tokens: u64) -> Self {
        Self {
            buffer: Vec::new(),
            started: false,
            finished: false,
            message_id: format!("msg_{}", uuid::Uuid::new_v4()),
            model: "unknown".to_string(),
            estimated_input_tokens,
            next_index: 0,
            text_index: None,
            tool_blocks: HashMap::new(),
            finish_reason: None,
            usage: TokenUsage::default(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.finished {
            return Vec::new();
        }
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.extend_from_slice(chunk);

        let mut out = String::new();
        let mut consumed = 0;
        while let Some((end, next)) = sse_frames::find_frame_end(&buffer[consumed..]) {
            let (end, next) = (consumed + end, consumed + next);
            if let Some(data) = sse_frames::frame_data(&buffer[consumed..end]) {
                self.handle_data(data.as_ref(), &mut out);
            }
            consumed = next;
        }
        if consumed > 0 {
            buffer.drain(..consumed);
        }

        if buffer.len() > SSE_FAIL_LIMIT {
            buffer.clear();
            self.error_event(&mut out, "api_error", "上游流式响应帧过大");
        }
        self.buffer = buffer;
        out.into_bytes()
    }

    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = String::new();
        if !self.finished && !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            if let Some(data) = sse_frames::frame_data(&rest) {
                self.handle_data(data.as_ref(), &mut out);
            }
        }
        if !self.finished {
            self.finalize(&mut out);
        }
        out.into_bytes()
    }

    fn handle_data(&mut self, data: &str, out: &mut String) {
        if self.finished {
            return;
        }
        if data.trim() == "[DONE]" {
            self.finalize(out);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if value.get("error").is_some() {
            let event = error_body(&value);
            let error = event.get("error").cloned().unwrap_or_else(|| json!({}));
            self.error_event_value(out, &error);
            return;
        }
        self.ensure_started(&value, out);
        if let Some(usage) = usage_parser::parse_usage(ApiFormat::OpenAiChat, &value) {
            self.usage.merge(usage);
        }
        let Some(choice) = value.get("choices").and_then(|choices| choices.get(0)) else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };
        if let Some(text) = delta.get("content").and_then(Value::as_str) {
            self.text_delta(text, out);
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for tool_call in tool_calls {
                self.tool_delta(tool_call, out);
            }
        }
    }

    fn ensure_started(&mut self, value: &Value, out: &mut String) {
        if self.started {
            return;
        }
        self.started = true;
        if let Some(id) = value.get("id").and_then(Value::as_str) {
            self.message_id = if id.starts_with("msg_") {
                id.to_string()
            } else {
                format!("msg_{id}")
            };
        }
        if let Some(model) = value.get("model").and_then(Value::as_str) {
            self.model = model.to_string();
        }
        let usage = anthropic_usage(&TokenUsage {
            input_tokens: self.estimated_input_tokens as i64,
            ..TokenUsage::default()
        });
        push_event(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.message_id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage
                }
            }),
        );
    }

    fn text_delta(&mut self, text: &str, out: &mut String) {
        if text.is_empty() {
            return;
        }
        if self.text_index.is_none() {
            let index = self.next_index;
            self.next_index += 1;
            self.text_index = Some(index);
            push_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": { "type": "text", "text": "" }
                }),
            );
        }
        let index = self.text_index.unwrap_or(0);
        push_event(
            out,
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": { "type": "text_delta", "text": text }
            }),
        );
    }

    fn tool_delta(&mut self, tool_call: &Value, out: &mut String) {
        let openai_index = tool_call.get("index").and_then(Value::as_u64).unwrap_or(0);
        let function = tool_call
            .get("function")
            .cloned()
            .unwrap_or_else(|| json!({}));

        if !self.tool_blocks.contains_key(&openai_index) && self.text_index.is_some() {
            if let Some(text_index) = self.text_index.take() {
                push_event(
                    out,
                    "content_block_stop",
                    &json!({ "type": "content_block_stop", "index": text_index }),
                );
            }
        }

        let block = self.tool_blocks.entry(openai_index).or_default();
        if block.id.is_empty() {
            if let Some(id) = tool_call.get("id").and_then(Value::as_str) {
                block.id = id.to_string();
            }
        }
        if let Some(name) = function
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            block.name = name.to_string();
        }
        if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
            block.arguments.push_str(arguments);
        }
    }

    fn finalize(&mut self, out: &mut String) {
        if self.finished {
            return;
        }
        self.finished = true;
        if !self.started {
            self.error_event(out, "api_error", "上游流式响应为空");
            return;
        }
        if let Some(index) = self.text_index.take() {
            push_event(
                out,
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": index }),
            );
        }
        let tool_blocks = std::mem::take(&mut self.tool_blocks);
        let mut tool_indices: Vec<u64> = tool_blocks.keys().copied().collect();
        tool_indices.sort_unstable();
        for tool_index in tool_indices {
            let block = &tool_blocks[&tool_index];
            let index = self.next_index;
            self.next_index += 1;
            let id = if block.id.is_empty() {
                format!("toolu_{}", uuid::Uuid::new_v4())
            } else {
                block.id.clone()
            };
            let name = if block.name.is_empty() {
                "tool"
            } else {
                block.name.as_str()
            };
            push_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": {
                        "type": "tool_use",
                        "id": id,
                        "name": name,
                        "input": {}
                    }
                }),
            );
            if !block.arguments.is_empty() {
                push_event(
                    out,
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": block.arguments
                        }
                    }),
                );
            }
            push_event(
                out,
                "content_block_stop",
                &json!({ "type": "content_block_stop", "index": index }),
            );
        }
        let reason = stop_reason(self.finish_reason.as_deref().unwrap_or("stop"));
        push_event(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": { "stop_reason": reason, "stop_sequence": null },
                "usage": anthropic_usage(&self.usage)
            }),
        );
        push_event(out, "message_stop", &json!({ "type": "message_stop" }));
    }

    fn error_event(&mut self, out: &mut String, error_type: &str, message: &str) {
        self.finished = true;
        push_event(
            out,
            "error",
            &json!({
                "type": "error",
                "error": { "type": error_type, "message": message }
            }),
        );
    }

    fn error_event_value(&mut self, out: &mut String, error: &Value) {
        self.finished = true;
        push_event(out, "error", &json!({ "type": "error", "error": error }));
    }
}
