//! 设置读写命令。
//!
//! `settings_get` 返回完整设置；`settings_update` 接受前端提交的局部补丁，
//! 未提交字段保持原值，避免覆盖内部配置。
//!
//! 校验与落盘统一走 [`crate::settings_store::SettingsStore::update`]：设置读锁内
//! 只做内存替换与策略写回，`to_vec_pretty`/`fsync`/`rename` 全部在锁外完成；
//! 失败时内存与运行期策略都保持原值。

use tauri::State;

use crate::{
    settings::{Settings, SettingsError, SettingsPatch},
    state::AppState,
};

#[tauri::command]
pub async fn settings_get(state: State<'_, AppState>) -> Result<Settings, String> {
    Ok(state.settings.snapshot().await)
}

/// 校验 → 原子落盘 → 更新内存态与运行期策略。
///
/// 记录正文 / 积分换算写回运行期策略后**立即生效**；端口与网关的变更仍需
/// 调用 `proxy_restart`（监听地址无法热切换）。
#[tauri::command]
pub async fn settings_update(
    state: State<'_, AppState>,
    settings: SettingsPatch,
) -> Result<Settings, String> {
    state
        .settings
        .update(settings)
        .await
        .map_err(|err| match err {
            SettingsError::Invalid(message) => format!("设置不合法：{message}"),
            err => format!("保存设置失败：{err}"),
        })
}
