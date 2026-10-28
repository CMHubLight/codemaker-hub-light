//! 代理依赖的凭据/模型抽象。
//!
//! 生产实现读取 [`crate::auth::service::AuthService`] 的内存态；测试注入 fake，
//! 不需要真实 keyring/网络。凭据快照本身不实现 `Serialize`，也不允许写日志。

use std::{future::Future, pin::Pin, sync::Arc};

use crate::auth::{service::AuthService, CredentialsSnapshot, ModelSummary};

pub type CredentialsFuture<'a> =
    Pin<Box<dyn Future<Output = Option<CredentialsSnapshot>> + Send + 'a>>;
pub type ModelsFuture<'a> = Pin<Box<dyn Future<Output = Vec<ModelSummary>> + Send + 'a>>;

/// 代理获取下游凭据与模型目录的抽象（便于测试替换）。
pub trait CredentialProvider: Send + Sync {
    /// 无凭据/需要重新登录时返回 `None`。
    fn credentials(&self) -> CredentialsFuture<'_>;
    /// `/v1/models` 使用的模型目录；空列表表示“尚未缓存”。
    fn models(&self) -> ModelsFuture<'_>;
}

/// 生产实现：透传 `AuthService` 的内存态。
pub struct AuthServiceCredentials {
    service: Arc<AuthService>,
}

impl AuthServiceCredentials {
    pub fn new(service: Arc<AuthService>) -> Self {
        Self { service }
    }
}

impl CredentialProvider for AuthServiceCredentials {
    fn credentials(&self) -> CredentialsFuture<'_> {
        let service = self.service.clone();
        Box::pin(async move { service.credentials_snapshot().await })
    }

    fn models(&self) -> ModelsFuture<'_> {
        let service = self.service.clone();
        Box::pin(async move { service.models_snapshot().await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        credentials: Option<CredentialsSnapshot>,
        models: Vec<ModelSummary>,
    }

    impl CredentialProvider for FakeProvider {
        fn credentials(&self) -> CredentialsFuture<'_> {
            let credentials = self.credentials.clone();
            Box::pin(async move { credentials })
        }

        fn models(&self) -> ModelsFuture<'_> {
            let models = self.models.clone();
            Box::pin(async move { models })
        }
    }

    #[tokio::test]
    async fn fake_provider_is_usable_without_auth_service() {
        let provider = FakeProvider {
            credentials: Some(CredentialsSnapshot {
                access_token: "test-token".to_string(),
                user: "test-user".to_string(),
                gateway_url: "https://api.example.test".to_string(),
            }),
            models: vec![ModelSummary {
                id: "claude-sonnet-4".to_string(),
                name: None,
            }],
        };
        let credentials = provider.credentials().await.expect("应有凭据");
        assert_eq!(credentials.user, "test-user");
        assert!(!format!("{credentials:?}").contains("test-token"));
        assert_eq!(provider.models().await.len(), 1);
    }
}
