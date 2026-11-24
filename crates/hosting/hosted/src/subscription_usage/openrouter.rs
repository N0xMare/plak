use super::{
    OpenRouterKeyUsage, UsageMeterReading, UsageMeterView, UsageMoney, UsageMoneyOverflow,
    UsageMoneyView, UsageVertical, VerticalUsageState, VerticalUsageView, format_unix_seconds,
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
        money: Some(openrouter_money(usage)),
        repair: None,
    }
}

fn openrouter_money(usage: &OpenRouterKeyUsage) -> Result<UsageMoneyView, UsageMoneyOverflow> {
    let [cap, remaining] = usage.limit_decimals();
    let mut limit = Vec::new();
    if let Some((value, decimal)) = usage.limit().zip(cap) {
        limit.push(UsageMoney {
            currency: "USD",
            cap_micros: Some(decimal_micros(decimal)?),
            remaining_micros: None,
            used_micros: None,
            display: format!("cap: {}", format_credits(value)),
        });
    }
    if let Some((value, decimal)) = usage.limit_remaining().zip(remaining) {
        limit.push(UsageMoney {
            currency: "USD",
            cap_micros: None,
            remaining_micros: Some(decimal_micros(decimal)?),
            used_micros: None,
            display: format!("remaining: {}", format_credits(value)),
        });
    }
    let mut spend = Vec::new();
    let displayed = [
        ("all time", usage.usage()),
        ("today", usage.usage_daily()),
        ("this week", usage.usage_weekly()),
        ("this month", usage.usage_monthly()),
    ];
    for ((label, value), decimal) in displayed.into_iter().zip(usage.usage_decimals()) {
        if let Some((value, decimal)) = value.zip(decimal) {
            spend.push(UsageMoney {
                currency: "USD",
                cap_micros: None,
                remaining_micros: None,
                used_micros: Some(decimal_micros(decimal)?),
                display: format!("{label}: {}", format_credits(value)),
            });
        }
    }
    Ok(UsageMoneyView {
        balances: Vec::new(),
        meters: vec![limit, spend],
    })
}

// Round a validated JSON decimal to the nearest micro-unit, ties away from zero.
fn decimal_micros(decimal: &str) -> Result<i64, UsageMoneyOverflow> {
    let (negative, magnitude) = decimal
        .strip_prefix('-')
        .map_or((false, decimal), |rest| (true, rest));
    let (mantissa, exponent) = magnitude.split_once(['e', 'E']).unwrap_or((magnitude, "0"));
    let exponent = exponent.parse::<i64>().unwrap_or_else(|_| {
        if exponent.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    let fraction = mantissa.split_once('.').map_or(0, |(_, tail)| tail.len());
    let digits: String = mantissa.chars().filter(|digit| *digit != '.').collect();
    let digits = digits.trim_start_matches('0').as_bytes();
    if digits.is_empty() {
        return Ok(0);
    }
    let significant = i64::try_from(digits.len()).map_err(|_| UsageMoneyOverflow)?;
    let fraction = i64::try_from(fraction).map_err(|_| UsageMoneyOverflow)?;
    let keep = exponent.saturating_add(significant + 6 - fraction);
    if keep < 0 {
        return Ok(0);
    }
    if keep > 19 {
        return Err(UsageMoneyOverflow);
    }
    let keep = usize::try_from(keep).map_err(|_| UsageMoneyOverflow)?;
    let mut magnitude = 0i128;
    for index in 0..keep {
        magnitude = magnitude * 10 + i128::from(digits.get(index).copied().unwrap_or(b'0') - b'0');
    }
    if digits.get(keep).is_some_and(|digit| *digit >= b'5') {
        magnitude += 1;
    }
    i64::try_from(if negative { -magnitude } else { magnitude }).map_err(|_| UsageMoneyOverflow)
}

#[cfg(test)]
#[test]
fn decimals_round_without_binary_float_or_saturating() {
    for (decimal, micros) in [
        ("0", Ok(0)),
        ("0.00000049", Ok(0)),
        ("-0.0000005", Ok(-1)),
        ("12.345675e-2", Ok(123_457)),
        ("9223372036854.775807", Ok(i64::MAX)),
        ("-9223372036854.775808", Ok(i64::MIN)),
        ("9223372036854.7758075", Err(UsageMoneyOverflow)),
        ("1e100", Err(UsageMoneyOverflow)),
    ] {
        assert_eq!(decimal_micros(decimal), micros, "{decimal}");
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
