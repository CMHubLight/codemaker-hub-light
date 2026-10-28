//! 认证契约、状态类型与深链解析（P1）。
//!
//! 契约来源：`docs/gateway-contract.md` §2、
//! `docs/authentication.md` §5。
//!
//! 安全约束：
//! - 本模块的日志、错误信息与返回前端的结构都不得包含 access_token / auth_key；
//! - 凭据只在 [`store::CredentialStore`] 加密落盘与 [`service::AuthService`] 内存中流转。

pub mod client;
pub mod hourly;
pub mod quota;
pub mod service;
pub mod store;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// 深链 scheme（docs/authentication.md）。
pub const DEEP_LINK_SCHEME: &str = "codemakerhub-light";
/// 登录应用标识（docs/gateway-contract.md）。
pub const LOGIN_IDE: &str = "codemaker-hub-light";
/// 前端轮询间隔（秒）。
pub const POLL_INTERVAL_SECS: u64 = 2;
/// 登录总超时（秒）。
pub const POLL_TIMEOUT_SECS: u64 = 300;
/// 连续网络错误阈值，达到后转 error。
pub const MAX_CONSECUTIVE_NETWORK_ERRORS: u32 = 3;
/// 续期检查周期、提前量与最长间隔（docs/authentication.md）。
pub const RENEW_CHECK_INTERVAL_SECS: u64 = 60;
pub const RENEW_AHEAD_SECS: i64 = 3600;
pub const RENEW_MAX_INTERVAL_SECS: i64 = 82_800;
/// `/validate` 轮询周期（秒）。
pub const VALIDATE_INTERVAL_SECS: u64 = 600;
/// 额度刷新周期（秒，前台手动 `force` 之外的后台刷新）。
pub const QUOTA_INTERVAL_SECS: u64 = 300;
/// 所有认证网络请求超时（秒）。
pub const HTTP_TIMEOUT_SECS: u64 = 20;
/// 单个凭据字段最大长度（防御异常深链/响应）。
pub const MAX_TOKEN_LEN: usize = 8192;
pub const MAX_USER_LEN: usize = 256;

/// 服务端未登录占位值（`/vs_login_callback` 返回），一律视为“没有值”。
pub const PLACEHOLDER_VALUES: &[&str] = &[
    "unknown_access_token",
    "unknown_sso_token",
    "unknown_auth_user",
];

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("网关地址无效：{0}")]
    InvalidGateway(String),
    #[error("深链无效：{0}")]
    InvalidDeepLink(String),
    #[error("凭据存储错误：{0}")]
    Store(#[from] store::StoreError),
    #[error("认证网络请求失败：{0}")]
    Http(String),
    #[error("尚未登录")]
    NotAuthenticated,
    #[error("凭据被服务端拒绝：{0}")]
    Rejected(String),
    #[error("登录会话已结束（可能已注销），已放弃本次操作")]
    SessionEnded,
    #[error("认证内部错误：{0}")]
    Internal(String),
}

/// 认证阶段。序列化为前端 `AuthState` 联合类型：
/// `unauthenticated | pending | authenticated | reauth_required | error`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthPhase {
    /// 未登录。
    #[default]
    LoggedOut,
    /// 已打开登录页，等待浏览器回调/轮询成功。
    PendingLogin,
    /// 已登录且凭据有效。
    LoggedIn,
    /// 校验不可用（超时/连接失败）但本地凭据仍可信。
    OfflineGrace,
    /// 凭据被服务端拒绝（403），需要重新登录。
    ReauthRequired,
    /// 登录失败（超时、网络、TLS 等）。
    Error,
}

impl AuthPhase {
    /// 前端联合类型取值（`offline_grace` 在界面上仍属于“已登录”）。
    pub fn frontend_state(self) -> &'static str {
        match self {
            AuthPhase::LoggedOut => "unauthenticated",
            AuthPhase::PendingLogin => "pending",
            AuthPhase::LoggedIn => "authenticated",
            AuthPhase::OfflineGrace => "authenticated",
            AuthPhase::ReauthRequired => "reauth_required",
            AuthPhase::Error => "error",
        }
    }
}

impl Serialize for AuthPhase {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.frontend_state())
    }
}

/// 返回给前端的认证状态（绝不含 token）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuthStatus {
    /// 前端 `AuthState`：unauthenticated | pending | authenticated | reauth_required | error
    pub state: &'static str,
    pub user: Option<String>,
    pub display_name: Option<String>,
    pub expires_at: Option<i64>,
    pub offline_grace: bool,
    /// 给用户看的状态说明（错误原因等）。
    pub message: Option<String>,
    // ---- 以下为诊断附加字段，前端可选读取 ----
    pub dept: Option<String>,
    pub server_url: Option<String>,
    pub issued_at: Option<i64>,
    pub credentials_valid: bool,
    pub pending_poll_uuid: Option<String>,
    pub pending_login_started_at: Option<String>,
    pub deep_link_received: bool,
    pub last_validated_at: Option<String>,
}

/// 两份状态快照是否有任何可观察字段不同。
///
/// 后台 tick 与登录轮询用它决定"要不要广播 `auth://state`"：状态没变时发事件
/// 会让前端把 Dashboard/请求页整页重拉（含一次 24h 云端费用请求）。
pub fn status_changed(prev: &AuthStatus, next: &AuthStatus) -> bool {
    prev != next
}

/// 账号档案（不含任何凭据）。
#[derive(Debug, Clone)]
pub struct AuthProfile {
    pub user: String,
    pub display_name: Option<String>,
    pub dept: Option<String>,
    pub server_url: String,
    pub issued_at: Option<i64>,
    pub expires_at: Option<i64>,
}

/// 模型目录条目（`/validate` 或 `/validate/models` 的 id/展示名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelSummary {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// 代理转发用的最小凭据快照（crate 内部；绝不 Serialize/日志/事件）。
#[derive(Clone)]
pub struct CredentialsSnapshot {
    pub access_token: String,
    pub user: String,
    /// 登录/校验得到的网关地址（可能为空，代理回退到设置里的 gateway_url）。
    pub gateway_url: String,
}

impl std::fmt::Debug for CredentialsSnapshot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialsSnapshot")
            .field("access_token", &"<redacted>")
            .field("user", &self.user)
            .field("gateway_url", &self.gateway_url)
            .finish()
    }
}

/// 一次进行中的登录。
#[derive(Debug, Clone)]
pub struct PendingLogin {
    pub poll_uuid: Uuid,
    pub login_url: String,
    pub redirect_uri: String,
    pub started_at: DateTime<Utc>,
    pub consecutive_network_errors: u32,
    pub deep_link_received: bool,
    /// 是否已经采纳过一份随深链带回的凭据（一次性语义）。
    ///
    /// 只拦"再次采纳凭据"，不清空 pending——轮询兜底仍需同一个 poll_uuid。
    pub credentials_consumed: bool,
}

impl PendingLogin {
    pub fn is_timed_out(&self, now: DateTime<Utc>) -> bool {
        (now - self.started_at).num_seconds() >= POLL_TIMEOUT_SECS as i64
    }
}

/// 深链解析结果；`access_token` 等字段只在内存中使用。
#[derive(Clone)]
pub struct DeepLinkPayload {
    pub poll_uuid: Option<String>,
    pub access_token: Option<String>,
    /// 深链的 `access_sso_token`（225 字符）。**不能**用于续期（服务端 403
    /// `decrypt auth key fail`），只在 `/get_auth_key` 失败时兜底。
    pub auth_key: Option<String>,
    pub user: Option<String>,
}

impl std::fmt::Debug for DeepLinkPayload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeepLinkPayload")
            .field("poll_uuid", &self.poll_uuid)
            .field("has_access_token", &self.access_token.is_some())
            .field("has_auth_key", &self.auth_key.is_some())
            .field("user", &self.user)
            .finish()
    }
}

impl DeepLinkPayload {
    /// 深链是否已经带回可直接完成登录的凭据。
    pub fn has_credentials(&self) -> bool {
        self.access_token.is_some()
    }
}

/// 构造深链回跳地址：`codemakerhub-light://auth/callback?pollUuid=<uuid>`。
pub fn build_redirect_uri(poll_uuid: &Uuid) -> String {
    format!("{DEEP_LINK_SCHEME}://auth/callback?pollUuid={poll_uuid}")
}

/// 构造登录页 URL（docs/gateway-contract.md）。
pub fn build_login_url(gateway: &str, redirect_uri: &str) -> Result<String, AuthError> {
    let gateway = gateway.trim().trim_end_matches('/');
    if !crate::settings::gateway_url_is_acceptable(gateway) {
        return Err(AuthError::InvalidGateway(gateway.to_string()));
    }
    Ok(format!(
        "{gateway}/vs_login?ide={}&redirect_uri={}",
        percent_encode(LOGIN_IDE),
        percent_encode(redirect_uri)
    ))
}

/// 解析登录深链。
///
/// 格式（注意第二个 `?`，不能用标准 query parser）：
/// ```text
/// codemakerhub-light://auth/callback?pollUuid=<uuid>?access_token=..&access_sso_token=..&access_user=..
/// ```
/// 同时在 `?` / `&` 上切分，避免 `pollUuid` 被第二个 `?` 之后的内容污染。
pub fn parse_deep_link(raw_url: &str) -> Result<DeepLinkPayload, AuthError> {
    let (scheme, rest) = raw_url
        .split_once("://")
        .ok_or_else(|| AuthError::InvalidDeepLink("缺少 ://".to_string()))?;
    if !scheme.eq_ignore_ascii_case(DEEP_LINK_SCHEME) {
        return Err(AuthError::InvalidDeepLink(format!(
            "scheme 不是 {DEEP_LINK_SCHEME}"
        )));
    }

    let (authority_and_path, params) = match rest.split_once('?') {
        Some((head, tail)) => (head, Some(tail)),
        None => (rest, None),
    };

    if !authority_and_path
        .trim_end_matches('/')
        .eq_ignore_ascii_case("auth/callback")
    {
        return Err(AuthError::InvalidDeepLink(
            "host/path 必须是 auth/callback".to_string(),
        ));
    }

    let mut pairs: Vec<(String, String)> = Vec::new();
    if let Some(params) = params {
        for segment in params.split(['&', '?']) {
            if segment.is_empty() {
                continue;
            }
            let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
            pairs.push((percent_decode(key), percent_decode(value)));
        }
    }

    let lookup = |keys: &[&str]| -> Option<String> {
        for key in keys {
            for (candidate, value) in &pairs {
                if candidate.eq_ignore_ascii_case(key) {
                    if let Some(cleaned) = clean_param(value, MAX_TOKEN_LEN) {
                        return Some(cleaned);
                    }
                }
            }
        }
        None
    };

    let poll_raw = lookup(&["pollUuid", "poll_uuid", "polluuid"]);
    let poll_uuid = match poll_raw {
        Some(ref value) => {
            Uuid::parse_str(value)
                .map_err(|_| AuthError::InvalidDeepLink("pollUuid 不是合法 UUID".to_string()))?;
            Some(value.clone())
        }
        None => None,
    };

    let access_token = lookup(&["access_token", "accessToken", "token"]);
    let auth_key = lookup(&["access_sso_token", "auth_key", "authkey", "sso_token"]);
    let user = lookup(&["access_user", "user", "auth_user"]).map(|value| {
        // 用户标识允许中文/工号，但同样拒绝控制字符与超长值。
        if value.chars().count() > MAX_USER_LEN {
            value.chars().take(MAX_USER_LEN).collect()
        } else {
            value
        }
    });

    Ok(DeepLinkPayload {
        poll_uuid,
        access_token,
        auth_key,
        user,
    })
}

fn clean_param(value: &str, max_len: usize) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.chars().count() > max_len {
        return None;
    }
    if trimmed.chars().any(char::is_control) {
        return None;
    }
    if PLACEHOLDER_VALUES
        .iter()
        .any(|placeholder| trimmed.eq_ignore_ascii_case(placeholder))
    {
        return None;
    }
    Some(trimmed.to_string())
}

/// RFC 3986 最小百分号编码（仅保留 unreserved 字符）。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(value) = u8::from_str_radix(hex, 16) {
                out.push(value);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_DEEP_LINK: &str = "codemakerhub-light://auth/callback?pollUuid=11111111-2222-4333-8444-555555555555?access_token=test-access-token&access_sso_token=test-sso-token&access_user=test-user";

    #[test]
    fn login_url_encodes_redirect_uri() {
        let uuid = Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap();
        let redirect = build_redirect_uri(&uuid);
        let url = build_login_url("https://api.example.test/", &redirect).expect("URL 应可构造");
        assert!(url.starts_with("https://api.example.test/vs_login?ide=codemaker-hub-light"));
        assert!(url.contains("redirect_uri=codemakerhub-light%3A%2F%2Fauth%2Fcallback"));
        assert!(!url.contains("redirect_uri=codemakerhub-light://"));
    }

    #[test]
    fn rejects_invalid_gateway() {
        assert!(build_login_url("api.example.test", "x").is_err());
    }

    #[test]
    fn parses_real_double_question_mark_deep_link() {
        let link = parse_deep_link(REAL_DEEP_LINK).expect("真实深链应可解析");
        assert_eq!(
            link.poll_uuid.as_deref(),
            Some("11111111-2222-4333-8444-555555555555")
        );
        assert_eq!(link.access_token.as_deref(), Some("test-access-token"));
        assert_eq!(link.auth_key.as_deref(), Some("test-sso-token"));
        assert_eq!(link.user.as_deref(), Some("test-user"));
        assert!(link.has_credentials());
    }

    #[test]
    fn parses_single_question_mark_form() {
        let raw = "codemakerhub-light://auth/callback?pollUuid=11111111-2222-4333-8444-555555555555&access_token=t";
        let link = parse_deep_link(raw).expect("单 ? 形式也应可解析");
        assert_eq!(link.access_token.as_deref(), Some("t"));
    }

    #[test]
    fn ignores_unknown_placeholders() {
        let raw = "codemakerhub-light://auth/callback?pollUuid=11111111-2222-4333-8444-555555555555?access_token=unknown_access_token&access_sso_token=unknown_sso_token&access_user=unknown_auth_user";
        let link = parse_deep_link(raw).expect("占位值不应导致解析失败");
        assert!(link.access_token.is_none());
        assert!(link.auth_key.is_none());
        assert!(link.user.is_none());
        assert!(!link.has_credentials());
    }

    #[test]
    fn rejects_wrong_scheme_path_and_bad_uuid() {
        assert!(parse_deep_link("codemakerhub://auth/callback?pollUuid=x").is_err());
        assert!(parse_deep_link("codemakerhub-light://auth/other").is_err());
        assert!(parse_deep_link("codemakerhub-light://auth/callback?pollUuid=not-a-uuid").is_err());
    }

    #[test]
    fn percent_encoded_values_are_decoded() {
        let raw = "codemakerhub-light://auth/callback?pollUuid=11111111-2222-4333-8444-555555555555?access_token=a%2Bb%2Fc%3D&access_user=%E5%B7%A5%E5%8F%B7";
        let link = parse_deep_link(raw).expect("编码值应可解析");
        assert_eq!(link.access_token.as_deref(), Some("a+b/c="));
        assert_eq!(link.user.as_deref(), Some("工号"));
    }

    #[test]
    fn rejects_overlong_token() {
        let token = "a".repeat(MAX_TOKEN_LEN + 1);
        let raw = format!(
            "codemakerhub-light://auth/callback?pollUuid=11111111-2222-4333-8444-555555555555?access_token={token}"
        );
        // 超长值被丢弃，但深链本身仍可解析（pollUuid 有效）。
        let link = parse_deep_link(&raw).expect("深链结构应合法");
        assert!(link.access_token.is_none());
    }

    #[test]
    fn build_redirect_uri_round_trip() {
        let uuid = Uuid::parse_str("11111111-2222-4333-8444-555555555555").unwrap();
        let raw = build_redirect_uri(&uuid);
        let link = parse_deep_link(&raw).expect("深链应合法");
        assert_eq!(link.poll_uuid.as_deref(), Some(uuid.to_string().as_str()));
        assert!(!link.has_credentials());
    }

    #[test]
    fn percent_encode_keeps_unreserved() {
        assert_eq!(percent_encode("aZ0-_.~"), "aZ0-_.~");
        assert_eq!(percent_encode("a b/"), "a%20b%2F");
    }

    fn sample_status() -> AuthStatus {
        AuthStatus {
            state: "authenticated",
            user: Some("tester".to_string()),
            display_name: Some("测试用户".to_string()),
            expires_at: Some(1_700_086_400),
            offline_grace: false,
            message: None,
            dept: Some("测试部门".to_string()),
            server_url: Some("https://api.example.test".to_string()),
            issued_at: Some(1_700_000_000),
            credentials_valid: true,
            pending_poll_uuid: None,
            pending_login_started_at: None,
            deep_link_received: false,
            last_validated_at: Some("2026-10-20T00:00:00Z".to_string()),
        }
    }

    /// `status_changed` 是"要不要广播 auth://state"的唯一判据：
    /// 每个前端可见字段都必须能触发变化，否则状态更新会被静默吞掉。
    #[test]
    fn status_changed_detects_every_observable_field() {
        let base = sample_status();
        assert!(
            !status_changed(&base, &base.clone()),
            "相同快照不应判为变化"
        );

        let mut mutations: Vec<AuthStatus> = Vec::new();
        macro_rules! mutate {
            ($field:ident, $value:expr) => {{
                let mut next = base.clone();
                next.$field = $value;
                mutations.push(next);
            }};
        }
        mutate!(state, "pending");
        mutate!(user, None);
        mutate!(display_name, None);
        mutate!(expires_at, None);
        mutate!(offline_grace, true);
        mutate!(message, Some("登录状态已失效，请重新登录".to_string()));
        mutate!(dept, None);
        mutate!(server_url, None);
        mutate!(issued_at, None);
        mutate!(credentials_valid, false);
        mutate!(pending_poll_uuid, Some("poll-uuid".to_string()));
        mutate!(
            pending_login_started_at,
            Some("2026-10-20T00:00:00Z".to_string())
        );
        mutate!(deep_link_received, true);
        mutate!(last_validated_at, None);

        for (index, next) in mutations.iter().enumerate() {
            assert!(
                status_changed(&base, next),
                "第 {index} 个字段变化必须被 status_changed 捕获"
            );
        }
    }
}
