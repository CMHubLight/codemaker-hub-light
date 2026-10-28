//! 托盘 tooltip 文本：`本月积分 [百分比] · [已用] / [总额]`。
//!
//! 纯函数，不读状态、不碰托盘；缺失或非有限值显示 `--`（不造假零）。

use crate::usage::QuotaInfo;

const TOOLTIP_PREFIX: &str = "本月积分";
pub(crate) const TOOLTIP_EMPTY: &str = "本月积分 -- · -- / --";

/// 生成托盘 tooltip。
pub(crate) fn format_tooltip(quota: Option<&QuotaInfo>) -> String {
    let Some(quota) = quota else {
        return TOOLTIP_EMPTY.to_string();
    };

    let total = quota.monthly_quota.filter(|value| value.is_finite());
    let used =
        quota
            .used
            .filter(|value| value.is_finite())
            .or_else(|| match (total, quota.remaining) {
                (Some(total), Some(remaining)) if remaining.is_finite() => {
                    Some((total - remaining).max(0.0))
                }
                _ => None,
            });
    let percentage = quota
        .percentage
        .filter(|value| value.is_finite())
        .or_else(|| match (used, total) {
            (Some(used), Some(total)) if total > 0.0 => Some(used / total * 100.0),
            _ => None,
        });

    if total.is_none() && used.is_none() && percentage.is_none() {
        return TOOLTIP_EMPTY.to_string();
    }

    format!(
        "{TOOLTIP_PREFIX} {} · {} / {}",
        format_percent(percentage),
        format_amount(used),
        format_amount(total)
    )
}

/// 文本未变化时无需重复写入托盘。
pub(crate) fn should_apply(previous: Option<&str>, next: &str) -> bool {
    previous != Some(next)
}

fn format_percent(value: Option<f64>) -> String {
    match value.filter(|value| value.is_finite()) {
        Some(value) => format!("{}%", format_amount(Some(value))),
        None => "--".to_string(),
    }
}

/// 最多两位小数，去掉无意义尾零；缺失或非有限值显示 `--`。
fn format_amount(value: Option<f64>) -> String {
    let Some(value) = value.filter(|value| value.is_finite()) else {
        return "--".to_string();
    };
    let rounded = (value * 100.0).round() / 100.0;
    let mut text = format!("{rounded:.2}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text == "-0" {
        "0".to_string()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quota(
        monthly_quota: Option<f64>,
        used: Option<f64>,
        remaining: Option<f64>,
        percentage: Option<f64>,
    ) -> QuotaInfo {
        QuotaInfo {
            monthly_quota,
            used,
            remaining,
            percentage,
            reset_day: Some(1),
            reset_at: None,
            period_start: Some("2026-10-01".to_string()),
            is_active: true,
            usage_cost: None,
            currency: "CNY".to_string(),
            fetched_at: "2026-10-20T00:00:00Z".to_string(),
            status: "ok".to_string(),
            timezone: None,
            total_tokens: None,
            app_tokens: Default::default(),
            app_costs: Default::default(),
            model_tokens: Default::default(),
            model_costs: Default::default(),
        }
    }

    #[test]
    fn formats_measured_quota_tooltip() {
        let item = quota(Some(3500.04), Some(60.4), Some(3439.64), Some(1.73));
        assert_eq!(
            format_tooltip(Some(&item)),
            "本月积分 1.73% · 60.4 / 3500.04"
        );
    }

    #[test]
    fn computes_percentage_when_missing() {
        let item = quota(Some(200.0), Some(50.0), Some(150.0), None);
        assert_eq!(format_tooltip(Some(&item)), "本月积分 25% · 50 / 200");
    }

    #[test]
    fn computes_used_from_remaining_when_missing() {
        let item = quota(Some(100.0), None, Some(25.0), None);
        assert_eq!(format_tooltip(Some(&item)), "本月积分 75% · 75 / 100");
    }

    #[test]
    fn rounds_two_decimals_and_trims_zeroes() {
        let item = quota(Some(3500.0), Some(60.4), None, Some(1.666));
        assert_eq!(format_tooltip(Some(&item)), "本月积分 1.67% · 60.4 / 3500");
    }

    #[test]
    fn no_quota_shows_placeholder() {
        assert_eq!(format_tooltip(None), TOOLTIP_EMPTY);
        assert_eq!(
            format_tooltip(Some(&quota(None, None, None, None))),
            TOOLTIP_EMPTY
        );
    }

    #[test]
    fn zero_total_does_not_divide() {
        let item = quota(Some(0.0), Some(0.0), Some(0.0), None);
        assert_eq!(format_tooltip(Some(&item)), "本月积分 -- · 0 / 0");
    }

    #[test]
    fn formats_large_integer_without_trailing_zeroes() {
        assert_eq!(format_amount(Some(3500.0)), "3500");
        assert_eq!(format_amount(Some(3500.04)), "3500.04");
        assert_eq!(format_amount(Some(-0.0)), "0");
    }

    #[test]
    fn identical_tooltip_text_is_skipped() {
        assert!(!should_apply(
            Some("本月积分 1% · 1 / 100"),
            "本月积分 1% · 1 / 100"
        ));
        assert!(should_apply(None, "本月积分 1% · 1 / 100"));
        assert!(should_apply(
            Some("本月积分 1% · 1 / 100"),
            "本月积分 2% · 2 / 100"
        ));
    }
}
