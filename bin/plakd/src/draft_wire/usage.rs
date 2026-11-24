use plakd_wire_types::{console, v13};
use plakous_host_protocol::views::presentation as source;
use plakous_host_protocol::wire::RequestId;

/// Projects the authorized console page without repeating the provider read.
#[must_use]
pub fn accounts_usage(id: RequestId, response: console::HostResponse) -> Option<v13::HostFrame> {
    let console::HostResponse::AccountsUsage {
        as_of,
        window,
        from,
        to,
        rows,
        unattributed,
        diagnostics,
        page,
    } = response
    else {
        return None;
    };
    let response = v13::HostResponseV13::AccountsUsage {
        as_of,
        window,
        from,
        to,
        unattributed,
        diagnostics,
        page,
        rows: rows
            .into_iter()
            .map(|row| {
                let console::Allowance {
                    observed_at,
                    stale_age_ms,
                    view,
                    unavailable,
                    remedies,
                } = row.allowance;
                v13::AccountUsage {
                    account_id: row.account_id,
                    recorded: row.recorded,
                    allowance: v13::Allowance {
                        observed_at,
                        stale_age_ms,
                        unavailable,
                        remedies,
                        view: view.map(vertical),
                    },
                }
            })
            .collect(),
    };
    Some(super::host_response(id, response))
}

/// Projects the usage limits observation, using the same amounts as the console.
#[must_use]
pub fn usage_limits(view: source::UsageLimitsView) -> v13::ObservationV13 {
    v13::ObservationV13::UsageLimits(Box::new(v13::UsageLimitsView {
        verticals: view.verticals.into_iter().map(vertical).collect(),
        session: view.session,
        stale_age_seconds: view.stale_age_seconds,
    }))
}

fn vertical(view: source::VerticalUsageView) -> v13::VerticalUsageView {
    let (state, balances) = match view.money.unwrap_or(Ok(source::UsageMoneyView::default())) {
        Ok(source::UsageMoneyView { balances, meters }) => (
            match view.state {
                source::VerticalUsageState::Ready { meters: readings } => {
                    v13::VerticalUsageState::Ready {
                        meters: readings
                            .into_iter()
                            .enumerate()
                            .map(|(index, reading)| {
                                meter(
                                    reading,
                                    meters.get(index).map(Vec::as_slice).unwrap_or_default(),
                                )
                            })
                            .collect(),
                    }
                }
                source::VerticalUsageState::NotConnected => v13::VerticalUsageState::NotConnected,
                source::VerticalUsageState::Unavailable { reason } => {
                    v13::VerticalUsageState::Unavailable { reason }
                }
            },
            balances.iter().map(money).collect(),
        ),
        Err(source::UsageMoneyOverflow) => (
            v13::VerticalUsageState::Unavailable {
                reason: "provider amount exceeds the supported range".to_owned(),
            },
            Vec::new(),
        ),
    };
    v13::VerticalUsageView {
        repair: view.repair,
        vertical: view.vertical,
        account_email: view.account_email,
        plan_label: view.plan_label,
        observed_at: view.observed_at,
        state,
        balances,
    }
}

fn meter(view: source::UsageMeterView, amounts: &[source::UsageMoney]) -> v13::UsageMeterView {
    let reading = match view.reading {
        source::UsageMeterReading::Percent {
            floored_percent,
            exact_percent,
            percent_left,
        } => v13::UsageMeterReading::Percent {
            floored_percent,
            exact_percent,
            percent_left,
        },
        source::UsageMeterReading::Amounts { .. } if !amounts.is_empty() => {
            v13::UsageMeterReading::Amounts {
                entries: amounts.iter().map(money).collect(),
            }
        }
        source::UsageMeterReading::Amounts { .. } | source::UsageMeterReading::Unreported => {
            v13::UsageMeterReading::Unreported
        }
    };
    v13::UsageMeterView {
        label: view.label,
        group: view.group,
        reading,
        resets_at: view.resets_at,
    }
}

fn money(amount: &source::UsageMoney) -> v13::Money {
    v13::Money {
        currency: amount.currency.to_owned(),
        remaining_micros: amount.remaining_micros,
        used_micros: amount.used_micros,
        cap_micros: amount.cap_micros,
        display: amount.display.clone(),
    }
}
