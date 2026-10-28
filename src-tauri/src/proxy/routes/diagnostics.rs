//! 诊断端点：`/health`、`/status`、`/v1/models`、`/v1/messages/count_tokens`、
//! `/internal/quota` 与 404 兜底。

use axum::{
    extract::{Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

use crate::{
    db::repo,
    proxy::{forward::ApiFormat, translate},
    usage::QuotaInfo,
};

use super::{
    errors::{json_error, local_error, payload_too_large},
    RoutesState,
};

pub(super) async fn health(State(state): State<RoutesState>) -> impl IntoResponse {
    (
        StatusCode::OK,
        Json(json!({
            "status": "healthy",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "uptimeSecs": state.info.uptime_secs(),
            "version": state.version,
            "mode": "proxy",
        })),
    )
}

pub(super) async fn status(State(state): State<RoutesState>) -> impl IntoResponse {
    (StatusCode::OK, Json(state.info.snapshot()))
}

/// OpenAI 形状的模型目录；未缓存时 503（前端/客户端应提示先登录）。
pub(super) async fn models(State(state): State<RoutesState>) -> Response {
    let models = state.provider.models().await;
    if models.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "error": {
                    "message": "模型目录尚未缓存，请先登录并完成 /validate",
                    "type": "service_unavailable",
                }
            })),
        )
            .into_response();
    }
    let data: Vec<Value> = models
        .iter()
        .map(|model| {
            json!({
                "id": model.id,
                "object": "model",
                "created": 0,
                "owned_by": "codemaker",
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

/// Anthropic count_tokens 的本地近似估算；不访问上游，避免 Claude Code 因 404 中断。
pub(super) async fn count_tokens(State(state): State<RoutesState>, request: Request) -> Response {
    let policy = state.policy.load();
    let body = match axum::body::to_bytes(request.into_body(), policy.max_request_body_bytes).await
    {
        Ok(bytes) => bytes,
        Err(_) => return payload_too_large(ApiFormat::AnthropicMessages),
    };
    match translate::estimate_input_tokens(&body) {
        Ok(tokens) => Json(json!({ "input_tokens": tokens })).into_response(),
        Err(message) => json_error(
            StatusCode::BAD_REQUEST,
            ApiFormat::AnthropicMessages,
            "invalid_request_error",
            &message,
        ),
    }
}

/// 最近一次成功额度快照（只读本地 DB，不触发网络）。
pub(super) async fn internal_quota(State(state): State<RoutesState>) -> Response {
    let quota = crate::db::blocking::run(&state.db, |db| {
        let conn = db.conn()?;
        repo::latest_quota_snapshot(&conn, "global")
    })
    .await;
    match quota {
        Ok(Some(snapshot)) => {
            let info = QuotaInfo::from_snapshot(snapshot);
            Json(json!({ "quota": info })).into_response()
        }
        Ok(None) => Json(json!({ "quota": null })).into_response(),
        // 连接池取连接失败与查询失败沿用两套错误码（wire 不变）。
        Err(err) => {
            let code = match err {
                crate::db::DbError::Pool(_) => "db_unavailable",
                _ => "quota_read_failed",
            };
            local_error(StatusCode::INTERNAL_SERVER_ERROR, code, err)
        }
    }
}

pub(super) async fn not_found(State(_state): State<RoutesState>, uri: axum::http::Uri) -> Response {
    not_found_response(uri.path())
}

pub(super) fn not_found_response(path: &str) -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": "not_found",
            "message": "未知的代理路径；支持 /v1/messages、/v1/chat/completions、/v1/responses、/v1/models",
            "path": path,
        })),
    )
        .into_response()
}
