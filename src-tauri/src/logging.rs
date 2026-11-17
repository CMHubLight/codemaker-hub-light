//! 日志初始化：按日滚动写入 `<数据目录>/logs/hub-light.log.<date>`。
//!
//! 用非阻塞写入器（后台线程落盘），进程生命周期内必须持有 [`LOG_GUARD`]；
//! 退出路径（`RunEvent::Exit`）调用 [`flush`] 显式 drop guard，把最后几行落盘。
//! 滚动文件保留最近 [`MAX_LOG_FILES`] 份，避免日志无限增长。

use std::{
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use tracing_appender::{non_blocking::WorkerGuard, rolling};

/// 日志文件保留份数（按日滚动）。
const MAX_LOG_FILES: usize = 30;
/// 诊断文件（exit.log / startup-error.log）超过该大小就只保留尾部。
const DIAGNOSTIC_MAX_BYTES: u64 = 256 * 1024;
/// 截断后保留的行数。
const DIAGNOSTIC_KEEP_LINES: usize = 100;

/// 持有非阻塞日志写入器；退出时由 [`flush`] 主动 drop。
static LOG_GUARD: OnceLock<Mutex<Option<WorkerGuard>>> = OnceLock::new();

/// 初始化全局日志订阅者（幂等：已初始化时只丢弃本次的写入器）。
pub(crate) fn init(data_dir: &Path) {
    let logs_dir = data_dir.join("logs");
    if let Err(err) = fs::create_dir_all(&logs_dir) {
        eprintln!("创建日志目录失败：{err}");
    }
    prune_diagnostics(&logs_dir);

    let appender = rolling::RollingFileAppender::builder()
        .rotation(rolling::Rotation::DAILY)
        .filename_prefix("hub-light.log")
        .max_log_files(MAX_LOG_FILES)
        .build(&logs_dir)
        .unwrap_or_else(|err| {
            eprintln!("初始化滚动日志失败，回退为无上限滚动：{err}");
            rolling::daily(&logs_dir, "hub-light.log")
        });
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = LOG_GUARD.set(Mutex::new(Some(guard)));

    // `RUST_LOG` 优先，缺省固定为 info + 本 crate debug。
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info,codemaker_hub_light_lib=debug")
    });

    let _ = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        .with_ansi(false)
        .try_init();
}

/// 退出前落盘：drop 非阻塞写入器，等待后台线程冲刷完队列。
pub(crate) fn flush() {
    let Some(guard) = LOG_GUARD.get() else {
        return;
    };
    if let Ok(mut guard) = guard.lock() {
        drop(guard.take());
    }
}

/// 当前实际写入的日志文件（`hub-light.log.<UTC 日期>`，取目录里最新的一份）。
///
/// 用于 `AppInfo.log_path`：旧实现拼的是 `hub-light.log`，该文件从不被创建。
pub(crate) fn current_log_path(data_dir: &Path) -> PathBuf {
    let logs_dir = data_dir.join("logs");
    let fallback = logs_dir.join("hub-light.log");
    let Ok(entries) = fs::read_dir(&logs_dir) else {
        return fallback;
    };
    let mut newest: Option<(String, PathBuf)> = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("hub-light.log.") {
            continue;
        }
        newest = Some(match newest {
            Some((current, path)) if current >= name => (current, path),
            _ => (name, entry.path()),
        });
    }
    newest.map(|(_, path)| path).unwrap_or(fallback)
}

/// 启动时清理诊断文件：超过上限只保留尾部若干行，避免无限增长。
fn prune_diagnostics(logs_dir: &Path) {
    for name in ["exit.log", "startup-error.log"] {
        let path = logs_dir.join(name);
        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };
        if metadata.len() <= DIAGNOSTIC_MAX_BYTES {
            continue;
        }
        let _ = truncate_tail(&path, DIAGNOSTIC_KEEP_LINES);
    }
}

/// 把一个文本日志文件截断为"最后 `keep_lines` 行"（原子替换）。
fn truncate_tail(path: &Path, keep_lines: usize) -> std::io::Result<()> {
    let mut file = fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(DIAGNOSTIC_MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut tail = String::new();
    file.read_to_string(&mut tail)?;
    let kept: Vec<&str> = tail.lines().rev().take(keep_lines).collect();
    let mut content: Vec<&str> = kept.into_iter().rev().collect();
    content.push("");
    let tmp = path.with_extension("log.tmp");
    fs::write(&tmp, content.join("\n"))?;
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "chl-logging-{tag}-{}",
            std::process::id() as u64
                ^ std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("时钟应可读")
                    .as_nanos() as u64
        ));
        fs::create_dir_all(&dir).expect("临时目录应可创建");
        dir
    }

    #[test]
    fn current_log_path_prefers_newest_rotated_file() {
        let dir = temp_dir("current");
        let logs = dir.join("logs");
        fs::create_dir_all(&logs).expect("logs 应可创建");
        fs::write(logs.join("hub-light.log.2026-11-16"), "old").expect("写文件");
        fs::write(logs.join("hub-light.log.2026-11-17"), "new").expect("写文件");

        let path = current_log_path(&dir);
        assert!(
            path.to_string_lossy().ends_with("hub-light.log.2026-11-17"),
            "应取最新一份：{path:?}"
        );

        let empty = temp_dir("empty");
        assert!(
            current_log_path(&empty)
                .to_string_lossy()
                .ends_with("hub-light.log"),
            "没有滚动文件时回退到默认路径"
        );
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&empty);
    }

    #[test]
    fn truncate_tail_keeps_last_lines() {
        let dir = temp_dir("truncate");
        let path = dir.join("exit.log");
        let content: String = (0..500).map(|index| format!("line-{index}\n")).collect();
        fs::write(&path, content).expect("写日志");
        truncate_tail(&path, 10).expect("截断应成功");
        let text = fs::read_to_string(&path).expect("读日志");
        assert!(text.contains("line-499"));
        assert!(!text.contains("line-480"), "只保留尾部：{text}");
        let _ = fs::remove_dir_all(&dir);
    }
}
