//! 用量查询命令（返回结构与前端 `src/lib/types.ts` 对齐）。
//!
//! 所有 SQLite 访问都经 `db::blocking::run`：`rusqlite` 是同步 API，直接在 async
//! fn 里调用会占住 Tokio worker（取连接最长还要等 5s），代理与 IPC 一起排队。

use chrono::{Duration, Local, SecondsFormat, Utc};
use tauri::State;

use crate::{
    db::blocking,
    state::AppState,
    usage::{
        queries, CostTrend, CostTrendPoint, RequestDetail, RequestFilter, RequestPage, UsageSummary,
    },
};

#[tauri::command]
pub async fn usage_summary(state: State<'_, AppState>) -> Result<UsageSummary, String> {
    blocking::run(&state.db, |db| queries::summary(&db))
        .await
        .map_err(|err| format!("读取用量汇总失败：{err}"))
}

/// 费用曲线：24h 走云端 `/vega/user_hourly_cost`，30d 走本机 usage_daily。
#[tauri::command]
pub async fn cost_trend_get(
    state: State<'_, AppState>,
    range: String,
) -> Result<CostTrend, String> {
    let credit_to_yuan = state.settings.read().await.credit_to_yuan;
    match range.as_str() {
        "24h" => {
            let end = Local::now();
            let start = end - Duration::hours(24);
            let end_text = end.format("%Y-%m-%d %H:%M:%S").to_string();
            let start_text = start.format("%Y-%m-%d %H:%M:%S").to_string();
            let gateway_url = state.settings.read().await.gateway_url.clone();
            let response = state
                .auth
                .hourly_cost(&gateway_url, &start_text, &end_text)
                .await
                .map_err(|err| err.to_string())?;

            let mut records = response.records;
            records.sort_by(|left, right| left.snapshot_hour.cmp(&right.snapshot_hour));
            let mut points = Vec::with_capacity(records.len());
            let mut total_cost = 0.0_f64;
            let mut peak_cost = 0.0_f64;
            let mut updated_at: Option<String> = None;
            for record in records {
                let cost = record.cost.max(0.0);
                total_cost += cost;
                peak_cost = peak_cost.max(cost);
                if let Some(synced) = record.synced_at {
                    updated_at =
                        Some(updated_at.map_or(synced.clone(), |current| current.max(synced)));
                }
                let label = record
                    .snapshot_hour
                    .split(' ')
                    .nth(1)
                    .and_then(|time| time.get(0..5))
                    .unwrap_or(record.snapshot_hour.as_str())
                    .to_string();
                points.push(CostTrendPoint {
                    label,
                    at: record.snapshot_hour,
                    cost,
                    tokens: record.tokens,
                });
            }
            Ok(CostTrend {
                range: "24h".to_string(),
                source: "cloud".to_string(),
                unit: "points".to_string(),
                points,
                total_cost,
                peak_cost,
                updated_at,
                generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
                credit_to_yuan,
            })
        }
        "30d" => blocking::run(&state.db, move |db| {
            queries::cost_trend_local(&db, 30, credit_to_yuan)
        })
        .await
        .map_err(|err| format!("读取本机费用趋势失败：{err}")),
        _ => Err(format!("不支持的费用曲线范围：{range}")),
    }
}

#[tauri::command]
pub async fn usage_requests(
    state: State<'_, AppState>,
    filter: Option<RequestFilter>,
) -> Result<RequestPage, String> {
    let filter = filter.unwrap_or_default();
    blocking::run(&state.db, move |db| queries::requests(&db, &filter))
        .await
        .map_err(|err| format!("读取请求列表失败：{err}"))
}

#[tauri::command]
pub async fn usage_request_detail(
    state: State<'_, AppState>,
    id: i64,
) -> Result<Option<RequestDetail>, String> {
    blocking::run(&state.db, move |db| queries::request_detail(&db, id))
        .await
        .map_err(|err| format!("读取请求详情失败：{err}"))
}
