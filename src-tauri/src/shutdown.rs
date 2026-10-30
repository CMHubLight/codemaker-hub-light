//! 进程生命周期埋点：把"正常退出"和"被外部终止 / 崩溃"区分开。
//!
//! 事故背景：release 是 GUI 子系统且 `panic = "abort"`，如果进程被任务管理器
//! /`taskkill`/`Stop-Process` 终止，或原生崩溃（栈溢出等）绕过了 panic hook，
//! 日志会**停在半截且没有任何退出记录**——从现场无法判断到底发生了什么。
//!
//! 做法：
//! - 启动时写 `logs\session.marker`（pid、版本、启动时间）；若它已存在，说明
//!   上一次没有走正常退出路径，返回旧内容供调用方记录 WARN。
//! - 正常退出（托盘退出、事件循环退出）时先追加 `logs\exit.log`（时间、原因、
//!   pid）并同步落盘，再删除 marker。
//!
//! 判别方法：重启后主日志出现「上次运行未正常退出」的 WARN ⇒ 上次进程被强杀或
//! 原生崩溃；再看 `logs\panic.log`（Rust panic）与事件查看器 Application Error
//! 1000（原生异常）即可定位。

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use chrono::{SecondsFormat, Utc};

/// 运行中标记文件（正常退出时删除）。
const MARKER_FILE: &str = "session.marker";
/// 正常退出记录文件。
const EXIT_LOG_FILE: &str = "exit.log";

fn marker_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join(MARKER_FILE)
}

fn exit_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("logs").join(EXIT_LOG_FILE)
}

/// 启动时登记本次运行；返回上一次未正常退出留下的 marker 内容。
pub(crate) fn mark_running(data_dir: &Path, version: &str) -> Option<String> {
    let path = marker_path(data_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let previous = std::fs::read_to_string(&path)
        .ok()
        .filter(|text| !text.trim().is_empty());
    let content = format!(
        "pid={} version={} started_at={}\n",
        std::process::id(),
        version,
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let _ = std::fs::write(&path, content.as_bytes());
    previous
}

/// 正常退出：先追加一条 `exit.log` 记录，再删除 marker。
///
/// 可重复调用（托盘退出后事件循环还会再报一次 Exit）；重复只会多一行记录。
pub(crate) fn record_exit(data_dir: &Path, reason: &str) {
    let line = format!(
        "{} exit reason={} pid={}\n",
        Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        reason,
        std::process::id()
    );
    let path = exit_log_path(data_dir);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = file.write_all(line.as_bytes());
        let _ = file.sync_all();
    }
    let _ = std::fs::remove_file(marker_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用唯一目录：进程号 + 纳秒，避免并行用例互相覆盖。
    fn unique_dir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "cmhl-shutdown-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    #[test]
    fn marker_lifecycle_distinguishes_clean_and_unclean_exit() {
        let dir = unique_dir("lifecycle");

        // 首次启动：没有历史现场。
        assert_eq!(mark_running(&dir, "0.0.0"), None);
        assert!(marker_path(&dir).exists(), "启动后应写入 marker");

        // 没有走正常退出就再次启动 ⇒ 应识别出上次未正常退出。
        let previous = mark_running(&dir, "0.0.0").expect("应发现未正常退出");
        assert!(
            previous.contains("pid="),
            "旧 marker 内容应可读：{previous}"
        );

        // 正常退出：写记录并删除 marker。
        record_exit(&dir, "tray-quit");
        assert!(!marker_path(&dir).exists(), "正常退出后 marker 应被删除");
        let exit_log = std::fs::read_to_string(exit_log_path(&dir)).expect("应写入 exit.log");
        assert!(exit_log.contains("reason=tray-quit"));
        assert!(exit_log.contains("pid="));

        // 正常退出后再次启动：不应再告警。
        assert_eq!(mark_running(&dir, "0.0.0"), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_exit_is_idempotent() {
        let dir = unique_dir("idempotent");
        mark_running(&dir, "0.0.0");
        record_exit(&dir, "exit-requested");
        record_exit(&dir, "exit");
        let exit_log = std::fs::read_to_string(exit_log_path(&dir)).expect("应写入 exit.log");
        assert_eq!(
            exit_log.lines().count(),
            2,
            "两次调用应各留一行：{exit_log}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
