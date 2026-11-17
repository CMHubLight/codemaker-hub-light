//! token 续期：到期判定、续期 key 获取、请求与结果提交。

use super::*;
use crate::auth::client::RenewedToken;

/// 一次续期尝试的结果（供 `/validate` 401/403 分支与手动"重新检测"复用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RenewOutcome {
    /// 续期成功，且新凭据已提交到内存/磁盘。
    Renewed,
    /// 没有可用的 `auth_key`，无法续期；状态已置为 `reauth_required`。
    NoKey,
    /// 服务端拒绝（401/403）；状态已置为 `reauth_required`。
    Rejected,
    /// 网络/其它瞬时失败；已记录退避，调用方可回落离线宽限。
    Transient,
}

impl AuthService {
    /// 续期检查；真正发起请求时返回 true。
    pub(super) async fn renew_if_due(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        credentials: &Credentials,
        now: DateTime<Utc>,
    ) -> bool {
        // 本次 tick 内 `/validate` 可能已用 auth_key 强制续期：以内存里的最新凭据
        // 为准，否则会拿旧的过期时间再续一次（同一 tick 内重复续期）。
        let current =
            { self.state.read().await.credentials.clone() }.unwrap_or_else(|| credentials.clone());
        // 1) 到期 / 静默期 / 退避判定（只读状态，不产生副作用）。
        if self
            .renewal_ready(epoch, current.expires_at, now)
            .await
            .is_none()
        {
            return false;
        }
        // 2) 取 key → 请求 → 提交（与强制续期共用同一路径）。
        self.perform_renewal(epoch, gateway, current).await;
        true
    }

    /// 强制续期（跳过"是否到期/静默期"判定），供 `/validate` 401/403 与手动重试复用。
    ///
    /// `respect_backoff`：自动路径必须尊重失败退避（否则 401/403 + 续期 5xx 会每轮
    /// 重试一遍）；用户手动「重新检测」传 `false`，允许立刻重试一次。
    ///
    /// 返回 `None` 表示当前没有可用的登录凭据；否则返回本次续期结果。
    pub(super) async fn renew_now(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        respect_backoff: bool,
    ) -> Option<RenewOutcome> {
        let current = { self.state.read().await.credentials.clone() }?;
        if respect_backoff {
            let renew = { self.state.read().await.renew.clone() };
            if let Some(last_attempt) = renew.last_attempt_at {
                let wait = renew.backoff_secs.max(RENEW_BACKOFF_MIN_SECS);
                if (Utc::now() - last_attempt).num_seconds() < wait {
                    info!("续期仍在退避窗口内，本次跳过");
                    return Some(RenewOutcome::Transient);
                }
            }
        }
        Some(self.perform_renewal(epoch, gateway, current).await)
    }

    /// 手动"重新检测/续期"：强制续期一次；成功后补一次 `/validate` 刷新账号档案与模型目录。
    ///
    /// 续期失败不返回错误：失败原因已写入 `last_error`/退避状态，界面读 `auth_status` 即可。
    pub async fn retry_renewal(&self, gateway_url: &str) -> Result<AuthUpdate, AuthError> {
        self.ensure_loaded().await;
        let epoch = self.epoch().await;
        if self.state.read().await.credentials.is_none() {
            return Ok(self.make_update(None, false).await);
        }
        let gateway = self.gateway(gateway_url).await?;
        if self.renew_now(epoch, &gateway, false).await == Some(RenewOutcome::Renewed) {
            // 续期成功后旧 access_token 已被替换，补一次校验让 profile/models 跟上新身份。
            let fresh = { self.state.read().await.credentials.clone() };
            if let Some(fresh) = fresh {
                self.validate_with(epoch, &gateway, &fresh).await;
            }
        }
        Ok(self.make_update(None, false).await)
    }

    /// 执行一次续期尝试：取 key → 请求 → 提交；不在这里做"到期/静默期"判断。
    ///
    /// 返回值只描述结果；所有状态写入仍走 `commit_if_current`（代次不变才提交）。
    async fn perform_renewal(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        current: Credentials,
    ) -> RenewOutcome {
        // 续期运行时快照（退避/静默期/提交序号）在发请求前取一次：
        // 本次请求的提交必须基于这份快照，晚到的结果才不会被误当成"最新"。
        let renew = { self.state.read().await.renew.clone() };
        // 取续期 key（每轮都取：服务端可能轮换，旧版本存的可能是错的那种凭据）。
        let stored_key = current.auth_key.clone();
        let fresh_key = self.fetch_renew_key(gateway, &current).await;
        let Some(primary_key) = fresh_key.or_else(|| stored_key.clone()) else {
            // 两处都没有 key：只能要求重新登录。
            self.commit_missing_renew_key(epoch, current.access_token(), &renew)
                .await;
            return RenewOutcome::NoKey;
        };

        // 发起续期（401/403 时用已存 key 重试一次）。
        let (used_key, outcome) = self
            .attempt_renew(gateway, &current, &primary_key, stored_key.as_deref())
            .await;

        match outcome {
            Ok(renewed) => {
                let committed = self
                    .commit_renew_success(epoch, current, renewed, used_key, Utc::now(), &renew)
                    .await;
                if committed {
                    RenewOutcome::Renewed
                } else {
                    // 代次已变 / 已被新登录或另一次续期取代：本次结果作废，按瞬时失败处理。
                    RenewOutcome::Transient
                }
            }
            Err(err) => {
                self.commit_renew_failure(epoch, &err, Utc::now(), &renew, current.access_token())
                    .await;
                if err.is_forbidden() {
                    RenewOutcome::Rejected
                } else {
                    RenewOutcome::Transient
                }
            }
        }
    }

    /// 没有可用续期 key：只能要求重新登录（与续期失败区分，文案更具体）。
    async fn commit_missing_renew_key(&self, epoch: u64, access_token: &str, renew: &RenewRuntime) {
        self.commit_if_session(epoch, access_token, |state| {
            if state.renew.seq != renew.seq {
                return;
            }
            state.phase = AuthPhase::ReauthRequired;
            state.credentials_valid = false;
            state.last_error = Some("缺少续期凭据（auth_key），请重新登录".to_string());
        })
        .await;
    }

    /// 续期到期判定：返回当期 [`RenewRuntime`] 快照；不该续期时返回 `None`。
    ///
    /// 判定 = 临近过期（或距上次成功超过 82800s）+ 已过 600s 静默期 + 已过退避窗口。
    async fn renewal_ready(
        &self,
        epoch: u64,
        expires_at: i64,
        now: DateTime<Utc>,
    ) -> Option<RenewRuntime> {
        if !self.still_current(epoch).await {
            return None;
        }
        let (renew, phase) = {
            let state = self.state.read().await;
            (state.renew.clone(), state.phase)
        };
        if phase == AuthPhase::ReauthRequired {
            return None;
        }

        let now_ts = now.timestamp();
        if !renewal_due(
            expires_at,
            renew.last_success_at.map(|t| t.timestamp()),
            now_ts,
        ) {
            return None;
        }
        // 刚续过就不再续：短有效期 token 否则会被每轮检查反复轮换。
        if !renew_interval_ok(
            renew.last_success_at.map(|t| t.timestamp()),
            expires_at,
            now_ts,
        ) {
            return None;
        }
        if let Some(last_attempt) = renew.last_attempt_at {
            if (now - last_attempt).num_seconds() < renew.backoff_secs.max(RENEW_BACKOFF_MIN_SECS) {
                return None;
            }
        }
        Some(renew)
    }

    /// 取一次续期 key：必须来自 `/get_auth_key`（128 字符）。
    ///
    /// 深链里的 SSO token（225 字符）会被服务端以 403 `decrypt auth key fail` 拒绝
    /// （见 docs/gateway-contract.md），所以每轮都优先取；失败才回退已存凭据。
    async fn fetch_renew_key(
        &self,
        gateway: &AuthGateway,
        credentials: &Credentials,
    ) -> Option<String> {
        match gateway.get_auth_key(credentials.access_token()).await {
            Ok(Some(key)) => {
                debug!(len = key.len(), "续期前已取到 auth_key");
                Some(key)
            }
            Ok(None) => {
                warn!("续期前 /get_auth_key 未返回 key，回退已存凭据");
                None
            }
            Err(err) => {
                warn!(error = %err, "续期前获取 auth_key 失败，回退已存凭据");
                None
            }
        }
    }

    /// 发起续期请求；401/403 可能是 key 已过期/被轮换，主 key 与已存 key 不同时再试一次。
    async fn attempt_renew(
        &self,
        gateway: &AuthGateway,
        credentials: &Credentials,
        primary_key: &str,
        stored_key: Option<&str>,
    ) -> (String, Result<RenewedToken, GatewayError>) {
        let mut used_key = primary_key.to_string();
        let mut outcome = gateway.renew_token(primary_key, &credentials.user).await;
        if matches!(&outcome, Err(GatewayError::Status { status }) if matches!(status, 401 | 403)) {
            if let Some(fallback) = stored_key.filter(|key| *key != primary_key) {
                warn!("续期被拒绝，回退已存 key 重试一次");
                used_key = fallback.to_string();
                outcome = gateway.renew_token(fallback, &credentials.user).await;
            }
        }
        (used_key, outcome)
    }

    /// 续期成功：更新内存凭据、写盘、提交状态；返回是否真正提交（注销/被取代则 false）。
    pub(super) async fn commit_renew_success(
        &self,
        epoch: u64,
        mut current: Credentials,
        renewed: RenewedToken,
        used_key: String,
        now: DateTime<Utc>,
        renew: &RenewRuntime,
    ) -> bool {
        // 先记下本次续期"基于哪一份凭据"，再改写本地副本：提交时用它判断身份是否还是同一个。
        let based_on = current.access_token.clone();
        let issued_at = now.timestamp();
        let expires_at = renewed
            .expires_at
            .unwrap_or(issued_at + DEFAULT_TOKEN_TTL_SECS);
        current.access_token = renewed.access_token;
        current.issued_at = issued_at;
        current.expires_at = expires_at;
        // 记下"这次真正生效的续期 key"：老记录里的 SSO token 会被升级成 128 字符 key。
        current.auth_key = Some(used_key);
        // 凭据写盘 + 提交的临界区：身份检查、写盘、提交三者必须原子，
        // 否则"检查时还是 A、写盘时已变成 B"会让磁盘留下上一个身份。
        let _write = self.credential_write.lock().await;
        // 写盘代次要在任何检查之前取：这样"检查通过之后发生的注销"必然让
        // save_if 代次失配（旧 P3-3 的窗口），而不是读到注销后的新代次。
        let generation = self.store.credential_generation();
        // 并发续期里输家不得把结果写进 auth.bin：seq 比对必须在写盘之前。
        if { self.state.read().await.renew.seq } != renew.seq {
            info!("续期结果已被另一次续期取代，丢弃本次写盘");
            return false;
        }
        if !self.session_still(epoch, &based_on).await {
            info!("续期结果所属的凭据已被替换，丢弃本次续期");
            return false;
        }
        // 写盘还要过"代次大门"：`session_still` 与真正拿锁写盘之间有窗口
        // （`save_async_if` 要排队进阻塞线程池），期间发生的注销必须让这次写盘作废。
        // 不能改用"提交失败就删刚写的文件"：续期与"注销后重新登录"可并发，
        // 补删会误删新会话刚写出的凭据。
        match self.store.save_async_if(generation, &current).await {
            Ok(true) => {}
            Ok(false) => return false,
            Err(err) => warn!(error = %err, "续期后保存凭据失败（内存已更新）"),
        }
        let committed = self
            .commit_if_session(epoch, &based_on, |state| {
                // 并发续期里晚到的那个不得覆盖先成功的结果。
                if state.renew.seq != renew.seq {
                    return false;
                }
                if let Some(profile) = state.profile.as_mut() {
                    profile.issued_at = Some(issued_at);
                    profile.expires_at = Some(expires_at);
                }
                state.credentials = Some(current);
                state.credentials_valid = true;
                state.offline_grace = false;
                state.phase = AuthPhase::LoggedIn;
                state.last_error = None;
                state.renew.last_success_at = Some(now);
                state.renew.last_attempt_at = Some(now);
                state.renew.backoff_secs = 0;
                state.renew.seq = renew.seq.wrapping_add(1);
                info!("token 续期成功");

                // 续期必须真的延长有效期；反复拿到同一个过期时间说明网关没在延长，
                // 继续轮换没有任何意义，只会让用户以为"一直在续"。
                let progressed = renewal_progress(renew.last_renewed_expires_at, expires_at);
                state.renew.last_renewed_expires_at = Some(expires_at);
                if progressed {
                    state.renew.no_progress_count = 0;
                } else {
                    state.renew.no_progress_count += 1;
                    let count = state.renew.no_progress_count;
                    if count >= RENEW_MAX_NO_PROGRESS {
                        state.phase = AuthPhase::ReauthRequired;
                        state.credentials_valid = false;
                        state.last_error = Some(
                            "网关未延长 token 有效期（自动续期无进展），请重新登录".to_string(),
                        );
                        warn!(count, "续期成功但有效期未前进，判定无进展：需要重新登录");
                    } else {
                        warn!(count, "续期成功但有效期未前进");
                    }
                }
                true
            })
            .await;
        matches!(committed, Some(true))
    }

    /// 续期失败：60s → 120s → 240s → 300s 退避；401/403 直接要求重新登录。
    pub(super) async fn commit_renew_failure(
        &self,
        epoch: u64,
        err: &GatewayError,
        now: DateTime<Utc>,
        renew: &RenewRuntime,
        access_token: &str,
    ) -> bool {
        let next_backoff = next_backoff(renew.backoff_secs);
        let committed = self
            .commit_if_session(epoch, access_token, |state| {
                // 另一次续期已经成功提交过：晚到的失败不得把登录态改回 reauth。
                if state.renew.seq != renew.seq {
                    return false;
                }
                state.renew.last_attempt_at = Some(now);
                state.renew.backoff_secs = next_backoff;
                if err.is_forbidden() {
                    state.phase = AuthPhase::ReauthRequired;
                    state.credentials_valid = false;
                    state.last_error = Some("续期被服务端拒绝，请重新登录".to_string());
                    warn!("/renew_token 返回 401/403：需要重新登录");
                } else {
                    state.last_error = Some(format!("自动续期失败，{} 秒后重试", next_backoff));
                    warn!(error = %err, "自动续期失败，进入退避");
                }
                true
            })
            .await;
        matches!(committed, Some(true))
    }
}

/// 是否需要续期：临近过期（`expires_at - now <= 1h`）或距上次成功续期 ≥82800s。
pub fn renewal_due(expires_at: i64, last_success_at: Option<i64>, now: i64) -> bool {
    if expires_at > 0 && expires_at - now <= RENEW_AHEAD_SECS {
        return true;
    }
    match last_success_at {
        Some(success) => now - success >= RENEW_MAX_INTERVAL_SECS,
        None => false,
    }
}

/// 续期静默期：成功续期后 [`RENEW_MIN_INTERVAL_SECS`] 内不再续期。
///
/// token 已过期（`expires_at <= now`）时不受静默期限制——那种情况必须尝试续期，
/// 失败由退避控制节奏。
pub fn renew_interval_ok(last_success_at: Option<i64>, expires_at: i64, now: i64) -> bool {
    if expires_at > 0 && expires_at <= now {
        return true;
    }
    last_success_at.is_none_or(|success| now - success >= RENEW_MIN_INTERVAL_SECS)
}

/// 这次续期是否真的延长了有效期（首次续期视为有进展）。
pub fn renewal_progress(prev_expires_at: Option<i64>, new_expires_at: i64) -> bool {
    prev_expires_at.is_none_or(|prev| new_expires_at > prev)
}

/// 续期失败退避：60s → 120s → 240s → 300s 封顶。
pub fn next_backoff(current: i64) -> i64 {
    if current < RENEW_BACKOFF_MIN_SECS {
        RENEW_BACKOFF_MIN_SECS
    } else {
        (current * 2).min(RENEW_BACKOFF_MAX_SECS)
    }
}
