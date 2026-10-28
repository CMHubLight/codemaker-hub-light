//! 应用共享状态：设置、数据库、认证服务、代理句柄。
//!
//! 由 Tauri 在 setup 中 `app.manage(...)` 注入，命令通过 `State<'_, AppState>` 访问。
//! token 只在 [`crate::auth::service::AuthService`] 的内存/加密文件中流转，
//! 不进入本结构体。
//!
//! 不变量：设置只有一个写入口（[`crate::settings_store::SettingsStore::update`]），
//! `policy` 与设置同源；字段是 crate 内共享句柄，路径拼接见文件末尾小函数。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    auth::service::AuthService,
    db::Db,
    proxy::{policy::PolicyCell, ProxyServer},
    settings::{self, Settings},
    settings_store::SettingsStore,
};

pub(crate) struct AppState {
    data_dir: PathBuf,
    /// 设置与代理策略的唯一持有者；读 `settings.read().await`，写 `settings.update(patch)`。
    pub(crate) settings: Arc<SettingsStore>,
    /// SQLite 连接池。
    pub(crate) db: Db,
    /// 认证服务（登录/token/额度/后台循环）。
    pub(crate) auth: Arc<AuthService>,
    /// 本地代理句柄。
    pub(crate) proxy: Arc<ProxyServer>,
    /// 代理运行策略句柄（与 `settings` 共享同一个 [`PolicyCell`]）：
    /// 写入只经 `SettingsStore::update`，这里供代理启动/重启传参使用。
    pub(crate) policy: Arc<PolicyCell>,
}

impl AppState {
    pub(crate) fn new(data_dir: PathBuf, settings: Settings, db: Db) -> Self {
        let settings = Arc::new(SettingsStore::new(data_dir.clone(), settings));
        let policy = settings.policy();
        let auth = Arc::new(AuthService::new(data_dir.clone(), db.clone()));
        let proxy = Arc::new(ProxyServer::new(data_dir.clone(), db.clone(), auth.clone()));
        Self {
            data_dir,
            settings,
            db,
            auth,
            proxy,
            policy,
        }
    }

    /// 数据目录（`~/.codemaker-hub-light`）。
    pub(crate) fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub(crate) fn db_path(&self) -> PathBuf {
        db_path(&self.data_dir)
    }

    pub(crate) fn logs_dir(&self) -> PathBuf {
        logs_dir(&self.data_dir)
    }

    /// CSV 导出目录。
    pub(crate) fn exports_dir(&self) -> PathBuf {
        exports_dir(&self.data_dir)
    }

    /// `proxy.port` 文件路径。
    pub(crate) fn port_file(&self) -> PathBuf {
        port_file(&self.data_dir)
    }
}

/// 数据库文件路径。
pub(crate) fn db_path(data_dir: &Path) -> PathBuf {
    data_dir.join(crate::db::DB_FILE_NAME)
}

/// 日志目录。
pub(crate) fn logs_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

/// CSV 导出目录。
pub(crate) fn exports_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("exports")
}

/// `proxy.port` 文件路径（实际监听端口，供外部 CLI 读取）。
pub(crate) fn port_file(data_dir: &Path) -> PathBuf {
    settings::proxy_port_path(data_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_helpers_are_relative_to_data_dir() {
        let dir = Path::new("C:/tmp/hub");
        assert!(db_path(dir).starts_with(dir));
        assert_eq!(logs_dir(dir), dir.join("logs"));
        assert_eq!(exports_dir(dir), dir.join("exports"));
        assert_eq!(port_file(dir), dir.join("proxy.port"));
    }
}
