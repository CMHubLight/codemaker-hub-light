//! 请求上下文与落库：`EarlyBase`/`EarlyFailure` 组装上下文，
//! 所有写库都经 `db::blocking::run`（绝不阻塞 async worker）。

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use tracing::warn;

use crate::proxy::{
    forward::ApiFormat,
    recorder::{self, ErrorKind, RequestCompletion, RequestContext},
};

use super::RoutesState;

/// 落库：`spawn_blocking` 里跑完整事务；顺序保持"先落库后返回响应"。
pub(super) async fn record_completion(
    state: &RoutesState,
    context: RequestContext,
    completion: RequestCompletion,
) {
    let request_id = context.request_id.clone();
    let result = crate::db::blocking::run(&state.db, move |db| {
        recorder::record(&db, &context, &completion)
    })
    .await;
    if let Err(err) = result {
        warn!(error = %err, request_id = %request_id, "代理记录落库失败（不影响响应）");
    }
}

/// `forward_handler` 早期失败共用的请求元数据（解析 body 之前就已确定）。
pub(super) struct EarlyBase {
    pub(super) request_id: String,
    pub(super) client_request_id: Option<String>,
    pub(super) path: String,
    pub(super) api_format: ApiFormat,
    pub(super) client: String,
    pub(super) session_key: String,
    pub(super) request_headers_json: Option<String>,
    pub(super) started_at: DateTime<Utc>,
}

impl EarlyBase {
    /// 组装完整上下文（消费 self，正常路径末尾调用，零克隆）。
    pub(super) fn into_context(
        self,
        model: Option<String>,
        request_bytes: i64,
        request_body: Option<String>,
        upstream_url: String,
        is_sse: bool,
    ) -> RequestContext {
        RequestContext {
            request_id: self.request_id,
            client_request_id: self.client_request_id,
            method: "POST".to_string(),
            request_path: self.path,
            api_format: self.api_format,
            upstream_url,
            model,
            client: self.client,
            session_key: self.session_key,
            is_sse,
            request_bytes,
            request_headers_json: self.request_headers_json,
            request_body,
            started_at: self.started_at,
        }
    }
}

/// 一次"没走到上游响应"的失败（401/413/400/504）。
///
/// 用装配器而不是 8 个位置参数：401/413 只需 `new`，翻译失败/超时再 `with_request`。
pub(super) struct EarlyFailure {
    status: StatusCode,
    error_kind: ErrorKind,
    model: Option<String>,
    request_bytes: i64,
    request_body: Option<String>,
    body_truncated: bool,
}

impl EarlyFailure {
    pub(super) fn new(status: StatusCode, error_kind: ErrorKind) -> Self {
        Self {
            status,
            error_kind,
            model: None,
            request_bytes: 0,
            request_body: None,
            body_truncated: false,
        }
    }

    pub(super) fn with_request(
        mut self,
        model: Option<String>,
        request_bytes: i64,
        request_body: Option<String>,
        body_truncated: bool,
    ) -> Self {
        self.model = model;
        self.request_bytes = request_bytes;
        self.request_body = request_body;
        self.body_truncated = body_truncated;
        self
    }
}

/// 记录一次"没走到上游响应"的失败（401/413/400/504），并计入错误计数。
///
/// 这些请求同样占用代理资源、同样让客户端看到错误，用量页不该把它们丢掉。
pub(super) async fn record_early_failure(
    state: &RoutesState,
    base: EarlyBase,
    failure: EarlyFailure,
) {
    let duration_ms = Utc::now()
        .signed_duration_since(base.started_at)
        .num_milliseconds();
    let context = base.into_context(
        failure.model,
        failure.request_bytes,
        failure.request_body,
        // 失败发生在拿到上游响应之前，没有可记录的上游地址。
        String::new(),
        false,
    );
    record_completion(
        state,
        context,
        RequestCompletion {
            body_truncated: failure.body_truncated,
            ..RequestCompletion::failure(
                failure.status.as_u16() as i64,
                duration_ms,
                failure.error_kind,
                Utc::now(),
            )
        },
    )
    .await;
    state.info.note_error();
}
