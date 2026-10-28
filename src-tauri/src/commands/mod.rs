//! Tauri IPC 命令集合。
//!
//! 约定：
//! - 所有命令返回 `Result<T, String>`；错误信息可直接展示给用户。
//! - 失败时返回 `Err(...)`，绝不 panic、不返回假成功。

pub mod app;
pub mod auth;
pub mod proxy;
pub mod settings;
pub mod usage;
