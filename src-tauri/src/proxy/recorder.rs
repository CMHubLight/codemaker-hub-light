//! 请求/成本落库（P2）：写 `proxy_sessions`、`proxy_session_requests`、
//! `proxy_request_costs`、`usage_daily`。
//!
//! - 全部在**单个事务**内完成；写失败只记录日志，不影响已转发的响应。
//! - 正文默认不落库；开启时由调用方按 64KB 截断后传入。
//! - 本模块只接收 token 计数与费用，不接收任何凭据。

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::params;

use crate::{
    db::{Db, DbError},
    proxy::{
        forward::ApiFormat,
        usage_parser::{CostInfo, CostSource, TokenUsage},
    },
    usage::{aggregate, local_day},
};

/// 一次请求的静态上下文（请求开始时构造）。
#[derive(Debug, Clone)]
pub struct RequestContext {
    pub request_id: String,
    pub client_request_id: Option<String>,
    pub method: String,
    /// 客户端原始路径（带查询串）。
    pub request_path: String,
    pub api_format: ApiFormat,
    /// 上游完整 URL（不含凭据，最多含业务 query）。
    pub upstream_url: String,
    pub model: Option<String>,
    pub client: String,
    /// 会话标识：客户端头，或 `anon:{client}:{date}`。
    pub session_key: String,
    pub is_sse: bool,
    pub request_bytes: i64,
    pub request_headers_json: Option<String>,
    pub request_body: Option<String>,
    pub started_at: DateTime<Utc>,
}

/// 请求异常分类：`as_str` 就是数据库 `error_kind` 文本，只在这里定义一次。
///
/// `usage::aggregate::status_kind` 依据这些文本把请求归入 success/error/cancelled，
/// 因此新增/改名都从枚举开始，避免字符串漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// 首字节超时或上游流 idle 超时。
    Timeout,
    /// 上游连接失败，或上游流中途报错。
    UpstreamError,
    /// 上游返回 3xx（代理拒绝跟随，避免凭据外泄）。
    UpstreamRedirect,
    /// 客户端提前断开（channel send 失败）。
    ClientAborted,
    /// 请求或响应翻译失败。
    TranslationError,
    /// 无凭据 401。
    Unauthorized,
    /// 请求体超过上限 413。
    RequestBody,
}

impl ErrorKind {
    /// 数据库 `error_kind` 文本（与前端/聚合共用）。
    pub const fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Timeout => "timeout",
            ErrorKind::UpstreamError => "upstream_error",
            ErrorKind::UpstreamRedirect => "upstream_redirect",
            ErrorKind::ClientAborted => "client_aborted",
            ErrorKind::TranslationError => "translation_error",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::RequestBody => "request_body",
        }
    }
}

/// 请求结束时的观测结果。
#[derive(Debug, Clone)]
pub struct RequestCompletion {
    pub status_code: Option<i64>,
    pub usage: TokenUsage,
    pub cost: CostInfo,
    pub response_bytes: u64,
    pub first_byte_ms: Option<i64>,
    pub duration_ms: i64,
    /// 异常分类；正常完成为 `None`。
    pub error_kind: Option<ErrorKind>,
    pub response_body: Option<String>,
    pub body_truncated: bool,
    pub ended_at: DateTime<Utc>,
}

impl RequestCompletion {
    /// 一次"没有正文、没有 usage"的失败记录（401/413/400/504 与上游连接失败共用）。
    pub fn failure(
        status_code: i64,
        duration_ms: i64,
        error_kind: ErrorKind,
        ended_at: DateTime<Utc>,
    ) -> Self {
        Self {
            status_code: Some(status_code),
            usage: TokenUsage::default(),
            cost: CostInfo::none(),
            response_bytes: 0,
            first_byte_ms: None,
            duration_ms,
            error_kind: Some(error_kind),
            response_body: None,
            body_truncated: false,
            ended_at,
        }
    }
}

/// 落库结果（诊断/测试用）。
#[derive(Debug, Clone)]
pub struct RecordOutcome {
    pub request_id: i64,
    pub session_id: i64,
}

fn rfc3339(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn cost_source_str(source: CostSource) -> &'static str {
    source.as_str()
}

/// 写一次请求的完整记录（session upsert + request + cost + usage_daily）。
pub fn record(
    db: &Db,
    context: &RequestContext,
    completion: &RequestCompletion,
) -> Result<RecordOutcome, DbError> {
    let mut conn = db.conn()?;
    let started_at = rfc3339(context.started_at);
    let ended_at = rfc3339(completion.ended_at);
    // 「本地日」全应用只有一个定义；请求列表与日聚合共用同一天。
    // 按**开始时间**归类：列表按 started_at 排序，"今天"也应指开始于今天。
    let day_local = local_day(context.started_at);
    // 状态分类也只在 usage::aggregate::status_kind 定义一次。
    let error_kind_text = completion.error_kind.map(ErrorKind::as_str);
    let status_kind = aggregate::status_kind(error_kind_text, completion.status_code);
    let is_error = completion
        .status_code
        .map(|code| code >= 400)
        .unwrap_or(false)
        || completion.error_kind.is_some();
    let error_increment: i64 = i64::from(is_error);
    let usage = completion.usage;
    let model = context.model.as_deref();

    let transaction = conn.transaction()?;

    // 1) 会话 upsert：单条语句 + RETURNING，避免并发首请求的 SELECT-then-INSERT 竞态
    //    （两个请求同时首次进入时，后写的会撞唯一索引；deferred 事务读升级写还会
    //    直接拿到 SQLITE_BUSY，busy_timeout 对升级不一定生效，整条事务一起回滚）。
    //    `ux_proxy_sessions_external` 是 partial index，冲突目标必须带同样的谓词。
    let session_id: i64 = transaction.query_row(
        "INSERT INTO proxy_sessions \
           (external_session_id, api_format, client, model, request_count, error_count, \
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
            accumulated_cost_yuan, first_seen_at, last_active_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?11, ?12) \
         ON CONFLICT(external_session_id, api_format) WHERE external_session_id IS NOT NULL \
         DO UPDATE SET \
           request_count = request_count + 1, \
           error_count = error_count + excluded.error_count, \
           input_tokens = input_tokens + excluded.input_tokens, \
           output_tokens = output_tokens + excluded.output_tokens, \
           cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
           cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
           accumulated_cost_yuan = accumulated_cost_yuan + excluded.accumulated_cost_yuan, \
           last_active_at = excluded.last_active_at, \
           model = COALESCE(excluded.model, model), \
           updated_at = excluded.updated_at \
         RETURNING id",
        params![
            context.session_key,
            context.api_format.as_str(),
            context.client,
            model,
            error_increment,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            completion.cost.cost_yuan,
            started_at,
            ended_at,
        ],
        |row| row.get(0),
    )?;

    // 2) 逐请求记录。
    transaction.execute(
        "INSERT INTO proxy_session_requests \
           (session_id, client_request_id, method, request_path, api_format, upstream_url, model, \
            status_code, is_sse, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
            request_bytes, response_bytes, duration_ms, first_byte_ms, error_kind, \
            request_headers, request_body, response_body, body_truncated, started_at, ended_at, \
            day_local, status_kind) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, \
                 ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        params![
            session_id,
            context.client_request_id,
            context.method,
            context.request_path,
            context.api_format.as_str(),
            context.upstream_url,
            model,
            completion.status_code,
            i64::from(context.is_sse),
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            context.request_bytes,
            completion.response_bytes as i64,
            completion.duration_ms,
            completion.first_byte_ms,
            completion.error_kind.map(ErrorKind::as_str),
            context.request_headers_json,
            context.request_body,
            completion.response_body,
            i64::from(completion.body_truncated),
            started_at,
            ended_at,
            day_local,
            status_kind,
        ],
    )?;
    let request_row_id = transaction.last_insert_rowid();

    // 3) 费用记录。
    let detail_json = serde_json::json!({
        "total_credit": completion.cost.total_credit,
        "cost_source": cost_source_str(completion.cost.cost_source),
        "trace_id": completion.cost.trace_id.clone(),
        "currency": completion.cost.currency,
    })
    .to_string();
    transaction.execute(
        "INSERT INTO proxy_request_costs \
           (request_id, session_id, api_format, model, input_tokens, output_tokens, \
            cache_read_tokens, cache_write_tokens, cost_yuan, cost_source, currency, \
            total_credit, trace_id, detail_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            request_row_id,
            session_id,
            context.api_format.as_str(),
            model,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            completion.cost.cost_yuan,
            cost_source_str(completion.cost.cost_source),
            completion.cost.currency,
            completion.cost.total_credit,
            completion.cost.trace_id,
            detail_json,
            ended_at,
        ],
    )?;

    // 4) 每日聚合 UPSERT（维度：day / api_format / model / client）。
    //    day 与请求行的 day_local 同源，保证「今日 KPI」与「今天列表」口径一致；
    //    同样用单条 UPSERT，避免并发首请求撞 `ux_usage_daily_dim` 回滚整条事务。
    let api_format = context.api_format.as_str();
    let model_key = model.unwrap_or("");
    let client_key = context.client.as_str();
    transaction.execute(
        "INSERT INTO usage_daily \
           (day, api_format, model, client, request_count, error_count, input_tokens, \
            output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
            total_duration_ms, updated_at) \
         VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) \
         ON CONFLICT(day, api_format, model, client) DO UPDATE SET \
           request_count = request_count + 1, \
           error_count = error_count + excluded.error_count, \
           input_tokens = input_tokens + excluded.input_tokens, \
           output_tokens = output_tokens + excluded.output_tokens, \
           cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
           cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
           cost_yuan = cost_yuan + excluded.cost_yuan, \
           total_duration_ms = total_duration_ms + excluded.total_duration_ms, \
           updated_at = excluded.updated_at",
        params![
            day_local,
            api_format,
            model_key,
            client_key,
            error_increment,
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_read_tokens,
            usage.cache_write_tokens,
            completion.cost.cost_yuan,
            completion.duration_ms,
            ended_at,
        ],
    )?;

    transaction.commit()?;
    Ok(RecordOutcome {
        request_id: request_row_id,
        session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, proxy::usage_parser::credit_to_yuan};

    fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("chl-recorder-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("临时目录应可创建");
        let db = Db::open(&dir.join("test.db")).expect("数据库应可打开");
        db::run_migrations(&db).expect("迁移应成功");
        (db, dir)
    }

    fn context() -> RequestContext {
        RequestContext {
            request_id: "hub-req-1".to_string(),
            client_request_id: Some("client-req-1".to_string()),
            method: "POST".to_string(),
            request_path: "/v1/messages?beta=1".to_string(),
            api_format: ApiFormat::AnthropicMessages,
            upstream_url: "https://api.example.test/anthropic/v1/messages?beta=1".to_string(),
            model: Some("claude-sonnet-4".to_string()),
            client: "claude-code".to_string(),
            session_key: "sess-1".to_string(),
            is_sse: true,
            request_bytes: 123,
            request_headers_json: Some(r#"{"user-agent":"claude-cli/1.0"}"#.to_string()),
            request_body: None,
            started_at: Utc::now(),
        }
    }

    fn completion(cost_yuan: f64) -> RequestCompletion {
        RequestCompletion {
            status_code: Some(200),
            usage: TokenUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_read_tokens: 30,
                cache_write_tokens: 5,
            },
            cost: CostInfo {
                total_credit: Some(284.3328),
                cost_yuan,
                cost_source: CostSource::Upstream,
                currency: "CNY",
                trace_id: Some("trace-1".to_string()),
            },
            response_bytes: 456,
            first_byte_ms: Some(12),
            duration_ms: 345,
            error_kind: None,
            response_body: None,
            body_truncated: false,
            ended_at: Utc::now(),
        }
    }

    fn scalar_i64(db: &Db, sql: &str) -> i64 {
        let conn = db.conn().expect("取连接应成功");
        conn.query_row(sql, [], |row| row.get(0))
            .expect("查询应成功")
    }

    fn scalar_f64(db: &Db, sql: &str) -> f64 {
        let conn = db.conn().expect("取连接应成功");
        conn.query_row(sql, [], |row| row.get(0))
            .expect("查询应成功")
    }

    fn scalar_text(db: &Db, sql: &str) -> Option<String> {
        let conn = db.conn().expect("取连接应成功");
        conn.query_row(sql, [], |row| row.get(0))
            .expect("查询应成功")
    }

    /// 落库时写入本地日与状态分类，且 usage_daily 用同一天。
    #[test]
    fn record_writes_day_local_and_status_kind() {
        let (db, dir) = temp_db("day-status");
        let mut ctx = context();
        ctx.started_at = chrono::DateTime::parse_from_rfc3339("2026-10-21T16:30:00Z")
            .expect("时间戳应可解析")
            .with_timezone(&Utc);
        let mut done = completion(0.5);
        done.ended_at = ctx.started_at + chrono::Duration::seconds(2);
        done.error_kind = Some(ErrorKind::UpstreamError);
        done.status_code = Some(502);

        record(&db, &ctx, &done).expect("落库应成功");

        let expected_day = local_day(ctx.started_at);
        assert_eq!(
            scalar_text(&db, "SELECT day_local FROM proxy_session_requests").as_deref(),
            Some(expected_day.as_str()),
            "请求行的本地日必须由 started_at 换算"
        );
        assert_eq!(
            scalar_text(&db, "SELECT status_kind FROM proxy_session_requests").as_deref(),
            Some("error"),
            "upstream_error 应落库为 error"
        );
        assert_eq!(
            scalar_text(&db, "SELECT day FROM usage_daily").as_deref(),
            Some(expected_day.as_str()),
            "日聚合必须与请求行同一天"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 并发首次请求：两个请求同时首次进入时，旧的 SELECT-then-INSERT 会撞唯一索引，
    /// 整条事务回滚 → 请求行、费用、usage 全丢。改用原子 UPSERT 后必须一条不少。
    #[test]
    fn concurrent_first_requests_do_not_lose_rows() {
        const THREADS: usize = 8;
        const PER_THREAD: usize = 3;

        let (db, dir) = temp_db("concurrent");
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let db = db.clone();
                std::thread::spawn(move || {
                    for _ in 0..PER_THREAD {
                        let mut ctx = context();
                        // 同一会话 + 同一日聚合维度：必然有一批请求同时"首次"写入。
                        ctx.session_key = "shared-session".to_string();
                        record(&db, &ctx, &completion(0.25)).expect("并发落库不应失败");
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().expect("落库线程不应 panic");
        }

        let total = (THREADS * PER_THREAD) as i64;
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM proxy_session_requests"),
            total,
            "每次请求都必须落库"
        );
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM proxy_sessions"),
            1,
            "同一会话只能有一行"
        );
        assert_eq!(
            scalar_i64(&db, "SELECT request_count FROM proxy_sessions"),
            total
        );
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM usage_daily"),
            1,
            "同一维度只能有一行日聚合"
        );
        assert_eq!(
            scalar_i64(&db, "SELECT request_count FROM usage_daily"),
            total
        );
        let cost = scalar_f64(&db, "SELECT cost_yuan FROM usage_daily");
        assert!(
            (cost - total as f64 * 0.25).abs() < 1e-9,
            "费用累加应完整：{cost}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn records_session_request_cost_and_usage_daily() {
        let (db, dir) = temp_db("full");
        let cost = credit_to_yuan(284.3328, 0.01);
        let outcome = record(&db, &context(), &completion(cost)).expect("落库应成功");
        assert!(outcome.request_id > 0);
        assert!(outcome.session_id > 0);

        assert_eq!(scalar_i64(&db, "SELECT COUNT(*) FROM proxy_sessions"), 1);
        assert_eq!(
            scalar_i64(&db, "SELECT request_count FROM proxy_sessions"),
            1
        );
        assert_eq!(
            scalar_i64(&db, "SELECT input_tokens FROM proxy_sessions"),
            100
        );
        let session_cost = scalar_f64(&db, "SELECT accumulated_cost_yuan FROM proxy_sessions");
        assert!((session_cost - cost).abs() < 1e-9);

        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM proxy_session_requests"),
            1
        );
        assert_eq!(
            scalar_text(&db, "SELECT client_request_id FROM proxy_session_requests").as_deref(),
            Some("client-req-1")
        );
        assert_eq!(
            scalar_i64(&db, "SELECT is_sse FROM proxy_session_requests"),
            1
        );
        assert_eq!(
            scalar_i64(&db, "SELECT status_code FROM proxy_session_requests"),
            200
        );

        assert_eq!(
            scalar_text(&db, "SELECT cost_source FROM proxy_request_costs").as_deref(),
            Some("upstream")
        );
        assert_eq!(
            scalar_text(&db, "SELECT trace_id FROM proxy_request_costs").as_deref(),
            Some("trace-1")
        );
        assert_eq!(scalar_i64(&db, "SELECT COUNT(*) FROM usage_daily"), 1);
        assert_eq!(
            scalar_text(&db, "SELECT model FROM usage_daily").as_deref(),
            Some("claude-sonnet-4")
        );
        assert_eq!(scalar_i64(&db, "SELECT request_count FROM usage_daily"), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn second_request_accumulates_dimensions() {
        let (db, dir) = temp_db("accumulate");
        let cost = credit_to_yuan(100.0, 0.01);
        record(&db, &context(), &completion(cost)).expect("第一次落库应成功");
        let outcome = record(&db, &context(), &completion(cost)).expect("第二次落库应成功");

        assert_eq!(scalar_i64(&db, "SELECT COUNT(*) FROM proxy_sessions"), 1);
        assert_eq!(
            scalar_i64(&db, "SELECT request_count FROM proxy_sessions"),
            2
        );
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM proxy_session_requests"),
            2
        );
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM usage_daily"),
            1,
            "相同 day/format/model/client 应合并为一行"
        );
        assert_eq!(scalar_i64(&db, "SELECT request_count FROM usage_daily"), 2);
        assert_eq!(
            scalar_i64(&db, "SELECT COUNT(*) FROM proxy_request_costs"),
            2
        );
        assert!(outcome.request_id > 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn error_requests_increment_error_counts() {
        let (db, dir) = temp_db("errors");
        let mut failed = completion(0.0);
        failed.status_code = Some(500);
        failed.error_kind = Some(ErrorKind::UpstreamError);
        failed.cost.cost_source = CostSource::None;
        record(&db, &context(), &failed).expect("落库应成功");
        assert_eq!(scalar_i64(&db, "SELECT error_count FROM proxy_sessions"), 1);
        assert_eq!(scalar_i64(&db, "SELECT error_count FROM usage_daily"), 1);
        assert_eq!(
            scalar_text(&db, "SELECT error_kind FROM proxy_session_requests").as_deref(),
            Some("upstream_error")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `ErrorKind::as_str` 是数据库文本的唯一来源，改名/新增必须让这里失败。
    #[test]
    fn error_kind_texts_are_stable() {
        let cases: &[(ErrorKind, &str, &str)] = &[
            (ErrorKind::Timeout, "timeout", "error"),
            (ErrorKind::UpstreamError, "upstream_error", "error"),
            (ErrorKind::UpstreamRedirect, "upstream_redirect", "error"),
            (ErrorKind::ClientAborted, "client_aborted", "cancelled"),
            (ErrorKind::TranslationError, "translation_error", "error"),
            (ErrorKind::Unauthorized, "unauthorized", "error"),
            (ErrorKind::RequestBody, "request_body", "error"),
        ];
        for (kind, text, bucket) in cases {
            assert_eq!(kind.as_str(), *text);
            assert_eq!(
                aggregate::status_kind(Some(kind.as_str()), None),
                *bucket,
                "{text} 的状态桶由 usage::aggregate::status_kind 决定"
            );
        }
    }
}
