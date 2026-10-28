//! 凭据加载、账号档案与额度快照落库。
//!
//! 纪律：所有阻塞 IO（读盘 / keyring / SQLite）都经阻塞线程池，且不在持有 `state` 锁时执行。

use super::*;

impl AuthService {
    /// 懒加载 `auth.bin`（失败只记录到状态，不打断命令）。
    pub async fn ensure_loaded(&self) {
        let already_loaded = self.state.read().await.loaded;
        if already_loaded {
            return;
        }
        let _ = self.load_credentials(false).await;
    }

    /// 启动时严格加载：`auth.bin` 存在但主密钥缺失/解密失败时返回错误（不降级）。
    pub async fn load_strict(&self) -> Result<(), AuthError> {
        self.load_credentials(true).await
    }

    async fn load_credentials(&self, strict: bool) -> Result<(), AuthError> {
        let epoch = {
            let state = self.state.read().await;
            if state.loaded && state.credentials.is_some() {
                return Ok(());
            }
            state.epoch
        };

        // 读盘（含 fs2 锁 / keyring / 解密）在阻塞线程池完成；此时不持有任何 async 锁。
        let credentials = match self.store.load_async().await {
            Ok(credentials) => credentials,
            Err(err) => {
                let committed = self
                    .commit_if_current(epoch, |state| {
                        state.loaded = true;
                        state.last_error = Some(format!("本地凭据加载失败：{err}"));
                    })
                    .await;
                warn!(error = %err, "本地凭据加载失败（未降级为明文）");
                // 加载期间发生注销：不要把"加载失败"写进新会话，也不当成启动失败。
                if committed.is_none() {
                    return Ok(());
                }
                return if strict {
                    Err(AuthError::Store(err))
                } else {
                    Ok(())
                };
            }
        };

        // 读盘与写状态之间可能已经注销/开始登录：先确认还要不要采纳这份凭据。
        let should_apply = {
            let state = self.state.read().await;
            state.epoch == epoch
                && state.credentials.is_none()
                && state.phase != AuthPhase::PendingLogin
        };
        if !should_apply {
            self.commit_if_current(epoch, |state| state.loaded = true)
                .await;
            return Ok(());
        }
        let Some(credentials) = credentials else {
            self.commit_if_current(epoch, |state| state.loaded = true)
                .await;
            return Ok(());
        };

        // 查库移到写锁之外（db_profile_for 内部走 spawn_blocking）。
        let db_profile = self.db_profile_for(&credentials).await;
        self.commit_if_current(epoch, |state| {
            state.loaded = true;
            // 读盘 + 查库期间可能已经完成登录或进入 pending：
            // 这条守卫必须在写锁内（与旧实现一致），否则旧盘凭据会覆盖新会话。
            if state.credentials.is_some() || state.phase == AuthPhase::PendingLogin {
                return;
            }
            state.profile = Some(AuthProfile {
                user: credentials.user.clone(),
                display_name: db_profile
                    .as_ref()
                    .and_then(|profile| profile.display_name.clone()),
                dept: db_profile.as_ref().and_then(|profile| profile.dept.clone()),
                server_url: db_profile
                    .as_ref()
                    .map(|profile| profile.server_url.clone())
                    .filter(|url| !url.is_empty())
                    .unwrap_or_else(|| credentials.server_url.clone()),
                issued_at: Some(credentials.issued_at),
                expires_at: Some(credentials.expires_at),
            });
            state.models = db_profile
                .as_ref()
                .and_then(|profile| profile.models_json.as_deref())
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
            state.phase = AuthPhase::LoggedIn;
            state.offline_grace = false;
            state.credentials_valid = true;
            state.last_error = None;
            state.credentials = Some(credentials);
        })
        .await;
        Ok(())
    }

    async fn db_profile_for(&self, credentials: &Credentials) -> Option<repo::AuthProfileRow> {
        let user = credentials.user.clone();
        let loaded = blocking::run(&self.db, move |db| {
            let conn = db.conn()?;
            let profile = repo::active_profile(&conn)?;
            Ok(profile)
        })
        .await;
        match loaded {
            Ok(Some(profile)) if profile.user == user => Some(profile),
            Ok(_) => None,
            Err(err) => {
                warn!(error = %err, "读取账号档案失败");
                None
            }
        }
    }

    pub(super) async fn active_profile_id(&self) -> Option<i64> {
        blocking::run(&self.db, |db| {
            let conn = db.conn()?;
            let profile = repo::active_profile(&conn)?;
            Ok(profile.map(|profile| profile.id))
        })
        .await
        .ok()
        .flatten()
    }

    pub(super) async fn persist_profile(
        &self,
        epoch: u64,
        profile: &AuthProfile,
        models: &[ModelSummary],
        validated_at: Option<DateTime<Utc>>,
    ) -> Option<i64> {
        // 注销后不要再把账号档案标记为 active。
        if !self.still_current(epoch).await {
            return None;
        }
        let user = profile.user.clone();
        let display_name = profile.display_name.clone();
        let dept = profile.dept.clone();
        let server_url = profile.server_url.clone();
        let models_json = serde_json::to_string(models).ok();
        let issued_at = profile.issued_at;
        let expires_at = profile.expires_at;
        let validated_at = validated_at.map(|t| t.timestamp());
        let saved = blocking::run(&self.db, move |db| {
            let conn = db.conn()?;
            let upsert = ProfileUpsert {
                user: &user,
                display_name: display_name.as_deref(),
                dept: dept.as_deref(),
                server_url: &server_url,
                auth_user: Some(user.as_str()),
                models_json: models_json.as_deref(),
                is_active: true,
                issued_at,
                expires_at,
                validated_at,
            };
            let id = repo::upsert_profile(&conn, &upsert)?;
            Ok(id)
        })
        .await;
        match saved {
            Ok(id) => Some(id),
            Err(err @ DbError::Pool(_)) => {
                warn!(error = %err, "打开数据库失败，账号档案未保存");
                None
            }
            Err(err) => {
                warn!(error = %err, "账号档案写入失败");
                None
            }
        }
    }

    pub(super) async fn persist_profile_current(
        &self,
        epoch: u64,
        validated_at: Option<DateTime<Utc>>,
    ) {
        let (profile, models) = {
            let state = self.state.read().await;
            (state.profile.clone(), state.models.clone())
        };
        if let Some(profile) = profile {
            let _ = self
                .persist_profile(epoch, &profile, &models, validated_at)
                .await;
        }
    }

    pub(super) async fn store_quota(
        &self,
        epoch: u64,
        profile_id: Option<i64>,
        data: &QuotaData,
    ) -> Result<QuotaInfo, AuthError> {
        // 注销后不再写额度快照，也不更新内存时间戳。
        if !self.still_current(epoch).await {
            return Err(AuthError::SessionEnded);
        }
        let raw = serde_json::to_string(&data.raw)
            .map_err(|err| AuthError::Internal(format!("额度 JSON 序列化失败：{err}")))?;
        // `NewQuotaSnapshot` 借用了 `data` 的字段；闭包必须是 'static，克隆一份进去。
        let owned = data.clone();
        let persisted = blocking::run(&self.db, move |db| {
            let conn = db.conn()?;
            let snapshot = owned.to_snapshot(profile_id, &raw);
            repo::insert_quota_snapshot(&conn, &snapshot)?;
            Ok(())
        })
        .await;
        match persisted {
            Ok(()) => {}
            Err(err @ DbError::Pool(_)) => {
                return Err(AuthError::Internal(format!("打开数据库失败：{err}")));
            }
            Err(err) => {
                return Err(AuthError::Internal(format!("写入额度快照失败：{err}")));
            }
        }
        self.commit_if_current(epoch, |state| state.last_quota_at = Some(Utc::now()))
            .await;
        Ok(data.to_quota_info())
    }
}
