//! 崩溃现场落盘。
//!
//! release 是 GUI 子系统（无控制台）且 `panic = "abort"`：默认 panic 不会留下
//! 任何痕迹，进程直接以 `0xc0000409`（`__fastfail`）退出。这里安装一个全局
//! panic hook，把位置/消息/回溯**同步追加**写入 `<数据目录>\logs\panic.log`，
//! 然后再调用默认 hook（保留 stderr 行为），让下一次崩溃可以定位。
//!
//! 落盘顺序是硬约束：**先写最小现场**（时间/线程/位置/消息 + `sync_all`），之后
//! 才尝试回溯与 `tracing`。反过来的话，一旦日志订阅者（`tracing_appender` 的
//! 非阻塞通道）或回溯符号化（dbghelp，常在 COM/WebView2 拆卸时被 LoaderLock
//! 卡住）失败，hook 会在写出第一个字节之前 abort——正是"进程崩了但 panic.log
//! 什么都没有"的那种现场。回溯只是锦上添花，失败只丢回溯，不影响定位。

use std::{io::Write, path::Path};

use chrono::SecondsFormat;

/// panic 记录文件名（位于数据目录的 `logs\` 下）。
const PANIC_LOG_FILE: &str = "panic.log";

/// 安装全局 panic hook；需在 `init_tracing` 之后调用（日志目录已就绪）。
pub fn install(data_dir: &Path) {
    install_with(data_dir, |location, message| {
        tracing::error!(location = %location, message = %message, "panic");
    });
}

/// [`install`] 的可测试版本：`trace` 在最小现场落盘之后被调用。
fn install_with<F>(data_dir: &Path, trace: F)
where
    F: Fn(&str, &str) + Send + Sync + 'static,
{
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

        // 1) 先同步落盘最小现场：不依赖 tracing，也不做符号化。
        if let Err(err) = write_record(&path, &location, &thread, &message) {
            // hook 内绝不能再 panic：写盘失败只放弃本次记录。
            eprintln!("写入 panic 日志失败（{}）：{err}", path.display());
        } else {
            // 2) 回溯单独追加；失败只丢回溯。
            append_backtrace(&path, &capture_backtrace());
        }

        // 3) 最小现场已落盘，才交给 tracing 与默认 hook。
        trace(&location, &message);
        default_hook(info);
    }));
}

/// 写最小现场（时间、线程、位置、消息）并同步到磁盘。
fn write_record(path: &Path, location: &str, thread: &str, message: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let timestamp = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    write!(
        file,
        "[{timestamp}] panic on thread '{thread}' at {location}\n{message}\n"
    )?;
    file.sync_all()
}

/// 追加回溯并以 `---` 收尾；失败静默（最小现场已经落盘）。
fn append_backtrace(path: &Path, backtrace: &str) {
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let _ = write!(file, "backtrace:\n{backtrace}\n---\n");
    let _ = file.sync_all();
}

/// 捕获回溯；符号化属于"尽力而为"，不能被它拖住 hook。
fn capture_backtrace() -> String {
    std::backtrace::Backtrace::force_capture().to_string()
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
    use std::{sync::Arc, sync::Mutex};

    use super::*;

    /// 测试用唯一路径：进程号 + 纳秒，避免并行用例互相覆盖。
    fn unique_dir(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "cmhl-panic-log-{tag}-{}-{nanos}",
            std::process::id()
        ))
    }

    fn panic_log_path(dir: &Path) -> std::path::PathBuf {
        dir.join("logs").join(PANIC_LOG_FILE)
    }

    #[test]
    fn record_contains_location_message_and_backtrace() {
        let dir = unique_dir("format");
        let path = panic_log_path(&dir);

        write_record(&path, "src/demo.rs:12:3", "main", "boom").expect("写入最小现场");
        append_backtrace(&path, "fake-backtrace");

        let content = std::fs::read_to_string(&path).expect("读取 panic 记录");
        assert!(content.contains("panic on thread 'main' at src/demo.rs:12:3"));
        assert!(content.contains("boom"));
        assert!(content.contains("backtrace:"));
        assert!(content.trim_end().ends_with("---"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_string_payload_is_reported() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        assert_eq!(
            panic_message(payload.as_ref()),
            "<non-string panic payload>"
        );
    }

    /// 安装 hook 后 panic：最小现场必须先于 `trace` 回调落盘（回归"崩了但
    /// panic.log 是空的"）。回调只在观察，不做断言——hook 内 panic 会 abort。
    #[test]
    fn installed_hook_writes_record_before_trace_callback() {
        let dir = unique_dir("hook");
        let seen: Arc<Mutex<Option<(String, String, String)>>> = Arc::new(Mutex::new(None));
        let seen_in_hook = Arc::clone(&seen);
        let observed_dir = dir.clone();
        let observed_path = panic_log_path(&observed_dir);

        let previous = std::panic::take_hook();
        install_with(&dir, move |location, message| {
            // hook 内只记录观察结果，不断言：hook 里 panic 会让测试进程直接 abort。
            let content = std::fs::read_to_string(&observed_path).unwrap_or_default();
            *seen_in_hook.lock().expect("锁可用") =
                Some((location.to_string(), message.to_string(), content));
        });
        let outcome = std::panic::catch_unwind(|| panic!("boom for test"));
        std::panic::set_hook(previous);

        assert!(outcome.is_err(), "panic 应被 catch_unwind 捕获");
        let (location, message, content_at_trace) = seen
            .lock()
            .expect("锁可用")
            .clone()
            .expect("trace 回调应被执行");
        assert_eq!(message, "boom for test");
        assert!(
            content_at_trace.contains("boom for test"),
            "trace 回调运行时最小现场应已落盘：{content_at_trace}"
        );
        assert!(
            content_at_trace.contains(&location),
            "trace 回调运行时位置应已落盘：{content_at_trace}"
        );

        let content = std::fs::read_to_string(panic_log_path(&dir)).expect("应写入 logs/panic.log");
        assert!(
            content.contains("panic_log.rs:"),
            "应记录触发位置：{content}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
