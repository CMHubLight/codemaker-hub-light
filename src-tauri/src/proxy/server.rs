//! 代理服务生命周期：绑定 127.0.0.1:15721（占用时 15722..=15800 回退，
//! 再失败交给系统分配），后台运行 Axum 代理服务。
//!
//! 关键事实（docs/proxy.md）：
//! - 只允许回环绑定；
//! - 实际端口写 `~/.codemaker-hub-light/proxy.port`；
//! - 路由与转发见 `routes.rs` / `forward.rs`，`/health`、`/status` 为诊断端点。

use std::{
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use tokio::{
    net::TcpListener,
    sync::{oneshot, Mutex},
    task::JoinHandle,
};
use tracing::{info, warn};

use crate::{
    auth::service::AuthService,
    db::Db,
    proxy::{
        credentials::{AuthServiceCredentials, CredentialProvider},
        policy::PolicyCell,
        routes::{self, RoutesState},
    },
    settings,
};

/// 端口回退上限（来自设置模块，保持单一口径）。
pub const PORT_FALLBACK_END: u16 = settings::PROXY_PORT_FALLBACK_END;
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ProxyError {
    #[error("代理只允许绑定回环地址，当前配置为 {0}")]
    NonLoopback(String),
    #[error("无法绑定 {bind}:{port}（{message}）")]
    Bind {
        bind: String,
        port: u16,
        message: String,
    },
}

/// 代理状态（返回给前端与 `/status`）。
///
/// 字段与前端 `ProxyStatus` 对齐（`host` / `base_url` / `gateway_url` / `last_error`），
/// 其余为诊断附加字段。
#[derive(Debug, Clone, Serialize)]
pub struct ProxyStatus {
    pub running: bool,
    pub host: String,
    pub port: u16,
    /// 客户端应使用的 Base URL，例如 `http://127.0.0.1:15721`。
    pub base_url: String,
    pub gateway_url: String,
    pub last_error: Option<String>,
    // ---- 诊断附加字段 ----
    pub started_at: Option<String>,
    pub uptime_secs: u64,
    pub request_count: u64,
    pub error_count: u64,
    pub port_file: String,
    pub health_url: String,
}

/// 运行中代理的共享信息（routes 与状态查询共用）。
#[derive(Debug)]
pub struct RuntimeInfo {
    pub bind: String,
    pub port: u16,
    pub gateway_url: String,
    pub started_at: Instant,
    pub started_at_utc: DateTime<Utc>,
    pub requests: AtomicU64,
    pub errors: AtomicU64,
    pub stopped: AtomicBool,
}

impl RuntimeInfo {
    fn new(bind: String, port: u16, gateway_url: String) -> Self {
        Self {
            bind,
            port,
            gateway_url,
            started_at: Instant::now(),
            started_at_utc: Utc::now(),
            requests: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
        }
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn is_running(&self) -> bool {
        !self.stopped.load(Ordering::SeqCst)
    }

    pub fn note_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn note_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// `/status` 的 JSON 投影。
    pub fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({
            "status": if self.is_running() { "running" } else { "stopped" },
            "mode": "proxy",
            "bind": self.bind.clone(),
            "port": self.port,
            "gatewayUrl": self.gateway_url.clone(),
            "startedAt": self.started_at_utc.to_rfc3339_opts(SecondsFormat::Secs, true),
            "uptimeSecs": self.uptime_secs(),
            "requestCount": self.requests.load(Ordering::Relaxed),
            "errorCount": self.errors.load(Ordering::Relaxed),
        })
    }
}

#[derive(Default)]
struct ProxyRuntime {
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
    info: Option<Arc<RuntimeInfo>>,
    last_error: Option<String>,
}

/// 代理服务器句柄（可跨 Tauri 命令共享）。
pub struct ProxyServer {
    data_dir: PathBuf,
    db: Db,
    provider: Arc<dyn CredentialProvider>,
    runtime: Mutex<ProxyRuntime>,
}

impl ProxyServer {
    /// 生产构造：凭据来自 `AuthService` 内存态。
    pub fn new(data_dir: PathBuf, db: Db, auth: Arc<AuthService>) -> Self {
        Self {
            data_dir,
            db,
            provider: Arc::new(AuthServiceCredentials::new(auth)),
            runtime: Mutex::new(ProxyRuntime::default()),
        }
    }

    /// 测试构造：注入 fake `CredentialProvider`，不触碰 keyring/网络。
    pub fn with_provider(data_dir: PathBuf, db: Db, provider: Arc<dyn CredentialProvider>) -> Self {
        Self {
            data_dir,
            db,
            provider,
            runtime: Mutex::new(ProxyRuntime::default()),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// `proxy.port` 文件路径。
    pub fn port_file(&self) -> PathBuf {
        settings::proxy_port_path(&self.data_dir)
    }

    /// 启动代理；已运行时直接返回当前状态。
    pub async fn start(
        &self,
        bind: &str,
        preferred_port: u16,
        gateway_url: &str,
        policy: Arc<PolicyCell>,
    ) -> Result<ProxyStatus, ProxyError> {
        let mut runtime = self.runtime.lock().await;
        if let Some(info) = runtime.info.clone() {
            return Ok(self.status_from(Some(info), &runtime));
        }
        match self
            .start_locked(&mut runtime, bind, preferred_port, gateway_url, policy)
            .await
        {
            Ok(()) => {
                let info = runtime.info.clone();
                Ok(self.status_from(info, &runtime))
            }
            Err(err) => {
                runtime.last_error = Some(err.to_string());
                Err(err)
            }
        }
    }

    /// 停止代理（幂等）。
    pub async fn stop(&self) {
        let mut runtime = self.runtime.lock().await;
        Self::stop_locked(&mut runtime).await;
    }

    /// 重启代理：先停止旧任务，再用当前设置重新绑定。
    pub async fn restart(
        &self,
        bind: &str,
        preferred_port: u16,
        gateway_url: &str,
        policy: Arc<PolicyCell>,
    ) -> Result<ProxyStatus, ProxyError> {
        let mut runtime = self.runtime.lock().await;
        Self::stop_locked(&mut runtime).await;
        match self
            .start_locked(&mut runtime, bind, preferred_port, gateway_url, policy)
            .await
        {
            Ok(()) => {
                let info = runtime.info.clone();
                Ok(self.status_from(info, &runtime))
            }
            Err(err) => {
                runtime.last_error = Some(err.to_string());
                Err(err)
            }
        }
    }

    /// 查询状态；未运行时使用设置里的期望配置填充展示字段。
    pub async fn status(
        &self,
        fallback_bind: &str,
        fallback_port: u16,
        fallback_gateway: &str,
    ) -> ProxyStatus {
        let runtime = self.runtime.lock().await;
        if let Some(info) = runtime.info.clone() {
            return self.status_from(Some(info), &runtime);
        }
        let last_error = runtime.last_error.clone();
        drop(runtime);
        let base_url = format!("http://{fallback_bind}:{fallback_port}");
        ProxyStatus {
            running: false,
            host: fallback_bind.to_string(),
            port: fallback_port,
            base_url: base_url.clone(),
            gateway_url: fallback_gateway.to_string(),
            last_error,
            started_at: None,
            uptime_secs: 0,
            request_count: 0,
            error_count: 0,
            port_file: self.port_file().to_string_lossy().into_owned(),
            health_url: format!("{base_url}/health"),
        }
    }

    async fn start_locked(
        &self,
        runtime: &mut ProxyRuntime,
        bind: &str,
        preferred_port: u16,
        gateway_url: &str,
        policy: Arc<PolicyCell>,
    ) -> Result<(), ProxyError> {
        let ip: IpAddr = bind.parse().map_err(|_| ProxyError::Bind {
            bind: bind.to_string(),
            port: preferred_port,
            message: "监听地址不是合法 IP".to_string(),
        })?;
        if !ip.is_loopback() {
            return Err(ProxyError::NonLoopback(bind.to_string()));
        }

        let listener = bind_listener(ip, bind, preferred_port).await?;
        let local_addr = listener.local_addr().map_err(|err| ProxyError::Bind {
            bind: bind.to_string(),
            port: preferred_port,
            message: err.to_string(),
        })?;

        let info = Arc::new(RuntimeInfo::new(
            bind.to_string(),
            local_addr.port(),
            gateway_url.to_string(),
        ));
        let router = routes::router(RoutesState::new(
            info.clone(),
            self.db.clone(),
            self.provider.clone(),
            policy,
        ));
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let task_info = info.clone();
        let task = tokio::spawn(async move {
            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            });
            if let Err(err) = server.await {
                warn!(error = %err, "代理服务异常退出");
                task_info.note_error();
            }
            task_info.stopped.store(true, Ordering::SeqCst);
        });

        if let Err(err) = settings::write_proxy_port(&self.data_dir, local_addr.port()) {
            warn!(error = %err, "写入 proxy.port 失败");
        }

        info!(
            bind = %bind,
            port = local_addr.port(),
            preferred_port,
            "本地代理已启动（三协议转发）"
        );

        runtime.shutdown = Some(shutdown_tx);
        runtime.task = Some(task);
        runtime.info = Some(info);
        runtime.last_error = None;
        Ok(())
    }

    async fn stop_locked(runtime: &mut ProxyRuntime) {
        if let Some(shutdown) = runtime.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(task) = runtime.task.take() {
            match tokio::time::timeout(STOP_TIMEOUT, task).await {
                Ok(Ok(())) => {}
                Ok(Err(err)) if !err.is_cancelled() => {
                    warn!(error = %err, "代理任务异常结束");
                }
                Ok(Err(_)) => {}
                Err(_) => warn!("等待代理任务退出超时"),
            }
        }
        runtime.info = None;
    }

    fn status_from(&self, info: Option<Arc<RuntimeInfo>>, runtime: &ProxyRuntime) -> ProxyStatus {
        let port_file = self.port_file().to_string_lossy().into_owned();
        match info {
            Some(info) => {
                let base_url = format!("http://{}:{}", info.bind, info.port);
                ProxyStatus {
                    running: info.is_running(),
                    host: info.bind.clone(),
                    port: info.port,
                    base_url: base_url.clone(),
                    gateway_url: info.gateway_url.clone(),
                    last_error: runtime.last_error.clone(),
                    started_at: Some(
                        info.started_at_utc
                            .to_rfc3339_opts(SecondsFormat::Secs, true),
                    ),
                    uptime_secs: info.uptime_secs(),
                    request_count: info.requests.load(Ordering::Relaxed),
                    error_count: info.errors.load(Ordering::Relaxed),
                    health_url: format!("{base_url}/health"),
                    port_file,
                }
            }
            None => {
                let base_url = format!("http://127.0.0.1:{}", settings::DEFAULT_PROXY_PORT);
                ProxyStatus {
                    running: false,
                    host: "127.0.0.1".to_string(),
                    port: settings::DEFAULT_PROXY_PORT,
                    base_url: base_url.clone(),
                    gateway_url: settings::DEFAULT_GATEWAY_URL.to_string(),
                    last_error: runtime.last_error.clone(),
                    started_at: None,
                    uptime_secs: 0,
                    request_count: 0,
                    error_count: 0,
                    port_file,
                    health_url: format!("{base_url}/health"),
                }
            }
        }
    }
}

/// 依次尝试 preferred..=15800，最后交给系统分配（port 0）。
async fn bind_listener(
    ip: IpAddr,
    bind: &str,
    preferred_port: u16,
) -> Result<TcpListener, ProxyError> {
    let mut candidates: Vec<u16> = if preferred_port <= PORT_FALLBACK_END {
        (preferred_port..=PORT_FALLBACK_END).collect()
    } else {
        vec![preferred_port]
    };
    candidates.push(0);

    let mut last_message = String::from("没有可用端口");
    for port in candidates {
        match TcpListener::bind(SocketAddr::new(ip, port)).await {
            Ok(listener) => {
                if port != preferred_port {
                    warn!(
                        bind = %bind,
                        actual = listener.local_addr().map(|a| a.port()).unwrap_or(port),
                        preferred = preferred_port,
                        "首选端口不可用，已回退"
                    );
                }
                return Ok(listener);
            }
            Err(err) => last_message = err.to_string(),
        }
    }

    Err(ProxyError::Bind {
        bind: bind.to_string(),
        port: preferred_port,
        message: last_message,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{CredentialsSnapshot, ModelSummary},
        proxy::credentials::{CredentialsFuture, ModelsFuture},
    };

    struct NoopProvider;

    impl CredentialProvider for NoopProvider {
        fn credentials(&self) -> CredentialsFuture<'_> {
            Box::pin(async { Option::<CredentialsSnapshot>::None })
        }

        fn models(&self) -> ModelsFuture<'_> {
            Box::pin(async { Vec::<ModelSummary>::new() })
        }
    }

    fn test_server(tag: &str) -> ProxyServer {
        let data_dir =
            std::env::temp_dir().join(format!("chl-proxy-{tag}-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&data_dir);
        let db = Db::open(&data_dir.join("test.db")).expect("数据库应可打开");
        ProxyServer::with_provider(data_dir, db, Arc::new(NoopProvider))
    }

    #[tokio::test]
    async fn status_reports_stopped_with_configured_values() {
        let server = test_server("status");
        let status = server
            .status("127.0.0.1", 15721, "https://example.test")
            .await;
        assert!(!status.running);
        assert_eq!(status.port, 15721);
        assert!(status.health_url.ends_with("/health"));
    }

    #[tokio::test]
    async fn rejects_non_loopback_bind() {
        let server = test_server("bind");
        let err = server
            .start(
                "0.0.0.0",
                15721,
                "https://example.test",
                Arc::new(PolicyCell::new(Default::default())),
            )
            .await
            .expect_err("非回环地址必须拒绝");
        assert!(matches!(err, ProxyError::NonLoopback(_)));
    }
}
