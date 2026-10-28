//! `/validate` 刷新与模型目录获取。

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
            Ok(mut info) => {
                // `/validate` 已带回模型目录时不再打 `/validate/models`；
                // 没带回时才补一次（失败不阻断 validate）。
                let models = load_models(
                    gateway,
                    credentials.access_token(),
                    std::mem::take(&mut info.models),
                )
                .await;
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
            Err(err) if err.is_forbidden() => {
                self.commit_if_current(epoch, |state| {
                    state.phase = AuthPhase::ReauthRequired;
                    state.credentials_valid = false;
                    state.offline_grace = false;
                    state.last_error = Some("登录状态已失效，请重新登录".to_string());
                })
                .await;
                warn!("/validate 返回 401/403：凭据已失效");
            }
            Err(err) => {
                self.commit_if_current(epoch, |state| {
                    state.phase = AuthPhase::OfflineGrace;
                    state.offline_grace = true;
                    state.last_error = Some("网络异常，已按离线宽限继续".to_string());
                })
                .await;
                warn!(error = %err, "/validate 网络失败，进入 offline grace");
            }
        }
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
}
