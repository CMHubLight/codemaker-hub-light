//! 崩溃现场落盘。
//!
//! release 是 GUI 子系统（无控制台）且 `panic = "abort"`：默认 panic 不会留下
//! 任何痕迹，进程直接以 `0xc0000409`（`__fastfail`）退出。这里安装一个全局
//! panic hook，把位置/消息/回溯**同步追加**写入 `<数据目录>\logs\panic.log`，
//! 然后再调用默认 hook（保留 stderr 行为），让下一次崩溃可以定位。
//!
//! 不用 `tracing` 的原因：日志走 `tracing_appender` 的非阻塞通道，abort 时
//! 后台写线程可能来不及落盘；这里直接 `OpenOptions::append + sync_all`。

use std::{io::Write, path::Path};

use chrono::SecondsFormat;

/// panic 记录文件名（位于数据目录的 `logs\` 下）。
const PANIC_LOG_FILE: &str = "panic.log";

/// 安装全局 panic hook；需在 `init_tracing` 之后调用（日志目录已就绪）。
pub fn install(data_dir: &Path) {
    let path = data_dir.join("logs").join(PANIC_LOG_FILE);
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = panic_message(info.payload());
        let thread = std::thread::current()
            .name()
            .unwrap_or("<unnamed>")
            .to_string();

        tracing::error!(location = %location, message = %message, "panic");
        if let Err(err) = append_record(&path, &location, &thread, &message) {
            // hook 内绝不能再 panic：写盘失败只放弃本次记录。
            eprintln!("写入 panic 日志失败（{}）：{err}", path.display());
        }

        default_hook(info);
    }));
}

/// 把一条 panic 记录追加到 `path` 并同步到磁盘。
fn append_record(path: &Path, location: &str, thread: &str, message: &str) -> std::io::Result<()> {
    let record = format_record(location, thread, message);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(record.as_bytes())?;
    file.sync_all()
}

/// 记录格式：时间、线程、位置、消息、回溯，尾部 `---` 分隔多条记录。
fn format_record(location: &str, thread: &str, message: &str) -> String {
    let timestamp = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let backtrace = std::backtrace::Backtrace::force_capture();
    format!(
        "[{timestamp}] panic on thread '{thread}' at {location}\n{message}\nbacktrace:\n{backtrace}\n---\n"
    )
}

fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用唯一路径：进程号 + 纳秒，避免并行用例互相覆盖。
    fn unique_path() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "cmhl-panic-log-test-{}-{nanos}.log",
            std::process::id()
        ))
    }

    #[test]
    fn record_contains_location_message_and_backtrace() {
        let path = unique_path();
        let _ = std::fs::remove_file(&path);

        append_record(&path, "src/demo.rs:12:3", "main", "boom").expect("写入 panic 记录");

        let content = std::fs::read_to_string(&path).expect("读取 panic 记录");
        assert!(content.contains("panic on thread 'main' at src/demo.rs:12:3"));
        assert!(content.contains("boom"));
        assert!(content.contains("backtrace:"));
        assert!(content.trim_end().ends_with("---"));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn non_string_payload_is_reported() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        assert_eq!(
            panic_message(payload.as_ref()),
            "<non-string panic payload>"
        );
    }

    #[test]
    fn installed_hook_writes_panic_record() {
        let dir = std::env::temp_dir().join(format!(
            "cmhl-panic-hook-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        ));

        let previous = std::panic::take_hook();
        install(&dir);
        let outcome = std::panic::catch_unwind(|| panic!("boom for test"));
        std::panic::set_hook(previous);

        assert!(outcome.is_err(), "panic 应被 catch_unwind 捕获");
        let content = std::fs::read_to_string(dir.join("logs").join(PANIC_LOG_FILE))
            .expect("应写入 logs/panic.log");
        assert!(content.contains("boom for test"));
        assert!(
            content.contains("panic_log.rs:"),
            "应记录触发位置：{content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
