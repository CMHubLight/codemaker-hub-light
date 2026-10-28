//! P2 代理端到端测试：mock upstream（临时端口）+ fake CredentialProvider + 临时 DB。
//!
//! 不访问真实 keyring/网络；所有 listener 都绑定 127.0.0.1:0。

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Uri},
    response::{IntoResponse, Response},
    routing::{any, post},
    Router,
};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;

use crate::{
    auth::{CredentialsSnapshot, ModelSummary},
    db::{self, Db},
    proxy::{
        credentials::{CredentialProvider, CredentialsFuture, ModelsFuture},
        policy::{PolicyCell, ProxyPolicy},
        server::ProxyServer,
    },
};

// ---------------------------------------------------------------------------
// fake provider
// ---------------------------------------------------------------------------

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

fn provider_with_credentials() -> Arc<dyn CredentialProvider> {
    Arc::new(FakeProvider {
        credentials: Some(CredentialsSnapshot {
            access_token: "test-access-token".to_string(),
            user: "test-user".to_string(),
            gateway_url: String::new(), // 走 ProxyServer 配置的 gateway_url（mock upstream）
        }),
        models: vec![
            ModelSummary {
                id: "claude-sonnet-4".to_string(),
                name: Some("Claude Sonnet 4".to_string()),
            },
            ModelSummary {
                id: "gpt-4o".to_string(),
                name: None,
            },
        ],
    })
}

fn provider_without_credentials() -> Arc<dyn CredentialProvider> {
    Arc::new(FakeProvider {
        credentials: None,
        models: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// mock upstream
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct CapturedRequest {
    path: String,
    headers: HeaderMap,
    body: String,
}

#[derive(Clone, Default)]
struct Capture {
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

impl Capture {
    fn push(&self, uri: &Uri, headers: HeaderMap, body: &Bytes) {
        self.requests.lock().unwrap().push(CapturedRequest {
            path: uri.path().to_string(),
            headers,
            body: String::from_utf8_lossy(body).into_owned(),
        });
    }

    fn last(&self) -> CapturedRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("upstream 应已收到请求")
    }
}

fn openai_sse_chunks() -> Vec<&'static str> {
    vec![
        "data: {\"id\":\"chatcmpl-sse\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-sse\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n",
        "data: {\"id\":\"chatcmpl-sse\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":42,\"prompt_tokens_details\":{\"cached_tokens\":10}},\"aigw\":{\"cost\":{\"total_credit\":\"2.843328\"},\"trace_id\":\"trace-sse\"}}\n\n",
        "data: [DONE]\n\n",
    ]
}

async fn mock_messages(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    let stream_requested = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    if stream_requested {
        let body = openai_sse_chunks().concat();
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("connection", "keep-alive")
            .body(Body::from(body))
            .expect("SSE 响应应可构造");
    }
    json_response(serde_json::json!({
        "id": "msg_1",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_input_tokens": 10,
            "cache_creation_input_tokens": 5
        },
        "aigw": { "cost": { "total_credit": 100.0 }, "trace_id": "trace-json" }
    }))
}

async fn mock_chat(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    let stream_requested = serde_json::from_slice::<Value>(&body)
        .ok()
        .and_then(|value| value.get("stream").and_then(Value::as_bool))
        .unwrap_or(false);
    if stream_requested {
        return Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("connection", "keep-alive")
            .body(Body::from(openai_sse_chunks().concat()))
            .expect("OpenAI SSE 响应应可构造");
    }
    json_response(serde_json::json!({
        "id": "chat_1",
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "hello" },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 10,
            "prompt_tokens_details": { "cached_tokens": 50 }
        },
        "aigw": { "cost": { "total_credit": "1.5" }, "trace_id": "trace-chat" }
    }))
}

async fn mock_responses(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    (
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        axum::Json(serde_json::json!({
            "error": { "message": "upstream boom", "type": "server_error" }
        })),
    )
        .into_response()
}

async fn mock_slow_sse() -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _ = tx
            .send(Ok(Bytes::from(openai_sse_chunks()[0].as_bytes().to_vec())))
            .await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        let _ = tx.send(Ok(Bytes::from_static(b"data: [DONE]\n\n"))).await;
    });
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("慢速 SSE 响应应可构造")
}

/// 非流式但响应体远超测试用 `max_buffer_bytes`（触发翻译缓冲溢出分支）。
async fn mock_big_chat(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    json_response(serde_json::json!({
        "id": "chat_big",
        "model": "deepseek-v4-flash",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": "y".repeat(4096) },
            "finish_reason": "stop"
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 },
        "aigw": { "cost": { "total_credit": "1.0" }, "trace_id": "trace-big" }
    }))
}

/// 回显 `x-hub-request-id` 的上游：钉住响应头 append 语义（同名多值不覆盖）。
async fn mock_echo_request_id(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    let mut response = json_response(serde_json::json!({
        "id": "chat_echo",
        "model": "gpt-4o",
        "choices": [{ "index": 0, "message": { "role": "assistant", "content": "ok" }, "finish_reason": "stop" }],
        "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
    }));
    response.headers_mut().append(
        "x-hub-request-id",
        axum::http::HeaderValue::from_static("upstream-echo"),
    );
    response
}

/// 发一帧后停 150ms 再断开的上游流：用于钉住“异常结束当前会发什么事件”。
///
/// 150ms 保证这一帧一定已经到达代理（纯回环），避免 RST 把接收缓冲一起丢掉。
async fn mock_broken_sse() -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _ = tx
            .send(Ok(Bytes::from_static(
                b"data: {\"id\":\"chatcmpl-broken\",\"model\":\"deepseek-v4-flash\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n",
            )))
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let _ = tx
            .send(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "upstream reset",
            )))
            .await;
    });
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("断开流响应应可构造")
}

/// 还没发出任何帧就直接断开：钉住“空流 / 首帧前断开”的 error 事件行为。
async fn mock_empty_broken_sse() -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    tokio::spawn(async move {
        let _ = tx
            .send(Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "upstream reset",
            )))
            .await;
    });
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .expect("空断开流响应应可构造")
}

fn json_response(value: Value) -> Response {
    (axum::http::StatusCode::OK, axum::Json(value)).into_response()
}

async fn start_upstream() -> (String, Capture, tokio::task::JoinHandle<()>) {
    let capture = Capture::default();
    let router = Router::new()
        .route("/anthropic/v1/messages", post(mock_messages))
        .route("/openai/v1/chat/completions", post(mock_chat))
        .route("/openai/v1/responses", post(mock_responses))
        .route("/slow/openai/v1/chat/completions", post(mock_slow_sse))
        .route("/big/openai/v1/chat/completions", post(mock_big_chat))
        .route(
            "/echo-id/openai/v1/chat/completions",
            post(mock_echo_request_id),
        )
        .route("/broken/openai/v1/chat/completions", post(mock_broken_sse))
        .route(
            "/empty-broken/openai/v1/chat/completions",
            post(mock_empty_broken_sse),
        )
        .with_state(capture.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("mock upstream 应可绑定");
    let address = listener.local_addr().expect("应取到地址");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), capture, handle)
}

/// 只回 302 的假上游：用于验证代理不会跟随重定向（避免带凭据跳到别的主机）。
async fn start_redirect_upstream(location: String) -> (String, tokio::task::JoinHandle<()>) {
    let router = Router::new().route(
        "/redirect/openai/v1/chat/completions",
        post(move || {
            let location = location.clone();
            async move {
                Response::builder()
                    .status(302)
                    .header("location", location)
                    .body(Body::from(""))
                    .expect("302 响应应可构造")
            }
        }),
    );
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("重定向 mock 应可绑定");
    let address = listener.local_addr().expect("应取到地址");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), handle)
}

/// 哨兵服务：如果代理错误地跟随了重定向，它会收到请求。
async fn start_sentinel() -> (String, Capture, tokio::task::JoinHandle<()>) {
    let capture = Capture::default();
    let router = Router::new()
        // 任意方法都捕获：302 后 reqwest 会把 POST 改成 GET，只接 POST 会漏记。
        .fallback(any(mock_leak_sink))
        .with_state(capture.clone());
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("哨兵应可绑定");
    let address = listener.local_addr().expect("应取到地址");
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{address}"), capture, handle)
}

async fn mock_leak_sink(
    State(capture): State<Capture>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    capture.push(&uri, headers, &body);
    json_response(serde_json::json!({ "id": "sentinel" }))
}

// ---------------------------------------------------------------------------
// proxy harness
// ---------------------------------------------------------------------------

struct TestProxy {
    server: ProxyServer,
    base_url: String,
    db: Db,
    data_dir: PathBuf,
}

async fn free_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("临时监听应可绑定");
    let port = listener.local_addr().expect("应取到端口").port();
    drop(listener);
    port
}

async fn start_proxy(
    provider: Arc<dyn CredentialProvider>,
    upstream_url: &str,
    policy: ProxyPolicy,
) -> TestProxy {
    start_proxy_with_cell(provider, upstream_url, Arc::new(PolicyCell::new(policy))).await
}

async fn start_proxy_with_cell(
    provider: Arc<dyn CredentialProvider>,
    upstream_url: &str,
    policy: Arc<PolicyCell>,
) -> TestProxy {
    let data_dir = std::env::temp_dir().join(format!("chl-proxy-it-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).expect("临时目录应可创建");
    let db = Db::open(&data_dir.join("proxy.db")).expect("数据库应可打开");
    db::run_migrations(&db).expect("迁移应成功");

    let server = ProxyServer::with_provider(data_dir.clone(), db.clone(), provider);
    let status = server
        .start("127.0.0.1", free_port().await, upstream_url, policy)
        .await
        .expect("代理应可启动");
    TestProxy {
        server,
        base_url: status.base_url,
        db,
        data_dir,
    }
}

impl TestProxy {
    async fn shutdown(self) {
        self.server.stop().await;
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn count(db: &Db, sql: &str) -> i64 {
    let conn = db.conn().expect("取连接应成功");
    conn.query_row(sql, [], |row| row.get(0))
        .expect("计数应成功")
}

fn text(db: &Db, sql: &str) -> Option<String> {
    let conn = db.conn().expect("取连接应成功");
    conn.query_row(sql, [], |row| row.get(0)).ok()
}

async fn wait_for_requests(db: &Db, expected: i64) {
    for _ in 0..200 {
        if count(db, "SELECT COUNT(*) FROM proxy_session_requests") >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!(
        "等待请求落库超时：当前 {} 行",
        count(db, "SELECT COUNT(*) FROM proxy_session_requests")
    );
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .expect("测试 HTTP 客户端应可构建")
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unauthorized_without_credentials_returns_401_shapes() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_without_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .body("{}")
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("错误应是 JSON");
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");

    let response = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .body("{}")
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 401);
    let body: Value = response.json().await.expect("错误应是 JSON");
    assert_eq!(body["error"]["type"], "authentication_error");

    // 401 也要计入 /status 的错误计数（此前只落库、不计数）。
    wait_for_requests(&proxy.db, 2).await;
    let status: Value = http()
        .get(format!("{}/status", proxy.base_url))
        .send()
        .await
        .expect("状态请求应完成")
        .json()
        .await
        .expect("状态应是 JSON");
    assert_eq!(status["errorCount"], 2, "两次 401 都应计为错误");
    assert_eq!(status["requestCount"], 2, "401 也是请求，不能被漏计");

    proxy.shutdown().await;
}

#[tokio::test]
async fn routes_forward_with_query_and_inject_headers() {
    let (upstream, capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages?beta=1", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .header("authorization", "Bearer client-secret")
        .header("x-api-key", "client-key")
        .header("cookie", "session=secret")
        .header("x-session-id", "sess-it-1")
        .header("anthropic-version", "2023-06-01")
        .header("x-client-request-id", "client-req-42")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("转发应完成");
    assert_eq!(response.status(), 200);
    assert!(response.headers().contains_key("x-hub-request-id"));
    let translated: Value = response.json().await.expect("响应应是 Anthropic JSON");
    assert_eq!(translated["type"], "message");
    assert_eq!(translated["content"][0]["text"], "hello");
    assert_eq!(translated["usage"]["input_tokens"], 50);

    let captured = capture.last();
    assert_eq!(captured.path, "/openai/v1/chat/completions");
    assert!(captured.body.contains("claude-sonnet-4"));
    assert_eq!(
        captured.headers.get("x-access-token").unwrap(),
        "test-access-token"
    );
    assert_eq!(captured.headers.get("x-auth-user").unwrap(), "test-user");
    // 不注入代理标识；客户端自己的 request id 原样保留。
    assert!(captured.headers.get("downstream-client").is_none());
    assert_eq!(
        captured.headers.get("x-client-request-id").unwrap(),
        "client-req-42"
    );
    // 翻译到 OpenAI 上游后不再发送 Anthropic 专属头。
    assert!(captured.headers.get("anthropic-version").is_none());
    assert_eq!(
        captured.headers.get("authorization").unwrap(),
        "Bearer test-access-token"
    );
    assert!(captured.headers.get("x-api-key").is_none());
    assert!(captured.headers.get("cookie").is_none());

    // 三个路由映射。
    let chat = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .json(&serde_json::json!({"model": "gpt-4o"}))
        .send()
        .await
        .expect("chat 转发应完成");
    assert_eq!(chat.status(), 200);
    assert_eq!(capture.last().path, "/openai/v1/chat/completions");

    let aliased = http()
        .post(format!("{}/responses", proxy.base_url))
        .json(&serde_json::json!({"model": "gpt-4o"}))
        .send()
        .await
        .expect("别名路由应完成");
    assert_eq!(aliased.status(), 500, "上游 500 应透传");
    assert_eq!(capture.last().path, "/openai/v1/responses");

    // 非流式 JSON 的 usage/cost 落库（按 chat 请求断言）。
    wait_for_requests(&proxy.db, 3).await;
    assert_eq!(
        text(
            &proxy.db,
            "SELECT api_format FROM usage_daily WHERE model = 'claude-sonnet-4'"
        )
        .as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        text(
            &proxy.db,
            "SELECT client FROM usage_daily WHERE model = 'claude-sonnet-4'"
        )
        .as_deref(),
        Some("claude-code")
    );
    assert_eq!(
        text(
            &proxy.db,
            "SELECT cost_source FROM proxy_request_costs WHERE model = 'claude-sonnet-4'"
        )
        .as_deref(),
        Some("upstream")
    );
    assert_eq!(
        text(
            &proxy.db,
            "SELECT model FROM proxy_request_costs WHERE model = 'gpt-4o'"
        )
        .as_deref(),
        Some("gpt-4o")
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn sse_is_streamed_and_usage_cost_recorded() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .header("x-session-id", "sse-session")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("SSE 转发应完成");
    assert_eq!(response.status(), 200);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(content_type.contains("text/event-stream"));
    let body = response.text().await.expect("SSE 正文应可读");
    assert!(body.contains("event: message_start"));
    assert!(body.contains("\"text\":\"Hello\""));
    assert!(body.contains("\"text\":\" world\""));
    assert!(body.contains("event: message_stop"));

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT api_format FROM proxy_session_requests").as_deref(),
        Some("anthropic")
    );
    assert_eq!(
        count(&proxy.db, "SELECT is_sse FROM proxy_session_requests"),
        1
    );
    assert_eq!(
        count(&proxy.db, "SELECT input_tokens FROM proxy_session_requests"),
        90
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT output_tokens FROM proxy_session_requests"
        ),
        42
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT cache_read_tokens FROM proxy_session_requests"
        ),
        10
    );
    assert_eq!(
        text(&proxy.db, "SELECT trace_id FROM proxy_request_costs").as_deref(),
        Some("trace-sse")
    );
    let cost: f64 = {
        let conn = proxy.db.conn().expect("取连接应成功");
        conn.query_row("SELECT cost_yuan FROM proxy_request_costs", [], |row| {
            row.get(0)
        })
        .expect("费用应可读")
    };
    assert!((cost - 0.028_433_28).abs() < 1e-9);

    proxy.shutdown().await;
}

#[tokio::test]
async fn client_disconnect_records_abort() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/slow"),
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("应拿到响应头");
    assert_eq!(response.status(), 200);
    // 读到第一块后立即丢弃（模拟客户端断开）。
    let mut stream = response.bytes_stream();
    use tokio_stream::StreamExt;
    let _ = stream.next().await;
    drop(stream);

    wait_for_requests(&proxy.db, 1).await;
    // 允许 producer 感知 send 失败并写记录。
    for _ in 0..100 {
        if text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref()
            == Some("client_aborted")
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("client_aborted")
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn count_tokens_is_available_without_upstream() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages/count_tokens", proxy.base_url))
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "system": "be brief",
            "messages": [{"role": "user", "content": "hello world"}]
        }))
        .send()
        .await
        .expect("count_tokens 应完成");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("应是 JSON");
    assert!(body["input_tokens"].as_u64().unwrap_or(0) > 0);

    proxy.shutdown().await;
}

#[tokio::test]
async fn models_endpoint_uses_provider_cache() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .get(format!("{}/v1/models", proxy.base_url))
        .send()
        .await
        .expect("模型列表应可读");
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("应是 JSON");
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "claude-sonnet-4");
    assert_eq!(body["data"][0]["object"], "model");

    let aliased = http()
        .get(format!("{}/models", proxy.base_url))
        .send()
        .await
        .expect("别名应可用");
    assert_eq!(aliased.status(), 200);

    proxy.shutdown().await;
}

#[tokio::test]
async fn models_endpoint_without_cache_returns_503() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_without_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;
    let response = http()
        .get(format!("{}/v1/models", proxy.base_url))
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 503);
    proxy.shutdown().await;
}

#[tokio::test]
async fn request_body_limit_returns_413() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let policy = ProxyPolicy {
        max_request_body_bytes: 1024,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &upstream, policy).await;

    let payload = "x".repeat(4096);
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .body(payload)
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 413);

    // 413 也必须落库并计为错误（此前客户端看到 413，用量里什么都没有）。
    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE status_code = 413"
        ),
        1
    );
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("request_body")
    );
    assert_eq!(
        text(&proxy.db, "SELECT status_kind FROM proxy_session_requests").as_deref(),
        Some("error")
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn cross_origin_requests_are_rejected() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;
    let response = http()
        .get(format!("{}/health", proxy.base_url))
        .header("origin", "https://evil.example")
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 403);
    proxy.shutdown().await;
}

#[tokio::test]
async fn non_sse_json_usage_is_recorded() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;
    let response = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .header("user-agent", "codex-cli/1.0")
        .json(&serde_json::json!({"model": "gpt-4o", "stream": false}))
        .send()
        .await
        .expect("转发应完成");
    assert_eq!(response.status(), 200);
    wait_for_requests(&proxy.db, 1).await;

    assert_eq!(
        text(&proxy.db, "SELECT client FROM usage_daily").as_deref(),
        Some("codex")
    );
    assert_eq!(
        count(&proxy.db, "SELECT input_tokens FROM proxy_session_requests"),
        50
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT output_tokens FROM proxy_session_requests"
        ),
        10
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT cache_read_tokens FROM proxy_session_requests"
        ),
        50
    );
    let cost: f64 = {
        let conn = proxy.db.conn().expect("取连接应成功");
        conn.query_row("SELECT cost_yuan FROM proxy_request_costs", [], |row| {
            row.get(0)
        })
        .expect("费用应可读")
    };
    assert!((cost - 0.015).abs() < 1e-9);
    assert_eq!(
        text(&proxy.db, "SELECT trace_id FROM proxy_request_costs").as_deref(),
        Some("trace-chat")
    );

    proxy.shutdown().await;
}

/// 改「记录请求正文」应立即生效，不必重启代理。
/// 旧实现把 ProxyPolicy 定格在启动时，保存设置后仍按旧开关走。
#[tokio::test]
async fn record_body_toggle_applies_without_restart() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let cell = Arc::new(PolicyCell::new(ProxyPolicy::default()));
    let proxy = start_proxy_with_cell(provider_with_credentials(), &upstream, cell.clone()).await;

    let body = serde_json::json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "stream": false,
        "messages": [{ "role": "user", "content": "first" }]
    });

    // 第一次：record_body = false，不应落库正文。
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&body)
        .send()
        .await
        .expect("转发应完成");
    assert_eq!(response.status(), 200);
    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE request_body IS NOT NULL"
        ),
        0,
        "开关关闭时不应记录正文"
    );

    // 热更新开关（等价于保存设置），不重启代理。
    cell.store(ProxyPolicy {
        record_body: true,
        ..ProxyPolicy::default()
    });

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&body)
        .send()
        .await
        .expect("转发应完成");
    assert_eq!(response.status(), 200);
    wait_for_requests(&proxy.db, 2).await;
    assert!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE request_body IS NOT NULL"
        ) >= 1,
        "开关打开后，下一次请求就应记录正文"
    );

    proxy.shutdown().await;
}

/// 开启「记录请求正文」后，>64KB 的请求曾把真实响应替换成错误体，
/// 并把 usage/cost 记成 0。录制标志不得影响中继字节与统计。
#[tokio::test]
async fn record_body_large_request_keeps_response_and_usage() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let policy = ProxyPolicy {
        record_body: true,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &upstream, policy).await;

    // 70KB 系统提示：超过 64KB 录制上限，但远低于 32MB 请求上限。
    let body = serde_json::json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "stream": false,
        "system": "x".repeat(70 * 1024),
        "messages": [{ "role": "user", "content": "hi" }]
    });

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&body)
        .send()
        .await
        .expect("转发应完成");
    assert_eq!(response.status(), 200);
    let translated: Value = response.json().await.expect("响应应是 Anthropic JSON");
    assert_eq!(translated["type"], "message", "真实答案不能被替换成错误体");
    assert_eq!(translated["content"][0]["text"], "hello");

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT cost_source FROM proxy_request_costs").as_deref(),
        Some("upstream"),
        "usage/cost 不能因为请求体被截断而丢失"
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE body_truncated = 1"
        ),
        1,
        "请求体确实被截断，落库标志应为 1"
    );

    proxy.shutdown().await;
}

/// 上游 3xx 不得被跟随，否则注入的账号凭据会发往 Location 主机。
#[tokio::test]
async fn upstream_redirect_is_not_followed() {
    let (sentinel, sentinel_capture, _sentinel_handle) = start_sentinel().await;
    let (upstream, _upstream_handle) = start_redirect_upstream(format!("{sentinel}/leaked")).await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/redirect"),
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .json(&serde_json::json!({ "model": "gpt-4o" }))
        .send()
        .await
        .expect("请求应完成");

    assert_eq!(response.status(), 502, "3xx 必须被代理拒绝");
    let body: Value = response.json().await.expect("应是 JSON 错误");
    assert_eq!(body["error"]["type"], "upstream_redirect");

    assert!(
        sentinel_capture
            .requests
            .lock()
            .expect("锁应可用")
            .is_empty(),
        "代理不得把带凭据的请求发往重定向目标"
    );

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("upstream_redirect")
    );

    proxy.shutdown().await;
}

// ---------------------------------------------------------------------------
// 第七批：入口 Host 白名单 / 失败请求落库
// ---------------------------------------------------------------------------

/// 裸 TCP 发一个请求：reqwest 会自己写 Host，测不了 Host 校验。
async fn raw_http(base_url: &str, request: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let addr = base_url.trim_start_matches("http://").to_string();
    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("应可连接代理");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("应可写入请求");
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
    String::from_utf8_lossy(&buf).to_string()
}

fn proxy_port(base_url: &str) -> u16 {
    base_url
        .rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .expect("base_url 应含端口")
}

#[tokio::test]
async fn host_guard_rejects_non_loopback_host() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;
    let port = proxy_port(&proxy.base_url);

    let evil = raw_http(
        &proxy.base_url,
        "GET /status HTTP/1.1\r\nHost: evil.com\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert!(
        evil.starts_with("HTTP/1.1 403"),
        "非回环 Host 必须 403：{evil}"
    );
    assert!(
        evil.contains("host_not_allowed"),
        "错误体应说明原因：{evil}"
    );

    let loopback = raw_http(
        &proxy.base_url,
        &format!("GET /status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"),
    )
    .await;
    assert!(
        loopback.starts_with("HTTP/1.1 200"),
        "回环 Host 应放行：{loopback}"
    );

    proxy.shutdown().await;
}

#[tokio::test]
async fn translation_failure_is_recorded() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    // 非对象 body → 翻译直接失败（400），不会打到上游。
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/2.0")
        .body("[]")
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 400);

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("translation_error")
    );
    assert_eq!(
        text(&proxy.db, "SELECT status_kind FROM proxy_session_requests").as_deref(),
        Some("error")
    );

    proxy.shutdown().await;
}

/// 接受连接但永远不回响应：用来触发代理的首字节超时。
async fn start_silent_upstream() -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::AsyncReadExt;

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("应可绑定");
    let addr = listener.local_addr().expect("应可读取地址");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                // 故意不回任何响应，让代理的首字节超时触发。
                tokio::time::sleep(Duration::from_secs(10)).await;
            });
        }
    });
    (format!("http://{addr}"), handle)
}

#[tokio::test]
async fn first_byte_timeout_is_recorded() {
    let (upstream, handle) = start_silent_upstream().await;
    let policy = ProxyPolicy {
        // 进策略的超时让这个用例 1s 出结果，不必等默认的 120s。
        first_byte_timeout_secs: 1,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &upstream, policy).await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/2.0")
        .body(r#"{"model":"claude-sonnet-4","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#)
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 504);

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE status_code = 504"
        ),
        1,
        "首字节超时必须落库"
    );
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("timeout")
    );
    assert_eq!(
        text(&proxy.db, "SELECT status_kind FROM proxy_session_requests").as_deref(),
        Some("error")
    );

    proxy.shutdown().await;
    handle.abort();
}

// ---------------------------------------------------------------------------
// 重构护栏：热路径快照（wire 字节 / SSE 事件序列 / 落库值）
//
// 这些用例把「当前输出」冻结下来。C2/C3 只改内部结构，任何字符串、状态码、
// 响应头、事件顺序或落库值的变化都必须让这里失败。
// ---------------------------------------------------------------------------

/// 按代理当前的 SSE 正文格式渲染 `(事件名, data)` 序列。
fn render_sse(events: &[(&str, serde_json::Value)]) -> String {
    let mut out = String::new();
    for (name, data) in events {
        out.push_str("event: ");
        out.push_str(name);
        out.push_str("\ndata: ");
        out.push_str(&data.to_string());
        out.push_str("\n\n");
    }
    out
}

/// 按出现顺序取出 `event:` 名。
fn sse_event_names(body: &str) -> Vec<&str> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            frame
                .lines()
                .find_map(|line| line.strip_prefix("event: "))
                .unwrap_or_else(|| panic!("SSE 帧缺少 event 行：{frame}"))
        })
        .collect()
}

/// 按出现顺序解析每条 `data:`（全部必须是 JSON）。
fn sse_event_data(body: &str) -> Vec<serde_json::Value> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let data = frame
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap_or_else(|| panic!("SSE 帧缺少 data 行：{frame}"));
            serde_json::from_str(data).expect("SSE data 应是 JSON")
        })
        .collect()
}

/// `/v1/messages` 流式：Anthropic SSE 的完整正文（事件序列 + 顺序 + 事件数 + data 字节）。
#[tokio::test]
async fn translated_sse_event_sequence_is_stable() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .header("x-client-request-id", "snapshot-sse-1")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("SSE 转发应完成");
    assert_eq!(response.status(), 200);
    // 这个头是每请求 uuid，不属于快照范围（只断言存在）。
    assert!(response.headers().contains_key("x-hub-request-id"));
    let body = response.text().await.expect("SSE 正文应可读");

    // 上游 chunk 自带 id，因此 message_id 不含随机 uuid，正文可以逐字节快照。
    let usage_start = serde_json::json!({
        "input_tokens": 1,
        "output_tokens": 0,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    });
    let usage_delta = serde_json::json!({
        "input_tokens": 90,
        "output_tokens": 42,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 10,
    });
    let expected = render_sse(&[
        (
            "message_start",
            serde_json::json!({
                "type": "message_start",
                "message": {
                    "id": "msg_chatcmpl-sse",
                    "type": "message",
                    "role": "assistant",
                    "model": "deepseek-v4-flash",
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": usage_start,
                }
            }),
        ),
        (
            "content_block_start",
            serde_json::json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": { "type": "text", "text": "" }
            }),
        ),
        (
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": "Hello" }
            }),
        ),
        (
            "content_block_delta",
            serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": { "type": "text_delta", "text": " world" }
            }),
        ),
        (
            "content_block_stop",
            serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        ),
        (
            "message_delta",
            serde_json::json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn", "stop_sequence": null },
                "usage": usage_delta
            }),
        ),
        (
            "message_stop",
            serde_json::json!({ "type": "message_stop" }),
        ),
    ]);

    assert_eq!(
        body, expected,
        "翻译流的完整响应体（事件序列/顺序/事件数/data 字节）不得变化"
    );
    assert_eq!(body.matches("event: ").count(), 7, "事件数必须为 7");
    assert_eq!(
        sse_event_names(&body),
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
    );

    proxy.shutdown().await;
}

/// 上游流中途断开：钉住**当前**事件序列。本轮不改语义；将来若改，必须先显式更新这个断言。
#[tokio::test]
async fn broken_upstream_stream_event_sequence_is_pinned() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/broken"),
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("上游断开后代理仍应给出响应");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("SSE 正文应可读");

    let names = sse_event_names(&body);
    assert_eq!(
        names,
        vec![
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ],
        "异常结束的当前事件序列（含 message_stop）不得变化：{body}"
    );
    let data = sse_event_data(&body);
    assert_eq!(data[2]["delta"]["text"], "partial");
    assert_eq!(data[3]["index"], 0);
    assert_eq!(data[5]["type"], "message_stop");
    // 上游没发 usage，message_delta 里的 usage 全 0。
    assert_eq!(data[4]["usage"]["input_tokens"], 0);
    assert_eq!(data[4]["usage"]["output_tokens"], 0);

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("upstream_error"),
        "上游断开应记为 upstream_error"
    );

    proxy.shutdown().await;
}

/// 上游在首帧之前断开：当前行为是合成一条 `event: error`，且**不**发 `message_stop`。
#[tokio::test]
async fn upstream_reset_before_first_frame_emits_error_event() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/empty-broken"),
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": true,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 200);
    let body = response.text().await.expect("SSE 正文应可读");
    assert_eq!(
        body,
        "event: error\ndata: {\"error\":{\"message\":\"上游流式响应为空\",\"type\":\"api_error\"},\"type\":\"error\"}\n\n"
    );
    assert_eq!(sse_event_names(&body), vec!["error"]);
    assert!(
        !body.contains("message_stop"),
        "空流不得伪装成正常完成：{body}"
    );

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("upstream_error")
    );

    proxy.shutdown().await;
}

/// 非流式翻译缓冲溢出：钉住 status / 响应体 / 落库值。
#[tokio::test]
async fn non_stream_translation_overflow_keeps_status_and_records() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let policy = ProxyPolicy {
        max_buffer_bytes: 1024,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/big"),
        policy,
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .json(&serde_json::json!({
            "model": "claude-sonnet-4",
            "max_tokens": 16,
            "stream": false,
            "messages": [{ "role": "user", "content": "hi" }]
        }))
        .send()
        .await
        .expect("请求应完成");

    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert_eq!(
        response.text().await.expect("正文应可读"),
        r#"{"error":{"message":"上游返回了无法解析的错误响应","type":"api_error"},"type":"error"}"#
    );

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        count(&proxy.db, "SELECT status_code FROM proxy_session_requests"),
        200
    );
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests"),
        None,
        "缓冲溢出当前不改写 error_kind"
    );
    assert_eq!(
        text(&proxy.db, "SELECT status_kind FROM proxy_session_requests").as_deref(),
        Some("success")
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT body_truncated FROM proxy_session_requests"
        ),
        1,
        "缓冲溢出必须落 body_truncated=1"
    );
    assert_eq!(
        count(&proxy.db, "SELECT input_tokens FROM proxy_session_requests"),
        0,
        "溢出时 usage 记 0（现状）"
    );
    assert_eq!(
        text(&proxy.db, "SELECT cost_source FROM proxy_request_costs").as_deref(),
        Some("none")
    );

    proxy.shutdown().await;
}

/// 四路早期失败的完整响应体 + 落库行（构成 wire 字节基线）。
#[tokio::test]
async fn early_failure_error_bodies_are_byte_stable() {
    const AUTH_MESSAGE: &str = "未登录或登录已失效，请先登录 CodeMaker Hub Light";

    // ---- 401：无凭据（Anthropic / OpenAI 两种形状） ----
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_without_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .body("{}")
        .send()
        .await
        .expect("401 请求应完成");
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.text().await.expect("正文应可读"),
        format!(
            r#"{{"error":{{"message":"{AUTH_MESSAGE}","type":"authentication_error"}},"type":"error"}}"#
        )
    );

    let response = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .body("{}")
        .send()
        .await
        .expect("401 请求应完成");
    assert_eq!(response.status(), 401);
    assert_eq!(
        response.text().await.expect("正文应可读"),
        format!(
            r#"{{"error":{{"code":"authentication_error","message":"{AUTH_MESSAGE}","type":"authentication_error"}}}}"#
        )
    );

    wait_for_requests(&proxy.db, 2).await;
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE status_code = 401 AND error_kind = 'unauthorized' AND status_kind = 'error'"
        ),
        2
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE request_bytes = 0"
        ),
        2
    );
    proxy.shutdown().await;

    // ---- 413：请求体超限 ----
    let (upstream, _capture, _handle) = start_upstream().await;
    let policy = ProxyPolicy {
        max_request_body_bytes: 1024,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &upstream, policy).await;
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .body("x".repeat(4096))
        .send()
        .await
        .expect("413 请求应完成");
    assert_eq!(response.status(), 413);
    assert_eq!(
        response.text().await.expect("正文应可读"),
        r#"{"error":{"message":"请求体超过 32MB 上限","type":"request_too_large"},"type":"error"}"#
    );
    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("request_body")
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE status_code = 413 AND status_kind = 'error'"
        ),
        1
    );
    proxy.shutdown().await;

    // ---- 400：翻译失败（body 不是 JSON 对象） ----
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &upstream,
        ProxyPolicy::default(),
    )
    .await;
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .body("[]")
        .send()
        .await
        .expect("400 请求应完成");
    assert_eq!(response.status(), 400);
    assert_eq!(
        response.text().await.expect("正文应可读"),
        r#"{"error":{"message":"请求体必须是 JSON 对象","type":"invalid_request_error"},"type":"error"}"#
    );
    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("translation_error")
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT request_bytes FROM proxy_session_requests"
        ),
        2
    );
    proxy.shutdown().await;

    // ---- 504：首字节超时 ----
    let (silent, handle) = start_silent_upstream().await;
    let policy = ProxyPolicy {
        first_byte_timeout_secs: 1,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &silent, policy).await;
    let request_body = r#"{"model":"claude-sonnet-4","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#;
    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .body(request_body)
        .send()
        .await
        .expect("504 请求应完成");
    assert_eq!(response.status(), 504);
    assert_eq!(
        response.text().await.expect("正文应可读"),
        r#"{"error":{"message":"上游首字节超时","type":"upstream_timeout"},"type":"error"}"#
    );
    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(&proxy.db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
        Some("timeout")
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT COUNT(*) FROM proxy_session_requests WHERE status_code = 504 AND status_kind = 'error'"
        ),
        1
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT request_bytes FROM proxy_session_requests"
        ),
        request_body.len() as i64
    );
    assert_eq!(
        text(&proxy.db, "SELECT model FROM proxy_session_requests").as_deref(),
        Some("claude-sonnet-4")
    );
    proxy.shutdown().await;
    handle.abort();
}

/// record_body=true + 70KB 请求：钉住中继字节与 `request_body/response_body/
/// body_truncated` 的落库值（C2 里 `RequestContext` 去克隆时的主要护栏）。
#[tokio::test]
async fn record_body_and_context_clone_equivalence() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let policy = ProxyPolicy {
        record_body: true,
        ..ProxyPolicy::default()
    };
    let proxy = start_proxy(provider_with_credentials(), &upstream, policy).await;

    let request = serde_json::json!({
        "model": "claude-sonnet-4",
        "max_tokens": 16,
        "temperature": 0.2,
        "stream": false,
        "system": "x".repeat(70 * 1024),
        "messages": [{ "role": "user", "content": "hi" }],
    });
    let request_text = serde_json::to_string(&request).expect("请求应可序列化");
    assert!(request_text.len() > 64 * 1024, "请求体必须超过录制上限");

    let response = http()
        .post(format!("{}/v1/messages", proxy.base_url))
        .header("user-agent", "claude-cli/1.0")
        .header("x-client-request-id", "snapshot-record-1")
        .body(request_text.clone())
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 200);
    let response_text = response.text().await.expect("响应正文应可读");
    let translated: Value = serde_json::from_str(&response_text).expect("应是 Anthropic JSON");
    assert_eq!(translated["type"], "message");
    assert_eq!(translated["content"][0]["text"], "hello");

    wait_for_requests(&proxy.db, 1).await;
    assert_eq!(
        text(
            &proxy.db,
            "SELECT client_request_id FROM proxy_session_requests"
        )
        .as_deref(),
        Some("snapshot-record-1")
    );
    assert_eq!(
        text(&proxy.db, "SELECT model FROM proxy_session_requests").as_deref(),
        Some("claude-sonnet-4")
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT request_bytes FROM proxy_session_requests"
        ),
        request_text.len() as i64
    );
    // 请求正文按 64KB 截断后落库（全 ASCII，截断点即 64*1024）。
    assert_eq!(
        text(&proxy.db, "SELECT request_body FROM proxy_session_requests").as_deref(),
        Some(&request_text[..64 * 1024])
    );
    // 录制的响应正文必须与客户端实际收到的字节完全一致（中继未被录制影响）。
    assert_eq!(
        text(
            &proxy.db,
            "SELECT response_body FROM proxy_session_requests"
        )
        .as_deref(),
        Some(response_text.as_str())
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT body_truncated FROM proxy_session_requests"
        ),
        1
    );
    assert_eq!(
        count(&proxy.db, "SELECT input_tokens FROM proxy_session_requests"),
        50
    );
    assert_eq!(
        count(
            &proxy.db,
            "SELECT output_tokens FROM proxy_session_requests"
        ),
        10
    );
    assert_eq!(
        text(&proxy.db, "SELECT cost_source FROM proxy_request_costs").as_deref(),
        Some("upstream")
    );

    proxy.shutdown().await;
}

/// 响应头 `x-hub-request-id` 必须是 **append**（与旧 `Response::builder().header`
/// 语义一致）：上游回显同名头时保留两个值，而不是覆盖。
#[tokio::test]
async fn response_request_id_header_is_appended_not_replaced() {
    let (upstream, _capture, _handle) = start_upstream().await;
    let proxy = start_proxy(
        provider_with_credentials(),
        &format!("{upstream}/echo-id"),
        ProxyPolicy::default(),
    )
    .await;

    let response = http()
        .post(format!("{}/v1/chat/completions", proxy.base_url))
        .header("user-agent", "codex-cli/1.0")
        .json(&serde_json::json!({ "model": "gpt-4o" }))
        .send()
        .await
        .expect("请求应完成");
    assert_eq!(response.status(), 200);
    let values: Vec<&str> = response
        .headers()
        .get_all("x-hub-request-id")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect();
    assert_eq!(
        values.len(),
        2,
        "上游回显的同名头必须保留（append 语义）：{values:?}"
    );
    assert_eq!(values[0], "upstream-echo");
    assert!(!values[1].is_empty(), "第二个值应是代理生成的请求 id");

    proxy.shutdown().await;
}
