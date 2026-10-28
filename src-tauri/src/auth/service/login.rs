//! 登录编排：发起 / 轮询 / 深链绑定，以及完成登录后的落盘与状态提交。

use super::validate::load_models;
use super::*;

impl AuthService {
    /// 发起登录：生成 pollUuid 与登录 URL，进入 PendingLogin。
    pub async fn start_login(&self, gateway_url: &str) -> Result<PendingLogin, AuthError> {
        let gateway = self.gateway(gateway_url).await?;
        let poll_uuid = uuid::Uuid::new_v4();
        let redirect_uri = auth::build_redirect_uri(&poll_uuid);
        let login_url = auth::build_login_url(gateway.base(), &redirect_uri)?;
        let pending = PendingLogin {
            poll_uuid,
            login_url,
            redirect_uri,
            started_at: Utc::now(),
            consecutive_network_errors: 0,
            deep_link_received: false,
            credentials_consumed: false,
        };

        let mut state = self.state.write().await;
        state.pending = Some(pending.clone());
        state.phase = AuthPhase::PendingLogin;
        state.last_error = None;
        Ok(pending)
    }

    /// 前端每 2s 调用：轮询 `/vs_login_info/{uuid}`，成功后完成登录。
    pub async fn poll_status(&self, gateway_url: &str) -> Result<AuthUpdate, AuthError> {
        self.ensure_loaded().await;
        let epoch = self.epoch().await;

        let pending = { self.state.read().await.pending.clone() };
        let Some(pending) = pending else {
            return Ok(self.make_update(None, false).await);
        };

        if pending.is_timed_out(Utc::now()) {
            self.commit_if_current(epoch, |state| {
                state.pending = None;
                state.phase = AuthPhase::Error;
                state.last_error = Some(format!(
                    "登录超时（{} 秒），请重试",
                    auth::POLL_TIMEOUT_SECS
                ));
            })
            .await;
            return Ok(self.make_update(None, false).await);
        }

        let gateway = self.gateway(gateway_url).await?;
        match gateway.poll_login(&pending.poll_uuid.to_string()).await {
            Ok(PollOutcome::Pending) => {
                self.commit_if_current(epoch, |state| {
                    if let Some(current) = state.pending.as_mut() {
                        current.consecutive_network_errors = 0;
                    }
                })
                .await;
                Ok(self.make_update(None, false).await)
            }
            Ok(PollOutcome::Rejected(message)) => {
                self.commit_if_current(epoch, |state| {
                    state.pending = None;
                    state.phase = AuthPhase::Error;
                    state.last_error = Some(message);
                })
                .await;
                Ok(self.make_update(None, false).await)
            }
            Ok(PollOutcome::Success(tokens)) => self.finish_login(epoch, gateway_url, tokens).await,
            Err(err) => {
                self.commit_if_current(epoch, |state| {
                    let consecutive_errors = if let Some(current) = state.pending.as_mut() {
                        current.consecutive_network_errors += 1;
                        current.consecutive_network_errors
                    } else {
                        0
                    };
                    match &err {
                        GatewayError::Network(_) => {
                            if consecutive_errors >= MAX_CONSECUTIVE_NETWORK_ERRORS {
                                state.pending = None;
                                state.phase = AuthPhase::Error;
                                state.last_error = Some(format!(
                                    "连续 {MAX_CONSECUTIVE_NETWORK_ERRORS} 次网络错误，登录失败"
                                ));
                            } else {
                                state.last_error = Some("轮询登录状态失败，正在重试".to_string());
                            }
                        }
                        other => {
                            state.last_error = Some(format!("轮询登录状态失败：{other}"));
                        }
                    }
                })
                .await;
                Ok(self.make_update(None, false).await)
            }
        }
    }

    /// 深链回调：只有"绑定到当前 pending 登录"时才采纳其中的凭据。
    ///
    /// 拒绝路径不写凭据、不切换身份，只留下静态提示文案；
    /// 这是防登录 CSRF / 回调链接重放的关键闸门。
    pub async fn handle_deep_link(
        &self,
        payload: &DeepLinkPayload,
        gateway_url: &str,
    ) -> Result<AuthUpdate, AuthError> {
        self.ensure_loaded().await;
        let epoch = self.epoch().await;

        let Some(verdict) = self
            .commit_if_current(epoch, |state| claim_deep_link(state, payload, Utc::now()))
            .await
        else {
            return Err(AuthError::SessionEnded);
        };

        match verdict {
            DeepLinkVerdict::Accepted => {}
            // 重复投递（Windows 上 second-instance 与插件会各投递一次）：静默忽略。
            // 第一次的完成流程仍在跑，轮询也仍可用同一个 poll_uuid 兜底。
            DeepLinkVerdict::Consumed => {
                info!("该回调链接已使用过，忽略重复投递");
                return Ok(self.make_update(None, false).await);
            }
            // 已登录时收到迟到的回调：同样静默忽略，不给用户弹错误。
            DeepLinkVerdict::NoPending if self.is_logged_in().await => {
                info!("重复或迟到的回调链接已忽略");
                return Ok(self.make_update(None, false).await);
            }
            // 不带凭据的回调只做"到达确认"（例如 app 重启前那次的确认链接），
            // 没有任何采纳风险，静默忽略即可。
            rejected if !payload.has_credentials() => {
                info!(reason = ?rejected, "无凭据的回调链接与当前会话不匹配，已忽略");
                return Ok(self.make_update(None, false).await);
            }
            rejected => {
                let message = rejected.message();
                self.commit_if_current(epoch, |state| {
                    state.last_error = Some(message.to_string());
                })
                .await;
                warn!(reason = %message, "深链回调被拒绝（未采纳其中的凭据）");
                return Err(AuthError::InvalidDeepLink(message.to_string()));
            }
        }

        if !payload.has_credentials() {
            return Ok(self.make_update(None, false).await);
        }

        let tokens = LoginTokens {
            access_token: payload.access_token.clone().unwrap_or_default(),
            auth_key: payload.auth_key.clone(),
            user: payload.user.clone(),
            server_url: None,
            expires_at: None,
        };

        match self.finish_login(epoch, gateway_url, tokens).await {
            Ok(update) => Ok(update),
            Err(AuthError::SessionEnded) => {
                // 完成过程中被注销：保持"已登出"，不要把状态改回 error。
                Ok(self.make_update(None, false).await)
            }
            Err(err) => {
                warn!(error = %err, "深链登录失败");
                self.record_failure(epoch, &err.to_string()).await;
                Ok(self.make_update(None, false).await)
            }
        }
    }

    async fn is_logged_in(&self) -> bool {
        matches!(
            self.state.read().await.phase,
            AuthPhase::LoggedIn | AuthPhase::OfflineGrace
        )
    }

    /// 登录成功后的落库与状态切换（validate → get_auth_key → 加密保存 → quota）。
    pub(super) async fn finish_login(
        &self,
        epoch: u64,
        gateway_url: &str,
        tokens: LoginTokens,
    ) -> Result<AuthUpdate, AuthError> {
        // 注销后到达的完成请求：立即放弃，不发网络、不落盘。
        if !self.still_current(epoch).await {
            return Err(AuthError::SessionEnded);
        }
        // 单飞：深链与轮询可能同时成功，重复做一遍 validate/落盘/额度快照毫无意义。
        let Ok(_serial) = self.login_serial.try_lock() else {
            info!("登录正在完成中，忽略并发的完成请求");
            return Ok(self.make_update(None, false).await);
        };
        let gateway = self.gateway(gateway_url).await?;

        // 1) /validate：403 直接失败；网络错误降级为离线宽限。
        let (mut validate_info, offline_grace) =
            self.validate_login(&gateway, &tokens.access_token).await?;

        // 2) 模型目录与续期 key 互不依赖：并发取（最坏登录时延少一个 20s 超时）。
        //    `/validate` 已带回模型目录时不会再打 `/validate/models`（见 `load_models`）。
        let (models, auth_key) = self
            .resolve_models_and_key(&gateway, &tokens, std::mem::take(&mut validate_info.models))
            .await;

        // 3) 组装 + 加密保存。
        let now = Utc::now();
        let (credentials, profile) =
            build_login_credentials(tokens, validate_info, auth_key, gateway.base(), now)?;
        // 落盘前最后一次确认会话仍然有效：注销后绝不能再写出一份凭据。
        if !self.still_current(epoch).await {
            return Err(AuthError::SessionEnded);
        }
        self.store.save_async(&credentials).await?;

        let committed = self
            .commit_login(
                epoch,
                credentials.clone(),
                profile.clone(),
                models.clone(),
                offline_grace,
                now,
            )
            .await;
        if committed.is_none() {
            // 极端窗口：刚写盘就发生注销。删掉刚写的文件，绝不复活凭据。
            let _ = self.store.delete_async().await;
            return Err(AuthError::SessionEnded);
        }

        let profile_id = self
            .persist_profile(
                epoch,
                &profile,
                &models,
                if offline_grace { None } else { Some(now) },
            )
            .await;

        // 4) 拉额度（失败不影响登录，保留上次快照）。
        let (quota_info, quota_updated) = self
            .fetch_and_store_quota(
                epoch,
                &gateway,
                credentials.access_token(),
                profile_id,
                "登录后",
            )
            .await;

        Ok(self.make_update(quota_info, quota_updated).await)
    }

    /// 登录流程的 `/validate`：403 → 直接失败；网络错误 → 离线宽限（仍尝试完成登录）。
    async fn validate_login(
        &self,
        gateway: &AuthGateway,
        access_token: &str,
    ) -> Result<(ValidateInfo, bool), AuthError> {
        match gateway.validate(access_token).await {
            Ok(info) => Ok((info, false)),
            Err(err) if err.is_forbidden() => Err(AuthError::Rejected(
                "登录凭据被服务端拒绝（403），请重新登录".to_string(),
            )),
            Err(err) => {
                warn!(error = %err, "登录后 /validate 失败，进入离线宽限");
                Ok((ValidateInfo::default(), true))
            }
        }
    }

    /// 并发获取模型目录与续期 key（两者都只依赖 access_token）。
    ///
    /// 续期 key 优先 `/get_auth_key`（128 字符）；失败/缺省回退深链 SSO token，两者都没有
    /// 也不阻断登录（只是暂时无法自动续期，续期前还会再取一次）。
    async fn resolve_models_and_key(
        &self,
        gateway: &AuthGateway,
        tokens: &LoginTokens,
        embedded_models: Vec<ModelSummary>,
    ) -> (Vec<ModelSummary>, Option<String>) {
        let models_future = load_models(gateway, &tokens.access_token, embedded_models);
        let key_future = gateway.get_auth_key(&tokens.access_token);
        let (models, key_result) = tokio::join!(models_future, key_future);

        let mut auth_key = match key_result {
            Ok(Some(key)) => {
                debug!(len = key.len(), "已取到续期 key");
                Some(key)
            }
            Ok(None) => None,
            Err(err) => {
                warn!(error = %err, "获取续期凭据失败（登录继续，续期前会重试）");
                None
            }
        };
        if auth_key.is_none() {
            if tokens.auth_key.is_some() {
                warn!("回退使用深链 SSO token 作为续期 key（可能无法续期）");
            }
            auth_key = tokens.auth_key.clone();
        }
        (models, auth_key)
    }

    /// 仅在代次未变时提交登录态；`None` 表示注销已发生（调用方负责删掉刚写的凭据）。
    async fn commit_login(
        &self,
        epoch: u64,
        credentials: Credentials,
        profile: AuthProfile,
        models: Vec<ModelSummary>,
        offline_grace: bool,
        now: DateTime<Utc>,
    ) -> Option<()> {
        self.commit_if_current(epoch, |state| {
            state.credentials = Some(credentials);
            state.profile = Some(profile);
            state.models = models;
            state.pending = None;
            state.offline_grace = offline_grace;
            state.credentials_valid = !offline_grace;
            state.phase = if offline_grace {
                AuthPhase::OfflineGrace
            } else {
                AuthPhase::LoggedIn
            };
            state.last_error =
                offline_grace.then(|| "登录成功，但 /validate 暂不可用（离线宽限）".to_string());
            state.last_validated_at = (!offline_grace).then_some(now);
            state.renew = RenewRuntime::default();
            state.loaded = true;
        })
        .await
    }
}

/// 由登录响应组装凭据与账号档案（纯函数，便于测试）。
///
/// `DEFAULT_TOKEN_TTL_SECS` 只在服务端未返回 `expire` 时兜底（契约推断为 1 天）。
fn build_login_credentials(
    tokens: LoginTokens,
    validate_info: ValidateInfo,
    auth_key: Option<String>,
    gateway_base: &str,
    now: DateTime<Utc>,
) -> Result<(Credentials, AuthProfile), AuthError> {
    let issued_at = now.timestamp();
    let expires_at = tokens
        .expires_at
        .unwrap_or(issued_at + DEFAULT_TOKEN_TTL_SECS);
    let user = tokens
        .user
        .clone()
        .or_else(|| validate_info.display_name.clone())
        .ok_or_else(|| AuthError::Internal("登录响应缺少用户标识".to_string()))?;
    let server_url = tokens
        .server_url
        .clone()
        .or_else(|| validate_info.server_url.clone())
        .or_else(|| validate_info.gateway.clone())
        .unwrap_or_else(|| gateway_base.to_string());

    let credentials = Credentials {
        access_token: tokens.access_token,
        auth_key,
        user: user.clone(),
        server_url: server_url.clone(),
        issued_at,
        expires_at,
    };
    let profile = AuthProfile {
        user,
        display_name: validate_info.display_name,
        dept: validate_info.dept,
        server_url,
        issued_at: Some(issued_at),
        expires_at: Some(expires_at),
    };
    Ok((credentials, profile))
}

/// 深链回调的裁决结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeepLinkVerdict {
    /// 绑定成功（`deep_link_received` / `credentials_consumed` 已更新）。
    Accepted,
    /// 当前没有进行中的登录。
    NoPending,
    /// pollUuid 不匹配，或带凭据却没有 pollUuid（无法证明属于本次登录）。
    Mismatch,
    /// 该次登录已经超时。
    Expired,
    /// 本次登录已经采纳过一份随深链带回的凭据。
    Consumed,
}

impl DeepLinkVerdict {
    /// 给用户看的原因文案（静态常量，绝不含 URL / token）。
    fn message(self) -> &'static str {
        match self {
            DeepLinkVerdict::NoPending => {
                "回调链接不属于当前登录会话（可能已过期或已使用），请点击「登录」重新发起"
            }
            DeepLinkVerdict::Mismatch => "回调链接与当前登录会话不匹配，已忽略",
            DeepLinkVerdict::Expired => "登录会话已超时，请重新点击「登录」",
            DeepLinkVerdict::Consumed => "该回调链接已使用过，已忽略",
            DeepLinkVerdict::Accepted => "回调链接已接受",
        }
    }
}

/// 判断一条深链能否绑定到当前 pending 登录（纯函数，便于穷举测试）。
///
/// 要点：带凭据的链接**必须**绑定到本次登录的 pollUuid，且每个 pending 只采纳一次。
/// 这是防登录 CSRF / 回调链接重放的关键；不带凭据的链接只做"到达确认"，
/// 因此没有 pending 时也不要报错（前端仍可能靠轮询完成）。
pub(super) fn claim_deep_link(
    state: &mut RuntimeState,
    payload: &DeepLinkPayload,
    now: DateTime<Utc>,
) -> DeepLinkVerdict {
    let Some(pending) = state.pending.as_mut() else {
        return DeepLinkVerdict::NoPending;
    };

    let bound = match payload.poll_uuid.as_deref() {
        Some(poll_uuid) => pending
            .poll_uuid
            .to_string()
            .eq_ignore_ascii_case(poll_uuid),
        // 带凭据却没有 pollUuid 的链接无法证明属于本次登录。
        None => !payload.has_credentials(),
    };
    if !bound {
        return DeepLinkVerdict::Mismatch;
    }
    if pending.is_timed_out(now) {
        return DeepLinkVerdict::Expired;
    }
    if payload.has_credentials() && pending.credentials_consumed {
        return DeepLinkVerdict::Consumed;
    }

    pending.deep_link_received = true;
    if payload.has_credentials() {
        pending.credentials_consumed = true;
    }
    DeepLinkVerdict::Accepted
}
