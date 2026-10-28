//! 认证相关命令（P1：深链/轮询完成登录、validate、额度、登出）。
//!
//! 返回结构保持 P0 已对齐的前端契约（`src/lib/types.ts`）：
//! - `auth_start_login` → `LoginSession { login_url, poll_uuid }`
//! - `auth_status` / `auth_poll_status` / `auth_refresh_validate` / `auth_logout` → `AuthStatus`
//! - `quota_get` → `QuotaInfo | null`（`force` 为可选参数）
//!
//! token 永不出现在返回值、错误字符串或事件里；事件只带 [`crate::auth::AuthStatus`]。

use serde::Serialize;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_opener::OpenerExt;

use crate::{
    auth::{self, service::AuthUpdate, AuthStatus},
    state::AppState,
    usage::QuotaInfo,
};

/// 发起登录的返回值（前端 `LoginSession` 读取 `login_url` / `poll_uuid`）。
#[derive(Debug, Clone, Serialize)]
pub struct AuthLoginStart {
    pub login_url: String,
    pub poll_uuid: String,
    // ---- 附加字段 ----
    pub redirect_uri: String,
    pub poll_interval_secs: u64,
    pub timeout_secs: u64,
    pub opened_browser: bool,
    pub hint: String,
}

fn emit_update(app: &AppHandle, update: &AuthUpdate) {
    // 事件契约：auth://state、quota://updated（docs/ipc-api.md）。
    let _ = app.emit("auth://state", &update.status);
    if update.quota_updated {
        if let Some(quota) = &update.quota {
            let _ = app.emit("quota://updated", quota);
        }
    }
}

#[tauri::command]
pub async fn auth_status(app: AppHandle, state: State<'_, AppState>) -> Result<AuthStatus, String> {
    let auth = state.auth.clone();
    // 启动后首次调用时懒加载 auth.bin；有凭据则确保后台循环已启动。
    auth.ensure_loaded().await;
    if auth.has_credentials().await {
        auth.ensure_loops(app).await;
    }
    Ok(auth.status().await)
}

#[tauri::command]
pub async fn auth_start_login(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AuthLoginStart, String> {
    let gateway_url = state.settings.read().await.gateway_url.clone();
    let pending = state
        .auth
        .start_login(&gateway_url)
        .await
        .map_err(|err| err.to_string())?;

    // 让旧登录页/其他窗口也能看到 pending 状态。
    let _ = app.emit("auth://state", &state.auth.status().await);

    // 调起系统浏览器；失败时前端可展示 login_url 供手动复制。
    let opened_browser = match app
        .opener()
        .open_url(pending.login_url.clone(), None::<&str>)
    {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(error = %err, "打开系统浏览器失败");
            false
        }
    };

    Ok(AuthLoginStart {
        login_url: pending.login_url,
        poll_uuid: pending.poll_uuid.to_string(),
        redirect_uri: pending.redirect_uri,
        poll_interval_secs: crate::auth::POLL_INTERVAL_SECS,
        timeout_secs: crate::auth::POLL_TIMEOUT_SECS,
        opened_browser,
        hint: "已打开系统浏览器；完成登录后会自动更新（也可点击重新登录）".to_string(),
    })
}

/// 轮询 `GET /vs_login_info/{uuid}`；成功后保存凭据、validate、拉额度并切换登录态。
#[tauri::command]
pub async fn auth_poll_status(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AuthStatus, String> {
    let gateway_url = state.settings.read().await.gateway_url.clone();
    // 登录页每 2s 轮询一次；只有状态/额度真的变化才广播事件，避免空转事件
    // 让 shell 与概览页（含一次 24h 云端费用请求）跟着重复重拉。
    let before = state.auth.status().await;
    let update = state
        .auth
        .poll_status(&gateway_url)
        .await
        .map_err(|err| err.to_string())?;
    if update.quota_updated || auth::status_changed(&before, &update.status) {
        emit_update(&app, &update);
    }
    if update.status.state == "authenticated" {
        state.auth.ensure_loops(app).await;
    }
    Ok(update.status)
}

/// 手动 `/validate`（前端暂未使用，预留给设置页“检查登录状态”）。
#[tauri::command]
pub async fn auth_refresh_validate(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<AuthStatus, String> {
    let gateway_url = state.settings.read().await.gateway_url.clone();
    let update = state
        .auth
        .refresh_validate(&gateway_url)
        .await
        .map_err(|err| err.to_string())?;
    emit_update(&app, &update);
    Ok(update.status)
}

/// 手动提交浏览器回调链接（深链 scheme 未注册/浏览器未自动拉起时的兜底）。
///
/// 前端只提交用户从地址栏/回调页复制的 `codemakerhub-light://auth/callback?...`；
/// 解析、校验与登录完成全部在 Rust 侧，token 不会回传前端。
#[tauri::command]
pub async fn auth_submit_deep_link(
    app: AppHandle,
    state: State<'_, AppState>,
    url: String,
) -> Result<AuthStatus, String> {
    let payload =
        auth::parse_deep_link(url.trim()).map_err(|err| format!("回调链接无效：{err}"))?;
    let gateway_url = state.settings.read().await.gateway_url.clone();
    // 未绑定到当前登录会话的链接会被拒绝；错误文案本身已是中文提示，直接透传。
    let update = state
        .auth
        .handle_deep_link(&payload, &gateway_url)
        .await
        .map_err(|err| err.to_string())?;
    emit_update(&app, &update);
    if update.status.state == "authenticated" {
        state.auth.ensure_loops(app.clone()).await;
    }
    Ok(update.status)
}

/// 退出登录：停止后台循环、删除 auth.bin 与 keyring 主密钥、清内存状态。
#[tauri::command]
pub async fn auth_logout(app: AppHandle, state: State<'_, AppState>) -> Result<AuthStatus, String> {
    let update = state.auth.logout().await;
    emit_update(&app, &update);
    Ok(update.status)
}

/// 读取额度快照：默认只读本地最新成功快照；`force = true` 时请求网关刷新。
#[tauri::command]
pub async fn quota_get(
    app: AppHandle,
    state: State<'_, AppState>,
    force: Option<bool>,
) -> Result<Option<QuotaInfo>, String> {
    let force = force.unwrap_or(false);
    let gateway_url = state.settings.read().await.gateway_url.clone();
    let quota = state
        .auth
        .refresh_quota(&gateway_url, force)
        .await
        .map_err(|err| err.to_string())?;
    if force {
        if let Some(quota) = &quota {
            let _ = app.emit("quota://updated", quota);
        }
    }
    Ok(quota)
}
