//! 应用信息与诊断命令（字段与前端 `AppInfo` / `AppLogResult` 对齐）。

use std::path::Path;

use serde::Serialize;
use tauri::State;

use crate::{db::repo, state::AppState};

/// 应用基础信息（关于页 / 诊断）。
#[derive(Debug, Clone, Serialize)]
pub struct AppInfo {
    pub name: String,
    pub version: String,
    /// Tauri 运行时版本（前端允许 null）。
    pub tauri_version: Option<String>,
    pub platform: String,
    pub data_dir: Option<String>,
    pub log_path: Option<String>,
    // ---- 诊断附加字段 ----
    pub identifier: String,
    pub arch: String,
    pub db_path: String,
    pub logs_dir: String,
    pub proxy_port_file: String,
    pub gateway_url: String,
    pub proxy_port: u16,
    pub schema_version: i64,
}

/// 打开日志目录的结果（前端 `AppLogResult`）。
#[derive(Debug, Clone, Serialize)]
pub struct OpenLogsResult {
    pub opened: bool,
    pub message: Option<String>,
    // ---- 附加字段 ----
    pub path: String,
}

#[tauri::command]
pub async fn app_info(state: State<'_, AppState>) -> Result<AppInfo, String> {
    let settings = state.settings.read().await.clone();
    let schema_version = {
        let conn = state
            .db
            .conn()
            .map_err(|err| format!("打开数据库失败：{err}"))?;
        repo::schema_version(&conn).map_err(|err| format!("读取 schema 版本失败：{err}"))?
    };

    let logs_dir = state.logs_dir();
    let log_path = logs_dir.join("hub-light.log");

    Ok(AppInfo {
        name: "CodeMaker Hub Light".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        tauri_version: Some(tauri::VERSION.to_string()),
        platform: std::env::consts::OS.to_string(),
        data_dir: Some(state.data_dir().to_string_lossy().into_owned()),
        log_path: Some(log_path.to_string_lossy().into_owned()),
        identifier: "com.codemaker.hub.light".to_string(),
        arch: std::env::consts::ARCH.to_string(),
        db_path: state.db_path().to_string_lossy().into_owned(),
        logs_dir: logs_dir.to_string_lossy().into_owned(),
        proxy_port_file: state.port_file().to_string_lossy().into_owned(),
        gateway_url: settings.gateway_url,
        proxy_port: settings.proxy_port,
        schema_version,
    })
}

/// 在系统文件管理器中打开日志目录。
#[tauri::command]
pub async fn app_open_logs(state: State<'_, AppState>) -> Result<OpenLogsResult, String> {
    let logs_dir = state.logs_dir();
    std::fs::create_dir_all(&logs_dir).map_err(|err| format!("创建日志目录失败：{err}"))?;
    open_in_file_manager(&logs_dir).map_err(|err| format!("打开日志目录失败：{err}"))?;
    Ok(OpenLogsResult {
        opened: true,
        message: None,
        path: logs_dir.to_string_lossy().into_owned(),
    })
}

fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(path).spawn()?;
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    Ok(())
}
