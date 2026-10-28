//! 用量只读查询与 CSV 导出。
//!
//! 空库返回零值结构（比率为 null），与前端空态一致；不伪造 0 费率。
//!
//! 本层只做"参数解析 + 读事务 + DTO 组装"，SQL 与行解码在 `db::sql` / `db::rows`。

use std::{
    borrow::Cow,
    collections::BTreeMap,
    io::{BufWriter, Write},
    path::Path,
};

use chrono::{Datelike, Duration, Local, NaiveDate, SecondsFormat, Utc};

use crate::{
    db::{repo, Db, DbError},
    usage::{
        aggregate, CostTrend, CostTrendPoint, CsvExport, Cursor, RequestDetail, RequestFilter,
        RequestPage, UsageSummary,
    },
};

const SUMMARY_DAYS: i64 = 30;
const DEFAULT_PAGE_SIZE: u32 = 50;
const MAX_PAGE_SIZE: u32 = 200;
const RECENT_LIMIT: u32 = 10;
const EPOCH_DAY: &str = "0000-01-01";
/// 单次 CSV 导出的行数硬上限：超过则截断并在结果里标记（前端应提示缩小范围）。
const MAX_EXPORT_ROWS: usize = 200_000;
/// CSV 表头（含 UTF-8 BOM，Excel 直接双击不乱码）。
const EXPORT_HEADER: &str = "\u{feff}day,api_format,model,client,request_count,error_count,\
     input_tokens,output_tokens,cache_read_tokens,cache_write_tokens,cost_yuan\n";

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn day_string(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// 从 ISO 日期/日期时间中取前 10 位合法日期。
fn day_prefix(value: &str) -> Option<String> {
    let candidate = value.trim().get(..10)?;
    NaiveDate::parse_from_str(candidate, "%Y-%m-%d").ok()?;
    Some(candidate.to_string())
}

fn normalize_protocol(value: &str) -> Option<&'static str> {
    match value.trim() {
        "anthropic" | "anthropic_messages" => Some("anthropic"),
        "openai_chat" => Some("openai_chat"),
        "openai_responses" => Some("openai_responses"),
        _ => None,
    }
}

fn normalize_status(value: &str) -> Option<&'static str> {
    match value.trim() {
        "success" => Some("success"),
        "error" => Some("error"),
        "cancelled" | "canceled" => Some("cancelled"),
        // 旧数据里 status_code/error_kind 都缺失的行，单独可选。
        "unknown" => Some("unknown"),
        _ => None,
    }
}

/// 解析协议筛选：未给（或空串）表示不过滤；**非法值返回错误**而不是静默变成
/// "不过滤"——否则用户选了个不存在的值会看到全部数据。
fn parse_protocol(value: Option<&str>) -> Result<Option<&'static str>, DbError> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(raw) => normalize_protocol(raw)
            .map(Some)
            .ok_or_else(|| DbError::InvalidArgument(format!("未知的协议筛选值：{raw}"))),
    }
}

/// 解析状态筛选：语义同 [`parse_protocol`]。
fn parse_status(value: Option<&str>) -> Result<Option<&'static str>, DbError> {
    match value.map(str::trim) {
        None | Some("") => Ok(None),
        Some(raw) => normalize_status(raw)
            .map(Some)
            .ok_or_else(|| DbError::InvalidArgument(format!("未知的状态筛选值：{raw}"))),
    }
}

/// 概览汇总：今日、本月、最近 30 天趋势、本月模型分布、最近请求。
///
/// 5 个查询在**同一个读事务**里执行：否则并发写入时"今日/本月/趋势/最近"
/// 分别来自不同快照，仪表盘内部数字会互相对不上。
pub fn summary(db: &Db) -> Result<UsageSummary, DbError> {
    let today_date = today();
    let today_string = day_string(today_date);
    let month_start = day_string(
        NaiveDate::from_ymd_opt(today_date.year(), today_date.month(), 1).unwrap_or(today_date),
    );
    let daily_from = day_string(today_date - Duration::days(SUMMARY_DAYS - 1));

    let conn = db.conn()?;
    let tx = conn.unchecked_transaction()?;
    let today_totals = repo::usage_totals(&tx, &today_string, &today_string)?;
    let month_totals = repo::usage_totals(&tx, &month_start, &today_string)?;
    let daily_rows = repo::usage_by_day(&tx, &daily_from, &today_string)?;
    let model_rows = repo::usage_by_dimension(
        &tx,
        repo::UsageDimension::Model,
        &month_start,
        &today_string,
    )?;
    let recent_rows = repo::list_requests(
        &tx,
        &repo::RequestQuery {
            limit: RECENT_LIMIT,
            ..Default::default()
        },
    )?;
    tx.commit()?;

    Ok(aggregate::build_summary(
        aggregate::totals_from_row(&today_totals),
        aggregate::totals_from_row(&month_totals),
        aggregate::daily_from_rows(daily_rows),
        aggregate::models_from_rows(model_rows),
        aggregate::recent_from_rows(&recent_rows),
    ))
}

/// 本机代理最近 `days` 个自然日的费用曲线（人民币）。
///
/// 缺失日期补 0，保证前端折线图横轴连续。
pub fn cost_trend_local(db: &Db, days: i64, credit_to_yuan: f64) -> Result<CostTrend, DbError> {
    let days = days.clamp(1, SUMMARY_DAYS);
    let end = today();
    let start = end - Duration::days(days - 1);
    let rows = {
        let conn = db.conn()?;
        repo::usage_by_day(&conn, &day_string(start), &day_string(end))?
    };
    let by_day: BTreeMap<String, (f64, i64)> = rows
        .into_iter()
        .map(|row| {
            let tokens = row.totals.input_tokens
                + row.totals.output_tokens
                + row.totals.cache_read_tokens
                + row.totals.cache_write_tokens;
            (row.day, (row.totals.cost_yuan, tokens))
        })
        .collect();

    let mut points = Vec::with_capacity(days as usize);
    let mut total_cost = 0.0_f64;
    let mut peak_cost = 0.0_f64;
    for offset in 0..days {
        let date = start + Duration::days(offset);
        let key = day_string(date);
        let (cost, tokens) = by_day.get(&key).copied().unwrap_or((0.0, 0));
        total_cost += cost;
        peak_cost = peak_cost.max(cost);
        points.push(CostTrendPoint {
            label: date.format("%m-%d").to_string(),
            at: key,
            cost,
            tokens: Some(tokens),
        });
    }

    Ok(CostTrend {
        range: "30d".to_string(),
        source: "local".to_string(),
        unit: "cny".to_string(),
        points,
        total_cost,
        peak_cost,
        updated_at: None,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
        credit_to_yuan,
    })
}

/// 请求分页列表。
///
/// 分页有两种模式：`after` 游标（keyset，并发写入下不重复/漏行）与 `offset`
/// （保留给前端 Phase 4 之前的翻页）；同时给出时游标优先。
/// `count` 与 `list` 在同一个读事务里执行，保证 `total` 与 `items` 一致。
pub fn requests(db: &Db, filter: &RequestFilter) -> Result<RequestPage, DbError> {
    let today_string = day_string(today());
    let from_day = filter
        .from
        .as_deref()
        .and_then(day_prefix)
        .unwrap_or_else(|| EPOCH_DAY.to_string());
    let to_day = filter
        .to
        .as_deref()
        .and_then(day_prefix)
        .unwrap_or(today_string);

    let model = filter
        .model
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let protocol = parse_protocol(filter.protocol.as_deref())?;
    let status = parse_status(filter.status.as_deref())?;
    let limit = filter
        .limit
        .unwrap_or(DEFAULT_PAGE_SIZE)
        .clamp(1, MAX_PAGE_SIZE);
    // 深 offset 是线性扫描，钳一下上限（正常翻页远到不了）。
    let offset = filter.offset.unwrap_or(0).min(100_000);
    let include_total = filter.include_total.unwrap_or(true);

    let query = repo::RequestQuery {
        from_day: &from_day,
        to_day: &to_day,
        model_like: model,
        api_format: protocol,
        status,
        after: filter.after.as_ref(),
        limit,
        offset,
    };

    let conn = db.conn()?;
    let tx = conn.unchecked_transaction()?;
    // 游标只决定取哪一页，不影响总数。
    let total = if include_total {
        repo::count_requests(&tx, &query)?
    } else {
        -1
    };
    let rows = repo::list_requests(&tx, &query)?;
    tx.commit()?;

    // 满页才可能有下一页；游标取最后一行（排序键含 id，严格唯一）。
    let next_cursor = if rows.len() == limit as usize {
        rows.last().map(|row| Cursor {
            day_local: row.day_local.clone(),
            started_at: row.started_at.clone(),
            id: row.id,
        })
    } else {
        None
    };

    Ok(RequestPage {
        total,
        items: aggregate::recent_from_rows(&rows),
        next_cursor,
        prev_cursor: filter.after.clone(),
    })
}

/// 单请求详情。
pub fn request_detail(db: &Db, id: i64) -> Result<Option<RequestDetail>, DbError> {
    let conn = db.conn()?;
    let Some(row) = repo::request_detail(&conn, id)? else {
        return Ok(None);
    };

    Ok(Some(RequestDetail {
        request: aggregate::recent_from_row(&row.request),
        upstream_url: row.upstream_url,
        request_bytes: row.request_bytes,
        response_bytes: row.response_bytes,
        first_byte_ms: row.first_byte_ms,
        request_headers: row.request_headers,
        request_body: row.request_body,
        response_body: row.response_body,
        body_truncated: row.body_truncated,
        ended_at: row.ended_at,
        cost_source: row.request.cost_source,
        cost_total_credit: row.cost_total_credit,
        cost_currency: row.cost_currency,
        cost_trace_id: row.cost_trace_id,
        cost_detail_json: row.cost_detail_json,
    }))
}

/// CSV 导出（UTF-8 BOM；文件名含日期区间，重复导出覆盖同名文件）。
///
/// 流式写盘：逐行从 SQLite 读、逐行写 `BufWriter`，内存占用与行数无关。
/// 超过 [`MAX_EXPORT_ROWS`] 截断并标记 `truncated`。
pub fn export_csv(
    db: &Db,
    exports_dir: &Path,
    from_day: &str,
    to_day: &str,
) -> Result<CsvExport, DbError> {
    export_csv_with_limit(db, exports_dir, from_day, to_day, MAX_EXPORT_ROWS)
}

fn export_csv_with_limit(
    db: &Db,
    exports_dir: &Path,
    from_day: &str,
    to_day: &str,
    max_rows: usize,
) -> Result<CsvExport, DbError> {
    std::fs::create_dir_all(exports_dir)?;
    let path = exports_dir.join(format!("usage_{from_day}_{to_day}.csv"));
    // 先写 .part 再改名：中途失败不会把上一次成功的导出截成半截文件。
    let temp_path = exports_dir.join(format!("usage_{from_day}_{to_day}.csv.part"));

    let (rows, truncated) = match write_export_file(db, &temp_path, from_day, to_day, max_rows) {
        Ok(result) => result,
        Err(err) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(err);
        }
    };
    let bytes = std::fs::metadata(&temp_path)?.len();
    // Windows 的 rename 目标已存在时会失败，先删旧文件再原子替换。
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    std::fs::rename(&temp_path, &path)?;

    Ok(CsvExport {
        path: path.to_string_lossy().into_owned(),
        rows,
        bytes,
        truncated,
    })
}

fn write_export_file(
    db: &Db,
    temp_path: &Path,
    from_day: &str,
    to_day: &str,
    max_rows: usize,
) -> Result<(usize, bool), DbError> {
    let file = std::fs::File::create(temp_path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(EXPORT_HEADER.as_bytes())?;

    let conn = db.conn()?;
    let (rows, truncated) = repo::export_rows_stream(&conn, from_day, to_day, max_rows, |row| {
        writeln!(
            writer,
            "{},{},{},{},{},{},{},{},{},{},{:.4}",
            csv_field(&row.day),
            csv_field(&row.api_format),
            csv_field(&row.model),
            csv_field(&row.client),
            row.request_count,
            row.error_count,
            row.input_tokens,
            row.output_tokens,
            row.cache_read_tokens,
            row.cache_write_tokens,
            // 固定 4 位小数：与界面 round4 一致，避免长尾与科学计数观感。
            row.cost_yuan
        )?;
        Ok(())
    })?;
    writer.flush()?;
    Ok((rows, truncated))
}

/// CSV 字段转义：含逗号/引号/换行时加引号并把 `"` 翻倍。
///
/// 先做公式中和再决定是否加引号（Excel/WPS 连引号内的 `=...` 也会执行）。
fn csv_field(value: &str) -> String {
    let value = sanitize_formula(value);
    if value
        .chars()
        .any(|ch| matches!(ch, ',' | '"' | '\n' | '\r'))
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.into_owned()
    }
}

/// 中和 CSV 公式注入：以 `= + - @` 或制表符/回车开头的单元格会被 Excel/WPS
/// 当公式执行，前置单引号让它按文本显示。
fn sanitize_formula(value: &str) -> Cow<'_, str> {
    match value.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => Cow::Owned(format!("'{value}")),
        _ => Cow::Borrowed(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_db() -> Db {
        let db = Db::open_in_memory().expect("内存库应可打开");
        crate::db::run_migrations(&db).expect("迁移应成功");
        db
    }

    #[test]
    fn csv_field_escapes_specials() {
        assert_eq!(csv_field("gpt-4o"), "gpt-4o");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
    }

    /// CSV 公式注入——外部可控的 model/client 以 = + - @ 开头会被 Excel 执行。
    #[test]
    fn csv_field_neutralizes_formula_prefixes() {
        for raw in ["=1+1", "+x", "-x", "@SUM(A1)", "\tcmd", "\rcmd"] {
            let out = csv_field(raw);
            // 回车会触发加引号，去掉外层引号后再看前缀。
            let cell = out.trim_matches('"');
            assert!(cell.starts_with('\''), "{raw:?} 应被中和：{out:?}");
            assert!(cell.contains(raw), "原始内容必须保留：{out:?}");
        }
        // 危险开头 + 逗号：先中和再加引号
        assert_eq!(csv_field("=a,b"), "\"'=a,b\"");
        // 普通文本不受影响
        assert_eq!(csv_field("claude-sonnet-4"), "claude-sonnet-4");
    }

    /// 导出时 model 被中和、费用固定 4 位小数。
    #[test]
    fn export_csv_sanitizes_model_and_rounds_cost() {
        let db = empty_db();
        {
            let conn = db.conn().expect("取连接应成功");
            conn.execute(
                "INSERT INTO usage_daily \
                   (day, api_format, model, client, request_count, error_count, input_tokens, \
                    output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
                    total_duration_ms, updated_at) \
                 VALUES ('2026-10-22', 'anthropic', '=cmd|calc', 'claude-code', 1, 0, 10, 5, 0, 0, \
                         0.0123456789, 100, '2026-10-22T00:00:00Z')",
                [],
            )
            .expect("插入日聚合应成功");
        }

        let dir = std::env::temp_dir().join(format!("chl-csv-inject-{}", uuid::Uuid::new_v4()));
        let export = export_csv(&db, &dir, "2026-10-22", "2026-10-22").expect("导出应成功");
        let content = std::fs::read_to_string(&export.path).expect("导出文件应可读");
        assert!(
            content.contains("'=cmd|calc"),
            "模型名必须被中和：{content}"
        );
        assert!(
            !content.contains(",=cmd"),
            "不得出现裸公式单元格：{content}"
        );
        assert!(content.contains("0.0123"), "费用应为 4 位小数：{content}");
        assert!(
            !content.contains("0.0123456789"),
            "费用不应带长尾：{content}"
        );
        assert!(!export.truncated, "单行导出不应被截断");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 行数超过上限时截断：只写上限行数，并如实标记 truncated。
    #[test]
    fn export_csv_truncates_at_row_limit() {
        let db = empty_db();
        {
            let conn = db.conn().expect("取连接应成功");
            for (index, model) in ["m-1", "m-2", "m-3"].iter().enumerate() {
                conn.execute(
                    "INSERT INTO usage_daily \
                       (day, api_format, model, client, request_count, error_count, input_tokens, \
                        output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
                        total_duration_ms, updated_at) \
                     VALUES ('2026-10-22', 'anthropic', ?1, 'claude-code', ?2, 0, 1, 1, 0, 0, \
                             0.5, 10, '2026-10-22T00:00:00Z')",
                    rusqlite::params![model, index as i64],
                )
                .expect("插入日聚合应成功");
            }
        }

        let dir = std::env::temp_dir().join(format!("chl-csv-cap-{}", uuid::Uuid::new_v4()));
        let export =
            export_csv_with_limit(&db, &dir, "2026-10-22", "2026-10-22", 2).expect("导出应成功");
        assert_eq!(export.rows, 2, "只应写出上限行数");
        assert!(export.truncated, "超出上限必须标记 truncated");
        let content = std::fs::read_to_string(&export.path).expect("导出文件应可读");
        // 表头 + 2 行数据
        assert_eq!(content.lines().count(), 3, "实际内容：{content}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn day_prefix_accepts_iso_date_and_datetime() {
        assert_eq!(day_prefix("2026-10-20"), Some("2026-10-20".to_string()));
        assert_eq!(
            day_prefix("2026-10-20T10:00:00+08:00"),
            Some("2026-10-20".to_string())
        );
        assert_eq!(day_prefix("not-a-date"), None);
    }

    #[test]
    fn empty_database_summary_is_zero_with_null_rates() {
        let db = empty_db();
        let summary = summary(&db).expect("汇总应成功");
        assert_eq!(summary.today.requests, 0);
        assert_eq!(summary.today.cache_hit_rate, None);
        assert_eq!(summary.today.error_rate, None);
        assert_eq!(summary.month.cost_yuan, 0.0);
        assert!(summary.daily.is_empty());
        assert!(summary.models.is_empty());
        assert!(summary.recent.is_empty());
        assert!(!summary.generated_at.is_empty());
    }

    #[test]
    fn local_cost_trend_fills_thirty_days() {
        let db = empty_db();
        let trend = cost_trend_local(&db, 30, 0.01).expect("费用趋势应成功");
        assert_eq!(trend.range, "30d");
        assert_eq!(trend.source, "local");
        assert_eq!(trend.unit, "cny");
        assert_eq!(trend.points.len(), 30);
        assert_eq!(trend.total_cost, 0.0);
        assert_eq!(trend.peak_cost, 0.0);
        assert_eq!(trend.credit_to_yuan, 0.01);
    }

    #[test]
    fn empty_database_request_page_is_empty() {
        let db = empty_db();
        let page = requests(&db, &RequestFilter::default()).expect("列表应成功");
        assert!(page.items.is_empty());
        assert_eq!(page.total, 0);
        assert!(page.next_cursor.is_none());
        assert!(page.prev_cursor.is_none());

        let filtered = requests(
            &db,
            &RequestFilter {
                from: Some("2026-10-01T00:00:00Z".to_string()),
                to: Some("2026-10-20".to_string()),
                protocol: Some("anthropic".to_string()),
                status: Some("success".to_string()),
                limit: Some(10),
                offset: Some(0),
                ..Default::default()
            },
        )
        .expect("带筛选的列表应成功");
        assert_eq!(filtered.total, 0);
    }

    /// 非法筛选值必须报错，而不是静默退回"不过滤"；空串仍按未选择处理。
    #[test]
    fn invalid_protocol_and_status_are_rejected() {
        let db = empty_db();
        let error = requests(
            &db,
            &RequestFilter {
                protocol: Some("grpc".to_string()),
                ..Default::default()
            },
        )
        .expect_err("未知协议应被拒绝");
        assert!(
            matches!(error, DbError::InvalidArgument(_)),
            "错误类型应为参数无效：{error}"
        );

        let error = requests(
            &db,
            &RequestFilter {
                status: Some("timeout".to_string()),
                ..Default::default()
            },
        )
        .expect_err("未知状态应被拒绝");
        assert!(
            matches!(error, DbError::InvalidArgument(_)),
            "错误类型应为参数无效：{error}"
        );

        let page = requests(
            &db,
            &RequestFilter {
                protocol: Some("  ".to_string()),
                status: Some(String::new()),
                ..Default::default()
            },
        )
        .expect("空串表示未选择，不应报错");
        assert_eq!(page.total, 0);
    }

    #[test]
    fn request_detail_missing_returns_none() {
        let db = empty_db();
        assert!(request_detail(&db, 1).expect("详情查询应成功").is_none());
    }

    /// 直接插一行请求（绕过 recorder），用于验证筛选口径。
    fn insert_request(
        db: &Db,
        started_at: &str,
        day_local: &str,
        status_code: Option<i64>,
        error_kind: Option<&str>,
        status_kind: &str,
    ) {
        let conn = db.conn().expect("取连接应成功");
        conn.execute(
            "INSERT INTO proxy_session_requests \
               (method, request_path, api_format, status_code, error_kind, \
                started_at, ended_at, day_local, status_kind) \
             VALUES ('POST', '/v1/messages', 'anthropic', ?1, ?2, ?3, ?3, ?4, ?5)",
            rusqlite::params![status_code, error_kind, started_at, day_local, status_kind],
        )
        .expect("插入请求行应成功");
    }

    #[test]
    fn requests_filter_uses_local_day_not_utc_prefix() {
        let db = empty_db();
        // UTC 是 10-21，但按 UTC+8 属于 10-22 凌晨 → 「今天」必须能查到。
        insert_request(
            &db,
            "2026-10-21T16:30:00Z",
            "2026-10-22",
            Some(200),
            None,
            "success",
        );
        // UTC 已是 10-22，但本地日是 10-21 → 查 10-22 不能命中。
        insert_request(
            &db,
            "2026-10-22T01:00:00Z",
            "2026-10-21",
            Some(200),
            None,
            "success",
        );

        let page = requests(
            &db,
            &RequestFilter {
                from: Some("2026-10-22".to_string()),
                to: Some("2026-10-22".to_string()),
                limit: Some(50),
                offset: Some(0),
                ..Default::default()
            },
        )
        .expect("列表应成功");

        assert_eq!(page.total, 1, "只能命中本地日为 10-22 的那一行");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].created_at, "2026-10-21T16:30:00Z");
    }

    /// 筛选命中的行，其渲染状态必须等于所选筛选项。
    #[test]
    fn status_filter_matches_rendered_status() {
        let db = empty_db();
        insert_request(
            &db,
            "2026-10-20T01:00:00Z",
            "2026-10-20",
            Some(200),
            None,
            "success",
        );
        insert_request(
            &db,
            "2026-10-20T02:00:00Z",
            "2026-10-20",
            Some(500),
            None,
            "error",
        );
        insert_request(
            &db,
            "2026-10-20T03:00:00Z",
            "2026-10-20",
            Some(502),
            Some("upstream_error"),
            "error",
        );
        insert_request(
            &db,
            "2026-10-20T04:00:00Z",
            "2026-10-20",
            None,
            Some("client_aborted"),
            "cancelled",
        );
        insert_request(
            &db,
            "2026-10-20T05:00:00Z",
            "2026-10-20",
            None,
            None,
            "unknown",
        );

        for status in ["success", "error", "cancelled", "unknown"] {
            let page = requests(
                &db,
                &RequestFilter {
                    status: Some(status.to_string()),
                    ..Default::default()
                },
            )
            .expect("带状态筛选的列表应成功");
            assert!(!page.items.is_empty(), "每个状态都应至少命中一行：{status}");
            assert_eq!(
                page.total as usize,
                page.items.len(),
                "total 与 items 必须一致"
            );
            for item in &page.items {
                assert_eq!(item.status, status, "筛选结果里出现了不同状态的行");
            }
        }

        let all = requests(&db, &RequestFilter::default()).expect("全量列表应成功");
        assert_eq!(all.total, 5);
    }

    /// keyset 翻页在并发写入/同秒多行下不能重复或漏行；`total` 不受游标影响。
    #[test]
    fn keyset_pagination_has_no_duplicate_or_gap() {
        let db = empty_db();
        for index in 0..25 {
            let day = if index < 10 {
                "2026-10-20"
            } else {
                "2026-10-21"
            };
            // 全部同一秒：排序完全依赖 (day_local, started_at, id) 的 id tie-break。
            insert_request(
                &db,
                &format!("{day}T01:00:00Z"),
                day,
                Some(200),
                None,
                "success",
            );
        }

        let filter = RequestFilter {
            from: Some("2026-10-20".to_string()),
            to: Some("2026-10-21".to_string()),
            limit: Some(7),
            ..Default::default()
        };

        let first = requests(&db, &filter).expect("首页应成功");
        assert_eq!(first.items.len(), 7);
        assert_eq!(first.total, 25);
        assert!(first.next_cursor.is_some(), "满页必须给出下一页游标");
        assert!(first.prev_cursor.is_none(), "首页没有进入游标");

        let mut seen: Vec<String> = first.items.iter().map(|item| item.id.clone()).collect();
        let mut cursor = first.next_cursor.clone();
        let mut pages = 1;
        while let Some(after) = cursor {
            let page = requests(
                &db,
                &RequestFilter {
                    after: Some(after),
                    ..filter.clone()
                },
            )
            .expect("游标翻页应成功");
            assert_eq!(page.total, 25, "total 不应受游标影响");
            seen.extend(page.items.iter().map(|item| item.id.clone()));
            cursor = page.next_cursor.clone();
            pages += 1;
            assert!(pages < 20, "翻页未收敛");
        }

        assert_eq!(seen.len(), 25, "游标翻页必须覆盖所有行");
        let mut unique = seen.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 25, "游标翻页不得重复行");
    }

    /// `include_total = false` 时跳过 COUNT，`total` 返回 -1。
    #[test]
    fn include_total_false_skips_count() {
        let db = empty_db();
        insert_request(
            &db,
            "2026-10-20T01:00:00Z",
            "2026-10-20",
            Some(200),
            None,
            "success",
        );
        let page = requests(
            &db,
            &RequestFilter {
                include_total: Some(false),
                ..Default::default()
            },
        )
        .expect("列表应成功");
        assert_eq!(page.total, -1, "未统计时 total 为 -1");
        assert_eq!(page.items.len(), 1);
    }

    #[test]
    fn export_csv_writes_header_even_when_empty() {
        let db = empty_db();
        let dir = std::env::temp_dir().join(format!("chl-csv-test-{}", uuid::Uuid::new_v4()));
        let export = export_csv(&db, &dir, "2026-01-01", "2026-01-02").expect("导出应成功");
        assert_eq!(export.rows, 0);
        let content = std::fs::read_to_string(&export.path).expect("导出文件应可读");
        assert!(content.contains("day,api_format,model,client"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
