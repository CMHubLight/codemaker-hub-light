//! 用量查询的性能与"查询计划"回归测试。
//!
//! - `usage_queries_use_index_scan`：**普通测试**，只断言 `EXPLAIN QUERY PLAN`
//!   的结构（命中索引、无全表扫描、无临时排序 B 树）。结构断言与机器快慢无关，
//!   所以放在默认 `cargo test` 里跑。
//! - `list_requests_latency_100k`：`#[ignore]`，灌 10 万行后跑 20 次列表查询，
//!   **只打印** p50/p95 与查询计划，不做时间断言（避免 CI 抖动误报）。
//!   手动运行：`cargo test --test perf_usage -- --ignored --nocapture`
//!
//! 这是集成测试（走 `codemaker_hub_light_lib` 的公开 API），验证的是
//! "SQL 构造 + 索引 + 事务"在真实库上的组合结果。

use std::time::{Duration, Instant};

use codemaker_hub_light_lib::{
    db::{self, repo, sql, Db},
    usage::{queries, RequestFilter},
};

/// 与 `db/pool.rs` 的可读连接数一致，保证测试库和真实库的索引集相同。
fn test_db() -> Db {
    let db = Db::open_in_memory().expect("内存库应可打开");
    db::run_migrations(&db).expect("迁移应成功");
    db
}

fn plan_rows(
    conn: &rusqlite::Connection,
    sql_text: &str,
    params: Vec<rusqlite::types::Value>,
) -> String {
    let mut stmt = conn
        .prepare(&format!("EXPLAIN QUERY PLAN {sql_text}"))
        .expect("EXPLAIN QUERY PLAN 应可准备");
    let rows = stmt
        .query_map(rusqlite::params_from_iter(params), |row| {
            row.get::<_, String>(3)
        })
        .expect("查询计划应可读");
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .expect("查询计划应可收集")
        .join("\n")
}

fn list_query() -> sql::RequestQuery<'static> {
    sql::RequestQuery {
        from_day: "2026-09-01",
        to_day: "2026-10-31",
        limit: 50,
        ..Default::default()
    }
}

/// 列表与计数都必须走索引区间扫描。这条断言挡住"把索引改坏/把 WHERE 写成
/// 可选过滤"导致的性能回退——只改 SQL 就会让它失败，与数据量无关。
#[test]
fn usage_queries_use_index_scan() {
    let db = test_db();
    let conn = db.conn().expect("应能取连接");

    let (list_sql, list_params) = sql::list_requests(&list_query());
    let list_plan = plan_rows(&conn, &list_sql, list_params);
    assert!(
        list_plan.contains("ix_psr_day_started_id"),
        "列表应命中 keyset 索引，实际计划：\n{list_plan}"
    );
    assert!(
        !list_plan.contains("SCAN proxy_session_requests"),
        "列表不得全表扫描：\n{list_plan}"
    );
    assert!(
        !list_plan.contains("USE TEMP B-TREE FOR ORDER BY"),
        "排序必须由索引满足：\n{list_plan}"
    );

    let (count_sql, count_params) = sql::count_requests(&list_query());
    let count_plan = plan_rows(&conn, &count_sql, count_params);
    assert!(
        !count_plan.contains("SCAN proxy_session_requests"),
        "计数不得全表扫描：\n{count_plan}"
    );
}

/// 单事务灌 `rows` 行请求（每行一个不同 id；同一天内 started_at 相同，
/// 专门让排序依赖 `id` tie-break）。
fn seed_requests(db: &Db, rows: i64) {
    let mut conn = db.conn().expect("应能取连接");
    let tx = conn.transaction().expect("应能开事务");
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO proxy_session_requests \
                   (method, request_path, api_format, model, status_code, started_at, \
                    day_local, status_kind) \
                 VALUES ('POST', '/v1/messages', 'anthropic', ?1, 200, ?2, ?3, 'success')",
            )
            .expect("应能准备插入");
        for index in 0..rows {
            let day_offset = index * 30 / rows.max(1);
            let day = format!(
                "2026-{:02}-{:02}",
                8 + (day_offset / 28),
                1 + (day_offset % 28)
            );
            stmt.execute(rusqlite::params![
                "claude-sonnet-4",
                format!("{day}T01:00:00Z"),
                day
            ])
            .expect("插入请求行应成功");
        }
    }
    tx.commit().expect("提交应成功");
}

fn percentile(sorted: &[Duration], percent: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let index = ((sorted.len() as f64 - 1.0) * percent).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// 10 万行下的列表查询延迟（只打印，不断言）。
#[test]
#[ignore = "性能采样：需要手动运行 --ignored --nocapture"]
fn list_requests_latency_100k() {
    let db = test_db();
    let seed_started = Instant::now();
    seed_requests(&db, 100_000);
    println!("seed 100k rows: {:?}", seed_started.elapsed());

    let conn = db.conn().expect("应能取连接");
    let (list_sql, list_params) = sql::list_requests(&list_query());
    println!("LIST PLAN:\n{}", plan_rows(&conn, &list_sql, list_params));
    let (count_sql, count_params) = sql::count_requests(&list_query());
    println!(
        "COUNT PLAN:\n{}",
        plan_rows(&conn, &count_sql, count_params)
    );
    drop(conn);

    let filter = RequestFilter {
        from: Some("2026-08-01".to_string()),
        to: Some("2026-10-31".to_string()),
        limit: Some(50),
        ..Default::default()
    };
    let mut samples: Vec<Duration> = Vec::with_capacity(20);
    for round in 0..20 {
        // 每轮都带 offset，模拟"翻到不同页"；游标路径另测一次首页。
        let started = Instant::now();
        let page = queries::requests(
            &db,
            &RequestFilter {
                offset: Some(round * 50),
                ..filter.clone()
            },
        )
        .expect("列表查询应成功");
        samples.push(started.elapsed());
        assert_eq!(page.items.len(), 50, "每页应取满 50 行");
    }
    samples.sort();
    println!(
        "list_requests (offset paging, 100k rows): p50={:?} p95={:?} max={:?}",
        percentile(&samples, 0.50),
        percentile(&samples, 0.95),
        samples.last().copied().unwrap_or_default()
    );

    let mut cursor_samples: Vec<Duration> = Vec::with_capacity(20);
    let mut cursor = None;
    for _ in 0..20 {
        let started = Instant::now();
        let page = queries::requests(
            &db,
            &RequestFilter {
                after: cursor.clone(),
                ..filter.clone()
            },
        )
        .expect("游标查询应成功");
        cursor_samples.push(started.elapsed());
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    cursor_samples.sort();
    println!(
        "list_requests (keyset paging, 100k rows): p50={:?} p95={:?} samples={}",
        percentile(&cursor_samples, 0.50),
        percentile(&cursor_samples, 0.95),
        cursor_samples.len()
    );

    // 关掉 COUNT：keyset + include_total=false 是"无限滚动"的目标形态。
    let mut no_total_samples: Vec<Duration> = Vec::with_capacity(20);
    let mut cursor = None;
    for _ in 0..20 {
        let started = Instant::now();
        let page = queries::requests(
            &db,
            &RequestFilter {
                after: cursor.clone(),
                include_total: Some(false),
                ..filter.clone()
            },
        )
        .expect("游标查询应成功");
        no_total_samples.push(started.elapsed());
        assert_eq!(page.total, -1, "未统计时 total 必须是 -1");
        cursor = page.next_cursor;
    }
    no_total_samples.sort();
    println!(
        "list_requests (keyset + no COUNT, 100k rows): p50={:?} p95={:?}",
        percentile(&no_total_samples, 0.50),
        percentile(&no_total_samples, 0.95)
    );

    // 详情查询也走一遍（主键 + 唯一索引）。
    let started = Instant::now();
    let first_id: i64 = {
        let conn = db.conn().expect("应能取连接");
        conn.query_row(
            "SELECT id FROM proxy_session_requests ORDER BY id ASC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("读取 id 应成功")
    };
    let detail = queries::request_detail(&db, first_id).expect("详情应成功");
    println!(
        "request_detail: exists={} elapsed={:?}",
        detail.is_some(),
        started.elapsed()
    );
}

/// 导出是逐行流式写盘：10 万行区间内导出不会把结果集读进内存。
#[test]
#[ignore = "性能采样：需要手动运行 --ignored --nocapture"]
fn export_csv_streams_100k_rows() {
    let db = test_db();
    {
        let mut conn = db.conn().expect("应能取连接");
        let tx = conn.transaction().expect("应能开事务");
        let mut stmt = tx
            .prepare(
                "INSERT INTO usage_daily \
                   (day, api_format, model, client, request_count, error_count, input_tokens, \
                    output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
                    total_duration_ms, updated_at) \
                 VALUES (?1, 'anthropic', ?2, 'claude-code', 1, 0, 10, 5, 0, 0, 0.5, 100, ?3)",
            )
            .expect("应能准备插入");
        for index in 0..100_000 {
            // model 唯一 ⇒ 不撞 ux_usage_daily_dim；day 全部落在 2026 年内。
            let day = format!(
                "2026-{:02}-{:02}",
                1 + (index / 3_500) % 12,
                1 + (index / 125) % 28
            );
            stmt.execute(rusqlite::params![
                day,
                format!("model-{index}"),
                "2026-10-01T00:00:00Z"
            ])
            .expect("插入日聚合应成功");
        }
        drop(stmt);
        tx.commit().expect("提交应成功");
    }

    let dir = std::env::temp_dir().join(format!("chl-perf-export-{}", uuid::Uuid::new_v4()));
    let started = Instant::now();
    let export = queries::export_csv(&db, &dir, "2026-01-01", "2026-12-31").expect("导出应成功");
    println!(
        "export 100k rows: rows={} bytes={} truncated={} elapsed={:?}",
        export.rows,
        export.bytes,
        export.truncated,
        started.elapsed()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// 跨层小校验：游标可以直接放进 `RequestFilter`（序列化/反序列化形状由
/// `Cursor` 的 serde 派生保证），`FromRow` 也能按列名解出总量行。
#[test]
fn cursor_filter_and_named_row_decoding() {
    let cursor = repo::Cursor {
        day_local: "2026-10-20".to_string(),
        started_at: "2026-10-20T01:00:00Z".to_string(),
        id: 7,
    };
    let filter = RequestFilter {
        after: Some(cursor.clone()),
        ..Default::default()
    };
    assert_eq!(filter.after.as_ref().map(|c| c.id), Some(7));

    let db = test_db();
    let conn = db.conn().expect("应能取连接");
    // 空表上的聚合行：命名列解码 + COALESCE 默认值一起验证。
    let totals = repo::usage_totals(&conn, "2026-01-01", "2026-12-31").expect("汇总应成功");
    assert_eq!(totals.request_count, 0);
    assert_eq!(totals.cost_yuan, 0.0);
}
