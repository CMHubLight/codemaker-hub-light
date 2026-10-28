//! 原 `translate.rs` 的单测（纯移动）。

use serde_json::{json, Value};

use super::*;

#[test]
fn translates_text_request_with_system() {
    let body = br#"{
            "model":"claude-sonnet-4",
            "max_tokens":128,
            "temperature":0.2,
            "stop_sequences":["END"],
            "system":[{"type":"text","text":"be brief"}],
            "messages":[{"role":"user","content":"hi"}]
        }"#;
    let prepared = anthropic_to_openai_request(body).expect("请求应可翻译");
    let value: Value = serde_json::from_slice(&prepared.upstream_body).expect("应是 JSON");
    assert_eq!(prepared.model, "claude-sonnet-4");
    assert!(!prepared.stream);
    assert_eq!(prepared.estimated_input_tokens, 2);
    assert_eq!(value["model"], "claude-sonnet-4");
    assert_eq!(value["messages"][0]["role"], "system");
    assert_eq!(value["messages"][0]["content"], "be brief");
    assert_eq!(value["messages"][1]["content"], "hi");
    assert_eq!(value["max_tokens"], 128);
    assert_eq!(value["temperature"], 0.2);
    assert_eq!(value["stop"][0], "END");
    assert_eq!(value["stream"], false);
}

#[test]
fn translates_tools_images_and_tool_results() {
    let body = json!({
        "model": "m",
        "max_tokens": 64,
        "system": "sys",
        "messages": [
            {"role":"assistant","content":[
                {"type":"text","text":"checking"},
                {"type":"tool_use","id":"toolu_1","name":"read","input":{"path":"a.txt"}}
            ]},
            {"role":"user","content":[
                {"type":"tool_result","tool_use_id":"toolu_1","content":"hello","is_error":false},
                {"type":"text","text":"what is next?"}
            ]}
        ],
        "tools":[{"name":"read","description":"read file","input_schema":{"type":"object","properties":{"path":{"type":"string"}}}}],
        "tool_choice":{"type":"tool","name":"read"}
    });
    let prepared =
        anthropic_to_openai_request(&serde_json::to_vec(&body).unwrap()).expect("请求应可翻译");
    let value: Value = serde_json::from_slice(&prepared.upstream_body).expect("应是 JSON");
    assert_eq!(value["messages"][1]["role"], "assistant");
    assert_eq!(value["messages"][1]["content"], "checking");
    assert_eq!(value["messages"][1]["tool_calls"][0]["id"], "toolu_1");
    assert!(
        value["messages"][1]["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap()
            .contains("a.txt")
    );
    assert_eq!(value["messages"][2]["role"], "tool");
    assert_eq!(value["messages"][2]["tool_call_id"], "toolu_1");
    assert_eq!(value["messages"][3]["role"], "user");
    assert_eq!(value["tools"][0]["function"]["name"], "read");
    assert_eq!(value["tool_choice"]["function"]["name"], "read");
}

#[test]
fn translates_image_to_data_url() {
    let body = json!({
        "model": "m",
        "messages": [{"role":"user","content":[
            {"type":"text","text":"look"},
            {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AAAA"}}
        ]}]
    });
    let prepared =
        anthropic_to_openai_request(&serde_json::to_vec(&body).unwrap()).expect("请求应可翻译");
    let value: Value = serde_json::from_slice(&prepared.upstream_body).expect("应是 JSON");
    assert_eq!(value["messages"][0]["content"][0]["text"], "look");
    assert_eq!(
        value["messages"][0]["content"][1]["image_url"]["url"],
        "data:image/png;base64,AAAA"
    );
}

#[test]
fn translates_non_stream_response_and_usage() {
    let body = json!({
        "id":"chat_1",
        "model":"deepseek-v4-flash",
        "choices":[{
            "message":{
                "role":"assistant",
                "content":"hello",
                "tool_calls":[{
                    "id":"call_1",
                    "type":"function",
                    "function":{"name":"read","arguments":"{\"path\":\"a.txt\"}"}
                }]
            },
            "finish_reason":"tool_calls"
        }],
        "usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":50}}
    });
    let translated =
        openai_to_anthropic_response(&serde_json::to_vec(&body).unwrap(), "claude-sonnet-4")
            .expect("响应应可翻译");
    let value: Value = serde_json::from_slice(&translated).expect("应是 JSON");
    assert_eq!(value["type"], "message");
    assert_eq!(value["content"][0]["text"], "hello");
    assert_eq!(value["content"][1]["type"], "tool_use");
    assert_eq!(value["content"][1]["input"]["path"], "a.txt");
    assert_eq!(value["stop_reason"], "tool_use");
    assert_eq!(value["usage"]["input_tokens"], 50);
    assert_eq!(value["usage"]["output_tokens"], 10);
    assert_eq!(value["usage"]["cache_read_input_tokens"], 50);
}

#[test]
fn translates_stream_text_events_in_order() {
    let chunks = vec![
            "data: {\"id\":\"chat_1\",\"model\":\"m\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat_1\",\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat_1\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        ];
    let mut translator = StreamTranslator::new(2);
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(translator.push(chunk.as_bytes()));
    }
    out.extend(translator.finish());
    let text = String::from_utf8(out).expect("应是 UTF-8");
    let order = [
        "event: message_start",
        "event: content_block_start",
        "event: content_block_delta",
        "\"text\":\"Hello\"",
        "\"text\":\" world\"",
        "event: content_block_stop",
        "event: message_delta",
        "event: message_stop",
    ];
    let mut cursor = 0;
    for needle in order {
        let found = text[cursor..]
            .find(needle)
            .unwrap_or_else(|| panic!("缺少事件 {needle}"));
        cursor += found + needle.len();
    }
    assert_eq!(text.matches("event: message_stop").count(), 1);
}

#[test]
fn translates_stream_tool_call_events() {
    let chunks = vec![
            "data: {\"id\":\"chat_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chat_1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ];
    let mut translator = StreamTranslator::new(5);
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(translator.push(chunk.as_bytes()));
    }
    out.extend(translator.finish());
    let text = String::from_utf8(out).expect("应是 UTF-8");
    assert!(text.contains("\"type\":\"tool_use\""));
    assert!(text.contains("\"name\":\"read\""));
    assert!(text.contains("\"type\":\"input_json_delta\""));
    assert!(text.contains("\"stop_reason\":\"tool_use\""));
}

#[test]
fn non_json_error_is_anthropic_json() {
    let body = openai_error_body(b"upstream exploded");
    let value: Value = serde_json::from_slice(&body).expect("应是 JSON");
    assert_eq!(value["type"], "error");
    assert_eq!(value["error"]["type"], "api_error");
}

#[test]
fn counts_input_tokens_approximately() {
    let body =
        br#"{"model":"m","system":"abcd","messages":[{"role":"user","content":"12345678"}]}"#;
    assert_eq!(estimate_input_tokens(body).expect("应可估算"), 3);
}
