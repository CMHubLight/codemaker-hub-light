//! 日志初始化：按日滚动写入 `<数据目录>/logs/hub-light.log`。
//!
//! 用非阻塞写入器（后台线程落盘），进程生命周期内必须持有 [`LOG_GUARD`]，
//! 否则最后几行日志可能丢失。

use std::{path::Path, sync::OnceLock};

use tracing_appender::non_blocking::WorkerGuard;

/// 持有非阻塞日志写入器，进程生命周期内不得 drop。
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// 初始化全局日志订阅者（幂等：已初始化时只丢弃本次的写入器）。
pub(crate) fn init(data_dir: &Path) {
    let logs_dir = data_dir.join("logs");
    if let Err(err) = std::fs::create_dir_all(&logs_dir) {
        eprintln!("创建日志目录失败：{err}");
    }

    let appender = tracing_appender::rolling::daily(&logs_dir, "hub-light.log");
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = LOG_GUARD.set(guard);

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
