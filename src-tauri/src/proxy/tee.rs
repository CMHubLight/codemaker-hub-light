//! SSE 透传 tee（P2）：边转发边按帧解析 usage/cost，绝不整段缓冲。
//!
//! 帧扫描与 `data:` 抽取见 [`crate::proxy::sse_frames`]（与 `translate` 共用同一实现）；
//! 每帧 JSON 交给 [`crate::proxy::usage_parser`]，Anthropic 的 start+delta 通过
//! [`TokenUsage::merge`] 合并。

use crate::proxy::{
    forward::ApiFormat,
    sse_frames::{self, SSE_PARSE_LIMIT},
    usage_parser::{self, CostInfo, CostSource, TokenUsage},
};

/// tee 结束后的观测结果（只含落库需要的字段；不保留任何正文）。
#[derive(Debug, Clone)]
pub struct TeeOutcome {
    pub usage: TokenUsage,
    pub cost: CostInfo,
    /// 缓冲超过解析上限后停止解析（字节仍已透传）。
    pub parse_overflow: bool,
}

/// 增量 SSE 观察器。
pub struct SseTee {
    format: ApiFormat,
    credit_to_yuan: f64,
    buffer: Vec<u8>,
    parse_overflow: bool,
    usage: TokenUsage,
    cost: CostInfo,
}

impl SseTee {
    pub fn new(format: ApiFormat, credit_to_yuan: f64) -> Self {
        Self {
            format,
            credit_to_yuan,
            buffer: Vec::new(),
            parse_overflow: false,
            usage: TokenUsage::default(),
            cost: CostInfo::none(),
        }
    }

    /// 记录一个已转发的字节块并解析其中完整 SSE 帧。
    ///
    /// 帧直接从缓冲切片里解析（不再 `to_vec`），整块处理完只 compact 一次。
    pub fn push(&mut self, chunk: &[u8]) {
        if self.parse_overflow {
            return;
        }
        let mut buffer = std::mem::take(&mut self.buffer);
        buffer.extend_from_slice(chunk);

        let mut consumed = 0;
        while let Some((end, next)) = sse_frames::find_frame_end(&buffer[consumed..]) {
            let (end, next) = (consumed + end, consumed + next);
            if let Some(data) = sse_frames::frame_data(&buffer[consumed..end]) {
                self.observe_data(data.as_ref());
            }
            consumed = next;
        }
        if consumed > 0 {
            buffer.drain(..consumed);
        }

        if buffer.len() > SSE_PARSE_LIMIT {
            self.parse_overflow = true;
            buffer.clear();
        }
        self.buffer = buffer;
    }

    /// 结束：处理没有末尾空行的最后一帧，并返回观测结果。
    pub fn finish(mut self) -> TeeOutcome {
        if !self.parse_overflow && !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            if let Some(data) = sse_frames::frame_data(&rest) {
                self.observe_data(data.as_ref());
            }
        }
        TeeOutcome {
            usage: self.usage,
            cost: self.cost,
            parse_overflow: self.parse_overflow,
        }
    }

    fn observe_data(&mut self, data: &str) {
        if data.trim() == "[DONE]" {
            return;
        }
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(data) else {
            // 非 JSON（心跳等）不影响 usage。
            return;
        };
        if let Some(usage) = usage_parser::parse_usage(self.format, &payload) {
            self.usage.merge(usage);
        }
        let cost = usage_parser::parse_cost(&payload, self.credit_to_yuan);
        if cost.cost_source != CostSource::None {
            // 后出现的上游费用/最后模型帧优先。
            self.cost = cost;
        } else if self.cost.trace_id.is_none() {
            self.cost.trace_id = cost.trace_id;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic_frames() -> String {
        [
            "event: message_start",
            r#"data: {"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":20}}}"#,
            "",
            "event: message_delta",
            r#"data: {"type":"message_delta","usage":{"output_tokens":42}}"#,
            "",
            "event: message_stop",
            r#"data: {"type":"message_stop"}"#,
            "",
        ]
        .join("\n")
    }

    #[test]
    fn parses_frames_split_across_chunks() {
        let payload = anthropic_frames();
        let bytes = payload.as_bytes();
        let split = bytes.len() / 2;
        let mut tee = SseTee::new(ApiFormat::AnthropicMessages, 0.01);
        tee.push(&bytes[..split]);
        tee.push(&bytes[split..]);
        let outcome = tee.finish();
        assert_eq!(outcome.usage.input_tokens, 100);
        assert_eq!(outcome.usage.cache_read_tokens, 20);
        assert_eq!(outcome.usage.output_tokens, 42);
        assert!(!outcome.parse_overflow);
    }

    #[test]
    fn parses_crlf_comments_multiline_and_done() {
        let mut tee = SseTee::new(ApiFormat::OpenAiChat, 0.01);
        tee.push(b": ping\n\n");
        tee.push(b"data: {\"usage\":\n");
        tee.push(b"data: {\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n");
        tee.push(b"data: [DONE]\n\n");
        let outcome = tee.finish();
        assert_eq!(outcome.usage.input_tokens, 10);
        assert_eq!(outcome.usage.output_tokens, 2);
    }

    #[test]
    fn crlf_delimiters_are_supported() {
        let mut tee = SseTee::new(ApiFormat::OpenAiChat, 0.01);
        tee.push(b"data: {\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1}}\r\n\r\n");
        let outcome = tee.finish();
        assert_eq!(outcome.usage.input_tokens, 5);
        assert_eq!(outcome.usage.output_tokens, 1);
    }

    #[test]
    fn extracts_cost_from_openai_final_frame() {
        let mut tee = SseTee::new(ApiFormat::OpenAiChat, 0.01);
        tee.push(
            b"data: {\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2},\"aigw\":{\"cost\":{\"total_credit\":\"2.843328\"},\"trace_id\":\"trace-9\"}}\n\n",
        );
        let outcome = tee.finish();
        assert_eq!(outcome.cost.cost_source, CostSource::Upstream);
        assert_eq!(outcome.cost.total_credit, Some(2.843_328));
        assert!((outcome.cost.cost_yuan - 0.028_433_28).abs() < 1e-9);
        assert_eq!(outcome.cost.trace_id.as_deref(), Some("trace-9"));
    }

    #[test]
    fn tail_frame_without_delimiter_is_parsed() {
        let mut tee = SseTee::new(ApiFormat::OpenAiChat, 0.01);
        tee.push(b"data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}");
        let outcome = tee.finish();
        assert_eq!(outcome.usage.input_tokens, 7);
        assert_eq!(outcome.usage.output_tokens, 3);
    }

    #[test]
    fn non_json_frames_do_not_affect_usage() {
        let mut tee = SseTee::new(ApiFormat::AnthropicMessages, 0.01);
        tee.push(b"event: ping\ndata: not-json\n\n");
        let outcome = tee.finish();
        assert!(outcome.usage.is_zero());
    }

    #[test]
    fn buffer_overflow_stops_parsing_without_touching_usage() {
        let mut tee = SseTee::new(ApiFormat::OpenAiChat, 0.01);
        // 无帧分隔符的超大块：触发解析上限后不再解析。
        let overflow = vec![b'x'; SSE_PARSE_LIMIT + 1];
        tee.push(&overflow);
        let outcome = tee.finish();
        assert!(outcome.parse_overflow);
        assert!(outcome.usage.is_zero());
    }
}
