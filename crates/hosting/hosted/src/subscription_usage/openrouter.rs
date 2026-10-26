use super::{
    OpenRouterKeyUsage, UsageMeterReading, UsageMeterView, UsageVertical, VerticalUsageState,
    VerticalUsageView, format_unix_seconds,
};
use plakous_host_protocol::views::presentation::UsageAmountView;
fn format_credits(value: f64) -> String {
    format!("${value:.2}")
}
pub(super) fn project(
    vertical: UsageVertical,
    usage: &OpenRouterKeyUsage,
    label: &str,
) -> VerticalUsageView {
    VerticalUsageView {
        vertical,
        account_email: None,
        plan_label: Some(openrouter_plan_label(usage, label)),
        observed_at: Some(format_unix_seconds(usage.observed_at().seconds())),
        state: VerticalUsageState::Ready {
            meters: openrouter_meters(usage),
        },
        balances: Vec::new(),
    }
}
/// Provider labels can contain masked key material. Only the product's safe
/// source label and an explicitly reported free tier reach the account line.
fn openrouter_plan_label(usage: &OpenRouterKeyUsage, label: &str) -> String {
    if usage.is_free_tier() == Some(true) {
        format!("{label} · free tier")
    } else {
        label.to_owned()
    }
}

fn openrouter_meters(usage: &OpenRouterKeyUsage) -> Vec<UsageMeterView> {
    let mut meters = Vec::new();
    // A `null` limit is "no cap"; it is never drawn as an unbounded bar and
    // never inferred from spend.
    let mut limit = vec![UsageAmountView {
        label: "cap".to_owned(),
        value: usage
            .limit()
            .map_or_else(|| "no cap".to_owned(), format_credits),
    }];
    if let Some(remaining) = usage.limit_remaining() {
        limit.push(UsageAmountView {
            label: "remaining".to_owned(),
            value: format_credits(remaining),
        });
    }
    limit.push(UsageAmountView {
        label: "resets".to_owned(),
        value: usage.limit_reset().unwrap_or("never").to_owned(),
    });
    if let Some(include_byok) = usage.include_byok_in_limit() {
        limit.push(UsageAmountView {
            label: "BYOK spend".to_owned(),
            value: if include_byok {
                "counted".to_owned()
            } else {
                "excluded".to_owned()
            },
        });
    }
    meters.push(UsageMeterView {
        label: "Key limit".to_owned(),
        group: None,
        reading: UsageMeterReading::Amounts { entries: limit },
        resets_at: None,
    });
    let spend = [
        ("all time", usage.usage()),
        ("today", usage.usage_daily()),
        ("this week", usage.usage_weekly()),
        ("this month", usage.usage_monthly()),
    ];
    let entries: Vec<_> = spend
        .into_iter()
        .filter_map(|(label, value)| {
            value.map(|value| UsageAmountView {
                label: label.to_owned(),
                value: format_credits(value),
            })
        })
        .collect();
    if !entries.is_empty() {
        meters.push(UsageMeterView {
            label: "Key usage".to_owned(),
            group: None,
            reading: UsageMeterReading::Amounts { entries },
            resets_at: None,
        });
    }
    meters
}
