//! 代理控制命令。

use tauri::{AppHandle, Emitter, State};

use crate::{proxy::ProxyStatus, state::AppState};

#[tauri::command]
pub async fn proxy_status(state: State<'_, AppState>) -> Result<ProxyStatus, String> {
    let settings = state.settings.read().await.clone();
    Ok(state
        .proxy
        .status(
            &settings.proxy_bind,
            settings.proxy_port,
            &settings.gateway_url,
        )
        .await)
}

/// 使用当前设置重启代理（停止旧任务 → 重新绑定 → 回写 proxy.port）。
#[tauri::command]
pub async fn proxy_restart(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<ProxyStatus, String> {
    let settings = state.settings.read().await.clone();
    let status = state
        .proxy
        .restart(
            &settings.proxy_bind,
            settings.proxy_port,
            &settings.gateway_url,
            state.policy.clone(),
        )
        .await
        .map_err(|err| format!("重启代理失败：{err}"))?;

    let _ = app.emit("proxy://status", &status);
    Ok(status)
}
