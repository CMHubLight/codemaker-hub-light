//! 测试辅助（仅 `cfg(test)` 编译）。
//!
//! Windows 上 SQLite/WAL 与监听套接字的句柄可能略晚于 `Drop` 释放，直接
//! `remove_dir_all` 会失败并静默留下 `%TEMP%\chl-*` 目录（历史上累积过 3000+
//! 个、约 100 MB）。调用方先 `drop` 掉数据库/服务句柄，再用这里的重试删除兜底。

use std::{fs, io::ErrorKind, path::Path, thread, time::Duration};

/// 删除临时目录；句柄未及时释放时重试（最多约 2 秒），最终失败不 panic。
pub(crate) fn remove_dir_all_retry(path: &Path) {
    for _ in 0..40 {
        match fs::remove_dir_all(path) {
            Ok(()) => return,
            Err(err) if err.kind() == ErrorKind::NotFound => return,
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
    // 只在真的删不掉时出声：说明还有句柄没释放（`cargo test -- --nocapture` 可见）。
    let case = thread::current().name().unwrap_or("<unnamed>").to_string();
    eprintln!(
        "警告：临时目录删除失败（{case}），仍残留 {}",
        path.display()
    );
}
