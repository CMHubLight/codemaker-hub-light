//! 转发主流程：解析请求 → 调用上游 → 组装响应头 → 交给 `stream_proxy` 产出响应体。

use std::time::{Duration, Instant};

use axum::{
    body::{Body, Bytes},
    extract::{Request, State},
    http::{header, HeaderName, HeaderValue, StatusCode},
    response::Response,
};
use chrono::Utc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::warn;

use crate::proxy::{
    forward::{self, ApiFormat},
    recorder::{ErrorKind, RequestCompletion},
    translate,
};

use super::{
    context::{record_completion, record_early_failure, EarlyBase, EarlyFailure},
    diagnostics::not_found_response,
    errors::{auth_error, first_byte_timeout, json_error, payload_too_large, upstream_unavailable},
    stream_proxy::{spawn_producer, ProducerPlan},
    RoutesState,
};

/// 核心转发处理器。
pub(super) async fn forward_handler(
    State(state): State<RoutesState>,
    request: Request,
) -> Response {
    // 每次请求取一份策略快照：设置里改「记录正文 / 积分换算」后立即生效。
    let policy = state.policy.load();
    // 拆包：headers 直接 move，避免为 `into_body()` 克隆整张 HeaderMap。
    let (parts, request_body) = request.into_parts();
    let path = parts.uri.path().to_string();
    let query = parts.uri.query().map(str::to_string);
    let Some(route) = forward::resolve(&path) else {
        return not_found_response(&path);
    };

    let started = Instant::now();
    let started_at = Utc::now();
    let headers = parts.headers;
    let client_request_id = headers
        .get(forward::CLIENT_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let client = forward::client_from_user_agent(
        headers
            .get(header::USER_AGENT)
            .and_then(|value| value.to_str().ok()),
    );
    let session_key = forward::external_session_id(&headers, &client);
    let request_headers_json = forward::recorded_headers_json(&headers);

    // 早期失败（无凭据/body 超限/翻译失败/首字节超时）与正常路径共用同一份元数据；
    // 正常路径末尾 `into_context` 消费它，全程零克隆。
    let base = EarlyBase {
        request_id: uuid::Uuid::new_v4().to_string(),
        client_request_id: Some(client_request_id),
        path,
        api_format: route.api_format,
        client,
        session_key,
        request_headers_json,
        started_at,
    };
    // 响应头用的请求 id 先转成 HeaderValue，末尾消费 base 之后就不必再克隆 String。
    let request_id_header = HeaderValue::from_str(&base.request_id).ok();
    // 只要进了代理就算一次请求（含随后 401 的），否则 /status 会出现
    // "errorCount 有数、requestCount 为 0"的矛盾。
    state.info.note_request();

    let credentials = match state.provider.credentials().await {
        Some(credentials) => credentials,
        None => {
            record_early_failure(
                &state,
                base,
                EarlyFailure::new(StatusCode::UNAUTHORIZED, ErrorKind::Unauthorized),
            )
            .await;
            return auth_error(route.api_format);
        }
    };

    // 请求体上限（默认 32MB，可配）。
    let body = match axum::body::to_bytes(request_body, policy.max_request_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            record_early_failure(
                &state,
                base,
                EarlyFailure::new(StatusCode::PAYLOAD_TOO_LARGE, ErrorKind::RequestBody),
            )
            .await;
            return payload_too_large(route.api_format);
        }
    };
    let request_bytes = body.len() as i64;

    // 录制标志只用于落库，绝不参与中继决策：
    // 一旦让"请求体被截断"影响响应缓冲，真实答案会被替换成错误体。
    let (request_body_recorded, request_body_truncated) = if policy.record_body {
        let (text, truncated) = policy.truncate_body(&body);
        (Some(text), truncated)
    } else {
        (None, false)
    };

    // 单趟解析：翻译路由同时产出 upstream body / model / stream / token 估算；
    // 透传路由一次解析拿到 model + stream。
    let translation = route.translation;
    let (upstream_body, model, client_stream_requested, estimated_input_tokens) = match translation
    {
        Some(_) => match translate::anthropic_to_openai_request(&body) {
            Ok(prepared) => (
                Bytes::from(prepared.upstream_body),
                normalize_model(prepared.model),
                prepared.stream,
                prepared.estimated_input_tokens,
            ),
            Err(message) => {
                warn!(error = %message, "Anthropic 请求翻译失败");
                // 失败行也要落 model：用一次 envelope 解析（错误路径，不在热路径上）。
                let model = forward::parse_envelope(&body).model;
                let failure =
                    EarlyFailure::new(StatusCode::BAD_REQUEST, ErrorKind::TranslationError)
                        .with_request(
                            model,
                            request_bytes,
                            request_body_recorded,
                            request_body_truncated,
                        );
                record_early_failure(&state, base, failure).await;
                return json_error(
                    StatusCode::BAD_REQUEST,
                    ApiFormat::AnthropicMessages,
                    "invalid_request_error",
                    &message,
                );
            }
        },
        None => {
            let envelope = forward::parse_envelope(&body);
            (body, envelope.model, envelope.stream, 0)
        }
    };

    // 优先用登录/校验返回的 serverUrl，但只接受 http(s)（否则回退到设置里的网关）。
    let snapshot_gateway = credentials.gateway_url.trim();
    let gateway =
        if snapshot_gateway.starts_with("http://") || snapshot_gateway.starts_with("https://") {
            snapshot_gateway
        } else {
            state.info.gateway_url.as_str()
        };
    let upstream = forward::upstream_url(gateway, route, query.as_deref());
    let mut upstream_headers = forward::build_upstream_headers(&headers, &credentials);
    if translation.is_some() {
        forward::apply_translation_headers(&mut upstream_headers, client_stream_requested);
    }

    let upstream_request = state
        .http
        .post(&upstream)
        .headers(upstream_headers)
        .body(upstream_body);
    let upstream_response = match tokio::time::timeout(
        Duration::from_secs(policy.first_byte_timeout_secs),
        upstream_request.send(),
    )
    .await
    {
        Ok(Ok(response)) => response,
        Ok(Err(err)) => {
            let status = if err.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            };
            let error_kind = if status == StatusCode::GATEWAY_TIMEOUT {
                ErrorKind::Timeout
            } else {
                ErrorKind::UpstreamError
            };
            warn!(error = %err, path = %base.path, "上游请求失败");
            let context = base.into_context(
                model,
                request_bytes,
                request_body_recorded,
                upstream,
                client_stream_requested,
            );
            record_completion(
                &state,
                context,
                RequestCompletion {
                    body_truncated: request_body_truncated,
                    ..RequestCompletion::failure(
                        status.as_u16() as i64,
                        started.elapsed().as_millis() as i64,
                        error_kind,
                        Utc::now(),
                    )
                },
            )
            .await;
            state.info.note_error();
            return upstream_unavailable(route.api_format, status);
        }
        Err(_) => {
            // 首字节超时：客户端看到 504，用量里也必须留下这一行（此前完全不可见）。
            warn!(path = %base.path, "上游首字节超时");
            let failure = EarlyFailure::new(StatusCode::GATEWAY_TIMEOUT, ErrorKind::Timeout)
                .with_request(
                    model,
                    request_bytes,
                    request_body_recorded,
                    request_body_truncated,
                );
            record_early_failure(&state, base, failure).await;
            return first_byte_timeout(route.api_format);
        }
    };

    let status = upstream_response.status();
    if status.is_server_error() || status.is_client_error() {
        state.info.note_error();
    }
    // 3xx 必须显式失败：reqwest 已禁用自动跟随，若把 Location 透给客户端，
    // 或者自己跟随，都会把注入的账号凭据带向非预期主机。
    if status.is_redirection() {
        state.info.note_error();
        warn!(upstream = %upstream, "上游返回重定向，已拒绝跟随");
        let context =
            base.into_context(model, request_bytes, request_body_recorded, upstream, false);
        record_completion(
            &state,
            context,
            RequestCompletion {
                body_truncated: request_body_truncated,
                ..RequestCompletion::failure(
                    StatusCode::BAD_GATEWAY.as_u16() as i64,
                    started.elapsed().as_millis() as i64,
                    ErrorKind::UpstreamRedirect,
                    Utc::now(),
                )
            },
        )
        .await;
        return json_error(
            StatusCode::BAD_GATEWAY,
            route.api_format,
            "upstream_redirect",
            "上游网关返回重定向，代理已拒绝跟随（避免凭据发往非预期主机）",
        );
    }
    let mut response_headers = forward::sanitize_response_headers(upstream_response.headers());
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_sse = content_type.contains("text/event-stream");
    if translation.is_some() {
        response_headers.remove(header::CONTENT_LENGTH);
        response_headers.remove(header::CONTENT_ENCODING);
        let stream_response = is_sse || (client_stream_requested && status.is_success());
        response_headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(if stream_response {
                "text/event-stream; charset=utf-8"
            } else {
                "application/json"
            }),
        );
    }
    let status_code = status.as_u16() as i64;
    let content_length_hint = upstream_response.content_length();
    let translation_model = model.clone().unwrap_or_default();
    let context = base.into_context(
        model,
        request_bytes,
        request_body_recorded,
        upstream,
        is_sse,
    );

    let plan = ProducerPlan {
        policy,
        status_code,
        is_sse,
        translation,
        tee_format: route.usage_format(),
        estimated_input_tokens,
        client_wants_stream: client_stream_requested,
        translation_model,
        buffer_hint: if is_sse {
            0
        } else {
            content_length_hint
                .unwrap_or(0)
                .min(policy.max_buffer_bytes as u64) as usize
        },
        request_body_truncated,
        started,
    };
    let rx = spawn_producer(state.clone(), context, upstream_response, plan);

    // 头只搬一次：直接复用 sanitize 后的 map。
    // 请求 id 用 append 而不是 insert：与旧的 `Response::builder().header(...)` 语义一致
    // （上游若回显同名头，保留多值而不是覆盖）。
    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    if let Some(value) = request_id_header {
        response.headers_mut().append(
            HeaderName::from_static(forward::HUB_REQUEST_ID_HEADER),
            value,
        );
    }
    response
}

/// 客户端要的 stream 在翻译后一定等于输入的 `stream`（翻译逻辑原样复制），
/// 但 model 需要按 `model_from_body` 的口径归一后才落库。
fn normalize_model(model: String) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.len() > 256 {
        None
    } else {
        Some(trimmed.to_string())
    }
}
