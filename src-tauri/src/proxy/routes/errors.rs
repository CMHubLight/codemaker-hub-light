//! 统一错误形状：Anthropic / OpenAI 两种协议 + 本地诊断形状。

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

use crate::proxy::forward::ApiFormat;

/// 请求体上限的展示文案（默认 32MB；用户改配置时仍按字节上限拒绝）。
const REQUEST_BODY_LIMIT_HINT: &str = "32MB";

pub(super) fn json_error(
    status: StatusCode,
    format: ApiFormat,
    code: &str,
    message: &str,
) -> Response {
    let body = if format.is_anthropic() {
        json!({
            "type": "error",
            "error": { "type": code, "message": message }
        })
    } else {
        json!({
            "error": { "message": message, "type": code, "code": code }
        })
    };
    (status, Json(body)).into_response()
}

/// 请求体超限（count_tokens 与转发两条路径共用同一文案与形状）。
pub(super) fn payload_too_large(format: ApiFormat) -> Response {
    json_error(
        StatusCode::PAYLOAD_TOO_LARGE,
        format,
        "request_too_large",
        // 文案与历史一致；实际上限由 policy 决定（默认 32MB）。
        &format!("请求体超过 {REQUEST_BODY_LIMIT_HINT} 上限"),
    )
}

/// 上游不可用：504 与 502 两套文案（逐字保留，不改 wire）。
pub(super) fn upstream_unavailable(format: ApiFormat, status: StatusCode) -> Response {
    if status == StatusCode::GATEWAY_TIMEOUT {
        json_error(status, format, "upstream_timeout", "上游响应超时")
    } else {
        json_error(status, format, "upstream_unavailable", "无法连接上游网关")
    }
}

/// 首字节超时（与 reqwest 自身超时区分：文案不同，码相同）。
pub(super) fn first_byte_timeout(format: ApiFormat) -> Response {
    json_error(
        StatusCode::GATEWAY_TIMEOUT,
        format,
        "upstream_timeout",
        "上游首字节超时",
    )
}

pub(super) fn auth_error(format: ApiFormat) -> Response {
    json_error(
        StatusCode::UNAUTHORIZED,
        format,
        "authentication_error",
        "未登录或登录已失效，请先登录 CodeMaker Hub Light",
    )
}

/// 本地诊断端点的错误形状：`{"error": <code>, "message": ...}`。
pub(super) fn local_error(
    status: StatusCode,
    code: &str,
    message: impl std::fmt::Display,
) -> Response {
    (
        status,
        Json(json!({ "error": code, "message": message.to_string() })),
    )
        .into_response()
}
