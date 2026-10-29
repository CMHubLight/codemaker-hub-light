//! 网关 HTTP 客户端与响应解析（P1）。
//!
//! 契约：`docs/gateway-contract.md` §2。所有请求 20s 超时；错误信息只带 HTTP 状态/
//! 分类，不打印响应正文（正文可能含 token）。
//!
//! 解析全部容忍字段别名与 `{"data": {...}}` 包裹，便于服务端小改字段时降级。

use std::time::Duration;

use reqwest::{
    header::{ACCEPT, CONTENT_TYPE},
    Client,
};
use serde_json::Value;

use crate::auth::{
    hourly::{parse_hourly_cost, HourlyCost},
    ModelSummary, HTTP_TIMEOUT_SECS, MAX_TOKEN_LEN, MAX_USER_LEN, PLACEHOLDER_VALUES,
};

const X_ACCESS_TOKEN: &str = "X-Access-Token";

#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("网络请求失败：{0}")]
    Network(String),
    #[error("上游返回 HTTP {status}")]
    Status { status: u16 },
    #[error("上游响应无法解析：{0}")]
    Decode(String),
    #[error("{0}")]
    Remote(String),
}

impl GatewayError {
    pub fn is_forbidden(&self) -> bool {
        matches!(self, GatewayError::Status { status } if matches!(status, 401 | 403))
    }
}

/// 只有"服务端明确不提供/无权限"才容忍（401/403/404）；5xx、429 等必须上抛。
///
/// 否则网关故障会被误读成"取不到 key"，继续拿旧凭据续期，把真实故障掩盖成
/// 一次静默降级。
fn tolerates_missing(status: u16) -> bool {
    matches!(status, 401 | 403 | 404)
}

/// 登录成功返回的 token 集合（只在内存流转；`Debug` 已脱敏）。
#[derive(Clone)]
pub struct LoginTokens {
    pub access_token: String,
    pub auth_key: Option<String>,
    pub user: Option<String>,
    pub server_url: Option<String>,
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for LoginTokens {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoginTokens")
            .field("access_token", &"<redacted>")
            .field("auth_key", &self.auth_key.as_ref().map(|_| "<redacted>"))
            .field("user", &self.user)
            .field("server_url", &self.server_url)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// `/validate` 解析结果。
#[derive(Debug, Clone, Default)]
pub struct ValidateInfo {
    pub dept: Option<String>,
    pub display_name: Option<String>,
    pub gateway: Option<String>,
    pub server_url: Option<String>,
    pub models: Vec<ModelSummary>,
}

#[derive(Debug, Clone)]
pub enum PollOutcome {
    /// 未登录 / 未知 uuid（返回 200 {}）。
    Pending,
    /// 服务端明确拒绝（credentials_invalid）。
    Rejected(String),
    Success(LoginTokens),
}

#[derive(Debug, Clone)]
pub enum QuotaOutcome {
    Ok(Value),
    Forbidden,
    NotFound,
}

/// `/vega/user_hourly_cost` 的三类可用结果。
#[derive(Debug, Clone)]
pub enum HourlyCostOutcome {
    Ok(HourlyCost),
    Forbidden,
    NotFound,
}

#[derive(Clone)]
pub struct RenewedToken {
    pub access_token: String,
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for RenewedToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RenewedToken")
            .field("access_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// 已规范化（无尾 `/`）的网关客户端。
pub struct AuthGateway {
    base: String,
    http: Client,
}

impl AuthGateway {
    pub fn new(gateway_url: &str) -> Result<Self, crate::auth::AuthError> {
        let base = normalize_gateway(gateway_url)
            .ok_or_else(|| crate::auth::AuthError::InvalidGateway(gateway_url.to_string()))?;
        let http = Client::builder()
            .timeout(Duration::from_secs(HTTP_TIMEOUT_SECS))
            .connect_timeout(Duration::from_secs(10))
            // 不跟随重定向：X-Access-Token 是自定义头，reqwest 跨主机重定向只剥离
            // authorization/cookie 等标准头，token 会跟着跳到 Location 主机。
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("codemaker-hub-light/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|err| {
                crate::auth::AuthError::Http(format!("初始化 HTTP 客户端失败：{err}"))
            })?;
        Ok(Self { base, http })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `GET /vs_login_info/{pollUuid}`（未登录返回 200 {}）。
    pub async fn poll_login(&self, poll_uuid: &str) -> Result<PollOutcome, GatewayError> {
        let url = format!("{}/vs_login_info/{}", self.base, poll_uuid);
        let (status, body) = self.get_json(&url).await?;
        if !(200..300).contains(&status) {
            if matches!(status, 401 | 403) {
                return Ok(PollOutcome::Rejected(
                    "服务端拒绝该登录请求，请重新发起登录".to_string(),
                ));
            }
            return Err(GatewayError::Status { status });
        }
        let value = body.unwrap_or(Value::Null);
        Ok(parse_poll_response(&value))
    }

    /// `POST /api/v2/tokens`：账号 + AuthKey 直登（不经过浏览器/轮询）。
    ///
    /// 请求体为 `{user, auth_key}`，响应为 `AuthV2TokenResponse`（字段见
    /// `docs/gateway-contract.md`）。401/403 由调用方映射为"账号或 AuthKey 不正确"。
    pub async fn login_with_auth_key(
        &self,
        user: &str,
        auth_key: &str,
    ) -> Result<LoginTokens, GatewayError> {
        let url = format!("{}/api/v2/tokens", self.base);
        let response = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&serde_json::json!({ "user": user, "auth_key": auth_key }))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // 不读正文：错误正文可能带服务端细节，且请求本身含凭据。
            return Err(GatewayError::Status { status });
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|err| GatewayError::Decode(format!("登录响应无法解析：{err}")))?;
        parse_auth_v2_token(&value)
    }

    /// `GET /get_auth_key`；401/403/404 容忍（登录可用但无法自动续期），其余失败上抛。
    pub async fn get_auth_key(&self, access_token: &str) -> Result<Option<String>, GatewayError> {
        let url = format!("{}/get_auth_key", self.base);
        let response = self
            .http
            .get(&url)
            .header(X_ACCESS_TOKEN, access_token)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return if tolerates_missing(status) {
                Ok(None)
            } else {
                Err(GatewayError::Status { status })
            };
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|err| GatewayError::Decode(format!("auth_key 响应无法解析：{err}")))?;
        Ok(parse_auth_key(&value))
    }

    /// `GET /validate`；403 视为凭据失效，网络错误交由调用方进入 offline grace。
    pub async fn validate(&self, access_token: &str) -> Result<ValidateInfo, GatewayError> {
        let url = format!("{}/validate", self.base);
        let (status, body) = self.get_json_with_token(&url, access_token).await?;
        if !(200..300).contains(&status) {
            return Err(GatewayError::Status { status });
        }
        Ok(parse_validate(&body.unwrap_or(Value::Null)))
    }

    /// `GET /validate/models`；401/403/404 容忍为空目录（登录不因此失败），其余上抛。
    pub async fn validate_models(
        &self,
        access_token: &str,
    ) -> Result<Vec<ModelSummary>, GatewayError> {
        let url = format!("{}/validate/models", self.base);
        let (status, body) = self.get_json_with_token(&url, access_token).await?;
        if !(200..300).contains(&status) {
            return if tolerates_missing(status) {
                Ok(Vec::new())
            } else {
                Err(GatewayError::Status { status })
            };
        }
        Ok(parse_models(&body.unwrap_or(Value::Null)))
    }

    /// `GET /api/v1/user_quota/usage?quota_type=global`。
    pub async fn quota(&self, access_token: &str) -> Result<QuotaOutcome, GatewayError> {
        let url = format!("{}/api/v1/user_quota/usage?quota_type=global", self.base);
        let (status, body) = self.get_json_with_token(&url, access_token).await?;
        match status {
            200..=299 => Ok(QuotaOutcome::Ok(body.unwrap_or(Value::Null))),
            401 | 403 => Ok(QuotaOutcome::Forbidden),
            404 => Ok(QuotaOutcome::NotFound),
            other => Err(GatewayError::Status { status: other }),
        }
    }

    /// `GET /vega/user_hourly_cost?start_time=...&end_time=...`。
    ///
    /// 该接口只接受 `X-Access-Token`，不需要 `X-Auth-User` 或额外签名；
    /// 服务端把“时间范围超过 24 小时”等业务错误放在 HTTP 200 的 `error` 字段中。
    pub async fn hourly_cost(
        &self,
        access_token: &str,
        start_time: &str,
        end_time: &str,
    ) -> Result<HourlyCostOutcome, GatewayError> {
        let url = format!("{}/vega/user_hourly_cost", self.base);
        let response = self
            .http
            .get(&url)
            .query(&[("start_time", start_time), ("end_time", end_time)])
            .header(X_ACCESS_TOKEN, access_token)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        if matches!(status, 401 | 403) {
            return Ok(HourlyCostOutcome::Forbidden);
        }
        if status == 404 {
            return Ok(HourlyCostOutcome::NotFound);
        }
        if !(200..300).contains(&status) {
            return Err(GatewayError::Status { status });
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|err| GatewayError::Decode(format!("小时费用响应无法解析：{err}")))?;
        parse_hourly_cost(&value)
            .map(HourlyCostOutcome::Ok)
            .map_err(GatewayError::Remote)
    }

    /// `POST /renew_token`：契约见 docs/gateway-contract.md。
    ///
    /// - 只接受 `POST` + `Content-Type: application/json`（三种"鉴权头"传法全部 422）；
    /// - body 必须是 `{"key": <续期 key>, "user": <用户标识>}`；
    /// - 响应 `{token, user, ttl, expire}`，`expire` 为 epoch 秒。
    ///
    /// 续期 key（128 字符）来自 [`Self::get_auth_key`]，**不是**深链里的
    /// `access_sso_token`（225 字符）：用错 key 时服务端返回 403 `decrypt auth key fail`。
    pub async fn renew_token(&self, key: &str, user: &str) -> Result<RenewedToken, GatewayError> {
        let url = format!("{}/renew_token", self.base);
        let response = self
            .http
            .post(&url)
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json")
            .json(&serde_json::json!({ "key": key, "user": user }))
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // 只记状态码：错误正文可能带服务端细节，且续期请求本身含凭据。
            tracing::warn!(status, "续期请求被拒绝");
            return Err(GatewayError::Status { status });
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|err| GatewayError::Decode(format!("续期响应无法解析：{err}")))?;
        parse_renew_response(&value)
    }

    /// 2xx 的响应体必须可解析：解析失败要上抛，不能退化成"成功但空"。
    ///
    /// 否则 200 + 畸形体会被 `/validate` 当成"已校验通过"，把损坏的响应写进登录态。
    async fn get_json(&self, url: &str) -> Result<(u16, Option<Value>), GatewayError> {
        let response = self
            .http
            .get(url)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Self::read_json_response(response).await
    }

    async fn get_json_with_token(
        &self,
        url: &str,
        access_token: &str,
    ) -> Result<(u16, Option<Value>), GatewayError> {
        let response = self
            .http
            .get(url)
            .header(X_ACCESS_TOKEN, access_token)
            .header(ACCEPT, "application/json")
            .send()
            .await
            .map_err(map_reqwest_error)?;
        Self::read_json_response(response).await
    }

    /// 状态码 + （仅 2xx 时）解析后的 JSON；非 2xx 不读正文（正文可能含敏感字段）。
    async fn read_json_response(
        response: reqwest::Response,
    ) -> Result<(u16, Option<Value>), GatewayError> {
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok((status, None));
        }
        let value = response
            .json::<Value>()
            .await
            .map_err(|err| GatewayError::Decode(format!("响应无法解析：{err}")))?;
        Ok((status, Some(value)))
    }
}

fn map_reqwest_error(err: reqwest::Error) -> GatewayError {
    if err.is_timeout() {
        return GatewayError::Network("请求超时".to_string());
    }
    if err.is_connect() {
        return GatewayError::Network("无法连接网关".to_string());
    }
    // 去掉 URL（避免把 query 里的信息写进错误/日志）。
    GatewayError::Network(err.without_url().to_string())
}

fn normalize_gateway(gateway_url: &str) -> Option<String> {
    let trimmed = gateway_url.trim().trim_end_matches('/');
    // 与设置页同一判定：https 任意主机；http 仅本机回环（凭据要发往该地址）。
    if crate::settings::gateway_url_is_acceptable(trimmed) {
        Some(trimmed.to_string())
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// 响应解析（纯函数，便于 fixture 单测）
// ---------------------------------------------------------------------------

/// 若响应被 `{"data": {...}}` 包裹则取内层。
pub(crate) fn unwrap_data(value: &Value) -> &Value {
    match value.get("data") {
        Some(inner) if inner.is_object() => inner,
        _ => value,
    }
}

pub(crate) fn string_field(value: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(found) = value.get(*key) {
            let text = match found {
                Value::String(text) => Some(text.trim().to_string()),
                Value::Number(number) => Some(number.to_string()),
                _ => None,
            };
            if let Some(text) = text.filter(|text| !text.is_empty()) {
                return Some(text);
            }
        }
    }
    None
}

pub(crate) fn numeric_field(value: &Value, keys: &[&str]) -> Option<f64> {
    for key in keys {
        if let Some(found) = value.get(*key) {
            match found {
                Value::Number(number) => {
                    if let Some(as_f64) = number.as_f64() {
                        return Some(as_f64);
                    }
                }
                Value::String(text) => {
                    if let Ok(parsed) = text.trim().parse::<f64>() {
                        return Some(parsed);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

pub(crate) fn int_field(value: &Value, keys: &[&str]) -> Option<i64> {
    numeric_field(value, keys).map(|number| number as i64)
}

pub(crate) fn bool_field(value: &Value, keys: &[&str]) -> Option<bool> {
    for key in keys {
        if let Some(found) = value.get(*key) {
            match found {
                Value::Bool(flag) => return Some(*flag),
                Value::Number(number) => return Some(number.as_i64().unwrap_or(0) != 0),
                Value::String(text) => {
                    let normalized = text.trim().to_ascii_lowercase();
                    if normalized == "true" || normalized == "1" {
                        return Some(true);
                    }
                    if normalized == "false" || normalized == "0" || normalized.is_empty() {
                        return Some(false);
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn clean_secret(value: Option<String>) -> Option<String> {
    value
        .filter(|text| text.chars().count() <= MAX_TOKEN_LEN)
        .filter(|text| !text.chars().any(char::is_control))
        .filter(|text| {
            !PLACEHOLDER_VALUES
                .iter()
                .any(|placeholder| text.eq_ignore_ascii_case(placeholder))
        })
}

fn clean_user(value: Option<String>) -> Option<String> {
    value
        .filter(|text| text.chars().count() <= MAX_USER_LEN)
        .filter(|text| !text.chars().any(char::is_control))
}

/// 解析 `/vs_login_info/{uuid}` 响应（别名见 docs/gateway-contract.md）。
pub(crate) fn parse_poll_response(value: &Value) -> PollOutcome {
    let data = unwrap_data(value);

    if bool_field(data, &["credentials_invalid", "credentialsInvalid"]).unwrap_or(false) {
        return PollOutcome::Rejected("服务端返回凭据已失效".to_string());
    }

    let access_token = clean_secret(string_field(
        data,
        &["access_token", "accessToken", "token"],
    ));
    let Some(access_token) = access_token else {
        return PollOutcome::Pending;
    };

    PollOutcome::Success(LoginTokens {
        access_token,
        auth_key: clean_secret(string_field(
            data,
            &["auth_key", "authkey", "access_sso_token", "sso_token"],
        )),
        user: clean_user(string_field(
            data,
            &["user", "access_user", "username", "employee_id"],
        )),
        server_url: string_field(data, &["serverUrl", "server_url", "server"]),
        expires_at: int_field(data, &["expire", "expires_at", "expiresAt", "expire_at"]),
    })
}

/// 解析 `/get_auth_key`。
pub(crate) fn parse_auth_key(value: &Value) -> Option<String> {
    clean_secret(string_field(
        unwrap_data(value),
        &["auth_key", "authkey", "sso_token"],
    ))
}

/// 解析 `/validate`。
pub(crate) fn parse_validate(value: &Value) -> ValidateInfo {
    let data = unwrap_data(value);
    ValidateInfo {
        dept: string_field(data, &["dept", "department"]),
        display_name: string_field(data, &["display_name", "displayName", "name"]),
        gateway: string_field(data, &["gateway"]),
        server_url: string_field(data, &["server_url", "serverUrl", "server"]),
        models: parse_models(data),
    }
}

/// 解析模型列表：
/// - 顶层数组；或 `models` / `model` / `completion_langs` 数组；
/// - 元素可以是字符串（id）或对象（`id`/`model`/`name`）。
pub(crate) fn parse_models(value: &Value) -> Vec<ModelSummary> {
    let unwrapped = unwrap_data(value);
    let array = if let Value::Array(items) = unwrapped {
        Some(items)
    } else {
        ["models", "model", "model_list", "completion_langs"]
            .iter()
            .find_map(|key| unwrapped.get(*key).and_then(Value::as_array))
    };

    let Some(items) = array else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|item| match item {
            Value::String(id) => {
                let id = id.trim();
                (!id.is_empty()).then(|| ModelSummary {
                    id: id.to_string(),
                    name: None,
                })
            }
            Value::Object(_) => {
                let id = string_field(item, &["id", "model", "name"])?;
                Some(ModelSummary {
                    id,
                    name: string_field(item, &["display_name", "displayName", "label"]).or_else(
                        || {
                            string_field(item, &["name"]).filter(|name| {
                                string_field(item, &["id", "model"]).as_deref() != Some(name)
                            })
                        },
                    ),
                })
            }
            _ => None,
        })
        .collect()
}

/// 解析 `POST /renew_token` 响应。
pub(crate) fn parse_renew_response(value: &Value) -> Result<RenewedToken, GatewayError> {
    let data = unwrap_data(value);
    let access_token = clean_secret(string_field(
        data,
        &["access_token", "accessToken", "token"],
    ))
    .ok_or_else(|| GatewayError::Decode("续期响应缺少 access_token".to_string()))?;
    Ok(RenewedToken {
        access_token,
        expires_at: int_field(data, &["expire", "expires_at", "expiresAt", "expire_at"]),
    })
}

/// 解析 `POST /api/v2/tokens` 响应（`AuthV2TokenResponse`）。
///
/// 线上样本只确认了 5 个字段的存在；这里按 `StoredCredentials` 的既有别名表容错，
/// 缺少 `expire` 时由 service 层按 `DEFAULT_TOKEN_TTL_SECS` 兜底。
pub(crate) fn parse_auth_v2_token(value: &Value) -> Result<LoginTokens, GatewayError> {
    let data = unwrap_data(value);
    let access_token = clean_secret(string_field(
        data,
        &["access_token", "accessToken", "token"],
    ))
    .ok_or_else(|| GatewayError::Decode("登录响应缺少 access_token".to_string()))?;
    Ok(LoginTokens {
        access_token,
        auth_key: clean_secret(string_field(
            data,
            &["auth_key", "authkey", "access_sso_token", "sso_token"],
        )),
        user: clean_user(string_field(
            data,
            &["access_user", "user", "username", "employee_id"],
        )),
        server_url: string_field(data, &["server_url", "serverUrl", "server"]),
        expires_at: int_field(data, &["expire", "expires_at", "expiresAt", "expire_at"]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN_PENDING: &str = include_str!("../../tests/fixtures/auth/login_pending.json");
    const LOGIN_SUCCESS: &str = include_str!("../../tests/fixtures/auth/login_success.json");
    const LOGIN_SUCCESS_ALIASES: &str =
        include_str!("../../tests/fixtures/auth/login_success_aliases.json");
    const LOGIN_INVALID: &str = include_str!("../../tests/fixtures/auth/login_invalid.json");
    const VALIDATE: &str = include_str!("../../tests/fixtures/auth/validate.json");
    const VALIDATE_WRAPPED: &str = include_str!("../../tests/fixtures/auth/validate_wrapped.json");
    const MODELS: &str = include_str!("../../tests/fixtures/auth/models.json");
    const RENEW: &str = include_str!("../../tests/fixtures/auth/renew.json");
    const RENEW_ALIASES: &str = include_str!("../../tests/fixtures/auth/renew_aliases.json");
    const AUTH_KEY: &str = include_str!("../../tests/fixtures/auth/auth_key.json");

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).expect("fixture 应为合法 JSON")
    }

    #[test]
    fn pending_login_is_pending() {
        assert!(matches!(
            parse_poll_response(&parse(LOGIN_PENDING)),
            PollOutcome::Pending
        ));
        let empty = parse("{}");
        assert!(matches!(parse_poll_response(&empty), PollOutcome::Pending));
        let logged_in_without_token = parse(r#"{"loggedIn": true}"#);
        assert!(matches!(
            parse_poll_response(&logged_in_without_token),
            PollOutcome::Pending
        ));
    }

    #[test]
    fn parses_login_success() {
        match parse_poll_response(&parse(LOGIN_SUCCESS)) {
            PollOutcome::Success(tokens) => {
                assert_eq!(tokens.access_token, "test-access-token");
                assert_eq!(tokens.auth_key.as_deref(), Some("test-auth-key"));
                assert_eq!(tokens.user.as_deref(), Some("test-user"));
                assert_eq!(tokens.expires_at, Some(1_700_086_400));
                assert_eq!(
                    tokens.server_url.as_deref(),
                    Some("https://api.example.test")
                );
            }
            other => panic!("期望成功，实际 {other:?}"),
        }
    }

    #[test]
    fn parses_login_success_with_aliases_and_data_wrapper() {
        match parse_poll_response(&parse(LOGIN_SUCCESS_ALIASES)) {
            PollOutcome::Success(tokens) => {
                assert_eq!(tokens.access_token, "test-access-token");
                assert_eq!(tokens.auth_key.as_deref(), Some("test-auth-key"));
                assert_eq!(tokens.user.as_deref(), Some("test-user"));
                assert_eq!(tokens.expires_at, Some(1_700_086_400));
            }
            other => panic!("期望成功，实际 {other:?}"),
        }
    }

    #[test]
    fn invalid_credentials_are_rejected() {
        assert!(matches!(
            parse_poll_response(&parse(LOGIN_INVALID)),
            PollOutcome::Rejected(_)
        ));
    }

    #[test]
    fn placeholder_tokens_are_ignored() {
        let value = parse(
            r#"{"loggedIn": true, "access_token": "unknown_access_token", "auth_key": "unknown_sso_token"}"#,
        );
        assert!(matches!(parse_poll_response(&value), PollOutcome::Pending));
    }

    #[test]
    fn parses_validate_and_models() {
        let info = parse_validate(&parse(VALIDATE));
        assert_eq!(info.dept.as_deref(), Some("测试部门"));
        assert_eq!(info.gateway.as_deref(), Some("https://api.example.test"));
        assert_eq!(info.models.len(), 2);
        assert_eq!(info.models[0].id, "claude-sonnet-4");
        assert_eq!(info.models[0].name.as_deref(), Some("Claude Sonnet 4"));
        assert_eq!(info.models[1].id, "gpt-4o");

        let wrapped = parse_validate(&parse(VALIDATE_WRAPPED));
        assert_eq!(wrapped.dept.as_deref(), Some("包装部门"));
        assert_eq!(wrapped.models.len(), 1);

        let models = parse_models(&parse(MODELS));
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4");
    }

    #[test]
    fn parses_auth_key() {
        assert_eq!(
            parse_auth_key(&parse(AUTH_KEY)).as_deref(),
            Some("test-auth-key")
        );
        assert!(parse_auth_key(&parse(r#"{}"#)).is_none());
    }

    #[test]
    fn parses_renew_responses() {
        let token = parse_renew_response(&parse(RENEW)).expect("续期样本应可解析");
        assert_eq!(token.access_token, "test-access-token-2");
        assert_eq!(token.expires_at, Some(1_700_172_800));

        let aliased = parse_renew_response(&parse(RENEW_ALIASES)).expect("别名样本应可解析");
        assert_eq!(aliased.access_token, "test-access-token-3");
        assert_eq!(aliased.expires_at, Some(1_700_259_200));

        assert!(parse_renew_response(&parse(r#"{"error":"denied"}"#)).is_err());
    }

    #[test]
    fn gateway_normalization() {
        assert_eq!(
            normalize_gateway("https://api.example.test///").as_deref(),
            Some("https://api.example.test")
        );
        assert!(normalize_gateway("api.example.test").is_none());
        assert!(AuthGateway::new("https://api.example.test").is_ok());
        assert!(AuthGateway::new("api.example.test").is_err());
    }

    /// 明文 http 会把账号凭据暴露在链路上：除本机回环外一律拒绝。
    #[test]
    fn plain_http_gateway_rejected_except_loopback() {
        assert!(normalize_gateway("http://gateway.example.test").is_none());
        assert!(AuthGateway::new("http://gateway.example.test").is_err());
        assert!(AuthGateway::new("http://127.0.0.1:9").is_ok());
        assert!(AuthGateway::new("http://localhost:9").is_ok());
    }

    /// `POST /renew_token` 只认 JSON body
    /// `{key, user}`；旧实现的三种鉴权头/`{auth_key}` 全部 422。
    #[tokio::test]
    async fn renew_posts_key_user_json_and_parses_response() {
        use std::sync::{Arc, Mutex};
        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let captured = Arc::new(Mutex::new(String::new()));
        let recorder = captured.clone();

        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&mut socket).await;
            *recorder.lock().expect("锁可用") = request;
            let body = r#"{"token":"new-token","user":"test-user","ttl":"86400000","expire":"1700172800"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let renewed = gateway
            .renew_token("fresh-128-char-key", "test-user")
            .await
            .expect("200 应解析成功");
        assert_eq!(renewed.access_token, "new-token");
        assert_eq!(renewed.expires_at, Some(1_700_172_800));

        let request = captured.lock().expect("锁可用").clone();
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.starts_with("post /renew_token"),
            "必须是 POST /renew_token：{lower}"
        );
        assert!(
            lower.contains("content-type: application/json"),
            "必须带 JSON Content-Type：{lower}"
        );
        let body_text = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let body: Value = serde_json::from_str(body_text).expect("body 应是 JSON");
        assert_eq!(
            body.get("key").and_then(Value::as_str),
            Some("fresh-128-char-key")
        );
        assert_eq!(body.get("user").and_then(Value::as_str), Some("test-user"));
        assert!(body.get("auth_key").is_none(), "不得再发旧的 auth_key 字段");

        server.abort();
    }

    /// 403（`decrypt auth key fail`）必须一次失败即返回，不做多次重试。
    #[tokio::test]
    async fn renew_403_is_single_attempt_and_forbidden() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let server = tokio::spawn(async move {
            let mut handles = Vec::new();
            for _ in 0..4 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                handles.push(tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = socket.read(&mut buf).await;
                    let body = r#"{"msg":"decrypt auth key fail","extra":null}"#;
                    let response = format!(
                        "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }));
            }
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let err = gateway
            .renew_token("wrong-kind-key", "test-user")
            .await
            .expect_err("403 必须失败");
        assert!(err.is_forbidden(), "403 应被识别为需要重新登录：{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "403 不应重试");

        server.abort();
    }

    /// 5xx 不能被当成"没有值"：`get_auth_key` / `validate_models` 必须上抛；
    /// 只有 401/403/404 容忍；200 + 畸形 JSON 也必须上抛（不能判成"已校验"）。
    #[tokio::test]
    async fn provider_5xx_is_not_a_success() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let responses: Vec<(&str, &str)> = vec![
            ("500 Internal Server Error", r#"{"error":"boom"}"#),
            ("500 Internal Server Error", r#"{"error":"boom"}"#),
            ("401 Unauthorized", r#"{"msg":"no"}"#),
            ("404 Not Found", r#"{}"#),
            ("200 OK", r#"{"models": [not-json"#),
        ];
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let server = tokio::spawn(async move {
            for (status_line, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");

        let err = gateway
            .get_auth_key("test-token")
            .await
            .expect_err("get_auth_key 5xx 不能返回 Ok(None)");
        assert!(
            matches!(err, GatewayError::Status { status: 500 }),
            "get_auth_key 5xx 应上抛状态码：{err}"
        );

        let err = gateway
            .validate_models("test-token")
            .await
            .expect_err("validate_models 5xx 不能返回空列表");
        assert!(
            matches!(err, GatewayError::Status { status: 500 }),
            "validate_models 5xx 应上抛状态码：{err}"
        );

        assert!(
            gateway
                .get_auth_key("test-token")
                .await
                .expect("401 应容忍")
                .is_none(),
            "401 表示该能力不可用，容忍为 None"
        );
        assert!(
            gateway
                .validate_models("test-token")
                .await
                .expect("404 应容忍")
                .is_empty(),
            "404 表示接口不存在，容忍为空目录"
        );

        let err = gateway
            .validate("test-token")
            .await
            .expect_err("200 + 畸形 JSON 必须失败");
        assert!(
            matches!(err, GatewayError::Decode(_)),
            "畸形响应应归类为解码失败：{err}"
        );

        server.abort();
    }

    /// 读取完整 HTTP 请求（头 + Content-Length 指定的 body）。
    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match socket.read(&mut chunk).await {
                Ok(0) => break,
                Ok(read) => {
                    buf.extend_from_slice(&chunk[..read]);
                    if let Some(header_end) =
                        buf.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let headers =
                            String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
                        let length = headers
                            .lines()
                            .find_map(|line| line.strip_prefix("content-length:"))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if buf.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                    if buf.len() > 16 * 1024 {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// `POST /api/v2/tokens` 只认 JSON body `{user, auth_key}`，且响应字段可解析。
    #[tokio::test]
    async fn auth_key_login_posts_user_and_key_json() {
        use std::sync::{Arc, Mutex};
        use tokio::{io::AsyncWriteExt, net::TcpListener};

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let captured = Arc::new(Mutex::new(String::new()));
        let recorder = captured.clone();

        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_http_request(&mut socket).await;
            *recorder.lock().expect("锁可用") = request;
            let body = r#"{"access_token":"new-access-token","access_sso_token":"new-auth-key","access_user":"alice","server_url":"https://api.example.test","login_time":"1700000000"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let tokens = gateway
            .login_with_auth_key("alice", "secret-auth-key")
            .await
            .expect("200 应解析成功");
        assert_eq!(tokens.access_token, "new-access-token");
        assert_eq!(tokens.auth_key.as_deref(), Some("new-auth-key"));
        assert_eq!(tokens.user.as_deref(), Some("alice"));
        assert_eq!(
            tokens.server_url.as_deref(),
            Some("https://api.example.test")
        );

        let request = captured.lock().expect("锁可用").clone();
        let lower = request.to_ascii_lowercase();
        assert!(
            lower.starts_with("post /api/v2/tokens"),
            "必须是 POST /api/v2/tokens：{lower}"
        );
        assert!(
            lower.contains("content-type: application/json"),
            "必须带 JSON Content-Type：{lower}"
        );
        assert!(
            !lower.contains("x-access-token"),
            "AuthKey 登录不应带 X-Access-Token：{lower}"
        );
        let body_text = request.split("\r\n\r\n").nth(1).unwrap_or_default();
        let body: Value = serde_json::from_str(body_text).expect("body 应是 JSON");
        assert_eq!(body.get("user").and_then(Value::as_str), Some("alice"));
        assert_eq!(
            body.get("auth_key").and_then(Value::as_str),
            Some("secret-auth-key")
        );

        server.abort();
    }

    /// 401/403 必须让调用方识别为"账号或 AuthKey 不正确"，不能被当成网络错误。
    #[tokio::test]
    async fn auth_key_login_401_is_forbidden() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"msg":"invalid auth key"}"#;
            let response = format!(
                "HTTP/1.1 401 Unauthorized\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let err = gateway
            .login_with_auth_key("alice", "wrong-key")
            .await
            .expect_err("401 必须失败");
        assert!(err.is_forbidden(), "401 应被识别为凭据错误：{err}");

        server.abort();
    }

    /// 200 但缺少 access_token 必须报解码错误，不能把损坏响应当成登录成功。
    #[tokio::test]
    async fn auth_key_login_empty_token_is_decode_error() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("应可绑定");
        let addr = listener.local_addr().expect("应可读取地址");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"access_user":"alice","login_time":"1700000000"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let err = gateway
            .login_with_auth_key("alice", "secret-auth-key")
            .await
            .expect_err("缺 access_token 必须失败");
        assert!(matches!(err, GatewayError::Decode(_)));

        server.abort();
    }

    /// 响应字段别名 / `data` 包裹的纯函数测试（不依赖网络）。
    #[test]
    fn parses_auth_v2_token_aliases() {
        let tokens = parse_auth_v2_token(&parse(
            r#"{"data":{"accessToken":"SECRET-ACCESS-VALUE","access_sso_token":"SECRET-KEY-VALUE","username":"bob","serverUrl":"https://gw.test","expires_at":1800000000}}"#,
        ))
        .expect("别名样本应可解析");
        assert_eq!(tokens.access_token, "SECRET-ACCESS-VALUE");
        assert_eq!(tokens.auth_key.as_deref(), Some("SECRET-KEY-VALUE"));
        assert_eq!(tokens.user.as_deref(), Some("bob"));
        assert_eq!(tokens.server_url.as_deref(), Some("https://gw.test"));
        assert_eq!(tokens.expires_at, Some(1_800_000_000));
        let debug = format!("{tokens:?}");
        assert!(
            !debug.contains("SECRET-ACCESS-VALUE") && !debug.contains("SECRET-KEY-VALUE"),
            "LoginTokens Debug 必须脱敏：{debug}"
        );
    }
}
