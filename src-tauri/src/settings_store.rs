//! 设置的唯一写入口：内存态 + 运行期代理策略 + 原子落盘。
//!
//! 锁语义：
//! - 设置读锁只在表达式内持有，**不跨 `to_vec_pretty` / `fsync` / `rename`**；
//! - 写者用独立的 async `Mutex` 串行，保证并发保存不会互相覆盖；
//! - 顺序固定为 校验 → 锁外落盘 → 内存替换 → 策略写回，任一步失败都不改变
//!   内存与策略（代理下一次请求读到的仍是旧策略）。

use std::{path::PathBuf, sync::Arc};

use tokio::sync::{Mutex, RwLock, RwLockReadGuard};

use crate::{
    proxy::{policy::PolicyCell, ProxyPolicy},
    settings::{Settings, SettingsError, SettingsPatch},
};

/// 设置 + 代理策略的进程内单一持有者。
pub(crate) struct SettingsStore {
    data_dir: PathBuf,
    settings: RwLock<Settings>,
    policy: Arc<PolicyCell>,
    /// 串行化写者：决定"谁先落盘、谁后替换内存"。
    writer: Mutex<()>,
}

impl SettingsStore {
    pub(crate) fn new(data_dir: PathBuf, settings: Settings) -> Self {
        let policy = Arc::new(PolicyCell::new(ProxyPolicy::from_settings(&settings)));
        Self {
            data_dir,
            settings: RwLock::new(settings),
            policy,
            writer: Mutex::new(()),
        }
    }

    /// 读当前设置（异步读锁，调用方不得把 guard 跨 await 长期持有）。
    pub(crate) async fn read(&self) -> RwLockReadGuard<'_, Settings> {
        self.settings.read().await
    }

    /// 当前设置快照（IPC 返回、命令内部计算用）。
    pub(crate) async fn snapshot(&self) -> Settings {
        self.settings.read().await.clone()
    }

    /// 代理运行策略句柄；与 [`SettingsStore::update`] 写回的是同一个 `PolicyCell`。
    pub(crate) fn policy(&self) -> Arc<PolicyCell> {
        self.policy.clone()
    }

    /// 合并局部设置：校验 → 锁外落盘 → 内存替换 → 策略写回。
    ///
    /// 失败（校验不过或落盘失败）时内存与策略都保持原值。
    pub(crate) async fn update(&self, patch: SettingsPatch) -> Result<Settings, SettingsError> {
        let _writer = self.writer.lock().await;
        let mut next = self.settings.read().await.clone();
        next.apply_patch(patch);
        next.normalize();
        next.validate()?;

        // 锁外磁盘 IO：设置读路径（auth 后台循环、深链、代理重启）不被 fsync 阻塞。
        next.save(&self.data_dir)?;

        *self.settings.write().await = next.clone();
        self.policy.store(ProxyPolicy::from_settings(&next));
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::settings_path;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chl-settings-store-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("临时目录应可创建");
        dir
    }

    /// 校验失败必须"不落盘、不改内存、不改策略"；合法路径三者同时生效。
    #[tokio::test]
    async fn update_validates_before_io_and_leaves_memory_untouched_on_failure() {
        let dir = temp_dir();
        let store = SettingsStore::new(dir.clone(), Settings::default());
        let before = store.snapshot().await;

        let bad = SettingsPatch {
            proxy_bind: Some("0.0.0.0".to_string()),
            record_body: Some(true),
            ..SettingsPatch::default()
        };
        let err = store.update(bad).await.expect_err("非回环绑定必须被拒绝");
        assert!(
            matches!(err, SettingsError::Invalid(_)),
            "应返回校验错误而非 IO 错误：{err}"
        );
        assert_eq!(
            store.snapshot().await.proxy_bind,
            before.proxy_bind,
            "校验失败不得改内存"
        );
        assert!(
            !store.policy().load().record_body,
            "校验失败不得改运行期策略"
        );
        assert!(!settings_path(&dir).exists(), "校验失败不得落盘");

        let ok = SettingsPatch {
            record_body: Some(true),
            credit_to_yuan: Some(0.02),
            ..SettingsPatch::default()
        };
        let saved = store.update(ok).await.expect("合法设置应保存成功");
        assert!(saved.record_body);
        assert!(settings_path(&dir).exists(), "成功路径应落盘");
        assert!(store.snapshot().await.record_body, "内存应同步");
        assert!(store.policy().load().record_body, "策略应同步写回");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
