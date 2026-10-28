//! 注销：代次作废 → 停机 → 删凭据 → 清状态 → 档案失效。

use super::*;

impl AuthService {
    pub async fn logout(&self) -> AuthUpdate {
        // 1) 代次自增：所有在途操作（validate/renew/quota/深链完成）立即失效。
        self.bump_epoch().await;
        // 2) 硬停机并等待循环真正结束，确保删除凭据之后没有残留写者。
        self.stop_loops().await;
        // 3) 删除本地凭据（auth.bin + 系统凭据库主密钥）。
        if let Err(err) = self.store.delete_async().await {
            warn!(error = %err, "删除本地凭据/主密钥失败");
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
        state.last_error = None;
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
