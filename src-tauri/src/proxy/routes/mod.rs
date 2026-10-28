//! Axum 路由与转发处理器（P2）。
//!
//! - `POST /v1/messages|/v1/chat/completions|/v1/responses`（及无 `/v1` 别名）
//! - `GET /v1/models|/models`、`/health`、`/status`、`/internal/quota`
//! - 安全基线：拒绝带 `Origin` 的浏览器请求；无凭据 401；请求体 32MB；只转发不重试。
//!
//! 响应体统一用 mpsc + `Body::from_stream` 边收边发：SSE 逐帧 tee，非流式限缓冲解析；
//! 客户端断开会通过 channel send 失败感知并记为 `client_aborted`。
//!
//! 热路径约束（改动时请保持）：
//! - 请求体只解析一趟（翻译路由由 `translate::anthropic_to_openai_request` 产出
//!   upstream body 与 model/stream/token 估算；透传路由用 `forward::parse_envelope`）；
//! - 透传 chunk 直接复用 `Bytes`（`Outgoing::Passthrough`），只有翻译/合成才 `Vec`；
//! - 落库走 `crate::db::blocking::run`，绝不阻塞 async worker。
//!
//! 子模块（纯移动）：`diagnostics` 诊断端点、`errors` 统一错误形状、`context`
//! 请求上下文与落库、`forward` 转发主流程、`stream_proxy` 响应体生产者。

mod context;
mod diagnostics;
mod errors;
mod forward;
mod stream_proxy;

use std::{sync::Arc, time::Duration};

use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::Response,
    routing::{get, post},
    Router,
};
use tracing::warn;

use crate::{
    db::Db,
    proxy::{credentials::CredentialProvider, policy::PolicyCell, server::RuntimeInfo},
};

use self::{
    diagnostics::{count_tokens, health, internal_quota, models, not_found, status},
    errors::local_error,
    forward::forward_handler,
};

/// 共享给路由处理器的状态。
#[derive(Clone)]
pub struct RoutesState {
    pub info: Arc<RuntimeInfo>,
    pub db: Db,
    pub provider: Arc<dyn CredentialProvider>,
    /// 运行期可替换的策略（记录正文 / 积分换算改完立即生效，无需重启代理）。
    pub policy: Arc<PolicyCell>,
    pub http: reqwest::Client,
    pub version: &'static str,
}

impl RoutesState {
    pub fn new(
        info: Arc<RuntimeInfo>,
        db: Db,
        provider: Arc<dyn CredentialProvider>,
        policy: Arc<PolicyCell>,
    ) -> Self {
        let http = match reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(
                crate::proxy::policy::CONNECT_TIMEOUT_SECS,
            ))
            .pool_idle_timeout(Duration::from_secs(90))
            // 不跟随重定向：注入的账号凭据是自定义头，reqwest 跨主机重定向只剥离
            // authorization/cookie 等标准头，自定义头会跟着跳到 Location 主机。
            .redirect(reqwest::redirect::Policy::none())
            .build()
        {
            Ok(client) => client,
            Err(err) => {
                warn!(error = %err, "构建转发 HTTP 客户端失败，回退默认客户端");
                reqwest::Client::new()
            }
        };
        Self {
            info,
            db,
            provider,
            policy,
            http,
            version: env!("CARGO_PKG_VERSION"),
        }
    }
}

/// 构造代理路由。
pub fn router(state: RoutesState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/status", get(status))
        .route("/v1/models", get(models))
        .route("/models", get(models))
        .route("/internal/quota", get(internal_quota))
        .route("/v1/messages", post(forward_handler))
        .route("/messages", post(forward_handler))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/messages/count_tokens", post(count_tokens))
        .route("/v1/chat/completions", post(forward_handler))
        .route("/chat/completions", post(forward_handler))
        .route("/v1/responses", post(forward_handler))
        .route("/responses", post(forward_handler))
        .fallback(not_found)
        // 两道入口闸，覆盖全部路由：Origin 拦浏览器发起的跨域请求，
        // Host 补上"页面把域名重解析到 127.0.0.1"时没有 Origin 的 rebinding 场景。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_loopback_host,
        ))
        .layer(axum::middleware::from_fn(reject_cross_origin))
        .with_state(state)
}

/// 只接受回环 Host（防 DNS rebinding 读取 `/status`、`/internal/quota`）。
async fn require_loopback_host(
    State(state): State<RoutesState>,
    request: Request,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if !crate::proxy::host_guard::host_header_allowed(host, &state.info.bind) {
        warn!(host = host.unwrap_or("-"), "拒绝非回环 Host 的代理请求");
        return local_error(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "本地代理只接受回环 Host",
        );
    }
    next.run(request).await
}

/// 拒绝带 Origin 的跨源请求（浏览器场景）。
async fn reject_cross_origin(request: Request, next: Next) -> Response {
    if request.headers().contains_key(header::ORIGIN) {
        return local_error(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "本地代理拒绝带 Origin 的请求",
        );
    }
    next.run(request).await
}
