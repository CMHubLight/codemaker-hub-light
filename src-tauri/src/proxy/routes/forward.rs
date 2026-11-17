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
    // 直通 Chat 补 include_usage 后，生产者需要吞掉多出来的 usage-only 帧。
    let mut drop_usage_frames = false;
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
            // OpenAI Chat 流式必须显式请求 usage，否则上游不回末帧 usage、本机账务
            // 恒记 0。翻译路由在请求构造时已补，这里补上直通路由；其余路由不动。
            // 超过 4 MiB 的请求不做二次解析/重序列化（避免同时持有 4 份 body 副本），
            // 这类请求仍可能记 0，由生产者的「usage 全 0」warn 暴露。
            let mut rewritten = None;
            if envelope.stream && route.api_format == ApiFormat::OpenAiChat {
                if body.len() <= 4 * 1024 * 1024 {
                    rewritten = forward::with_stream_usage(&body);
                } else {
                    warn!(
                        bytes = body.len(),
                        "请求体超过 4 MiB，跳过 stream_options 注入（本次 usage 可能为 0）"
                    );
                }
            }
            drop_usage_frames = rewritten.is_some();
            let body = rewritten.map(Bytes::from).unwrap_or(body);
            (body, envelope.model, envelope.stream, 0)
        }
    };

    // 优先用登录/校验返回的 serverUrl，但它必须通过设置层同一套网关地址策略：
    // 凭据是以请求头形式发出去的，明文 http 只允许本机回环，否则会把 token
    // 暴露在同网段链路（docs/gateway-contract.md 的承诺）。不合法就回退设置网关。
    let gateway = effective_gateway(&credentials.gateway_url, &state.info.gateway_url);
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
            // 连接失败（DNS/拒绝/连接超时）按 502 记；只有响应阶段超时才记 504。
            let status = if err.is_connect() {
                StatusCode::BAD_GATEWAY
            } else if err.is_timeout() {
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
    // 已强制 accept-encoding: identity；上游若仍返回压缩体，usage/cost 解析会拿到
    // 压缩字节并静默记 0。这里留一条 warn，便于把"用量为 0"定位到上游行为上。
    if let Some(encoding) = upstream_response
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
    {
        if !encoding.eq_ignore_ascii_case("identity") {
            warn!(
                encoding,
                upstream = %upstream,
                "上游忽略 accept-encoding: identity，返回了压缩响应；本次用量可能无法解析"
            );
        }
    }
    let content_type = response_headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_sse = content_type.contains("text/event-stream");
    // 翻译路由必须读原始字节：若上游无视 identity 仍压缩，剥掉 content-encoding 会把
    // 压缩字节当成 JSON/SSE 解析（客户端拿到损坏正文）。这里直接失败并留一条账，
    // 让"网关忽略 identity"从静默损坏变成可定位的 502。
    if translation.is_some() {
        let encoding = upstream_response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("identity");
        if !encoding.eq_ignore_ascii_case("identity") && !is_sse {
            state.info.note_error();
            warn!(encoding, upstream = %upstream, "翻译路由收到压缩响应，拒绝解码并返回 502");
            let context = base.into_context(
                model,
                request_bytes,
                request_body_recorded,
                upstream.clone(),
                is_sse,
            );
            record_completion(
                &state,
                context,
                RequestCompletion {
                    body_truncated: request_body_truncated,
                    ..RequestCompletion::failure(
                        StatusCode::BAD_GATEWAY.as_u16() as i64,
                        started.elapsed().as_millis() as i64,
                        ErrorKind::UpstreamError,
                        Utc::now(),
                    )
                },
            )
            .await;
            return json_error(
                StatusCode::BAD_GATEWAY,
                route.api_format,
                "upstream_encoding_unsupported",
                "上游返回了压缩响应，代理无法翻译；请让网关对本机代理返回 identity 编码",
            );
        }
    }
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
    // 过滤 usage 帧会改变正文长度：保留上游 content-length 会让客户端读到截断的 body。
    if drop_usage_frames {
        response_headers.remove(header::CONTENT_LENGTH);
    }
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
        drop_usage_frames,
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

/// 选择本次转发的上游网关地址。
///
/// `serverUrl` 来自登录/校验响应，不能因为"看起来像 URL"就直接信任：它与设置项
/// `gateway_url` 走同一套策略（`https://` 允许任意主机；`http://` 只允许本机回环）。
/// 不合法时回退设置网关并留一条 warn，便于定位"实际用了哪个网关"。
fn effective_gateway<'a>(snapshot: &'a str, fallback: &'a str) -> &'a str {
    let snapshot = snapshot.trim();
    if crate::settings::gateway_url_is_acceptable(snapshot) {
        return snapshot;
    }
    if !snapshot.is_empty() {
        warn!(
            gateway = snapshot,
            "登录返回的 serverUrl 未通过网关地址策略，回退设置里的 gateway_url"
        );
    }
    fallback
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

#[cfg(test)]
mod tests {
    use super::effective_gateway;

    const FALLBACK: &str = "https://settings.example.test";

    /// 合法 serverUrl（https 任意主机 / http 本机回环）优先于设置网关。
    #[test]
    fn effective_gateway_prefers_acceptable_snapshot() {
        for snapshot in [
            "https://snapshot.example.test",
            "https://snapshot.example.test/api",
            "http://127.0.0.1:8080",
            "http://localhost:9000",
            "http://[::1]:9000",
        ] {
            assert_eq!(
                effective_gateway(snapshot, FALLBACK),
                snapshot.trim(),
                "合法快照应采用：{snapshot}"
            );
        }
        assert_eq!(
            effective_gateway("  http://127.0.0.1:8080/base  ", FALLBACK),
            "http://127.0.0.1:8080/base"
        );
    }

    /// 明文非回环、非 http(s)、空值都必须回退设置网关（P1：凭据不得明文外发）。
    #[test]
    fn effective_gateway_falls_back_for_untrusted_snapshot() {
        for snapshot in [
            "",
            "   ",
            "http://10.0.0.5:8080",
            "http://192.168.1.10:9000",
            "http://evil.example.test",
            "ftp://example.test",
            "api.example.test",
            "//example.test/path",
        ] {
            assert_eq!(
                effective_gateway(snapshot, FALLBACK),
                FALLBACK,
                "不合法快照必须回退：{snapshot}"
            );
        }
    }
}
