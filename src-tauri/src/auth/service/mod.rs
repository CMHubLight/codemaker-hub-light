//! `AuthService`：认证编排（深链/轮询完成登录、validate、quota、renew）与后台循环。
//!
//! 模块划分（原本是单文件 `service.rs`，2026-10 重构按职责拆分）：
//! - `mod.rs`：共享状态类型、构造、代次（epoch）工具、状态快照与广播判据；
//! - `persist.rs`：凭据加载、账号档案与额度快照落库（阻塞 IO 走线程池）；
//! - `login.rs`：发起/轮询/深链绑定与登录完成（单飞 + 代次校验）；
//! - `validate.rs`：`/validate` 刷新与模型目录获取；
//! - `quota_flow.rs`：额度快照读取、拉取并落库（三个调用场景共用同一 HTTP 入口）；
//! - `renewal.rs`：续期判定、key 获取、请求与结果提交；
//! - `loops.rs`：后台循环启动/停止与单次 tick（只在状态变化时广播）；
//! - `logout.rs`：注销（代次作废 → 停机 → 删凭据 → 清状态 → 档案失效）。
//!
//! 设计要点：
//! - 凭据在内存中只在 `RuntimeState.credentials` 出现，落盘走 [`store::CredentialStore`]；
//!   任何 `AuthStatus` / 日志 / 数据库都不含 token。
//! - 登录完成顺序按 development-plan §5.2：validate → （模型目录与 get_auth_key 并发）→
//!   加密落盘 → 拉额度 → 启动后台循环。
//! - 后台循环：启动立即 tick 一次，之后每 60s 检查 validate（600s）/quota（300s）/renew；
//!   登出时通过 oneshot 取消并 abort 任务。
//! - 网络失败语义：validate 网络错误 → offline grace；403 → reauth_required；
//!   renew 失败 60s→300s 退避；quota 失败保留上次快照。
//! - 写路径纪律：所有状态写入都经 [`AuthService::commit_if_current`]（锁内校验代次），
//!   锁外 IO（写盘/DB）由调用方在代次失配时做补偿删除。

use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{DateTime, SecondsFormat, Utc};
use tokio::{
    sync::{oneshot, Mutex, RwLock},
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use tauri::Manager;

use crate::{
    auth::{
        self,
        client::{
            AuthGateway, GatewayError, HourlyCostOutcome, LoginTokens, PollOutcome, QuotaOutcome,
            ValidateInfo,
        },
        hourly::HourlyCost,
        quota::{self, QuotaData},
        store::{CredentialStore, Credentials},
        AuthError, AuthPhase, AuthProfile, AuthStatus, DeepLinkPayload, ModelSummary, PendingLogin,
        MAX_CONSECUTIVE_NETWORK_ERRORS, QUOTA_INTERVAL_SECS, RENEW_AHEAD_SECS,
        RENEW_CHECK_INTERVAL_SECS, RENEW_MAX_INTERVAL_SECS, VALIDATE_INTERVAL_SECS,
    },
    db::{blocking, repo, repo::ProfileUpsert, Db, DbError},
    usage::QuotaInfo,
};

/// 续期失败退避区间（秒）。
pub const RENEW_BACKOFF_MIN_SECS: i64 = 60;
pub const RENEW_BACKOFF_MAX_SECS: i64 = 300;
/// 成功续期后的静默期（秒）：有效期短于 [`RENEW_AHEAD_SECS`] 的 token 不再被
/// 每轮检查反复续期，避免高频轮换 auth_key。
pub const RENEW_MIN_INTERVAL_SECS: i64 = 600;
/// 连续多少次"续期成功但有效期没往前挪"就判定为无进展、要求重新登录。
const RENEW_MAX_NO_PROGRESS: u32 = 3;
/// 服务端未返回 expire 时的 token 默认有效期（秒，契约推断为 1 天）。
const DEFAULT_TOKEN_TTL_SECS: i64 = 86_400;
/// 停止后台循环时给常规退出的宽限窗口（秒）；超时后 abort 并等待落地。
const LOOP_GRACEFUL_STOP_SECS: u64 = 1;

/// 一次认证动作返回给命令层的更新。
#[derive(Debug, Clone)]
pub struct AuthUpdate {
    pub status: AuthStatus,
    pub quota: Option<QuotaInfo>,
    pub quota_updated: bool,
}

#[derive(Debug, Default, Clone)]
struct RenewRuntime {
    last_success_at: Option<DateTime<Utc>>,
    last_attempt_at: Option<DateTime<Utc>>,
    backoff_secs: i64,
    /// 上一次续期成功时拿到的 `expires_at`，用于判断"续期有没有真的延长有效期"。
    last_renewed_expires_at: Option<i64>,
    /// 连续"续期成功但有效期未前进"的次数。
    no_progress_count: u32,
}

#[derive(Default)]
pub(super) struct RuntimeState {
    phase: AuthPhase,
    profile: Option<AuthProfile>,
    pending: Option<PendingLogin>,
    last_error: Option<String>,
    last_validated_at: Option<DateTime<Utc>>,
    last_quota_at: Option<DateTime<Utc>>,
    offline_grace: bool,
    credentials_valid: bool,
    credentials: Option<Credentials>,
    models: Vec<ModelSummary>,
    loaded: bool,
    renew: RenewRuntime,
    /// 登录代次：注销时自增，用于让所有在途操作立即失效。
    ///
    /// 写路径纪律：代次判断与写入**必须在同一把 `state.write()` 里**完成，
    /// 不能"先检查、解锁、再写"；`store.save` 这类锁外写入则先做 `still_current`。
    epoch: u64,
}

#[derive(Default)]
struct LoopHandle {
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

/// 认证服务（跨 Tauri 命令与后台任务共享）。
pub struct AuthService {
    store: CredentialStore,
    db: Db,
    state: RwLock<RuntimeState>,
    loops: Mutex<LoopHandle>,
    gateway_cache: RwLock<Option<(String, Arc<AuthGateway>)>>,
    /// 登录完成单飞：深链/轮询/重复投递不得并发各做一遍 validate + 落盘。
    login_serial: Mutex<()>,
}

impl AuthService {
    pub fn new(data_dir: PathBuf, db: Db) -> Self {
        let store = CredentialStore::new(&data_dir);
        Self::with_store(db, store)
    }

    /// 测试注入口（避免测试触碰真实 keyring）。
    pub fn with_store(db: Db, store: CredentialStore) -> Self {
        Self {
            store,
            db,
            state: RwLock::new(RuntimeState::default()),
            loops: Mutex::new(LoopHandle::default()),
            gateway_cache: RwLock::new(None),
            login_serial: Mutex::new(()),
        }
    }

    pub async fn status(&self) -> AuthStatus {
        let state = self.state.read().await;
        Self::status_from(&state)
    }

    /// 当前登录代次。注销会自增，使注销前发起的操作全部失效。
    pub(super) async fn epoch(&self) -> u64 {
        self.state.read().await.epoch
    }

    /// 使所有在途操作失效（注销入口调用）。
    pub(super) async fn bump_epoch(&self) -> u64 {
        let mut state = self.state.write().await;
        state.epoch = state.epoch.wrapping_add(1);
        state.epoch
    }

    /// 代次是否仍为 `epoch`。
    ///
    /// 用于锁外写入（`store.save` / `repo::*`）之前的前置检查；
    /// 锁内写入请走 [`Self::commit_if_current`]，不要拆成"检查-解锁-写入"。
    pub(super) async fn still_current(&self, epoch: u64) -> bool {
        self.state.read().await.epoch == epoch
    }

    /// 在"代次未变"的前提下修改状态；返回 `None` 表示会话已作废、闭包未执行。
    ///
    /// 两个纪律：
    /// - 闭包内**不得 `.await`**（`state.write()` 不能跨 await 持有）；
    /// - 锁外副作用（写盘、删盘、DB）由调用方在 `None` 分支上收尾。
    ///
    /// 这样所有写路径只有一种写法，新增入口不会漏掉代次判断。
    pub(super) async fn commit_if_current<T>(
        &self,
        epoch: u64,
        action: impl FnOnce(&mut RuntimeState) -> T,
    ) -> Option<T> {
        let mut state = self.state.write().await;
        if state.epoch != epoch {
            return None;
        }
        Some(action(&mut state))
    }

    pub async fn has_credentials(&self) -> bool {
        self.state.read().await.credentials.is_some()
    }

    /// 代理转发用的最小凭据快照；`reauth_required` 时返回 `None`（让代理回 401）。
    pub(crate) async fn credentials_snapshot(&self) -> Option<crate::auth::CredentialsSnapshot> {
        let state = self.state.read().await;
        if state.phase == AuthPhase::ReauthRequired {
            return None;
        }
        let credentials = state.credentials.as_ref()?;
        Some(crate::auth::CredentialsSnapshot {
            access_token: credentials.access_token.clone(),
            user: credentials.user.clone(),
            gateway_url: state
                .profile
                .as_ref()
                .map(|profile| profile.server_url.clone())
                .unwrap_or_default(),
        })
    }

    /// `/validate` 缓存的模型目录（`/v1/models` 用；不触发网络）。
    pub(crate) async fn models_snapshot(&self) -> Vec<ModelSummary> {
        self.state.read().await.models.clone()
    }

    pub(super) async fn gateway(&self, gateway_url: &str) -> Result<Arc<AuthGateway>, AuthError> {
        let mut cache = self.gateway_cache.write().await;
        if let Some((url, gateway)) = cache.as_ref() {
            if url == gateway_url {
                return Ok(Arc::clone(gateway));
            }
        }
        let gateway = Arc::new(AuthGateway::new(gateway_url)?);
        *cache = Some((gateway_url.to_string(), Arc::clone(&gateway)));
        Ok(gateway)
    }

    pub(super) async fn make_update(
        &self,
        quota: Option<QuotaInfo>,
        quota_updated: bool,
    ) -> AuthUpdate {
        AuthUpdate {
            status: self.status().await,
            quota,
            quota_updated,
        }
    }

    pub(super) async fn record_failure(&self, epoch: u64, message: &str) {
        // 失败发生在注销之后时不要再把状态改回 error。
        self.commit_if_current(epoch, |state| {
            state.phase = AuthPhase::Error;
            state.pending = None;
            state.last_error = Some(message.to_string());
        })
        .await;
    }

    pub(super) fn status_from(state: &RuntimeState) -> AuthStatus {
        let pending_poll_uuid = state.pending.as_ref().map(|p| p.poll_uuid.to_string());
        let pending_login_started_at = state
            .pending
            .as_ref()
            .map(|p| p.started_at.to_rfc3339_opts(SecondsFormat::Secs, true));
        let deep_link_received = state
            .pending
            .as_ref()
            .map(|p| p.deep_link_received)
            .unwrap_or(false);
        let message = state.last_error.clone().or_else(|| {
            state
                .pending
                .as_ref()
                .map(|_| "等待浏览器完成登录（已完成登录会自动更新）".to_string())
        });

        AuthStatus {
            state: state.phase.frontend_state(),
            user: state.profile.as_ref().map(|p| p.user.clone()),
            display_name: state.profile.as_ref().and_then(|p| p.display_name.clone()),
            expires_at: state
                .credentials
                .as_ref()
                .map(|c| c.expires_at)
                .or_else(|| state.profile.as_ref().and_then(|p| p.expires_at)),
            offline_grace: state.offline_grace,
            message,
            dept: state.profile.as_ref().and_then(|p| p.dept.clone()),
            server_url: state.profile.as_ref().map(|p| p.server_url.clone()),
            issued_at: state
                .credentials
                .as_ref()
                .map(|c| c.issued_at)
                .or_else(|| state.profile.as_ref().and_then(|p| p.issued_at)),
            credentials_valid: state.credentials_valid,
            pending_poll_uuid,
            pending_login_started_at,
            deep_link_received,
            last_validated_at: state
                .last_validated_at
                .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true)),
        }
    }
}

/// 后台 tick 是否值得广播：可观察状态发生变化，或额度确有更新。
///
/// 状态没变时发事件会让前端整页重拉（见 [`crate::auth::status_changed`]）。
pub(crate) fn update_is_publishable(before: &AuthStatus, update: &AuthUpdate) -> bool {
    update.quota_updated || crate::auth::status_changed(before, &update.status)
}

mod login;
mod logout;
mod loops;
mod persist;
mod quota_flow;
mod renewal;
mod validate;

#[cfg(test)]
mod test_support;

pub use renewal::{next_backoff, renew_interval_ok, renewal_due, renewal_progress};

#[cfg(test)]
mod tests {
    use super::login::{claim_deep_link, DeepLinkVerdict};
    use super::test_support::{deep_link_payload, pending_uuid, state_with_pending, temp_service};
    use super::validate::merge_models;
    use super::*;
    use std::fs;

    #[test]
    fn renewal_due_when_near_expiry() {
        let now = 1_700_000_000;
        assert!(renewal_due(now + 600, None, now));
        assert!(renewal_due(now + RENEW_AHEAD_SECS, None, now));
        assert!(!renewal_due(now + RENEW_AHEAD_SECS + 60, None, now));
    }

    #[test]
    fn renewal_due_after_max_interval() {
        let now = 1_700_000_000;
        let success = now - RENEW_MAX_INTERVAL_SECS;
        assert!(renewal_due(now + 2 * RENEW_AHEAD_SECS, Some(success), now));
        assert!(!renewal_due(
            now + 2 * RENEW_AHEAD_SECS,
            Some(now - 100),
            now
        ));
    }

    /// 静默期：刚续过就不再续，避免短有效期 token 被反复轮换。
    #[test]
    fn renew_interval_blocks_repeated_success() {
        let now = 1_700_000_000;
        let far_expiry = now + 7 * 24 * 3600;
        // 60 秒前刚成功 → 静默期内
        assert!(!renew_interval_ok(Some(now - 60), far_expiry, now));
        // 刚好到静默期，放行
        assert!(renew_interval_ok(
            Some(now - RENEW_MIN_INTERVAL_SECS),
            far_expiry,
            now
        ));
        // 从未成功过 → 放行
        assert!(renew_interval_ok(None, far_expiry, now));
        // token 已过期：即使刚续过也必须尝试
        assert!(renew_interval_ok(Some(now - 5), now, now));
        assert!(renew_interval_ok(Some(now - 5), now - 60, now));
    }

    /// 续期是否真的延长了有效期。
    #[test]
    fn renewal_progress_detects_stall() {
        assert!(renewal_progress(None, 1_700_000_000));
        assert!(renewal_progress(Some(1_700_000_000), 1_700_003_600));
        assert!(!renewal_progress(Some(1_700_000_000), 1_700_000_000));
        assert!(!renewal_progress(Some(1_700_000_000), 1_699_999_000));
    }

    #[test]
    fn backoff_progression_is_bounded() {
        assert_eq!(next_backoff(0), 60);
        assert_eq!(next_backoff(60), 120);
        assert_eq!(next_backoff(120), 240);
        assert_eq!(next_backoff(240), 300);
        assert_eq!(next_backoff(300), 300);
    }

    #[test]
    fn merge_models_deduplicates() {
        let a = vec![ModelSummary {
            id: "a".into(),
            name: Some("A".into()),
        }];
        let b = vec![
            ModelSummary {
                id: "a".into(),
                name: None,
            },
            ModelSummary {
                id: "b".into(),
                name: None,
            },
        ];
        let merged = merge_models(a, b);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].name.as_deref(), Some("A"));
    }

    #[tokio::test]
    async fn persist_profile_roundtrip_with_models_json() {
        let (service, dir) = temp_service();
        let profile = AuthProfile {
            user: "test-user".to_string(),
            display_name: Some("测试用户".to_string()),
            dept: Some("测试部门".to_string()),
            server_url: "https://api.example.test".to_string(),
            issued_at: Some(1_700_000_000),
            expires_at: Some(1_700_086_400),
        };
        let models = vec![ModelSummary {
            id: "claude-sonnet-4".to_string(),
            name: Some("Claude Sonnet 4".to_string()),
        }];
        let epoch = service.epoch().await;
        let id = service
            .persist_profile(epoch, &profile, &models, Some(Utc::now()))
            .await
            .expect("profile 写入应成功");

        let conn = service.db.conn().expect("取连接应成功");
        let row = repo::active_profile(&conn)
            .expect("读取应成功")
            .expect("应有激活档案");
        assert_eq!(row.id, id);
        assert_eq!(row.user, "test-user");
        assert_eq!(row.dept.as_deref(), Some("测试部门"));
        assert!(row.validated_at.is_some());
        let cached: Vec<ModelSummary> =
            serde_json::from_str(row.models_json.as_deref().unwrap()).unwrap();
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].id, "claude-sonnet-4");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn status_and_logout_never_expose_tokens() {
        let (service, dir) = temp_service();
        // 未登录状态。
        let status = service.status().await;
        assert_eq!(status.state, "unauthenticated");
        assert!(!status.credentials_valid);

        let update = service.logout().await;
        assert_eq!(update.status.state, "unauthenticated");

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn store_quota_persists_snapshot_and_returns_info() {
        let (service, dir) = temp_service();
        let data = quota::parse_quota(&serde_json::json!({
            "monthly_quota": 1000.0,
            "used": 250.0,
            "remaining": 750.0,
            "reset_day": 1,
            "is_active": true,
        }));
        let epoch = service.epoch().await;
        let info = service
            .store_quota(epoch, None, &data)
            .await
            .expect("额度写入应成功");
        assert_eq!(info.remaining, Some(750.0));
        assert!(info.is_active);

        let latest = service.latest_quota().await.expect("读取应成功");
        assert_eq!(latest.map(|quota| quota.remaining), Some(Some(750.0)));

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // 第二批：注销屏障 + 深链回调绑定
    // -----------------------------------------------------------------------

    /// 没有进行中的登录时，绝不能采纳回调里的凭据。
    #[test]
    fn claim_deep_link_requires_pending() {
        let now = Utc::now();
        let mut state = RuntimeState::default();

        assert_eq!(
            claim_deep_link(&mut state, &deep_link_payload(None, Some("token")), now),
            DeepLinkVerdict::NoPending
        );
        assert!(state.credentials.is_none(), "不得写入凭据");
        assert!(state.pending.is_none());
    }

    /// 带凭据但没有 pollUuid、或 pollUuid 不匹配，都视为未绑定。
    #[test]
    fn claim_deep_link_rejects_unbound_or_mismatched_uuid() {
        let now = Utc::now();
        let mut state = state_with_pending(now);

        assert_eq!(
            claim_deep_link(&mut state, &deep_link_payload(None, Some("token")), now),
            DeepLinkVerdict::Mismatch
        );
        let other = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            claim_deep_link(
                &mut state,
                &deep_link_payload(Some(&other), Some("token")),
                now
            ),
            DeepLinkVerdict::Mismatch
        );

        let pending = state.pending.as_ref().expect("pending 应保留");
        assert!(!pending.deep_link_received, "被拒的链接不该标记已到达");
        assert!(!pending.credentials_consumed);
    }

    #[test]
    fn claim_deep_link_rejects_expired_pending() {
        let started_at = Utc::now() - chrono::Duration::seconds(auth::POLL_TIMEOUT_SECS as i64 + 5);
        let mut state = state_with_pending(started_at);
        let uuid = pending_uuid(&state);

        assert_eq!(
            claim_deep_link(
                &mut state,
                &deep_link_payload(Some(&uuid), Some("token")),
                Utc::now()
            ),
            DeepLinkVerdict::Expired
        );
        assert!(
            !state
                .pending
                .as_ref()
                .expect("pending 应保留")
                .credentials_consumed
        );
    }

    /// 每个 pending 只采纳一次凭据（Windows 上会双投递同一条链接）。
    #[test]
    fn claim_deep_link_consumes_credentials_once() {
        let now = Utc::now();
        let mut state = state_with_pending(now);
        let uuid = pending_uuid(&state);
        let payload = deep_link_payload(Some(&uuid), Some("token"));

        assert_eq!(
            claim_deep_link(&mut state, &payload, now),
            DeepLinkVerdict::Accepted
        );
        {
            let pending = state.pending.as_ref().expect("pending 应保留");
            assert!(pending.deep_link_received);
            assert!(pending.credentials_consumed);
        }
        assert_eq!(
            claim_deep_link(&mut state, &payload, now),
            DeepLinkVerdict::Consumed,
            "同一条链接第二次必须只被忽略"
        );
    }

    /// 不带凭据的回调只做"到达确认"，不消耗一次性名额（轮询仍需同一个 uuid）。
    #[test]
    fn claim_deep_link_without_credentials_only_marks_received() {
        let now = Utc::now();
        let mut state = state_with_pending(now);
        let uuid = pending_uuid(&state);

        assert_eq!(
            claim_deep_link(&mut state, &deep_link_payload(Some(&uuid), None), now),
            DeepLinkVerdict::Accepted
        );
        {
            let pending = state.pending.as_ref().expect("pending 应保留");
            assert!(pending.deep_link_received);
            assert!(!pending.credentials_consumed);
        }
        // 随后的带凭据链接仍然可以被采纳。
        assert_eq!(
            claim_deep_link(
                &mut state,
                &deep_link_payload(Some(&uuid), Some("token")),
                now
            ),
            DeepLinkVerdict::Accepted
        );
    }

    /// 端到端闸门：没有 pending 时带凭据的回调必须被拒绝且不落盘；
    /// 不带凭据的迟到回调则静默忽略（不该打扰用户）。
    #[tokio::test]
    async fn deep_link_without_pending_never_adopts_credentials() {
        let (service, dir) = temp_service();

        let rejected = service
            .handle_deep_link(
                &deep_link_payload(None, Some("attacker-token")),
                "https://example.test",
            )
            .await;
        assert!(
            matches!(rejected, Err(AuthError::InvalidDeepLink(_))),
            "带凭据但无 pending 必须拒绝"
        );
        assert!(!service.has_credentials().await);
        assert!(!service.store.exists(), "被拒的回调不能落盘凭据");

        let quiet = service
            .handle_deep_link(&deep_link_payload(None, None), "https://example.test")
            .await;
        assert!(quiet.is_ok(), "不带凭据的迟到回调应静默忽略");

        let _ = fs::remove_dir_all(&dir);
    }

    /// 注销必须清掉内存凭据与磁盘文件，并让旧代次失效。
    #[tokio::test]
    async fn logout_clears_credentials_and_files() {
        let (service, dir) = temp_service();
        let credentials = Credentials {
            access_token: "test-access-token".to_string(),
            auth_key: Some("test-auth-key".to_string()),
            user: "tester".to_string(),
            server_url: "https://api.example.test".to_string(),
            issued_at: 1,
            expires_at: 2,
        };
        service.store.save(&credentials).expect("写凭据应成功");
        service.load_strict().await.expect("严格加载应成功");
        assert!(service.has_credentials().await);
        assert!(service.store.exists(), "load_strict 后应存在 auth.bin");

        // 模拟注销前发起的在途操作：它持有的是旧代次。
        let stale_epoch = service.epoch().await;

        let update = service.logout().await;

        assert_eq!(update.status.state, "unauthenticated");
        assert!(!service.has_credentials().await);
        assert!(!service.store.exists(), "auth.bin 必须被删除");
        assert!(
            !service.still_current(stale_epoch).await,
            "注销后旧代次必须失效"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧代次的完成请求必须立刻放弃：不碰网络、不落盘。
    #[tokio::test]
    async fn finish_login_rejects_stale_epoch_without_network() {
        let (service, dir) = temp_service();
        let stale_epoch = service.epoch().await;
        service.logout().await; // 代次 +1

        let tokens = LoginTokens {
            access_token: "test-access-token".to_string(),
            auth_key: None,
            user: Some("tester".to_string()),
            server_url: None,
            expires_at: None,
        };
        // 网关地址指向不可达主机；代次检查在 gateway() 之前，因此不会真的发请求。
        let result = service
            .finish_login(stale_epoch, "https://example.test", tokens)
            .await;

        assert!(matches!(result, Err(AuthError::SessionEnded)));
        assert!(!service.store.exists(), "被放弃的完成请求不能落盘凭据");

        let _ = fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // C1：状态广播去噪 + 凭据快照
    // -----------------------------------------------------------------------

    fn sample_status() -> AuthStatus {
        AuthStatus {
            state: "authenticated",
            user: Some("tester".to_string()),
            display_name: Some("测试用户".to_string()),
            expires_at: Some(1_700_086_400),
            offline_grace: false,
            message: None,
            dept: Some("测试部门".to_string()),
            server_url: Some("https://api.example.test".to_string()),
            issued_at: Some(1_700_000_000),
            credentials_valid: true,
            pending_poll_uuid: None,
            pending_login_started_at: None,
            deep_link_received: false,
            last_validated_at: Some("2026-10-20T00:00:00Z".to_string()),
        }
    }

    /// 后台 tick 只在可观察状态变化或额度更新时广播：
    /// 状态没变就发事件会让前端整页重拉（含一次 24h 云端费用请求）。
    #[test]
    fn background_tick_only_emits_on_status_change() {
        let base = sample_status();
        let quota =
            Some(quota::parse_quota(&serde_json::json!({"remaining": 1.0})).to_quota_info());

        // 状态与额度都没变：不广播。
        assert!(!update_is_publishable(
            &base,
            &AuthUpdate {
                status: base.clone(),
                quota: None,
                quota_updated: false,
            }
        ));

        // 额度确实更新：即使状态没变也要广播（quota://updated 依赖这条路径）。
        assert!(update_is_publishable(
            &base,
            &AuthUpdate {
                status: base.clone(),
                quota: quota.clone(),
                quota_updated: true,
            }
        ));

        // 状态字段变化（例如 validate 刷新了 last_validated_at）：广播。
        let mut refreshed = base.clone();
        refreshed.last_validated_at = Some("2026-10-20T01:00:00Z".to_string());
        assert!(update_is_publishable(
            &base,
            &AuthUpdate {
                status: refreshed,
                quota: None,
                quota_updated: false,
            }
        ));

        // 网络失败导致 offline grace：phase/message 变化同样要广播。
        let mut degraded = base.clone();
        degraded.state = "authenticated";
        degraded.offline_grace = true;
        degraded.message = Some("网络异常，已按离线宽限继续".to_string());
        assert!(update_is_publishable(
            &base,
            &AuthUpdate {
                status: degraded,
                quota: None,
                quota_updated: false,
            }
        ));
    }

    /// 代理依赖的凭据快照：加载后立即出现，注销后立即消失（不得再发旧 token）。
    #[tokio::test]
    async fn snapshot_tracks_login_and_logout() {
        let (service, dir) = temp_service();
        assert!(
            service.credentials_snapshot().await.is_none(),
            "未登录时不得给代理凭据"
        );

        let credentials = Credentials {
            access_token: "test-access-token".to_string(),
            auth_key: Some("test-auth-key".to_string()),
            user: "tester".to_string(),
            server_url: "https://api.example.test".to_string(),
            issued_at: 1,
            expires_at: 2,
        };
        service.store.save(&credentials).expect("写凭据应成功");
        service.load_strict().await.expect("严格加载应成功");

        let snapshot = service
            .credentials_snapshot()
            .await
            .expect("加载后应有凭据快照");
        assert_eq!(snapshot.access_token, "test-access-token");
        assert_eq!(snapshot.user, "tester");
        assert_eq!(snapshot.gateway_url, "https://api.example.test");
        assert!(
            !format!("{snapshot:?}").contains("test-access-token"),
            "快照 Debug 必须脱敏"
        );

        service.logout().await;
        assert!(
            service.credentials_snapshot().await.is_none(),
            "注销后代理不得再拿到旧 token"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
