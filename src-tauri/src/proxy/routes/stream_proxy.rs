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
    sse_frames::{self, SseBuffer},
    tee::SseTee,
    translate::{self, StreamTranslator},
    usage_parser::{self, CostInfo, TokenUsage},
};

use super::{context::record_completion, RoutesState};

const STREAM_CHANNEL_CAPACITY: usize = 16;

/// 直通 usage 帧过滤的缓冲上限：超过仍不成帧就原样放行并停用过滤（不挂流）。
const USAGE_FILTER_LIMIT: usize = 1024 * 1024;

/// 直通 SSE 的 usage-only 帧判定。
///
/// 只丢「本代理注入 `include_usage` 后多出来的那一帧」：`usage` 存在、`choices`
/// 缺失或为空数组，且没有正文/错误。其它帧（含客户端自己声明的）一律原样转发。
fn is_usage_only_frame(frame: &[u8]) -> bool {
    let Some(data) = sse_frames::frame_data(frame) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(data.as_ref()) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.contains_key("error") || object.get("usage").is_none() {
        return false;
    }
    match object.get("choices") {
        None => true,
        Some(Value::Array(choices)) => choices.is_empty(),
        Some(_) => false,
    }
}

/// `spawn_producer` 需要的全部输入（替代原来散落在闭包捕获里的局部变量）。
pub(super) struct ProducerPlan {
    pub policy: ProxyPolicy,
    pub status_code: i64,
    pub is_sse: bool,
    /// 本代理给上游补了 `stream_options.include_usage`：需要吞掉多出来的 usage 帧。
    pub drop_usage_frames: bool,
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
            drop_usage_frames,
        } = plan;
        let max_buffer = policy.max_buffer_bytes;
        let record_body = policy.record_body;
        let max_recorded_bytes = policy.max_body_bytes;
        let mut sse_tee = is_sse.then(|| SseTee::new(tee_format, policy.credit_to_yuan));
        let mut translator = translation.map(|_| StreamTranslator::new(estimated_input_tokens));
        // 直通 Chat 流式：本代理注入 include_usage 后，上游会多发一帧 usage-only，
        // 需要在这里吞掉（只在"注入过"时生效，客户端自己声明的原样转发）。
        let mut usage_filter =
            (drop_usage_frames && is_sse && translation.is_none()).then(SseBuffer::new);
        let mut collected: Vec<u8> = Vec::with_capacity(buffer_hint);
        let mut recorded_response_body: Vec<u8> = Vec::new();
        let mut response_bytes: u64 = 0;
        let mut first_byte_ms = None;
        let mut error_kind: Option<ErrorKind> = None;
        let mut upstream_failed = false;
        // 只反映"响应侧"是否超限（缓冲上限 / SSE 解析上限）。
        let mut response_truncated = false;

        // 单次注册的 idle 定时器，每个 chunk 重置；不再每 chunk 新建 timeout。
        let idle_timeout = Duration::from_secs(IDLE_TIMEOUT_SECS);
        let mut idle_deadline: Pin<Box<Sleep>> = Box::pin(sleep(idle_timeout));

        loop {
            let chunk = tokio::select! {
                _ = &mut idle_deadline => {
                    upstream_failed = true;
                    error_kind = Some(ErrorKind::Timeout);
                    break;
                }
                next = upstream_stream.next() => match next {
                    None => break,
                    Some(Err(_)) => {
                        upstream_failed = true;
                        error_kind = Some(ErrorKind::UpstreamError);
                        break;
                    }
                    Some(Ok(chunk)) => chunk,
                },
            };
            idle_deadline
                .as_mut()
                .reset(TokioInstant::now() + idle_timeout);

            // 非流式没有"首字节"语义：只有 SSE 才记录（docs/proxy.md）。
            if is_sse && first_byte_ms.is_none() {
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

            // 透传时直接复用 `Bytes`（零拷贝）；翻译/合成/过滤才需要自有缓冲。
            let outgoing = if let Some(filter) = usage_filter.as_mut() {
                filter.push(&chunk);
                let mut kept: Vec<u8> = Vec::new();
                while let Some(frame) = filter.next_frame() {
                    if !is_usage_only_frame(&frame) {
                        kept.extend_from_slice(&frame);
                    }
                }
                if filter.len() > USAGE_FILTER_LIMIT {
                    // 超大且未成帧：原样放行并停用过滤，绝不因为过滤把流挂住。
                    if let Some(rest) = filter.take_rest() {
                        kept.extend_from_slice(&rest);
                    }
                    usage_filter = None;
                    tracing::warn!(
                        request_id = %context.request_id,
                        "直通流式帧缓冲超过 1 MiB，已停用 usage 帧过滤"
                    );
                }
                Outgoing::Owned(kept)
            } else if is_sse {
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
                if let Err(kind) =
                    send_with_idle_deadline(&tx, outgoing, &mut idle_deadline, idle_timeout).await
                {
                    error_kind = Some(kind);
                    break;
                }
            }
        }

        // 过滤模式下的尾部残留帧（上游没有以空行结束）：原样放行。
        if let Some(mut filter) = usage_filter {
            if let Some(rest) = filter.take_rest() {
                if !rest.is_empty() {
                    response_bytes += rest.len() as u64;
                    if record_body {
                        append_recorded(&mut recorded_response_body, max_recorded_bytes, &rest);
                    }
                    if let Err(kind) = send_with_idle_deadline(
                        &tx,
                        Bytes::from(rest),
                        &mut idle_deadline,
                        idle_timeout,
                    )
                    .await
                    {
                        if error_kind.is_none() {
                            error_kind = Some(kind);
                        }
                    }
                }
            }
        }

        // 非流式 200 + in-band error：客户端会看到错误体，账务也必须记成失败。
        if !is_sse
            && error_kind.is_none()
            && status_code < 400
            && !collected.is_empty()
            && serde_json::from_slice::<Value>(&collected)
                .ok()
                .is_some_and(|value| value.get("error").is_some())
        {
            error_kind = Some(ErrorKind::UpstreamError);
        }

        let mut translator_error = false;
        if let Some(mut translator) = translator {
            let outgoing = if is_sse {
                translator.finish(upstream_failed)
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
            // 翻译层看到了 in-band error / 空流 / 帧过大：账务必须记成上游失败。
            translator_error = translator.took_upstream_error();
            if !outgoing.is_empty() {
                if upstream_failed {
                    idle_deadline
                        .as_mut()
                        .reset(TokioInstant::now() + idle_timeout);
                }
                response_bytes += outgoing.len() as u64;
                if record_body {
                    append_recorded(&mut recorded_response_body, max_recorded_bytes, &outgoing);
                }
                if let Err(kind) = send_with_idle_deadline(
                    &tx,
                    Bytes::from(outgoing),
                    &mut idle_deadline,
                    idle_timeout,
                )
                .await
                {
                    if error_kind.is_none() {
                        error_kind = Some(kind);
                    }
                }
            }
        }
        if translator_error && error_kind.is_none() {
            error_kind = Some(ErrorKind::UpstreamError);
        }
        drop(tx);

        let (usage, cost, tee_error) = match sse_tee {
            Some(tee) => {
                let outcome = tee.finish();
                response_truncated |= outcome.parse_overflow;
                (outcome.usage, outcome.cost, outcome.upstream_error)
            }
            None => {
                if response_truncated || collected.is_empty() {
                    (TokenUsage::default(), CostInfo::none(), false)
                } else {
                    let (usage, cost) =
                        parse_buffered(&collected, tee_format, policy.credit_to_yuan);
                    (usage, cost, false)
                }
            }
        };
        if tee_error && error_kind.is_none() && status_code < 400 {
            error_kind = Some(ErrorKind::UpstreamError);
        }
        // SSE 正常结束但 usage 全 0：这是"账务静默记 0"的唯一可观测入口。
        if is_sse && error_kind.is_none() && status_code < 400 && usage.is_zero() {
            tracing::warn!(
                request_id = %context.request_id,
                "SSE 正常结束但 usage 全 0（网关未回 usage 或客户端显式关闭 include_usage）"
            );
        }

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

async fn send_with_idle_deadline(
    tx: &mpsc::Sender<Result<Bytes, std::io::Error>>,
    bytes: Bytes,
    idle_deadline: &mut Pin<Box<Sleep>>,
    idle_timeout: Duration,
) -> Result<(), ErrorKind> {
    match tokio::select! {
        result = tx.send(Ok(bytes)) => result.map_err(|_| ErrorKind::ClientAborted),
        _ = idle_deadline.as_mut() => Err(ErrorKind::Timeout),
    } {
        Ok(()) => {
            // 慢客户端可能刚好在超时前消费完一块：写完必须重置 deadline，
            // 否则下一轮会立刻用过期的 sleep 把健康流判成 timeout。
            idle_deadline
                .as_mut()
                .reset(TokioInstant::now() + idle_timeout);
            Ok(())
        }
        Err(kind) => Err(kind),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_only_frame_detection_is_narrow() {
        assert!(is_usage_only_frame(
            b"data: {\"id\":\"x\",\"choices\":[],\"usage\":{\"prompt_tokens\":1}}\n\n"
        ));
        assert!(
            is_usage_only_frame(b"data: {\"usage\":{\"prompt_tokens\":1}}\n\n"),
            "缺少 choices 也属于 usage-only"
        );
        assert!(
            !is_usage_only_frame(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"usage\":{}}\n\n"
            ),
            "带正文的帧不能被吞"
        );
        assert!(
            !is_usage_only_frame(b"data: {\"error\":{\"message\":\"x\"},\"usage\":{}}\n\n"),
            "错误帧不能被吞"
        );
        assert!(!is_usage_only_frame(b"data: [DONE]\n\n"));
        assert!(!is_usage_only_frame(b": ping\n\n"));
    }

    #[tokio::test]
    async fn send_with_idle_deadline_times_out_when_receiver_stalls() {
        let (tx, _rx) = mpsc::channel(1);
        tx.send(Ok(Bytes::from_static(b"full")))
            .await
            .expect("第一块应填满通道");
        let mut deadline: Pin<Box<Sleep>> = Box::pin(sleep(Duration::from_millis(1)));

        let result = send_with_idle_deadline(
            &tx,
            Bytes::from_static(b"blocked"),
            &mut deadline,
            Duration::from_millis(50),
        )
        .await;

        assert_eq!(result, Err(ErrorKind::Timeout));
    }
}
