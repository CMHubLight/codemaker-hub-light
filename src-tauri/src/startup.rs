//! 启动期致命错误上报：写日志 + Windows 原生弹窗。
//!
//! release 是 `windows_subsystem = "windows"`（没有控制台），setup 阶段的 `?`
//! 一旦失败，用户看到的就是"双击没反应"。这里把失败原因落盘并弹窗，
//! 避免把可诊断的故障变成无声退出。

use std::{io::Write, path::Path};

/// 启动失败日志文件名（位于 `<data_dir>/logs/`）。
pub const STARTUP_ERROR_LOG: &str = "startup-error.log";

/// 上报一次启动失败：先追加写日志，再弹窗（弹窗会阻塞到用户确认）。
pub fn report(data_dir: &Path, stage: &str, detail: &str) {
    let log_path = write_log(data_dir, stage, detail);
    tracing::error!(stage, detail, log = %log_path.display(), "启动失败");
    show_dialog(stage, detail, &log_path);
}

fn write_log(data_dir: &Path, stage: &str, detail: &str) -> std::path::PathBuf {
    let logs_dir = data_dir.join("logs");
    let _ = std::fs::create_dir_all(&logs_dir);
    let path = logs_dir.join(STARTUP_ERROR_LOG);
    let entry = format!(
        "{} 启动失败\n阶段：{stage}\n错误：{detail}\n数据目录：{}\n\n",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        data_dir.display()
    );
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(mut file) => {
            let _ = file.write_all(entry.as_bytes());
            let _ = file.sync_all();
        }
        Err(err) => eprintln!("写启动失败日志失败（{}）：{err}", path.display()),
    }
    path
}

#[cfg(windows)]
fn show_dialog(stage: &str, detail: &str, log_path: &Path) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
    };

    let body = format!(
        "阶段：{stage}\n\n{detail}\n\n日志：{}\n\n把该日志脱敏后发给维护者即可定位。",
        log_path.display()
    );
    let text = wide(&body);
    let title = wide("CodeMaker Hub Light 启动失败");

    // SAFETY: 两个句柄参数均为 NULL（无父窗口）；两段文本都是由 `wide` 产出的
    // NUL 结尾 UTF-16 缓冲区，在本次调用期间保持存活；MessageBoxW 只读取它们。
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

/// 转成 NUL 结尾的 UTF-16（内部 NUL 会被剔除，避免字符串被提前截断）。
#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16()
        .filter(|unit| *unit != 0)
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(not(windows))]
fn show_dialog(stage: &str, detail: &str, log_path: &Path) {
    eprintln!(
        "启动失败（{stage}）：{detail}\n日志：{}",
        log_path.display()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_startup_error_log() {
        let dir = std::env::temp_dir().join(format!("chl-startup-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("临时目录应可创建");

        let path = write_log(&dir, "数据库迁移", "duplicate column name: models_json");

        let content = std::fs::read_to_string(&path).expect("启动失败日志应存在");
        assert!(content.contains("数据库迁移"), "日志应包含阶段：{content}");
        assert!(
            content.contains("duplicate column name"),
            "日志应包含错误详情：{content}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
