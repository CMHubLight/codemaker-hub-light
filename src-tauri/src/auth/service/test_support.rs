//! 认证层测试共享夹具（仅在 `cfg(test)` 下编译）。

use super::*;
use crate::{
    auth::store::{CredentialStore, StaticKey},
    db,
};
use std::fs;

pub(super) fn temp_service() -> (AuthService, PathBuf) {
    let dir = std::env::temp_dir().join(format!("chl-auth-svc-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).expect("临时目录应可创建");
    let db = Db::open_in_memory().expect("内存库应可打开");
    db::run_migrations(&db).expect("迁移应成功");
    let store = CredentialStore::with_key_source(&dir, Box::new(StaticKey([9u8; 32])));
    (AuthService::with_store(db, store), dir)
}

pub(super) fn deep_link_payload(poll_uuid: Option<&str>, token: Option<&str>) -> DeepLinkPayload {
    DeepLinkPayload {
        poll_uuid: poll_uuid.map(str::to_string),
        access_token: token.map(str::to_string),
        auth_key: Some("auth-key".to_string()),
        user: Some("tester".to_string()),
    }
}

pub(super) fn state_with_pending(started_at: DateTime<Utc>) -> RuntimeState {
    RuntimeState {
        pending: Some(PendingLogin {
            poll_uuid: uuid::Uuid::nil(),
            login_url: String::new(),
            redirect_uri: String::new(),
            started_at,
            consecutive_network_errors: 0,
            deep_link_received: false,
            credentials_consumed: false,
        }),
        ..RuntimeState::default()
    }
}

pub(super) fn pending_uuid(state: &RuntimeState) -> String {
    state
        .pending
        .as_ref()
        .expect("应有 pending 登录")
        .poll_uuid
        .to_string()
}
