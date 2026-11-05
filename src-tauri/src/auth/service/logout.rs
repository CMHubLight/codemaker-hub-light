//! 注销：代次作废 → 停机 → 删凭据 → 清状态 → 档案失效。

use super::*;

impl AuthService {
    pub async fn logout(&self) -> AuthUpdate {
        // 1) 代次自增：所有在途操作（validate/renew/quota/深链完成）立即失效。
        self.bump_epoch().await;
        // 2) 硬停机并等待循环真正结束，确保删除凭据之后没有残留写者。
        self.stop_loops().await;
        // 3) 删除本地凭据（auth.bin + 系统凭据库主密钥）。
        //    失败重试一次再报告：Windows 上杀软/备份/OneDrive 持有句柄是瞬时冲突。
        //    注意 `store.delete` 会先删主密钥，因此即使文件删不掉，残留密文也已不可解密。
        let mut delete_error = None;
        for attempt in 0..2u32 {
            match self.store.delete_async().await {
                Ok(()) => {
                    delete_error = None;
                    break;
                }
                Err(err) => {
                    if attempt == 0 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    delete_error = Some(err);
                }
            }
        }
        if let Some(err) = &delete_error {
            // 内存态照旧清空（代理立即 401），但"磁盘没清干净"必须可见：
            // 这是 SECURITY.md 对用户的承诺，不能只留一条 warn。
            error!(error = %err, "退出登录未能清除本地凭据");
        }

        // 4) 清空内存态（epoch 自身保留，继续作废在途操作）。
        let mut state = self.state.write().await;
        state.phase = AuthPhase::LoggedOut;
        state.credentials = None;
        state.profile = None;
        state.pending = None;
        state.models.clear();
        state.offline_grace = false;
        state.credentials_valid = false;
        // 删除失败时用 `message` 把原因带给界面（登录页会展示）。
        state.last_error = delete_error.map(|err| {
            format!(
                "退出登录未能清除本地凭据（{err}）。代理已停止使用该凭据；\
                 请重试退出，或手动删除数据目录下的 auth.bin 与系统凭据库条目。"
            )
        });
        state.last_validated_at = None;
        state.last_quota_at = None;
        state.renew = RenewRuntime::default();
        state.loaded = true;
        let status = Self::status_from(&state);
        drop(state);

        // 5) DB 里的账号档案标记失效。
        if let Err(err) = blocking::run(&self.db, |db| {
            let conn = db.conn()?;
            repo::deactivate_profiles(&conn)?;
            Ok(())
        })
        .await
        {
            warn!(error = %err, "标记账号档案失效失败");
        }

        AuthUpdate {
            status,
            quota: None,
            quota_updated: false,
        }
    }
}
