//! 主窗口生命周期：显示/聚焦，以及"关闭即隐藏到托盘"的拦截。
//!
//! 托盘菜单、托盘左键、单实例二次启动、深链登录完成都走 [`show_main`]，
//! 保证四个入口显示窗口的方式与日志一致。

use std::sync::atomic::Ordering;

use tauri::{Manager, Window, WindowEvent};
use tracing::warn;

use crate::tray::TrayState;

/// 显示、恢复并聚焦主窗口。
pub(crate) fn show_main(app: &tauri::AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        warn!("显示主窗口失败：找不到 main 窗口");
        return;
    };
    if let Err(err) = window.unminimize() {
        warn!(error = %err, "恢复主窗口失败");
    }
    if let Err(err) = window.show() {
        warn!(error = %err, "显示主窗口失败");
    }
    if let Err(err) = window.set_focus() {
        warn!(error = %err, "聚焦主窗口失败");
    }
}

/// 窗口事件：关闭只隐藏到托盘；托盘"退出"会先置 `quitting=true` 再 `app.exit(0)`。
pub(crate) fn handle_window_event(window: &Window, event: &WindowEvent) {
    let WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let quitting = window
        .app_handle()
        .try_state::<TrayState>()
        .map(|state| state.quitting.load(Ordering::SeqCst))
        .unwrap_or(false);
    if quitting {
        return;
    }
    api.prevent_close();
    if let Err(err) = window.hide() {
        warn!(error = %err, "隐藏主窗口失败");
    }
}
