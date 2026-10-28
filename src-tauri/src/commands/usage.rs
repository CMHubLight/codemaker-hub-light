//! 用量查询与导出命令（返回结构与前端 `src/lib/types.ts` 对齐）。
//!
//! 所有 SQLite 访问都经 `db::blocking::run`：`rusqlite` 是同步 API，直接在 async
//! fn 里调用会占住 Tokio worker（取连接最长还要等 5s），代理与 IPC 一起排队。

use chrono::{Duration, Local, NaiveDate, SecondsFormat, Utc};
use tauri::State;

use crate::{
    db::blocking,
    state::AppState,
    usage::{
        queries, CostTrend, CostTrendPoint, RequestDetail, RequestFilter, RequestPage,
        UsageExportResult, UsageSummary,
    },
};

const EXPORT_DEFAULT_DAYS: i64 = 30;

fn parse_day(value: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(value.trim(), "%Y-%m-%d")
        .map_err(|_| format!("日期格式应为 YYYY-MM-DD：{value}"))
}

/// 导出区间：缺省最近 30 天（含今天）。
fn resolve_export_range(
    from_day: Option<String>,
    to_day: Option<String>,
) -> Result<(String, String), String> {
    let today = Local::now().date_naive();
    let from = match from_day.as_deref() {
        Some(value) => parse_day(value)?,
        None => today - Duration::days(EXPORT_DEFAULT_DAYS - 1),
    };
    let to = match to_day.as_deref() {
        Some(value) => parse_day(value)?,
        None => today,
    };
    if from > to {
        return Err("起始日期不能晚于结束日期".to_string());
    }
    Ok((from.to_string(), to.to_string()))
}

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

/// 导出 CSV 到 `~/.codemaker-hub-light/exports/`（UTF-8 BOM，Excel 友好）。
#[tauri::command]
pub async fn usage_export_csv(
    state: State<'_, AppState>,
    from_day: Option<String>,
    to_day: Option<String>,
) -> Result<UsageExportResult, String> {
    let (from, to) = resolve_export_range(from_day, to_day)?;
    tracing::info!(from = %from, to = %to, "导出 CSV（只按时间范围，不应用列表筛选）");
    let exports_dir = state.exports_dir();
    match blocking::run(&state.db, move |db| {
        queries::export_csv(&db, &exports_dir, &from, &to)
    })
    .await
    {
        Ok(export) => Ok(UsageExportResult {
            saved: true,
            path: Some(export.path),
            message: None,
            rows: export.rows,
            bytes: export.bytes,
            truncated: export.truncated,
        }),
        Err(err) => Ok(UsageExportResult {
            saved: false,
            path: None,
            message: Some(format!("导出 CSV 失败：{err}")),
            rows: 0,
            bytes: 0,
            truncated: false,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_range_defaults_to_last_30_days() {
        let (from, to) = resolve_export_range(None, None).expect("默认区间应合法");
        let from = parse_day(&from).unwrap();
        let to = parse_day(&to).unwrap();
        assert_eq!((to - from).num_days(), EXPORT_DEFAULT_DAYS - 1);
    }

    #[test]
    fn export_range_rejects_reversed_dates() {
        let err = resolve_export_range(Some("2026-02-01".into()), Some("2026-01-01".into()))
            .expect_err("倒序区间应被拒绝");
        assert!(err.contains("起始日期"));
    }

    #[test]
    fn export_range_rejects_bad_format() {
        assert!(resolve_export_range(Some("2026/01/01".into()), None).is_err());
    }
}
