//! `/validate` 刷新与模型目录获取。

use super::renewal::RenewOutcome;
use super::*;

impl AuthService {
    /// 手动 `/validate`（命令 auth_refresh_validate 使用）。
    pub async fn refresh_validate(&self, gateway_url: &str) -> Result<AuthUpdate, AuthError> {
        self.ensure_loaded().await;
        let epoch = self.epoch().await;
        let credentials = { self.state.read().await.credentials.clone() };
        let Some(credentials) = credentials else {
            return Ok(self.make_update(None, false).await);
        };
        let gateway = self.gateway(gateway_url).await?;
        self.validate_with(epoch, &gateway, &credentials).await;
        Ok(self.make_update(None, false).await)
    }

    pub(super) async fn validate_with(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        credentials: &Credentials,
    ) {
        match gateway.validate(credentials.access_token()).await {
            Ok(info) => {
                self.apply_validate_info(epoch, gateway, credentials.access_token(), info)
                    .await;
            }
            Err(err) if err.is_forbidden() => {
                // 兼容网关语义：token 过期/被拒时先用 auth_key 续期，再校验一次；
                // 只有续期也不可用或续期后仍被拒才要求重新登录。
                self.handle_validate_forbidden(epoch, gateway).await;
            }
            Err(err) => {
                self.mark_offline_grace(epoch, "网络异常，已按离线宽限继续")
                    .await;
                warn!(error = %err, "/validate 网络失败，进入 offline grace");
            }
        }
    }

    /// `/validate` 成功：合并模型目录并提交账号档案/登录态。
    async fn apply_validate_info(
        &self,
        epoch: u64,
        gateway: &AuthGateway,
        access_token: &str,
        mut info: ValidateInfo,
    ) {
        // `/validate` 已带回模型目录时不再打 `/validate/models`；
        // 没带回时才补一次（失败不阻断 validate）。
        let models = load_models(gateway, access_token, std::mem::take(&mut info.models)).await;
        let committed = self
            .commit_if_current(epoch, |state| {
                if let Some(profile) = state.profile.as_mut() {
                    if let Some(dept) = info.dept.clone() {
                        profile.dept = Some(dept);
                    }
                    if let Some(display_name) = info.display_name.clone() {
                        profile.display_name = Some(display_name);
                    }
                    if let Some(server_url) =
                        info.server_url.clone().or_else(|| info.gateway.clone())
                    {
                        profile.server_url = server_url;
                    }
                }
                state.models = models;
                state.last_validated_at = Some(Utc::now());
                state.credentials_valid = true;
                state.offline_grace = false;
                state.phase = AuthPhase::LoggedIn;
                state.last_error = None;
            })
            .await;
        if committed.is_none() {
            return;
        }
        self.persist_profile_current(epoch, Some(Utc::now())).await;
        info!("/validate 成功");
    }

    /// `/validate` 返回 401/403：先尝试用 auth_key 自动续期，成功后重新校验一次。
    ///
    /// 分支语义（先续期、再判失效）：
    /// - 续期成功 + 重新校验成功 → 正常登录态；
    /// - 续期成功 + 重新校验被拒 → 重新登录；
    /// - 续期成功 + 重新校验网络失败 → 离线宽限（相信刚续到的新凭据）；
    /// - 续期瞬时失败 → 离线宽限（保留旧凭据，等后台重试）；
    /// - 没有 auth_key / 续期被拒 → 重新登录（`renew_now` 已写入状态与文案）。
    async fn handle_validate_forbidden(&self, epoch: u64, gateway: &AuthGateway) {
        warn!("/validate 返回 401/403：先尝试用 auth_key 自动续期");
        match self.renew_now(epoch, gateway).await {
            Some(RenewOutcome::Renewed) => {
                let fresh = { self.state.read().await.credentials.clone() };
                let Some(fresh) = fresh else {
                    return;
                };
                match gateway.validate(fresh.access_token()).await {
                    Ok(info) => {
                        info!("自动续期后 /validate 成功");
                        self.apply_validate_info(epoch, gateway, fresh.access_token(), info)
                            .await;
                    }
                    Err(err) if err.is_forbidden() => {
                        self.mark_reauth(epoch, "续期后仍被服务端拒绝，请重新登录")
                            .await;
                        warn!(error = %err, "续期后 /validate 仍返回 401/403");
                    }
                    Err(err) => {
                        self.mark_offline_grace(epoch, "续期成功但校验网络失败，已按离线宽限继续")
                            .await;
                        warn!(error = %err, "续期后 /validate 网络失败，进入 offline grace");
                    }
                }
            }
            Some(RenewOutcome::NoKey) | Some(RenewOutcome::Rejected) => {
                // renew_now 已把状态置为 reauth_required（缺 key / 服务端拒绝文案不同）。
                warn!("/validate 401/403 且自动续期不可用，需要重新登录");
            }
            Some(RenewOutcome::Transient) | None => {
                self.mark_offline_grace(epoch, "自动续期失败（网络异常），已按离线宽限继续")
                    .await;
            }
        }
    }

    /// 凭据已失效：要求重新登录。
    async fn mark_reauth(&self, epoch: u64, message: &str) {
        self.commit_if_current(epoch, |state| {
            state.phase = AuthPhase::ReauthRequired;
            state.credentials_valid = false;
            state.offline_grace = false;
            state.last_error = Some(message.to_string());
        })
        .await;
    }

    /// 网络类失败：进入离线宽限（界面仍算已登录，但不再刷新校验时间）。
    async fn mark_offline_grace(&self, epoch: u64, message: &str) {
        self.commit_if_current(epoch, |state| {
            state.phase = AuthPhase::OfflineGrace;
            state.offline_grace = true;
            state.last_error = Some(message.to_string());
        })
        .await;
    }
}

pub(super) fn merge_models(
    mut primary: Vec<ModelSummary>,
    extra: Vec<ModelSummary>,
) -> Vec<ModelSummary> {
    for model in extra {
        if !primary.iter().any(|existing| existing.id == model.id) {
            primary.push(model);
        }
    }
    primary
}

/// 取模型目录：`/validate` 已带回目录时直接复用，**不再打** `/validate/models`。
///
/// 登录完成与 10 分钟一次的 validate 共用；只有目录缺失时才补一次二次请求，
/// 失败只降级（目录保持为空），不阻断 validate。
pub(super) async fn load_models(
    gateway: &AuthGateway,
    access_token: &str,
    embedded: Vec<ModelSummary>,
) -> Vec<ModelSummary> {
    if !embedded.is_empty() {
        return embedded;
    }
    match gateway.validate_models(access_token).await {
        Ok(more) => merge_models(embedded, more),
        Err(err) => {
            debug!(error = %err, "拉取模型目录失败，保留空目录");
            embedded
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// 契约：`/validate` 已带回模型目录时，登录/校验不得再打 `/validate/models`；
    /// 只有目录为空时才补一次二次请求。
    #[tokio::test]
    async fn validate_embedded_models_skips_secondary_request() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应可绑定回环端口");
        let addr = listener.local_addr().expect("应可读取监听地址");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0u8; 2048];
                let _ = socket.read(&mut buffer).await;
                let body = r#"{"models":[{"id":"from-secondary"}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");

        let embedded = vec![ModelSummary {
            id: "claude-sonnet-4".to_string(),
            name: None,
        }];
        let models = load_models(&gateway, "test-token", embedded).await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "已有内嵌目录时不得再请求 /validate/models"
        );

        let fetched = load_models(&gateway, "test-token", Vec::new()).await;
        assert_eq!(fetched.len(), 1);
        assert_eq!(fetched[0].id, "from-secondary");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "目录为空时应补一次 /validate/models"
        );

        server.abort();
    }

    /// `/validate` 返回 5xx：不能判为"已校验"，必须降级为 offline grace，
    /// 且不刷新 `last_validated_at`（否则后台循环会以为刚校验过、10 分钟不再重试）。
    #[tokio::test]
    async fn validate_5xx_falls_back_to_offline_grace() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应可绑定回环端口");
        let addr = listener.local_addr().expect("应可读取监听地址");
        let server = tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 2048];
            let _ = socket.read(&mut buffer).await;
            let body = r#"{"error":"gateway down"}"#;
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.shutdown().await;
        });

        let (service, dir) = super::super::test_support::temp_service();
        let epoch = service.epoch().await;
        service
            .commit_if_current(epoch, |state| {
                state.phase = AuthPhase::LoggedIn;
                state.credentials_valid = true;
            })
            .await;
        let gateway = AuthGateway::new(&format!("http://{addr}")).expect("回环网关应可用");
        let credentials = Credentials {
            access_token: "test-token".to_string(),
            auth_key: None,
            user: "tester".to_string(),
            server_url: "https://api.example.test".to_string(),
            issued_at: 1,
            expires_at: 2,
        };

        service.validate_with(epoch, &gateway, &credentials).await;

        let status = service.status().await;
        assert!(status.offline_grace, "5xx 必须降级为 offline grace");
        assert_eq!(status.state, "authenticated", "离线宽限在界面上仍算已登录");
        assert!(
            status.last_validated_at.is_none(),
            "5xx 不能刷新 last_validated_at（否则 10 分钟内不再重试）"
        );
        assert!(
            status
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("离线宽限"),
            "应给出离线宽限提示：{:?}",
            status.message
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 最小可编排网关：按 `responses` 顺序回放（每次连接回一条），用于构造
    /// "validate 403 → 续期 → 再 validate" 这类多步场景。
    async fn scripted_gateway(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应可绑定回环端口");
        let addr = listener.local_addr().expect("应可读取监听地址");
        let server = tokio::spawn(async move {
            for (status, body) in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    500 => "Internal Server Error",
                    _ => "OK",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), server)
    }

    /// 与 [`scripted_gateway`] 相同，但额外统计被连接次数，并给多余请求回 500：
    /// 用于断言"本次操作没有发出预期之外的网络请求"。
    async fn counting_gateway(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("应可绑定回环端口");
        let addr = listener.local_addr().expect("应可读取监听地址");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let server = tokio::spawn(async move {
            let mut pending = responses.into_iter();
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = [0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let (status, body) = pending.next().unwrap_or((500, "{}"));
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    403 => "Forbidden",
                    500 => "Internal Server Error",
                    _ => "OK",
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), hits, server)
    }

    /// 种一份"已登录但 token 已过期"的凭据，模拟运行中 token 失效。
    async fn seed_expired_credentials(
        service: &AuthService,
        auth_key: Option<&str>,
    ) -> Credentials {
        let credentials = Credentials {
            access_token: "old-token".to_string(),
            auth_key: auth_key.map(str::to_string),
            user: "tester".to_string(),
            server_url: "https://api.example.test".to_string(),
            issued_at: 1,
            expires_at: 2,
        };
        let epoch = service.epoch().await;
        let seeded = credentials.clone();
        service
            .commit_if_current(epoch, move |state| {
                state.phase = AuthPhase::LoggedIn;
                state.credentials_valid = true;
                state.credentials = Some(seeded);
            })
            .await;
        credentials
    }

    /// 过期/被拒时先用 auth_key 续期，再重新校验成功。
    #[tokio::test]
    async fn validate_403_renews_with_auth_key_then_succeeds() {
        let (base, server) = scripted_gateway(vec![
            (403, "{}"),
            (200, r#"{"auth_key":"fresh-key"}"#),
            (200, r#"{"token":"new-token","expire":"1794000000"}"#),
            (200, r#"{"display_name":"tester","models":[{"id":"m1"}]}"#),
        ])
        .await;
        let (service, dir) = super::super::test_support::temp_service();
        let credentials = seed_expired_credentials(&service, Some("old-key")).await;
        let epoch = service.epoch().await;
        let gateway = AuthGateway::new(&base).expect("回环网关应可用");

        service.validate_with(epoch, &gateway, &credentials).await;

        let status = service.status().await;
        assert_eq!(status.state, "authenticated");
        assert!(status.credentials_valid);
        assert!(!status.offline_grace);
        assert!(
            status.last_validated_at.is_some(),
            "续期后应重新校验成功并刷新校验时间"
        );
        let snapshot = service
            .credentials_snapshot()
            .await
            .expect("续期后应有可用凭据");
        assert_eq!(
            snapshot.access_token, "new-token",
            "续期拿到的 token 应已替换旧凭据"
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没有 auth_key 可续期时，401/403 仍应要求重新登录，且不发续期请求。
    #[tokio::test]
    async fn validate_403_without_auth_key_requires_reauth() {
        let (base, server) = scripted_gateway(vec![(403, "{}"), (403, "{}")]).await;
        let (service, dir) = super::super::test_support::temp_service();
        let credentials = seed_expired_credentials(&service, None).await;
        let epoch = service.epoch().await;
        let gateway = AuthGateway::new(&base).expect("回环网关应可用");

        service.validate_with(epoch, &gateway, &credentials).await;

        let status = service.status().await;
        assert_eq!(status.state, "reauth_required");
        assert!(!status.credentials_valid);
        assert!(
            status
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("重新登录"),
            "应提示重新登录：{:?}",
            status.message
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 续期遇到网络/5xx 瞬时失败：保留旧凭据、回落离线宽限，而不是立刻判失效。
    #[tokio::test]
    async fn validate_403_renewal_transient_falls_back_to_offline_grace() {
        let (base, server) = scripted_gateway(vec![
            (403, "{}"),
            (200, r#"{"auth_key":"fresh-key"}"#),
            (500, "{}"),
        ])
        .await;
        let (service, dir) = super::super::test_support::temp_service();
        let credentials = seed_expired_credentials(&service, Some("old-key")).await;
        let epoch = service.epoch().await;
        let gateway = AuthGateway::new(&base).expect("回环网关应可用");

        service.validate_with(epoch, &gateway, &credentials).await;

        let status = service.status().await;
        assert_eq!(status.state, "authenticated", "离线宽限在界面上仍算已登录");
        assert!(status.offline_grace);
        assert!(
            status
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("离线宽限"),
            "应说明已回落离线宽限：{:?}",
            status.message
        );

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动"重新检测/续期"：强制续期成功后补一次 validate，刷新账号档案。
    #[tokio::test]
    async fn retry_renewal_forces_renewal_and_refreshes_validate() {
        let (base, server) = scripted_gateway(vec![
            (200, r#"{"auth_key":"fresh-key"}"#),
            (200, r#"{"token":"new-token","expire":"1794000000"}"#),
            (200, r#"{"display_name":"tester","models":[{"id":"m1"}]}"#),
        ])
        .await;
        let (service, dir) = super::super::test_support::temp_service();
        let _ = seed_expired_credentials(&service, Some("old-key")).await;

        service
            .retry_renewal(&base)
            .await
            .expect("手动续期不应返回错误");

        let status = service.status().await;
        assert_eq!(status.state, "authenticated");
        assert!(status.credentials_valid);
        assert!(
            status.last_validated_at.is_some(),
            "续期成功后应补做 /validate 并刷新校验时间"
        );
        let snapshot = service
            .credentials_snapshot()
            .await
            .expect("续期后应有可用凭据");
        assert_eq!(snapshot.access_token, "new-token");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 同一个 tick 内 `/validate` 已强制续期后，`renew_if_due` 不能再拿旧凭据续一次。
    #[tokio::test]
    async fn renew_if_due_does_not_repeat_renewal_after_forced_one() {
        let (base, hits, server) = counting_gateway(vec![
            (200, r#"{"auth_key":"fresh-key"}"#),
            (200, r#"{"token":"new-token","expire":"1794000000"}"#),
        ])
        .await;
        let (service, dir) = super::super::test_support::temp_service();
        let stale_credentials = seed_expired_credentials(&service, Some("old-key")).await;
        let epoch = service.epoch().await;
        let gateway = AuthGateway::new(&base).expect("回环网关应可用");

        assert_eq!(
            service.renew_now(epoch, &gateway).await,
            Some(RenewOutcome::Renewed)
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "强制续期应只发取 key + 续期两次请求"
        );

        // 模拟 loops::tick 的后续调用：传入 tick 开始时的旧凭据快照。
        let acted = service
            .renew_if_due(epoch, &gateway, &stale_credentials, Utc::now())
            .await;
        assert!(!acted, "内存凭据已刷新，不应再次判定到期");
        assert_eq!(hits.load(Ordering::SeqCst), 2, "同一 tick 不得重复续期");

        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
