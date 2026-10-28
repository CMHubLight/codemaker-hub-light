//! 响应体生产者：从上游 `bytes_stream` 逐 chunk 中继，同时 tee 解析 usage/cost、
//! 翻译 SSE、按上限缓冲非流式响应，结束后落库。
//!
//! 只做函数搬迁：`spawn_producer` 的闭包体与原 `forward_handler` 内一致。

use std::{
    pin::Pin,
    time::{Duration, Instant},
};

use axum::body::Bytes;
use chrono::Utc;
use serde_json::Value;
use tokio::{
    sync::mpsc,
    time::{sleep, Instant as TokioInstant, Sleep},
};
use tokio_stream::StreamExt;

use crate::proxy::{
    forward::{ApiFormat, Translation},
    policy::{ProxyPolicy, IDLE_TIMEOUT_SECS},
    recorder::{ErrorKind, RequestCompletion, RequestContext},
    tee::SseTee,
    translate::{self, StreamTranslator},
    usage_parser::{self, CostInfo, TokenUsage},
};

use super::{context::record_completion, RoutesState};

const STREAM_CHANNEL_CAPACITY: usize = 16;

/// `spawn_producer` 需要的全部输入（替代原来散落在闭包捕获里的局部变量）。
pub(super) struct ProducerPlan {
    pub policy: ProxyPolicy,
    pub status_code: i64,
    pub is_sse: bool,
    pub translation: Option<Translation>,
    pub tee_format: ApiFormat,
    pub estimated_input_tokens: u64,
    pub client_wants_stream: bool,
    pub translation_model: String,
    pub buffer_hint: usize,
    pub request_body_truncated: bool,
    pub started: Instant,
}

/// 中继上游响应体：逐 chunk tee/翻译/缓冲，结束后落库。
pub(super) fn spawn_producer(
    state: RoutesState,
    context: RequestContext,
    upstream_response: reqwest::Response,
    plan: ProducerPlan,
) -> mpsc::Receiver<Result<Bytes, std::io::Error>> {
    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(STREAM_CHANNEL_CAPACITY);
    let mut upstream_stream = upstream_response.bytes_stream();
    let producer_state = state;
    tokio::spawn(async move {
        let ProducerPlan {
            policy,
            status_code,
            is_sse,
            translation,
            tee_format,
            estimated_input_tokens,
            client_wants_stream,
            translation_model,
            buffer_hint,
            request_body_truncated,
            started: producer_started,
        } = plan;
        let max_buffer = policy.max_buffer_bytes;
        let record_body = policy.record_body;
        let max_recorded_bytes = policy.max_body_bytes;
        let mut sse_tee = is_sse.then(|| SseTee::new(tee_format, policy.credit_to_yuan));
        let mut translator = translation.map(|_| StreamTranslator::new(estimated_input_tokens));
        let mut collected: Vec<u8> = Vec::with_capacity(buffer_hint);
        let mut recorded_response_body: Vec<u8> = Vec::new();
        let mut response_bytes: u64 = 0;
        let mut first_byte_ms = None;
        let mut error_kind: Option<ErrorKind> = None;
        // 只反映"响应侧"是否超限（缓冲上限 / SSE 解析上限）。
        let mut response_truncated = false;

        // 单次注册的 idle 定时器，每个 chunk 重置；不再每 chunk 新建 timeout。
        let idle_timeout = Duration::from_secs(IDLE_TIMEOUT_SECS);
        let mut idle_deadline: Pin<Box<Sleep>> = Box::pin(sleep(idle_timeout));

        loop {
            let chunk = tokio::select! {
                _ = &mut idle_deadline => {
                    error_kind = Some(ErrorKind::Timeout);
                    break;
                }
                next = upstream_stream.next() => match next {
                    None => break,
                    Some(Err(_)) => {
                        error_kind = Some(ErrorKind::UpstreamError);
                        break;
                    }
                    Some(Ok(chunk)) => chunk,
                },
            };
            idle_deadline
                .as_mut()
                .reset(TokioInstant::now() + idle_timeout);

            if first_byte_ms.is_none() {
                first_byte_ms = Some(producer_started.elapsed().as_millis() as i64);
            }

            if let Some(tee) = sse_tee.as_mut() {
                tee.push(&chunk);
            } else if !response_truncated {
                if collected.len() + chunk.len() <= max_buffer {
                    collected.extend_from_slice(&chunk);
                } else {
                    response_truncated = true;
                    collected.clear();
                }
            }

            // 透传时直接复用 `Bytes`（零拷贝）；翻译/合成才需要自有缓冲。
            let outgoing = if is_sse {
                match translator.as_mut() {
                    Some(translator) => Outgoing::Owned(translator.push(&chunk)),
                    None => Outgoing::Passthrough(chunk),
                }
            } else if translation.is_some() {
                Outgoing::Owned(Vec::new())
            } else {
                Outgoing::Passthrough(chunk)
            };
            let outgoing = match outgoing {
                Outgoing::Passthrough(bytes) => bytes,
                Outgoing::Owned(bytes) => Bytes::from(bytes),
            };

            if !outgoing.is_empty() {
                response_bytes += outgoing.len() as u64;
                if record_body {
                    append_recorded(&mut recorded_response_body, max_recorded_bytes, &outgoing);
                }
                if tx.send(Ok(outgoing)).await.is_err() {
                    error_kind = Some(ErrorKind::ClientAborted);
                    break;
                }
            }
        }

        if let Some(mut translator) = translator {
            let outgoing = if is_sse {
                translator.finish()
            } else {
                let translated = if status_code >= 400 {
                    translate::openai_error_body(&collected)
                } else if response_truncated || collected.is_empty() {
                    translate::openai_error_body(b"")
                } else {
                    match translate::openai_to_anthropic_response(&collected, &translation_model) {
                        Ok(body) => body,
                        Err(err) => {
                            error_kind = Some(ErrorKind::TranslationError);
                            translate::openai_error_body(err.as_bytes())
                        }
                    }
                };
                if client_wants_stream && status_code < 400 {
                    translate::message_json_to_sse(&translated)
                } else {
                    translated
                }
            };
            if !outgoing.is_empty() {
                response_bytes += outgoing.len() as u64;
                if record_body {
                    append_recorded(&mut recorded_response_body, max_recorded_bytes, &outgoing);
                }
                let _ = tx.send(Ok(Bytes::from(outgoing))).await;
            }
        }
        drop(tx);

        let (usage, cost) = match sse_tee {
            Some(tee) => {
                let outcome = tee.finish();
                response_truncated |= outcome.parse_overflow;
                (outcome.usage, outcome.cost)
            }
            None => {
                if response_truncated || collected.is_empty() {
                    (TokenUsage::default(), CostInfo::none())
                } else {
                    parse_buffered(&collected, tee_format, policy.credit_to_yuan)
                }
            }
        };

        // 录制侧已在 append 时夹到上限，这里不会再截断。
        let response_body = record_body.then(|| policy.truncate_body(&recorded_response_body).0);

        let completion = RequestCompletion {
            status_code: Some(status_code),
            usage,
            cost,
            response_bytes,
            first_byte_ms,
            duration_ms: producer_started.elapsed().as_millis() as i64,
            error_kind,
            response_body,
            // 落库语义：任一侧被截断都标 1，仅用于展示，不参与任何中继决策。
            body_truncated: request_body_truncated || response_truncated,
            ended_at: Utc::now(),
        };
        record_completion(&producer_state, context, completion).await;
    });
    rx
}

/// 一个 chunk 的中继形态：透传直接复用 `Bytes`，翻译/合成才拥有缓冲。
enum Outgoing {
    Passthrough(Bytes),
    Owned(Vec<u8>),
}

/// 按录制上限追加响应字节（只截断录制，不影响中继）。
fn append_recorded(buffer: &mut Vec<u8>, limit: usize, bytes: &[u8]) {
    if buffer.len() >= limit {
        return;
    }
    let room = limit - buffer.len();
    buffer.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

fn parse_buffered(bytes: &[u8], format: ApiFormat, credit_to_yuan: f64) -> (TokenUsage, CostInfo) {
    let Ok(value) = serde_json::from_slice::<Value>(bytes) else {
        return (TokenUsage::default(), CostInfo::none());
    };
    let usage = usage_parser::parse_usage(format, &value).unwrap_or_default();
    (usage, usage_parser::parse_cost(&value, credit_to_yuan))
}
