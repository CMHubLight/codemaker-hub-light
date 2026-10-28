//! Anthropic `/v1/messages` 请求 → OpenAI Chat 请求（单趟解析）。

use serde_json::{json, Map, Value};

// ---------------------------------------------------------------------------
// 请求：Anthropic → OpenAI Chat
// ---------------------------------------------------------------------------

/// 单趟解析 `POST /v1/messages` 的产物：上游 body + 转发/落库需要的元信息。
///
/// 此前路由层对同一份 body 解析 5 次（model / stream / 翻译 / 再解析译文取 stream /
/// 估算 token）；这里一次解析全部产出。
pub struct PreparedRequest {
    /// 翻译后的 OpenAI Chat 请求体。
    pub upstream_body: Vec<u8>,
    /// 输入里的 `model`（翻译要求非空）。
    pub model: String,
    /// 上游是否需要流式（来自输入 `stream`）。
    pub stream: bool,
    /// 流式 `message_start` 用的输入 token 估算。
    pub estimated_input_tokens: u64,
}

/// 将 Anthropic `/v1/messages` 请求体翻译为 OpenAI Chat 请求体（单趟解析）。
pub fn anthropic_to_openai_request(body: &[u8]) -> Result<PreparedRequest, String> {
    let input: Value =
        serde_json::from_slice(body).map_err(|err| format!("请求体不是合法 JSON：{err}"))?;
    let object = input
        .as_object()
        .ok_or_else(|| "请求体必须是 JSON 对象".to_string())?;
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .ok_or_else(|| "缺少 model".to_string())?;
    // 复用同一棵树做流式 message_start 的输入估算，避免路由层再解析一遍。
    let estimated_input_tokens = estimate_tokens_from_value(&input);

    let mut output = Map::new();
    output.insert("model".to_string(), Value::String(model.to_string()));

    let mut messages = Vec::new();
    if let Some(system) = object.get("system") {
        let text = system_text(system);
        if !text.is_empty() {
            messages.push(json!({ "role": "system", "content": text }));
        }
    }

    let source_messages = object
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "缺少 messages 数组".to_string())?;
    if source_messages.is_empty() {
        return Err("messages 不能为空".to_string());
    }
    for message in source_messages {
        translate_message(message, &mut messages)?;
    }
    output.insert("messages".to_string(), Value::Array(messages));

    if let Some(value) = object.get("max_tokens").filter(|value| value.is_number()) {
        output.insert("max_tokens".to_string(), value.clone());
    }
    if let Some(value) = object.get("temperature").filter(|value| value.is_number()) {
        output.insert("temperature".to_string(), value.clone());
    }
    if let Some(value) = object.get("top_p").filter(|value| value.is_number()) {
        output.insert("top_p".to_string(), value.clone());
    }
    if let Some(stop) = object.get("stop_sequences").and_then(Value::as_array) {
        if !stop.is_empty() {
            output.insert("stop".to_string(), Value::Array(stop.clone()));
        }
    }

    let stream = object
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    output.insert("stream".to_string(), Value::Bool(stream));
    if stream {
        output.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );
    }

    if let Some(tools) = translate_tools(object.get("tools")) {
        output.insert("tools".to_string(), tools);
    }
    if let Some(choice) = object.get("tool_choice").and_then(translate_tool_choice) {
        if output.contains_key("tools") {
            output.insert("tool_choice".to_string(), choice);
        }
    }

    let upstream_body = serde_json::to_vec(&Value::Object(output))
        .map_err(|err| format!("序列化翻译请求失败：{err}"))?;
    Ok(PreparedRequest {
        upstream_body,
        model: model.to_string(),
        stream,
        estimated_input_tokens,
    })
}

fn translate_message(message: &Value, output: &mut Vec<Value>) -> Result<(), String> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let Some(content) = message.get("content") else {
        output.push(json!({ "role": role, "content": "" }));
        return Ok(());
    };

    if let Some(text) = content.as_str() {
        output.push(json!({ "role": role, "content": text }));
        return Ok(());
    }
    let Some(blocks) = content.as_array() else {
        return Err("message.content 必须是字符串或数组".to_string());
    };

    if role == "assistant" {
        translate_assistant_blocks(blocks, output);
    } else {
        translate_user_blocks(blocks, output);
    }
    Ok(())
}

fn translate_assistant_blocks(blocks: &[Value], output: &mut Vec<Value>) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(value) = block.get("text").and_then(Value::as_str) {
                    text.push_str(value);
                }
            }
            Some("tool_use") => {
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("toolu_{}", uuid::Uuid::new_v4()));
                let arguments = block
                    .get("input")
                    .map(|input| serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string()))
                    .unwrap_or_else(|| "{}".to_string());
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": { "name": name, "arguments": arguments }
                }));
            }
            // thinking / redacted_thinking / 图片等：OpenAI assistant 没有等价形状。
            _ => {}
        }
    }

    let mut message = Map::new();
    message.insert("role".to_string(), json!("assistant"));
    if tool_calls.is_empty() {
        message.insert("content".to_string(), Value::String(text));
    } else {
        message.insert(
            "content".to_string(),
            if text.is_empty() {
                Value::Null
            } else {
                Value::String(text)
            },
        );
        message.insert("tool_calls".to_string(), Value::Array(tool_calls));
    }
    output.push(Value::Object(message));
}

fn translate_user_blocks(blocks: &[Value], output: &mut Vec<Value>) {
    let mut parts = Vec::new();
    let mut has_image = false;
    let mut tool_messages = Vec::new();

    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(json!({ "type": "text", "text": text }));
                }
            }
            Some("image") => {
                if let Some(url) = image_url(block) {
                    has_image = true;
                    parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": url }
                    }));
                }
            }
            Some("tool_result") => {
                let Some(tool_use_id) = block.get("tool_use_id").and_then(Value::as_str) else {
                    continue;
                };
                let mut text = tool_result_text(block);
                if block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    text = format!("[tool_error] {text}");
                }
                tool_messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": text
                }));
            }
            Some("document") => {
                if let Some(text) = document_text(block) {
                    parts.push(json!({ "type": "text", "text": text }));
                }
            }
            // thinking / redacted_thinking / 未知块：丢弃。
            _ => {}
        }
    }

    output.extend(tool_messages);
    if parts.is_empty() {
        return;
    }
    let content = if has_image {
        Value::Array(parts)
    } else {
        let text = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        Value::String(text)
    };
    output.push(json!({ "role": "user", "content": content }));
}

fn system_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn image_url(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str)? {
        "base64" => {
            let media_type = source.get("media_type").and_then(Value::as_str)?;
            let data = source.get("data").and_then(Value::as_str)?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        "url" => source
            .get("url")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn document_text(block: &Value) -> Option<String> {
    let source = block.get("source")?;
    match source.get("type").and_then(Value::as_str)? {
        "text" => source
            .get("data")
            .and_then(Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

fn tool_result_text(block: &Value) -> String {
    match block.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn translate_tools(tools: Option<&Value>) -> Option<Value> {
    let tools = tools?.as_array()?;
    let translated: Vec<Value> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?;
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let parameters = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({ "type": "object", "properties": {} }));
            Some(json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": parameters
                }
            }))
        })
        .collect();
    (!translated.is_empty()).then_some(Value::Array(translated))
}

fn translate_tool_choice(choice: &Value) -> Option<Value> {
    match choice.get("type").and_then(Value::as_str)? {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({ "type": "function", "function": { "name": name } })),
        _ => None,
    }
}

/// 估算 Anthropic 输入 token，仅用于 count_tokens 与流式 `message_start`。
pub fn estimate_input_tokens(body: &[u8]) -> Result<u64, String> {
    let input: Value =
        serde_json::from_slice(body).map_err(|err| format!("请求体不是合法 JSON：{err}"))?;
    Ok(estimate_tokens_from_value(&input))
}

/// 从**已解析**的请求体估算输入 token（与 [`estimate_input_tokens`] 同一口径）。
pub(super) fn estimate_tokens_from_value(input: &Value) -> u64 {
    let mut chars = 0usize;
    if let Some(system) = input.get("system") {
        count_text_chars(system, &mut chars);
    }
    if let Some(messages) = input.get("messages").and_then(Value::as_array) {
        for message in messages {
            if let Some(content) = message.get("content") {
                count_content_chars(content, &mut chars);
            }
        }
    }
    if let Some(tools) = input.get("tools").and_then(Value::as_array) {
        for tool in tools {
            if let Some(description) = tool.get("description").and_then(Value::as_str) {
                chars += description.chars().count();
            }
            if let Some(schema) = tool.get("input_schema") {
                chars += json_char_count(schema);
            }
        }
    }
    (chars / 4).max(1) as u64
}

/// 统计 `value` 序列化后的**字符数**，但不分配中间字符串。
///
/// 口径与旧的 `value.to_string().chars().count()` 完全一致：serde_json 输出合法
/// UTF-8，一个字符只由一个非续字节（`0b10xxxxxx` 之外）引导，因此扫一遍字节即可
/// 精确计数。`serde_json::to_writer` 与 `Value::to_string` 使用同一序列化器。
fn json_char_count(value: &Value) -> usize {
    struct CharCounter {
        chars: usize,
    }

    impl std::io::Write for CharCounter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.chars += buf
                .iter()
                .filter(|byte| (**byte & 0b1100_0000) != 0b1000_0000)
                .count();
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = CharCounter { chars: 0 };
    // 仅当 Write 实现返回 Err 才会失败；本实现永不失败。
    let _ = serde_json::to_writer(&mut counter, value);
    counter.chars
}

fn count_text_chars(value: &Value, chars: &mut usize) {
    match value {
        Value::String(text) => *chars += text.chars().count(),
        Value::Array(values) => {
            for value in values {
                count_text_chars(value, chars);
            }
        }
        Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                *chars += text.chars().count();
            }
        }
        _ => {}
    }
}

fn count_content_chars(value: &Value, chars: &mut usize) {
    match value {
        Value::String(text) => *chars += text.chars().count(),
        Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => count_text_chars(block, chars),
                    Some("image") => *chars += 4000, // 约 1000 token 的固定估算
                    Some("tool_use") => {
                        if let Some(input) = block.get("input") {
                            *chars += json_char_count(input);
                        }
                    }
                    Some("tool_result") => {
                        if let Some(content) = block.get("content") {
                            count_content_chars(content, chars);
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}
