//! 上游路由映射、头清洗与客户端识别（契约见 docs/gateway-contract.md）。
//!
//! 规则：
//! - 客户端路径同时接受带 `/v1` 与不带 `/v1` 的写法；
//! - 上游 URL = 网关 + 固定协议前缀 + 原始 query；
//! - 入站头剥离凭据/浏览器上下文/hop-by-hop，再注入本地凭据与请求 id；
//! - 响应头只去 hop-by-hop，其余（含 content-type）透传。

use axum::http::{header, header::HeaderName, HeaderMap, HeaderValue};
use chrono::Local;
use serde::{Deserialize, Serialize};

use crate::auth::CredentialsSnapshot;

/// 注入的请求 id 头。
pub const CLIENT_REQUEST_ID_HEADER: &str = "x-client-request-id";
/// 响应里回传的代理请求 id。
pub const HUB_REQUEST_ID_HEADER: &str = "x-hub-request-id";

/// 上游协议格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiFormat {
    AnthropicMessages,
    OpenAiChat,
    OpenAiResponses,
    Unknown,
}

impl ApiFormat {
    /// 与前端 `RecentRequest.protocol` / DB `api_format` 对齐的协议名。
    pub fn as_str(self) -> &'static str {
        match self {
            ApiFormat::AnthropicMessages => "anthropic",
            ApiFormat::OpenAiChat => "openai_chat",
            ApiFormat::OpenAiResponses => "openai_responses",
            ApiFormat::Unknown => "unknown",
        }
    }

    /// 该协议的错误响应形状（401 等）。
    pub fn is_anthropic(self) -> bool {
        matches!(self, ApiFormat::AnthropicMessages)
    }
}

/// 客户端协议到上游协议的翻译方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translation {
    /// Anthropic Messages 请求/响应 ↔ OpenAI Chat。
    AnthropicToOpenAiChat,
}

/// 一条客户端路径映射；`translation=None` 表示同协议直通。
#[derive(Debug, Clone, Copy)]
pub struct RouteDef {
    pub client_path: &'static str,
    pub upstream_path: &'static str,
    pub api_format: ApiFormat,
    pub translation: Option<Translation>,
}

impl RouteDef {
    /// 上游响应实际使用的协议；翻译路由按 OpenAI Chat 解析 usage。
    pub fn usage_format(self) -> ApiFormat {
        match self.translation {
            Some(Translation::AnthropicToOpenAiChat) => ApiFormat::OpenAiChat,
            None => self.api_format,
        }
    }
}

/// 三条已证实的上游映射（docs/gateway-contract.md）。
pub const UPSTREAM_ROUTES: &[RouteDef] = &[
    RouteDef {
        client_path: "/v1/messages",
        upstream_path: "/openai/v1/chat/completions",
        api_format: ApiFormat::AnthropicMessages,
        translation: Some(Translation::AnthropicToOpenAiChat),
    },
    RouteDef {
        client_path: "/v1/chat/completions",
        upstream_path: "/openai/v1/chat/completions",
        api_format: ApiFormat::OpenAiChat,
        translation: None,
    },
    RouteDef {
        client_path: "/v1/responses",
        upstream_path: "/openai/v1/responses",
        api_format: ApiFormat::OpenAiResponses,
        translation: None,
    },
];

/// 无 `/v1` 前缀的等价路径。
pub const ROUTE_ALIASES: &[(&str, &str)] = &[
    ("/messages", "/v1/messages"),
    ("/chat/completions", "/v1/chat/completions"),
    ("/responses", "/v1/responses"),
];

/// 必须剥离的入站头（客户端自带凭据/浏览器上下文/hop-by-hop）。
pub const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    "x-api-key",
    // Azure / Gemini 约定的客户端 key 头：绝不能让它们顶替账号凭据带给上游网关。
    "api-key",
    "x-goog-api-key",
    "x-access-token",
    "cookie",
    "origin",
    "host",
    "content-length",
];

/// hop-by-hop 头（请求与响应都要去）。
pub const HOP_BY_HOP_HEADERS: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 允许记录到 `request_headers` 的白名单（绝不含凭据）。
pub const RECORDED_REQUEST_HEADERS: &[&str] = &[
    "user-agent",
    "content-type",
    "accept",
    "anthropic-version",
    "anthropic-beta",
    "openai-beta",
    "x-client-request-id",
    "x-claude-code-session-id",
    "x-codemaker-session-id",
    "x-session-id",
    "x-opencode-session-id",
    "downstream-client",
    "x-stainless-lang",
    "x-stainless-package-version",
    "x-stainless-os",
    "x-stainless-arch",
    "x-stainless-runtime",
    "x-stainless-runtime-version",
];

/// 会话 id 头（按优先级）。
pub const SESSION_ID_HEADERS: &[&str] = &[
    "x-claude-code-session-id",
    "x-codemaker-session-id",
    "x-session-id",
    "x-opencode-session-id",
];

fn path_only(path: &str) -> &str {
    path.split('?').next().unwrap_or(path)
}

fn canonical_path(path: &str) -> &str {
    let path = path_only(path);
    ROUTE_ALIASES
        .iter()
        .find_map(|(alias, canonical)| (*alias == path).then_some(*canonical))
        .unwrap_or(path)
}

/// 客户端路径 → 映射（忽略查询串；同时接受无 `/v1` 别名）。
pub fn resolve(path: &str) -> Option<RouteDef> {
    let canonical = canonical_path(path);
    UPSTREAM_ROUTES
        .iter()
        .copied()
        .find(|route| route.client_path == canonical)
}

/// 拼接上游完整 URL，并保留客户端 query。
pub fn upstream_url(gateway_url: &str, route: RouteDef, query: Option<&str>) -> String {
    let mut url = format!(
        "{}{}",
        gateway_url.trim_end_matches('/'),
        route.upstream_path
    );
    if let Some(query) = query.filter(|query| !query.is_empty()) {
        url.push('?');
        url.push_str(query);
    }
    url
}

fn header_name(name: &str) -> Option<HeaderName> {
    HeaderName::from_bytes(name.as_bytes()).ok()
}

fn is_stripped_request_header(name: &str) -> bool {
    STRIPPED_REQUEST_HEADERS.contains(&name) || HOP_BY_HOP_HEADERS.contains(&name)
}

/// 构造上游请求头：复制业务头 → 剥离敏感/hop 头 → 注入本地凭据。
///
/// 返回值中绝不包含客户端自带的 authorization/cookie 等；token 只在内存里。
pub fn build_upstream_headers(
    incoming: &HeaderMap,
    credentials: &CredentialsSnapshot,
) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in incoming.iter() {
        let name_str = name.as_str();
        if is_stripped_request_header(name_str) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }

    let inject = |headers: &mut HeaderMap, name: &str, value: &str| {
        if value.is_empty() {
            return;
        }
        if let (Some(name), Ok(value)) = (header_name(name), HeaderValue::from_str(value)) {
            headers.insert(name, value);
        }
    };
    inject(&mut headers, "x-access-token", &credentials.access_token);
    // 部分上游端点只认标准 Authorization，因此一并注入。
    inject(
        &mut headers,
        "authorization",
        &format!("Bearer {}", credentials.access_token),
    );
    inject(&mut headers, "x-auth-user", &credentials.user);
    // 不向上游注入代理标识头：部分网关会据此判定为非直连客户端并拒绝。
    headers
}

/// 响应头清洗：只去 hop-by-hop，其余（含 content-type/content-encoding）透传。
pub fn sanitize_response_headers(upstream: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    for (name, value) in upstream.iter() {
        if HOP_BY_HOP_HEADERS.contains(&name.as_str()) {
            continue;
        }
        headers.append(name.clone(), value.clone());
    }
    headers
}

/// 头白名单 JSON（用于排障；没有白名单头时返回 `None`）。
pub fn recorded_headers_json(incoming: &HeaderMap) -> Option<String> {
    let mut map = serde_json::Map::new();
    for name in RECORDED_REQUEST_HEADERS {
        if let Some(value) = incoming.get(*name) {
            if let Ok(text) = value.to_str() {
                map.insert(
                    (*name).to_string(),
                    serde_json::Value::String(text.to_string()),
                );
            }
        }
    }
    (!map.is_empty()).then(|| serde_json::Value::Object(map).to_string())
}

/// 按 UA 识别下游客户端（claude/codex/opencode/kimi，其余 unknown）。
pub fn client_from_user_agent(user_agent: Option<&str>) -> String {
    let Some(user_agent) = user_agent else {
        return "unknown".to_string();
    };
    let lowered = user_agent.to_ascii_lowercase();
    for (needle, client) in [
        ("claude", "claude-code"),
        ("codex", "codex"),
        ("opencode", "opencode"),
        ("kimi", "kimi"),
    ] {
        if lowered.contains(needle) {
            return client.to_string();
        }
    }
    "unknown".to_string()
}

/// 会话 id：优先取客户端头，否则 `anon:{client}:{本地日期}`。
pub fn external_session_id(incoming: &HeaderMap, client: &str) -> String {
    for name in SESSION_ID_HEADERS {
        if let Some(value) = incoming.get(*name).and_then(|value| value.to_str().ok()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() && trimmed.len() <= 128 && !trimmed.chars().any(char::is_control)
            {
                return trimmed.to_string();
            }
        }
    }
    format!("anon:{}:{}", client, Local::now().format("%Y-%m-%d"))
}

/// 请求体的两项转发元信息（一次解析同时得到，避免同一 body 解析两遍）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestEnvelope {
    /// 归一口径与旧实现一致：trim 后非空且 ≤256 字节。
    pub model: Option<String>,
    /// 缺失或非布尔一律 false。
    pub stream: bool,
}

/// 单趟解析请求体的 `model` 与 `stream`（非 JSON 时两项均取默认值）。
pub fn parse_envelope(body: &[u8]) -> RequestEnvelope {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(body) else {
        return RequestEnvelope {
            model: None,
            stream: false,
        };
    };
    RequestEnvelope {
        model: value
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty() && model.len() <= 256),
        stream: value
            .get("stream")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

/// 翻译路由的上游请求头：去掉 Anthropic 专属头，声明 JSON/SSE 期望。
pub fn apply_translation_headers(headers: &mut HeaderMap, stream: bool) {
    for name in ["anthropic-version", "anthropic-beta", "x-api-key"] {
        headers.remove(name);
    }
    headers.remove(header::CONTENT_LENGTH);
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::ACCEPT,
        HeaderValue::from_static(if stream {
            "text/event-stream"
        } else {
            "application/json"
        }),
    );
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static("identity"),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn snapshot() -> CredentialsSnapshot {
        CredentialsSnapshot {
            access_token: "test-token".to_string(),
            user: "test-user".to_string(),
            gateway_url: "https://api.example.test".to_string(),
        }
    }

    #[test]
    fn resolves_known_routes_and_aliases() {
        assert_eq!(
            resolve("/v1/messages").map(|r| r.api_format),
            Some(ApiFormat::AnthropicMessages)
        );
        assert_eq!(
            resolve("/messages").map(|r| r.api_format),
            Some(ApiFormat::AnthropicMessages)
        );
        assert_eq!(
            resolve("/chat/completions?x=1").map(|r| r.api_format),
            Some(ApiFormat::OpenAiChat)
        );
        assert_eq!(
            resolve("/v1/responses").map(|r| r.api_format),
            Some(ApiFormat::OpenAiResponses)
        );
        assert!(resolve("/v1/unknown").is_none());
    }

    #[test]
    fn builds_upstream_url_with_query() {
        let route = resolve("/v1/responses").expect("路由应存在");
        assert_eq!(
            upstream_url("https://api.example.test/", route, None),
            "https://api.example.test/openai/v1/responses"
        );
        assert_eq!(
            upstream_url("https://api.example.test", route, Some("a=1&b=2")),
            "https://api.example.test/openai/v1/responses?a=1&b=2"
        );
    }

    #[test]
    fn upstream_headers_are_stripped_and_injected() {
        let mut incoming = HeaderMap::new();
        incoming.insert("authorization", HeaderValue::from_static("Bearer client"));
        incoming.insert("x-api-key", HeaderValue::from_static("client-key"));
        incoming.insert("api-key", HeaderValue::from_static("azure-client-key"));
        incoming.insert(
            "x-goog-api-key",
            HeaderValue::from_static("gemini-client-key"),
        );
        incoming.insert("cookie", HeaderValue::from_static("a=b"));
        incoming.insert("origin", HeaderValue::from_static("https://x"));
        incoming.insert("host", HeaderValue::from_static("127.0.0.1:15721"));
        incoming.insert("content-length", HeaderValue::from_static("12"));
        incoming.insert("content-type", HeaderValue::from_static("application/json"));
        incoming.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
        incoming.insert("x-client-request-id", HeaderValue::from_static("old-id"));

        let headers = build_upstream_headers(&incoming, &snapshot());
        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-token");
        assert!(headers.get("x-api-key").is_none());
        assert!(headers.get("api-key").is_none());
        assert!(headers.get("x-goog-api-key").is_none());
        assert!(headers.get("cookie").is_none());
        assert!(headers.get("origin").is_none());
        assert!(headers.get("host").is_none());
        assert!(headers.get("content-length").is_none());
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("anthropic-version").unwrap(), "2023-06-01");
        assert_eq!(headers.get("x-access-token").unwrap(), "test-token");
        assert_eq!(headers.get("authorization").unwrap(), "Bearer test-token");
        assert_eq!(headers.get("x-auth-user").unwrap(), "test-user");
        // 不注入代理标识，避免网关反代检测。
        assert!(headers.get("downstream-client").is_none());
        assert_eq!(headers.get("x-client-request-id").unwrap(), "old-id");
    }

    #[test]
    fn response_headers_drop_hop_by_hop_only() {
        let mut upstream = HeaderMap::new();
        upstream.insert(
            "content-type",
            HeaderValue::from_static("text/event-stream"),
        );
        upstream.insert("transfer-encoding", HeaderValue::from_static("chunked"));
        upstream.insert("connection", HeaderValue::from_static("keep-alive"));
        upstream.insert("x-upstream", HeaderValue::from_static("1"));
        let headers = sanitize_response_headers(&upstream);
        assert!(headers.get("transfer-encoding").is_none());
        assert!(headers.get("connection").is_none());
        assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
        assert_eq!(headers.get("x-upstream").unwrap(), "1");
    }

    #[test]
    fn recorded_headers_are_whitelisted() {
        let mut incoming = HeaderMap::new();
        incoming.insert("authorization", HeaderValue::from_static("Bearer secret"));
        incoming.insert("user-agent", HeaderValue::from_static("claude-cli/1.0"));
        incoming.insert("x-session-id", HeaderValue::from_static("sess-1"));
        let json = recorded_headers_json(&incoming).expect("应有白名单头");
        assert!(json.contains("claude-cli/1.0"));
        assert!(json.contains("sess-1"));
        assert!(!json.contains("secret"));
        assert!(!json.contains("authorization"));
    }

    #[test]
    fn identifies_clients_and_session_ids() {
        assert_eq!(
            client_from_user_agent(Some("claude-cli/2.0")),
            "claude-code"
        );
        assert_eq!(client_from_user_agent(Some("codex-cli/1.2")), "codex");
        assert_eq!(client_from_user_agent(Some("opencode/0.3")), "opencode");
        assert_eq!(client_from_user_agent(Some("kimi-cli")), "kimi");
        assert_eq!(client_from_user_agent(Some("curl/8")), "unknown");
        assert_eq!(client_from_user_agent(None), "unknown");

        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", HeaderValue::from_static("abc"));
        assert_eq!(external_session_id(&headers, "codex"), "abc");
        assert!(external_session_id(&HeaderMap::new(), "codex").starts_with("anon:codex:"));
    }

    #[test]
    fn parses_model_and_stream_in_one_pass() {
        let envelope = parse_envelope(br#"{"model":"claude-sonnet-4","stream":true}"#);
        assert_eq!(envelope.model.as_deref(), Some("claude-sonnet-4"));
        assert!(envelope.stream);

        let envelope = parse_envelope(br#"{"model":"x"}"#);
        assert_eq!(envelope.model.as_deref(), Some("x"));
        assert!(!envelope.stream);

        let envelope = parse_envelope(b"not json");
        assert!(envelope.model.is_none());
        assert!(!envelope.stream);
    }
}
