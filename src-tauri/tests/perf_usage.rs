//! 用量查询的性能与"查询计划"回归测试。
//!
//! - `usage_queries_use_index_scan`：**普通测试**，先种少量行再断言
//!   `EXPLAIN QUERY PLAN` 的结构（命中索引、无全表扫描、无临时排序 B 树）。
//!   结构断言与机器快慢无关，所以放在默认 `cargo test` 里跑。
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

/// 列表、计数与用量聚合都必须走索引。这条断言挡住"把索引改坏/把 WHERE 写成
/// 可选过滤"导致的性能回退。
///
/// 先种少量行再取计划：空表上"碰巧"选对索引不能证明真实数据分布下也选对。
#[test]
fn usage_queries_use_index_scan() {
    let db = test_db();
    seed_requests(&db, 50);
    seed_usage_daily(&db, 40);
    seed_quota_snapshots(&db, 20);
    let conn = db.conn().expect("应能取连接");

    // 1) 列表：keyset 索引 + 区间扫描，排序由索引满足（无临时 B 树）。
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

    // 2) 计数同样不得退化成全表扫描。
    let (count_sql, count_params) = sql::count_requests(&list_query());
    let count_plan = plan_rows(&conn, &count_sql, count_params);
    assert!(
        !count_plan.contains("SCAN proxy_session_requests"),
        "计数不得全表扫描：\n{count_plan}"
    );

    // 3) 状态筛选 + 游标：等值列打头的复合索引必须同时满足筛选、日区间与排序。
    let cursor = sql::Cursor {
        day_local: "2026-10-20".to_string(),
        started_at: "2026-10-20T01:00:00Z".to_string(),
        id: 1,
    };
    let (status_sql, status_params) = sql::list_requests(&sql::RequestQuery {
        status: Some("success"),
        after: Some(&cursor),
        ..list_query()
    });
    let status_plan = plan_rows(&conn, &status_sql, status_params);
    assert!(
        status_plan.contains("ix_psr_status_day_started"),
        "状态筛选 + 游标应命中 (status_kind, day_local, started_at, id) 索引，实际计划：\n{status_plan}"
    );
    assert!(
        !status_plan.contains("SCAN proxy_session_requests"),
        "状态筛选不得全表扫描：\n{status_plan}"
    );
    assert!(
        !status_plan.contains("USE TEMP B-TREE FOR ORDER BY"),
        "状态筛选的排序必须由索引满足：\n{status_plan}"
    );

    // 4) 日区间总量 / 按日聚合：覆盖索引（index-only，不回表）。
    let (totals_sql, totals_params) = (sql::usage_totals(), day_params("2026-08-01", "2026-10-31"));
    let totals_plan = plan_rows(&conn, &totals_sql, totals_params);
    assert!(
        totals_plan.contains("ix_usage_daily_day_cover"),
        "日区间汇总应命中覆盖索引，实际计划：\n{totals_plan}"
    );

    let (by_day_sql, by_day_params) = (sql::usage_by_day(), day_params("2026-08-01", "2026-10-31"));
    let by_day_plan = plan_rows(&conn, &by_day_sql, by_day_params);
    assert!(
        by_day_plan.contains("ix_usage_daily_day_cover"),
        "按日聚合应命中覆盖索引，实际计划：\n{by_day_plan}"
    );
    assert!(
        !by_day_plan.contains("USE TEMP B-TREE"),
        "按日分组/排序应由覆盖索引顺序满足：\n{by_day_plan}"
    );

    // 5) 最新成功额度快照：部分索引（status = 'ok'）。
    let quota_plan = plan_rows(
        &conn,
        &sql::latest_quota_snapshot(),
        vec![
            rusqlite::types::Value::Text("global".to_string()),
            rusqlite::types::Value::Null,
        ],
    );
    assert!(
        quota_plan.contains("ix_quota_snapshots_ok_latest"),
        "最新成功快照应命中部分索引，实际计划：\n{quota_plan}"
    );
}

fn day_params(from: &str, to: &str) -> Vec<rusqlite::types::Value> {
    vec![
        rusqlite::types::Value::Text(from.to_string()),
        rusqlite::types::Value::Text(to.to_string()),
    ]
}

/// 种少量 `usage_daily` 行（每天一行，供覆盖索引的计划断言使用）。
fn seed_usage_daily(db: &Db, rows: i64) {
    let mut conn = db.conn().expect("应能取连接");
    let tx = conn.transaction().expect("应能开事务");
    {
        let mut stmt = tx
            .prepare(
                "INSERT OR REPLACE INTO usage_daily \
                   (day, api_format, model, client, request_count, error_count, input_tokens, \
                    output_tokens, cache_read_tokens, cache_write_tokens, cost_yuan, \
                    total_duration_ms, updated_at) \
                 VALUES (?1, 'anthropic', 'claude-sonnet-4', 'claude-code', 1, 0, 10, 5, 2, 1, \
                         0.01, 100, '2026-10-01T00:00:00Z')",
            )
            .expect("应能准备插入");
        for index in 0..rows {
            let day = format!("2026-{:02}-{:02}", 8 + index / 28, 1 + index % 28);
            stmt.execute(rusqlite::params![day])
                .expect("插入日聚合应成功");
        }
    }
    tx.commit().expect("提交应成功");
}

/// 种少量额度快照（全部成功，供部分索引的计划断言使用）。
fn seed_quota_snapshots(db: &Db, rows: i64) {
    let mut conn = db.conn().expect("应能取连接");
    let tx = conn.transaction().expect("应能开事务");
    {
        let mut stmt = tx
            .prepare(
                "INSERT INTO quota_snapshots (quota_type, status, monthly_quota, fetched_at) \
                 VALUES ('global', 'ok', ?1, ?2)",
            )
            .expect("应能准备插入");
        for index in 0..rows {
            let fetched_at = format!("2026-10-{:02}T00:00:00Z", 1 + index % 28);
            stmt.execute(rusqlite::params![index as f64, fetched_at])
                .expect("插入额度快照应成功");
        }
    }
    tx.commit().expect("提交应成功");
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
