//! 被忽略的代理开销基准（Phase 5 性能证据）。
//!
//! 只**打印**耗时，不对时间做任何断言（避免 CI 抖动）。单独运行：
//!
//! ```text
//! cargo test --manifest-path src-tauri/Cargo.toml --lib proxy::perf_tests -- --ignored --nocapture
//! ```
//!
//! 口径：同一个 reqwest client、串行请求、预热 3 次后测 30 次（`record_body` 组 10 次），
//! 每次读完响应体再停表；上游是零延迟的本地 mock（固定小 JSON）。

use std::{path::PathBuf, sync::Arc, time::Instant};

use axum::{routing::post, Json, Router};
use tokio::net::TcpListener;

use crate::{
    auth::{CredentialsSnapshot, ModelSummary},
    db::{self, Db},
    proxy::{
        credentials::{CredentialProvider, CredentialsFuture, ModelsFuture},
        policy::{PolicyCell, ProxyPolicy},
        server::ProxyServer,
    },
};

/// 预热次数（两组相同）。
const WARMUP: usize = 3;
/// 默认策略的测量次数。
const ITERATIONS: usize = 30;
/// 开启「记录正文」后的测量次数（代价更高，样本少一些）。
const RECORD_BODY_ITERATIONS: usize = 10;

const MODEL: &str = "perf-model";

struct FixedProvider {
    credentials: Option<CredentialsSnapshot>,
}

impl CredentialProvider for FixedProvider {
    fn credentials(&self) -> CredentialsFuture<'_> {
        let credentials = self.credentials.clone();
        Box::pin(async move { credentials })
    }

    fn models(&self) -> ModelsFuture<'_> {
        Box::pin(async { Vec::<ModelSummary>::new() })
    }
}

/// 零延迟 mock 上游：固定小 JSON，非流式。
async fn start_mock_upstream() -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new().route(
        "/openai/v1/chat/completions",
        post(|| async {
            Json(serde_json::json!({
                "id": "chatcmpl-perf",
                "object": "chat.completion",
                "model": MODEL,
                "choices": [{
                    "index": 0,
                    "message": { "role": "assistant", "content": "ok" },
                    "finish_reason": "stop"
                }],
                "usage": { "prompt_tokens": 16, "completion_tokens": 2 }
            }))
        }),
    );
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock 上游应可绑定");
    let address = listener.local_addr().expect("应取到 mock 地址");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), handle)
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("应可绑定临时端口");
    listener.local_addr().expect("应取到端口").port()
}

struct BenchProxy {
    server: ProxyServer,
    base_url: String,
    data_dir: PathBuf,
}

impl BenchProxy {
    async fn shutdown(self) {
        self.server.stop().await;
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

async fn start_proxy(upstream_url: &str, policy: ProxyPolicy) -> BenchProxy {
    let data_dir = std::env::temp_dir().join(format!("chl-proxy-perf-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("临时目录应可创建");
    let db = Db::open(&data_dir.join("proxy.db")).expect("数据库应可打开");
    db::run_migrations(&db).expect("迁移应成功");

    let provider = Arc::new(FixedProvider {
        credentials: Some(CredentialsSnapshot {
            access_token: "perf-token".to_string(),
            user: "perf-user".to_string(),
            // 空网关：使用 `start` 传入的 mock 上游地址。
            gateway_url: String::new(),
        }),
    });
    let server = ProxyServer::with_provider(data_dir.clone(), db, provider);
    let status = server
        .start(
            "127.0.0.1",
            free_port().await,
            upstream_url,
            Arc::new(PolicyCell::new(policy)),
        )
        .await
        .expect("代理应可启动");
    BenchProxy {
        server,
        base_url: status.base_url,
        data_dir,
    }
}

/// 串行测 `iterations` 次完整往返（含读完响应体），返回毫秒样本。
async fn measure(client: &reqwest::Client, url: &str, iterations: usize) -> Vec<f64> {
    let body = serde_json::json!({
        "model": MODEL,
        "messages": [{ "role": "user", "content": "ping" }]
    });
    for _ in 0..WARMUP {
        let response = client
            .post(url)
            .json(&body)
            .send()
            .await
            .expect("预热请求应完成");
        let _ = response.bytes().await.expect("预热响应体应可读");
    }

    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let started = Instant::now();
        let response = client
            .post(url)
            .json(&body)
            .send()
            .await
            .expect("请求应完成");
        assert_eq!(response.status(), 200, "基准请求不应失败");
        let bytes = response.bytes().await.expect("响应体应可读");
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        assert!(!bytes.is_empty(), "响应体不应为空");
        samples.push(elapsed_ms);
    }
    samples
}

/// `(p50, p95, mean)`，单位毫秒。
fn stats(samples: &mut [f64]) -> (f64, f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let percentile = |q: f64| {
        let index = ((samples.len() - 1) as f64 * q).round() as usize;
        samples[index]
    };
    (percentile(0.5), percentile(0.95), mean)
}

fn report(label: &str, samples: &mut [f64]) -> (f64, f64, f64) {
    let (p50, p95, mean) = stats(samples);
    println!("{label:<34} p50 {p50:8.2} ms   p95 {p95:8.2} ms   mean {mean:8.2} ms");
    (p50, p95, mean)
}

#[tokio::test]
#[ignore = "性能基准：手动运行 cargo test --lib proxy::perf_tests -- --ignored --nocapture"]
async fn proxy_overhead_benchmark() {
    let (upstream_url, upstream_handle) = start_mock_upstream().await;
    let upstream_endpoint = format!("{upstream_url}/openai/v1/chat/completions");

    let proxy = start_proxy(&upstream_url, ProxyPolicy::default()).await;
    let proxy_endpoint = format!("{}/v1/chat/completions", proxy.base_url);

    // 同一个 client 打两条路径，避免连接池差异污染对比。
    let client = reqwest::Client::builder().build().expect("client 应可构建");

    println!(
        "== 代理开销基准（{}，{} 次/组，预热 {} 次）==",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        ITERATIONS,
        WARMUP
    );

    let mut direct = measure(&client, &upstream_endpoint, ITERATIONS).await;
    let mut via_proxy = measure(&client, &proxy_endpoint, ITERATIONS).await;
    let (_, _, direct_mean) = report("直连上游（基线）", &mut direct);
    let (_, _, proxy_mean) = report("经本地代理（默认策略）", &mut via_proxy);
    let delta = proxy_mean - direct_mean;
    println!(
        "→ 代理净开销 mean {:+.2} ms（{:+.1}%），含录库、鉴权注入、头清洗、响应体中继",
        delta,
        delta / direct_mean * 100.0
    );

    // 可选对比组：开启「记录请求/响应正文」（更多拷贝与 DB 列写入）。
    let record_proxy = start_proxy(
        &upstream_url,
        ProxyPolicy {
            record_body: true,
            ..ProxyPolicy::default()
        },
    )
    .await;
    let record_endpoint = format!("{}/v1/chat/completions", record_proxy.base_url);
    let mut recorded = measure(&client, &record_endpoint, RECORD_BODY_ITERATIONS).await;
    let (_, _, recorded_mean) = report("经本地代理（record_body=true）", &mut recorded);
    println!("→ 相对默认策略 mean {:+.2} ms", recorded_mean - proxy_mean);

    record_proxy.shutdown().await;
    proxy.shutdown().await;
    upstream_handle.abort();
}
