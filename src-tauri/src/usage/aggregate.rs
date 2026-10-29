//! 聚合计算：把仓储层的总量行转成前端指标（纯函数，便于单测）。

use chrono::{SecondsFormat, Utc};

use crate::{
    db::repo::{RequestRow, UsageDayRow, UsageDimRow, UsageTotalsRow},
    usage::{DailyUsage, ModelUsage, RecentRequest, UsageSummary, UsageTotals},
};

/// 统一保留 4 位小数（UI 展示精度）。
pub fn round4(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    (value * 10_000.0).round() / 10_000.0
}

/// 比率（0..=1），分母为 0 时返回 None（前端据此显示“—”而非 0）。
pub fn ratio(numerator: i64, denominator: i64) -> Option<f64> {
    if denominator <= 0 {
        return None;
    }
    Some(round4(numerator as f64 / denominator as f64))
}

/// 总量行 → 前端 `UsageTotals`。
pub fn totals_from_row(row: &UsageTotalsRow) -> UsageTotals {
    let total_tokens =
        row.input_tokens + row.output_tokens + row.cache_read_tokens + row.cache_write_tokens;
    UsageTotals {
        requests: row.request_count,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cache_read_tokens: row.cache_read_tokens,
        cache_write_tokens: row.cache_write_tokens,
        total_tokens,
        cost_yuan: round4(row.cost_yuan),
        cache_hit_rate: ratio(
            row.cache_read_tokens,
            row.input_tokens + row.cache_read_tokens,
        ),
        avg_latency_ms: if row.request_count > 0 {
            Some(round4(
                row.total_duration_ms as f64 / row.request_count as f64,
            ))
        } else {
            None
        },
        error_rate: ratio(row.error_count, row.request_count),
    }
}

/// 按日聚合行 → 前端趋势点。
pub fn daily_from_rows(rows: Vec<UsageDayRow>) -> Vec<DailyUsage> {
    rows.into_iter()
        .map(|row| DailyUsage {
            date: row.day,
            requests: row.totals.request_count,
            tokens: row.totals.input_tokens
                + row.totals.output_tokens
                + row.totals.cache_read_tokens
                + row.totals.cache_write_tokens,
            cost_yuan: round4(row.totals.cost_yuan),
        })
        .collect()
}

/// 维度聚合行 → 前端模型分布。
pub fn models_from_rows(rows: Vec<UsageDimRow>) -> Vec<ModelUsage> {
    rows.into_iter()
        .map(|row| ModelUsage {
            model: if row.key.is_empty() {
                "unknown".to_string()
            } else {
                row.key
            },
            requests: row.totals.request_count,
            tokens: row.totals.input_tokens
                + row.totals.output_tokens
                + row.totals.cache_read_tokens
                + row.totals.cache_write_tokens,
            cost_yuan: round4(row.totals.cost_yuan),
        })
        .collect()
}

/// 请求状态的**唯一定义**：落库列 `status_kind`、界面标签、筛选条件都用它。
///
/// 返回 `success | error | cancelled | unknown`。改这里的判定必须同步
/// `migrations/0003_request_day_status.sql` 里的历史回填 CASE。
pub fn status_kind(error_kind: Option<&str>, status_code: Option<i64>) -> &'static str {
    match error_kind {
        Some(kind)
            if kind.contains("disconnect") || kind.contains("cancel") || kind.contains("abort") =>
        {
            "cancelled"
        }
        Some(_) => "error",
        None => match status_code {
            Some(code) if (200..400).contains(&code) => "success",
            Some(_) => "error",
            None => "unknown",
        },
    }
}

/// 请求状态归一（读取侧）：委托给 [`status_kind`]，保证与落库列同源。
pub fn request_status(row: &RequestRow) -> &'static str {
    status_kind(row.error_kind.as_deref(), row.status_code)
}

/// 请求行 → 前端 `RecentRequest`。
pub fn recent_from_row(row: &RequestRow) -> RecentRequest {
    RecentRequest {
        id: row.id.to_string(),
        created_at: row.started_at.clone(),
        model: row
            .model
            .clone()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_string()),
        protocol: row.api_format.clone(),
        // 请求列表暂未关联 proxy_sessions.client，客户端列固定为 unknown。
        client: "unknown".to_string(),
        status: request_status(row).to_string(),
        latency_ms: row.duration_ms,
        first_byte_ms: row.first_byte_ms,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        cost_yuan: round4(row.cost_yuan.unwrap_or(0.0)),
    }
}

pub fn recent_from_rows(rows: &[RequestRow]) -> Vec<RecentRequest> {
    rows.iter().map(recent_from_row).collect()
}

/// 组装概览汇总。
pub fn build_summary(
    today: UsageTotals,
    month: UsageTotals,
    daily: Vec<DailyUsage>,
    models: Vec<ModelUsage>,
    recent: Vec<RecentRequest>,
) -> UsageSummary {
    UsageSummary {
        today,
        month,
        daily,
        models,
        recent,
        generated_at: Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn totals_row() -> UsageTotalsRow {
        UsageTotalsRow {
            request_count: 4,
            error_count: 1,
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 300,
            cache_write_tokens: 10,
            cost_yuan: 1.234_567,
            total_duration_ms: 1_000,
        }
    }

    fn request_row(status_code: Option<i64>, error_kind: Option<&str>) -> RequestRow {
        RequestRow {
            id: 7,
            started_at: "2026-10-20T10:00:00Z".to_string(),
            day_local: "2026-10-20".to_string(),
            request_path: "/v1/messages".to_string(),
            api_format: "anthropic".to_string(),
            model: Some("claude-sonnet-4".to_string()),
            status_code,
            is_sse: true,
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 3,
            cache_write_tokens: 1,
            duration_ms: Some(1234),
            first_byte_ms: Some(210),
            error_kind: error_kind.map(str::to_string),
            session_id: None,
            client_request_id: None,
            cost_yuan: Some(0.012_345_6),
            cost_source: Some("upstream".to_string()),
        }
    }

    #[test]
    fn computes_rates() {
        let totals = totals_from_row(&totals_row());
        assert_eq!(totals.error_rate, Some(0.25));
        assert_eq!(totals.cache_hit_rate, Some(0.75));
        assert_eq!(totals.avg_latency_ms, Some(250.0));
        assert_eq!(totals.total_tokens, 460);
    }

    #[test]
    fn zero_database_yields_null_rates() {
        let totals = totals_from_row(&UsageTotalsRow::default());
        assert_eq!(totals.requests, 0);
        assert_eq!(totals.cache_hit_rate, None);
        assert_eq!(totals.error_rate, None);
        assert_eq!(totals.avg_latency_ms, None);
    }

    #[test]
    fn maps_request_status() {
        assert_eq!(request_status(&request_row(Some(200), None)), "success");
        assert_eq!(request_status(&request_row(Some(500), None)), "error");
        assert_eq!(
            request_status(&request_row(None, Some("client_disconnected"))),
            "cancelled"
        );
        assert_eq!(
            request_status(&request_row(None, Some("upstream_error"))),
            "error"
        );
    }

    #[test]
    fn maps_recent_request() {
        let item = recent_from_row(&request_row(Some(200), None));
        assert_eq!(item.id, "7");
        assert_eq!(item.protocol, "anthropic");
        assert_eq!(item.status, "success");
        assert_eq!(item.cost_yuan, 0.012_3);
        assert_eq!(item.latency_ms, Some(1234));
        assert_eq!(
            item.first_byte_ms,
            Some(210),
            "首字节延迟必须透出到前端请求列表"
        );
    }

    /// 写入侧（落库列 status_kind）与读取侧（界面标签）必须同源，
    /// 且覆盖代理真实产生的 error_kind 词表。
    #[test]
    fn status_kind_is_the_single_definition() {
        let cases: &[(Option<&str>, Option<i64>, &str)] = &[
            (None, Some(200), "success"),
            (None, Some(302), "success"),
            (None, Some(400), "error"),
            (None, Some(500), "error"),
            (None, None, "unknown"),
            (Some("client_aborted"), Some(200), "cancelled"),
            (Some("client_disconnected"), None, "cancelled"),
            (Some("upstream_cancel"), None, "cancelled"),
            (Some("upstream_error"), Some(502), "error"),
            (Some("upstream_redirect"), Some(502), "error"),
            (Some("timeout"), None, "error"),
            (Some("translation_error"), Some(200), "error"),
            (Some("unauthorized"), Some(401), "error"),
        ];
        for (error_kind, status_code, expected) in cases {
            assert_eq!(
                status_kind(*error_kind, *status_code),
                *expected,
                "落库判定错误：{error_kind:?}/{status_code:?}"
            );
            assert_eq!(
                request_status(&request_row(*status_code, *error_kind)),
                *expected,
                "读取侧必须委托给同一份定义"
            );
        }
    }

    #[test]
    fn round4_handles_non_finite() {
        assert_eq!(round4(f64::NAN), 0.0);
        assert_eq!(round4(1.234_56), 1.234_6);
    }
}
