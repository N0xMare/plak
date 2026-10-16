//! The single host-owned Usage projection.
//!
//! Every text Usage surface — `plak usage`, `plak usage --session`, and the
//! `UsageLimits` host event — is built from [`project_limits`](crate::subscription_usage::project_limits) and rendered
//! from [`render_limits_text`](crate::subscription_usage::render_limits_text), so they can never disagree. `SubscriptionUsageView`
//! is now only the disposition of that read — loading, unavailable with a
//! reason, or landed — and carries no second projection of its own.
//!
//! The honesty clauses of spec 02 §7.2 are enforced here:
//!
//! - all four verticals are always listed, a vertical with no data by reason;
//! - a plan/tier line appears only when the authenticated endpoint reported it;
//! - a non-Ready vertical carries no numbers and draws no bar;
//! - an unknown cost is `not available`, never `$0`;
//! - idle `/usage` reuses a projection younger than [`USAGE_PROJECTION_FRESHNESS`](crate::subscription_usage::USAGE_PROJECTION_FRESHNESS);
//! - a rate-limited or Busy vertical keeps the last Ready meters rather than replacing them;
//! - a projection that is only Busy is not treated as fresh.

use std::fmt::Write as _;
use std::time::Duration;

use plakous::model::{
    SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO, SUBSCRIPTION_BALANCE_PREPAID_CREDITS,
    SubscriptionUsageBalance, SubscriptionUsageCredentialState, SubscriptionUsagePeriod,
    SubscriptionUsageRead, SubscriptionUsageSnapshot, SubscriptionUsageUnavailable,
};
use plakous::openrouter::OpenRouterKeyUsage;
use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::views::presentation::{
    SessionLedgerView, UsageAmountView, UsageBalanceView, UsageCostView, UsageLimitsView,
    UsageMeterReading, UsageMeterView, UsageVertical, VerticalUsageState, VerticalUsageView,
};

/// Width of the Limits bar, in cells. The fill is the floored percent *left*,
/// matching provider truncation, so a bar never overstates the allowance that
/// remains: a full bar is an untouched limit, an empty bar a reached one.
pub const LIMIT_BAR_CELLS: usize = 20;

/// Black square (U+25A0). Fill and empty share this glyph so the unused track
/// cannot overrun the fill. It is mid-cell like `[` `]` in Ghostty, iTerm,
/// tmux, and herdr — a lower-block sits on the baseline, a box-drawing
/// horizontal is centered but too thin. Empty cells are painted grey.
pub const LIMIT_BAR_FILL: char = '\u{25a0}';

/// Grey SGR wrapped around unused bar cells. The glyph is still
/// [`LIMIT_BAR_FILL`]; only the colour recedes.
const LIMIT_BAR_EMPTY_SGR: &str = "\u{1b}[90m";
const LIMIT_BAR_RESET_SGR: &str = "\u{1b}[0m";

/// How long a successful Usage projection stays current for idle `/usage` and
/// Usage/Context tab opens. The meters are 5-hour and weekly windows; a minute
/// is enough to stop Claude's `/api/oauth/usage` 429s from tab switching, and
/// short enough that a real reset is not hidden. In-turn `/usage` still serves
/// the held projection with its age instead of re-querying.
pub const USAGE_PROJECTION_FRESHNESS: Duration = Duration::from_secs(60);

/// Exact operator sentence for [`SubscriptionUsageUnavailable::RateLimited`].
pub const USAGE_RATE_LIMITED_REASON: &str = "provider rate-limited this read; it was not retried";

/// Exact operator sentence for [`SubscriptionUsageUnavailable::Busy`].
pub const USAGE_BUSY_REASON: &str = "another Plak session or an in-flight credential refresh holds this provider's lock; nothing was sent, retry in a moment";

/// Returns whether `age` is still within [`USAGE_PROJECTION_FRESHNESS`].
///
/// A fresh cache is served as current (`stale_age_seconds = None`), not as the
/// in-turn stale overlay.
#[must_use]
pub fn cached_if_fresh(view: &UsageLimitsView, age: Duration) -> Option<UsageLimitsView> {
    if projection_is_lock_contention(view) {
        return None;
    }
    (age < USAGE_PROJECTION_FRESHNESS).then(|| {
        let mut view = view.clone();
        view.stale_age_seconds = None;
        view
    })
}

/// Keeps a previous Ready vertical when the new read is only a rate-limit
/// or a Busy lock miss.
///
/// Claude's usage GET 429s under rapid tab opens, and a sibling flock can
/// miss as Busy. The last Ready meters are still a true observation;
/// replacing them with those sentences throws away numbers the host already
/// holds.
#[must_use]
pub fn preserve_ready_across_rate_limit(
    mut incoming: UsageLimitsView,
    previous: Option<&UsageLimitsView>,
) -> UsageLimitsView {
    let Some(previous) = previous else {
        return incoming;
    };
    for vertical in &mut incoming.verticals {
        if !matches!(
            &vertical.state,
            VerticalUsageState::Unavailable { reason }
                if is_rate_limited_reason(reason) || is_busy_reason(reason)
        ) {
            continue;
        }
        if let Some(kept) = previous.verticals.iter().find(|candidate| {
            candidate.vertical == vertical.vertical
                && matches!(candidate.state, VerticalUsageState::Ready { .. })
        }) {
            *vertical = kept.clone();
        }
    }
    incoming
}

/// Whether `reason` is the rate-limit sentence [`unavailable_reason`] emits.
#[must_use]
pub fn is_rate_limited_reason(reason: &str) -> bool {
    reason == USAGE_RATE_LIMITED_REASON
}

/// Plak's Usage roster, in presentation order.
///
/// The protocol carries a vertical as an account id plus its operator-facing
/// label; the roster that fills it is product data and lives here, so the
/// neutral crate holds no closed list of Plak's accounts.
#[must_use]
pub fn product_verticals() -> [UsageVertical; 4] {
    [
        codex_vertical(),
        supergrok_vertical(),
        claude_vertical(),
        openrouter_vertical(),
    ]
}

/// The ChatGPT/Codex OAuth subscription vertical.
#[must_use]
pub fn codex_vertical() -> UsageVertical {
    UsageVertical::new("codex", "Codex")
}

/// The `SuperGrok` OAuth subscription vertical.
#[must_use]
pub fn supergrok_vertical() -> UsageVertical {
    UsageVertical::new("grok", "SuperGrok")
}

/// The Claude OAuth subscription vertical.
#[must_use]
pub fn claude_vertical() -> UsageVertical {
    UsageVertical::new("claude", "Claude")
}

/// The `OpenRouter` inference-key vertical.
#[must_use]
pub fn openrouter_vertical() -> UsageVertical {
    UsageVertical::new("openrouter", "OpenRouter")
}

/// Whether `reason` is the Busy sentence [`unavailable_reason`] emits.
#[must_use]
pub fn is_busy_reason(reason: &str) -> bool {
    reason == USAGE_BUSY_REASON
}

/// Whether this projection is only a lock miss — no Ready meters, at least
/// one Busy vertical.
///
/// A lock miss is contention, not a stable observation. Serving it as a
/// fresh idle cache would paint Busy for a minute and make the operator
/// bounce through `plak usage` to unstick the TUI.
#[must_use]
pub fn projection_is_lock_contention(view: &UsageLimitsView) -> bool {
    let mut any_busy = false;
    for vertical in &view.verticals {
        match &vertical.state {
            VerticalUsageState::Ready { .. } => return false,
            VerticalUsageState::Unavailable { reason } if is_busy_reason(reason) => {
                any_busy = true;
            }
            _ => {}
        }
    }
    any_busy
}

/// One vertical's terminal observation, as the host collected it.
#[derive(Clone, Debug)]
pub enum VerticalObservation {
    /// No ready credential exists for this vertical.
    NotConnected,
    /// A native subscription allowance read completed.
    Native(SubscriptionUsageRead),
    /// An `OpenRouter` allowance read and its product-supplied safe key label.
    OpenRouterKey(Box<OpenRouterKeyUsage>, String),
    /// A credential exists but no allowance can truthfully be reported.
    Unavailable(String),
}

/// Builds the one Usage projection every surface renders.
///
/// `observations` need not be complete or ordered: any vertical the caller did
/// not supply is listed as `NotConnected`, so all four are always present.
#[must_use]
pub fn project_limits(
    observations: impl IntoIterator<Item = (UsageVertical, VerticalObservation)>,
    session: SessionLedgerView,
) -> UsageLimitsView {
    let collected: Vec<(UsageVertical, VerticalObservation)> = observations.into_iter().collect();
    let verticals = product_verticals()
        .into_iter()
        .map(|vertical| {
            let observation = collected
                .iter()
                .find(|(candidate, _)| *candidate == vertical)
                .map_or(&VerticalObservation::NotConnected, |(_, value)| value);
            project_vertical(vertical, observation)
        })
        .collect();
    UsageLimitsView {
        verticals,
        session,
        stale_age_seconds: None,
    }
}

fn project_vertical(
    vertical: UsageVertical,
    observation: &VerticalObservation,
) -> VerticalUsageView {
    match observation {
        VerticalObservation::NotConnected => VerticalUsageView {
            vertical,
            account_email: None,
            plan_label: None,
            observed_at: None,
            state: VerticalUsageState::NotConnected,
            balances: Vec::new(),
        },
        VerticalObservation::Unavailable(reason) => VerticalUsageView {
            vertical,
            account_email: None,
            plan_label: None,
            observed_at: None,
            state: VerticalUsageState::Unavailable {
                reason: reason.clone(),
            },
            balances: Vec::new(),
        },
        VerticalObservation::Native(SubscriptionUsageRead::Unavailable {
            observed_at,
            reason,
        }) => {
            let state = match reason {
                SubscriptionUsageUnavailable::NotConnected => VerticalUsageState::NotConnected,
                other => VerticalUsageState::Unavailable {
                    reason: unavailable_reason(*other, vertical.account()),
                },
            };
            VerticalUsageView {
                vertical,
                account_email: None,
                plan_label: None,
                observed_at: Some(format_unix_seconds(observed_at.seconds())),
                state,
                balances: Vec::new(),
            }
        }
        VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)) => {
            let meters = native_meters(&vertical, snapshot);
            VerticalUsageView {
                vertical,
                account_email: snapshot.account().map(str::to_owned),
                plan_label: snapshot.plan().map(title_case),
                observed_at: Some(format_unix_seconds(snapshot.observed_at().seconds())),
                state: VerticalUsageState::Ready { meters },
                balances: snapshot
                    .balances()
                    .iter()
                    .filter(|balance| report_balance(balance))
                    .map(balance_view)
                    .collect(),
            }
        }
        VerticalObservation::OpenRouterKey(usage, label) => VerticalUsageView {
            vertical,
            account_email: None,
            plan_label: Some(openrouter_plan_label(usage, label)),
            observed_at: Some(format_unix_seconds(usage.observed_at().seconds())),
            state: VerticalUsageState::Ready {
                meters: openrouter_meters(usage),
            },
            balances: Vec::new(),
        },
    }
}

/// Whether a provider-reported balance is worth a product line.
///
/// A prepaid remaining of `$0` and a pay-as-you-go cap of `$0` are the
/// provider saying those features are unused / switched off. Grok Build's
/// `/usage` omits them; printing `$0.00` here made SuperGrok look empty.
fn report_balance(balance: &SubscriptionUsageBalance) -> bool {
    match balance.id() {
        SUBSCRIPTION_BALANCE_PREPAID_CREDITS => balance.remaining_cents() != Some(0),
        SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO => balance.cap_cents() != Some(0),
        _ => true,
    }
}

/// Names one provider-reported balance and formats the amounts it carries.
///
/// The label is chosen from the neutral balance identity; an amount the
/// provider did not report is omitted from the clause, never printed as `$0`.
fn balance_view(balance: &SubscriptionUsageBalance) -> UsageBalanceView {
    let label = match balance.id() {
        SUBSCRIPTION_BALANCE_PREPAID_CREDITS => "Credits",
        SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO => "Pay as you go",
        other => other,
    };
    let mut clauses = Vec::new();
    // A reported zero cap is the provider stating the allowance is switched
    // off, which reads better as words than as `$0.00 used of $0.00 cap`.
    if balance.id() == SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO && balance.cap_cents() == Some(0) {
        return UsageBalanceView {
            label: label.to_owned(),
            value: "not enabled".to_owned(),
        };
    }
    if let Some(remaining) = balance.remaining_cents() {
        clauses.push(format_cents(remaining));
    }
    if let Some(used) = balance.used_cents() {
        clauses.push(format!("{} used", format_cents(used)));
    }
    if let Some(cap) = balance.cap_cents() {
        clauses.push(format!("of {} cap", format_cents(cap)));
    }
    UsageBalanceView {
        label: label.to_owned(),
        value: if clauses.is_empty() {
            "not reported".to_owned()
        } else {
            clauses.join(" ")
        },
    }
}

/// Formats a provider-reported minor-unit amount as USD.
fn format_cents(cents: i64) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "cent counts stay far below 2^53 and the rendering is display-only"
    )]
    let dollars = cents as f64 / 100.0;
    format!("${dollars:.2}")
}

/// Projects native percent meters, longest window first so the spec's
/// weekly-before-5h ordering falls out of the provider's own durations.
fn native_meters(
    vertical: &UsageVertical,
    snapshot: &SubscriptionUsageSnapshot,
) -> Vec<UsageMeterView> {
    let mut meters = Vec::new();
    for meter in snapshot.meters() {
        let mut windows: Vec<_> = meter
            .windows()
            .iter()
            .map(|window| {
                let seconds = match window.period() {
                    Some(SubscriptionUsagePeriod::Rolling { duration_seconds }) => {
                        *duration_seconds
                    }
                    Some(SubscriptionUsagePeriod::ProviderPeriod { .. }) | None => 0,
                };
                (seconds, window)
            })
            .collect();
        windows.sort_by_key(|(seconds, _)| std::cmp::Reverse(*seconds));
        for (seconds, window) in windows {
            let exact = window.used_percent().get();
            let (group, label) = meter_identity(
                vertical,
                meter.id(),
                meter.name(),
                window.name(),
                seconds,
                window.period(),
            );
            meters.push(UsageMeterView {
                label,
                group,
                reading: UsageMeterReading::Percent {
                    floored_percent: floor_percent(exact),
                    exact_percent: format_percent(exact),
                    percent_left: Some(floor_percent_left(exact)),
                },
                resets_at: window
                    .ends_at()
                    .map(|timestamp| format_reset_wall_clock(timestamp.seconds())),
            });
        }
    }
    meters
}

/// A meter's `(group, label)`: the primary meter of a vertical sits at the
/// vertical's own level, and every extra metered feature names itself as a
/// group so two windows can never be confused for one (spec 02 §7.2).
fn meter_identity(
    vertical: &UsageVertical,
    meter_id: &str,
    meter_name: Option<&str>,
    window_name: Option<&str>,
    duration_seconds: u64,
    period: Option<&SubscriptionUsagePeriod>,
) -> (Option<String>, String) {
    // SuperGrok reports exactly one included-credit window, and names its own
    // cadence in the period type. An unnamed cadence stays "Usage limit"
    // rather than being guessed as weekly.
    if vertical.account() == supergrok_vertical().account() {
        let kind = match period {
            Some(SubscriptionUsagePeriod::ProviderPeriod { kind, .. }) => kind.as_str(),
            Some(SubscriptionUsagePeriod::Rolling { .. }) | None => "",
        };
        let label = if kind.contains("WEEKLY") {
            "Weekly limit"
        } else if kind.contains("MONTHLY") {
            "Monthly limit"
        } else {
            "Usage limit"
        };
        return (None, label.to_owned());
    }
    if vertical.account() == claude_vertical().account() {
        let window = match (window_name, duration_seconds) {
            (Some("five_hour"), _) | (_, 18_000) => "5h limit".to_owned(),
            (Some("seven_day"), _) | (_, 604_800) => "Weekly limit".to_owned(),
            (Some(name), _) => format!("{name} limit"),
            (None, _) => "Limit".to_owned(),
        };
        if meter_id == "claude" {
            return (None, window);
        }
        let feature = meter_name.unwrap_or(meter_id);
        return (Some(format!("{feature} limit")), window);
    }
    let window = match (window_name, duration_seconds) {
        (_, 18_000) => "5h limit".to_owned(),
        (_, 604_800) => "Weekly limit".to_owned(),
        (_, seconds) if seconds > 0 && seconds % 86_400 == 0 => {
            format!("{}-day limit", seconds / 86_400)
        }
        (_, seconds) if seconds > 0 && seconds % 3_600 == 0 => {
            format!("{}h limit", seconds / 3_600)
        }
        (Some(name), _) => format!("{name} limit"),
        (None, _) => "Limit".to_owned(),
    };
    // The Codex primary meter is the plan itself; every other metered feature
    // becomes its own indented sub-block.
    if meter_id == "codex" {
        return (None, window);
    }
    let feature = meter_name.unwrap_or(meter_id);
    (Some(format!("{feature} limit")), window)
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

/// Builds the session ledger the Usage surfaces report.
///
/// `cost` is always `not available`: no Plakous provider route reports a cost,
/// and `$0` for an unknown cost is forbidden.
#[must_use]
pub fn project_session_ledger(
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    model_calls: u64,
    reporting_model_calls: u64,
    turn_wall_time: Option<Duration>,
) -> SessionLedgerView {
    SessionLedgerView {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        reasoning_tokens,
        model_calls,
        reporting_model_calls,
        turn_wall_time: turn_wall_time.map(format_duration),
        cost: UsageCostView::NotAvailable {
            reason: if model_calls > reporting_model_calls {
                "not reported for some calls".to_owned()
            } else {
                "no provider reported a cost".to_owned()
            },
        },
    }
}

/// Renders the projection as the shared plain-text Limits block.
#[must_use]
pub fn render_limits_text(view: &UsageLimitsView) -> String {
    let mut out = String::from("Limits\n");
    if let Some(age) = view.stale_age_seconds {
        let _ = writeln!(
            out,
            "\nShowing the last projection, {} old; a turn is in flight.",
            format_duration(Duration::from_secs(age))
        );
    }
    let label_width = limits_label_width(view);
    for vertical in &view.verticals {
        out.push('\n');
        let _ = writeln!(out, "{}", vertical.vertical.provider_label());
        // The account and its plan are their own line; the header names only
        // the vertical (spec 02 §7.2).
        match (
            vertical.account_email.as_deref(),
            vertical.plan_label.as_deref(),
        ) {
            (Some(email), Some(plan)) => {
                let _ = writeln!(out, "  Account: {email} ({plan})");
            }
            (Some(email), None) => {
                let _ = writeln!(out, "  Account: {email}");
            }
            (None, Some(plan)) => {
                let _ = writeln!(out, "  Account: {plan}");
            }
            (None, None) => {}
        }
        match &vertical.state {
            VerticalUsageState::NotConnected => {
                let _ = writeln!(
                    out,
                    "  not connected · run `plak login {}`",
                    vertical.vertical.account()
                );
            }
            VerticalUsageState::Unavailable { reason } => {
                let _ = writeln!(out, "  {reason}");
            }
            VerticalUsageState::Ready { meters } => {
                if meters.is_empty() {
                    let _ = writeln!(
                        out,
                        "  the provider reported no allowance meters; usage is unknown"
                    );
                }
                for meter in meters.iter().filter(|meter| meter.group.is_none()) {
                    render_meter(&mut out, meter, "", label_width);
                }
                // Grouped meters follow as indented sub-blocks under their
                // own name, in first-seen order.
                let mut seen: Vec<&str> = Vec::new();
                for group in meters.iter().filter_map(|meter| meter.group.as_deref()) {
                    if seen.contains(&group) {
                        continue;
                    }
                    seen.push(group);
                    let _ = writeln!(out, "  {group}:");
                    for meter in meters
                        .iter()
                        .filter(|meter| meter.group.as_deref() == Some(group))
                    {
                        render_meter(&mut out, meter, "  ", label_width);
                    }
                }
            }
        }
        // Provider-reported balances close the block: they are amounts, not
        // allowances, so they never take a bar.
        for balance in &vertical.balances {
            let _ = writeln!(out, "  {:<label_width$}  {}", balance.label, balance.value);
        }
    }
    out.push('\n');
    render_session_ledger(&mut out, &view.session);
    out
}

/// Shared label column: the widest ungrouped label, group name, or indented
/// grouped label, so every bar starts in the same column.
fn limits_label_width(view: &UsageLimitsView) -> usize {
    let mut width = 0;
    for vertical in &view.verticals {
        for balance in &vertical.balances {
            width = width.max(balance.label.chars().count());
        }
        let VerticalUsageState::Ready { meters } = &vertical.state else {
            continue;
        };
        for meter in meters {
            let indent = if meter.group.is_some() { 2 } else { 0 };
            width = width.max(indent + meter.label.chars().count());
            if let Some(group) = meter.group.as_deref() {
                width = width.max(group.chars().count());
            }
        }
    }
    width
}

fn render_meter(out: &mut String, meter: &UsageMeterView, indent: &str, label_width: usize) {
    let reset = meter
        .resets_at
        .as_ref()
        .map_or_else(String::new, |resets| format!(" (resets {resets})"));
    let label = format!("{indent}{}", meter.label);
    match &meter.reading {
        UsageMeterReading::Percent {
            floored_percent,
            percent_left,
            ..
        } => {
            let left = percent_left.map_or_else(
                || 100_u8.saturating_sub(*floored_percent),
                |left| left.min(100),
            );
            let _ = writeln!(
                out,
                "  {label:<label_width$}  {} {left:>3}% left{reset}",
                limit_bar(left)
            );
        }
        UsageMeterReading::Amounts { entries } => {
            let rendered: Vec<String> = entries
                .iter()
                .map(|entry| format!("{} {}", entry.value, entry.label))
                .collect();
            let _ = writeln!(
                out,
                "  {label:<label_width$}  {}{reset}",
                rendered.join(" · ")
            );
        }
        UsageMeterReading::Unreported => {
            let _ = writeln!(
                out,
                "  {label:<label_width$}  {}{reset}",
                plakous_host_protocol::views::presentation::USAGE_NOT_REPORTED
            );
        }
    }
}

fn render_session_ledger(out: &mut String, session: &SessionLedgerView) {
    let _ = writeln!(out, "Session usage (since start or last resume):");
    let _ = writeln!(
        out,
        "  Input tokens:   {} ({} cached)",
        session.input_tokens, session.cached_input_tokens
    );
    let _ = writeln!(
        out,
        "  Output tokens:  {} ({} reasoning)",
        session.output_tokens, session.reasoning_tokens
    );
    // Host-measured turn wall time, not a provider-reported API duration: it
    // includes local tool execution and any approval wait (spec 02 §7.2).
    let wall_time = session.turn_wall_time.as_deref().unwrap_or("not available");
    let _ = writeln!(
        out,
        "  Model calls:    {} · turn wall time (host-measured): {wall_time}",
        session.model_calls
    );
    let cost = match &session.cost {
        UsageCostView::NotAvailable { reason } => format!("not available ({reason})"),
        UsageCostView::Reported { amount } => amount.clone(),
    };
    let _ = writeln!(out, "  Cost:           {cost}");
}

/// Renders the exactly-`LIMIT_BAR_CELLS`-wide bracketed bar whose fill is the
/// floored percent **left**: a full bar is an untouched limit (spec 02 §7.2).
/// Empty cells use the same mid-cell square as fill and a grey SGR, so they
/// cannot sit taller than the fill.
#[must_use]
pub fn limit_bar(percent_left: u8) -> String {
    let filled = usize::from(percent_left.min(100))
        .saturating_mul(LIMIT_BAR_CELLS)
        .saturating_div(100);
    let empty = LIMIT_BAR_CELLS.saturating_sub(filled);
    let mut bar = String::with_capacity(LIMIT_BAR_CELLS * 3 + 16);
    bar.push('[');
    bar.extend(std::iter::repeat_n(LIMIT_BAR_FILL, filled));
    if empty > 0 {
        bar.push_str(LIMIT_BAR_EMPTY_SGR);
        bar.extend(std::iter::repeat_n(LIMIT_BAR_FILL, empty));
        bar.push_str(LIMIT_BAR_RESET_SGR);
    }
    bar.push(']');
    bar
}

/// The whole percent of the allowance that remains, floored the same way the
/// provider truncates the percent it reports as used.
fn floor_percent_left(used_percent: f64) -> u8 {
    if !used_percent.is_finite() || used_percent <= 0.0 {
        return 100;
    }
    100_u8.saturating_sub(floor_percent(used_percent.ceil()))
}

fn floor_percent(value: f64) -> u8 {
    if !value.is_finite() || value <= 0.0 {
        return 0;
    }
    let floored = value.floor();
    if floored >= 100.0 {
        return 100;
    }
    // `floored` is finite and within 0..100 here, so the cast is exact.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "value is finite and bounded to 0 through 99 immediately above"
    )]
    let percent = floored as u8;
    percent
}

fn format_percent(value: f64) -> String {
    let text = format!("{value:.2}");
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn format_credits(value: f64) -> String {
    format!("${value:.2}")
}

fn format_duration(value: Duration) -> String {
    let seconds = value.as_secs();
    let (hours, minutes, seconds) = (seconds / 3_600, (seconds % 3_600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

/// Maps a native provider onto its Usage vertical.
#[must_use]
pub fn vertical_for_native(provider: NativeProvider) -> UsageVertical {
    match provider {
        NativeProvider::Xai => supergrok_vertical(),
        NativeProvider::Claude => claude_vertical(),
        _ => codex_vertical(),
    }
}

/// Returns the operator-facing label for one native subscription vertical.
#[must_use]
pub const fn subscription_provider_label(provider: NativeProvider) -> &'static str {
    match provider {
        NativeProvider::Xai => "Grok Subscription",
        NativeProvider::Claude => "Claude Subscription",
        _ => "Codex Subscription",
    }
}

/// Formats a Unix second count as an operator-facing wall-clock timestamp.
///
/// Spec 02 §7.2: a reset time is read against the operator's own clock, so it is
/// rendered in the local offset. When the process cannot determine that offset
/// the value is rendered in UTC and says so, rather than presenting a UTC
/// instant as if it were local time.
#[must_use]
pub fn format_unix_seconds(seconds: u64) -> String {
    let Ok(seconds) = i64::try_from(seconds) else {
        return seconds.to_string();
    };
    let Ok(timestamp) = time::OffsetDateTime::from_unix_timestamp(seconds) else {
        return seconds.to_string();
    };
    let (timestamp, suffix) = match time::UtcOffset::current_local_offset() {
        Ok(offset) => (timestamp.to_offset(offset), ""),
        Err(_) => (timestamp, " UTC"),
    };
    let Ok(description) = time::format_description::parse_borrowed::<2>(WALL_CLOCK_FORMAT) else {
        return seconds.to_string();
    };
    timestamp
        .format(&description)
        .map(|rendered| format!("{rendered}{suffix}"))
        .unwrap_or_else(|_| seconds.to_string())
}

const WALL_CLOCK_FORMAT: &str = "[year]-[month]-[day] [hour]:[minute]";

/// Formats a reset instant the way an operator reads it: the wall-clock time
/// first, then the day it falls on (spec 02 §7.2). Local offset, and UTC with a
/// label when the process cannot determine the offset, exactly as
/// [`format_unix_seconds`].
#[must_use]
pub fn format_reset_wall_clock(seconds: u64) -> String {
    let Ok(seconds) = i64::try_from(seconds) else {
        return seconds.to_string();
    };
    let Ok(timestamp) = time::OffsetDateTime::from_unix_timestamp(seconds) else {
        return seconds.to_string();
    };
    let (timestamp, suffix) = match time::UtcOffset::current_local_offset() {
        Ok(offset) => (timestamp.to_offset(offset), ""),
        Err(_) => (timestamp, " UTC"),
    };
    let Ok(description) = time::format_description::parse_borrowed::<2>(RESET_CLOCK_FORMAT) else {
        return seconds.to_string();
    };
    timestamp
        .format(&description)
        .map(|rendered| format!("{rendered}{suffix}"))
        .unwrap_or_else(|_| seconds.to_string())
}

const RESET_CLOCK_FORMAT: &str =
    "[hour]:[minute] on [day padding:none] [month repr:short case_sensitive:false]";

/// Returns the one operator-facing sentence for a typed unavailability reason.
///
/// `account` is the vertical's `plak login` account name, so a credential-state
/// refusal can name its remedy. Only `Transport` says the HTTP exchange failed;
/// a local lock, lease, or credential state is named as exactly that. Calling
/// every local refusal "usage transport failed" is what hid the SuperGrok
/// compatibility-lock race from the operator.
#[must_use]
pub fn unavailable_reason(reason: SubscriptionUsageUnavailable, account: &str) -> String {
    match reason {
        SubscriptionUsageUnavailable::NotConnected => "credential is not ready".to_owned(),
        SubscriptionUsageUnavailable::CredentialState(state) => match state {
            SubscriptionUsageCredentialState::RefreshRequired => format!(
                "credential needs a token refresh; the next turn refreshes it, or run `plak refresh {account}`"
            ),
            // Grok reclaims an ambiguous refresh on its next refresh, so a
            // re-login is the fallback rather than the first remedy there.
            SubscriptionUsageCredentialState::Quarantined if account == "grok" => {
                "credential is quarantined after an ambiguous refresh; the next turn retries it, or run `plak refresh grok`; run `plak login grok` if that fails"
                    .to_owned()
            }
            SubscriptionUsageCredentialState::Quarantined => format!(
                "credential is quarantined after an ambiguous refresh; run `plak login {account}` to recover"
            ),
            SubscriptionUsageCredentialState::Revoked => {
                format!("credential is revoked; run `plak login {account}`")
            }
            SubscriptionUsageCredentialState::LoginRequired => format!(
                "the stored login was made by an earlier build and cannot be used; run `plak login {account}`"
            ),
            SubscriptionUsageCredentialState::WrittenByNewerBuild => format!(
                "the stored login was written by a newer plak build; use that build, or replace it with `plak login {account}` (that build must then log in again)"
            ),
        },
        SubscriptionUsageUnavailable::Busy => USAGE_BUSY_REASON.to_owned(),
        SubscriptionUsageUnavailable::LocalState => {
            "local compatibility or storage state refused this read".to_owned()
        }
        SubscriptionUsageUnavailable::Unauthorized => {
            "provider rejected the selected credential".to_owned()
        }
        SubscriptionUsageUnavailable::RateLimited => USAGE_RATE_LIMITED_REASON.to_owned(),
        SubscriptionUsageUnavailable::ProviderUnavailable => {
            "provider usage service is unavailable".to_owned()
        }
        SubscriptionUsageUnavailable::Transport(cause) => {
            cause.map_or_else(|| "usage HTTP request to the provider failed".to_owned(), |cause| format!("usage HTTP request failed: {cause}"))
        }
        SubscriptionUsageUnavailable::InvalidResponse => {
            "provider returned an invalid bounded response".to_owned()
        }
        SubscriptionUsageUnavailable::Stale => {
            "provider reported a usage window that has already ended".to_owned()
        }
        SubscriptionUsageUnavailable::AccountChanged => {
            "selected account changed during the read".to_owned()
        }
        SubscriptionUsageUnavailable::Unqualified => {
            "exact OAuth usage transport is not yet qualified; use the provider dashboard"
                .to_owned()
        }
    }
}

/// Title-cases a provider plan token for display: `pro` becomes `Pro`;
/// `ChatGPT Pro` is left alone because only a leading lowercase letter of each
/// word is raised. Nothing else about the wire value is changed.
fn title_case(plan: &str) -> String {
    plan.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::model::{
        SubscriptionUsageMeter, SubscriptionUsagePercent, SubscriptionUsageWindow, Timestamp,
    };
    use plakous::openrouter::decode_openrouter_key_usage;

    #[test]
    fn stale_reason_names_only_provider_window_data() {
        assert_eq!(
            unavailable_reason(SubscriptionUsageUnavailable::Stale, "grok"),
            "provider reported a usage window that has already ended"
        );
    }

    fn codex_snapshot() -> Result<SubscriptionUsageRead, Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1_788_000_000, 0)?;
        Ok(SubscriptionUsageRead::Snapshot(
            SubscriptionUsageSnapshot::new(
                observed,
                Some("ChatGPT Pro".to_owned()),
                vec![SubscriptionUsageMeter::new(
                    "codex".to_owned(),
                    None,
                    vec![
                        SubscriptionUsageWindow::new(
                            Some("primary".to_owned()),
                            SubscriptionUsagePercent::new(22.9)?,
                            Some(SubscriptionUsagePeriod::Rolling {
                                duration_seconds: 18_000,
                            }),
                            Some(Timestamp::new(1_788_018_000, 0)?),
                        )?,
                        SubscriptionUsageWindow::new(
                            Some("secondary".to_owned()),
                            SubscriptionUsagePercent::new(50.7)?,
                            Some(SubscriptionUsagePeriod::Rolling {
                                duration_seconds: 604_800,
                            }),
                            Some(Timestamp::new(1_788_600_000, 0)?),
                        )?,
                    ],
                )?],
            )?,
        ))
    }

    fn grok_snapshot() -> Result<SubscriptionUsageRead, Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1_788_000_000, 0)?;
        Ok(SubscriptionUsageRead::Snapshot(
            SubscriptionUsageSnapshot::new(
                observed,
                Some("SuperGrok Heavy".to_owned()),
                vec![SubscriptionUsageMeter::new(
                    "included-credits".to_owned(),
                    None,
                    vec![SubscriptionUsageWindow::new(
                        None,
                        SubscriptionUsagePercent::new(46.4)?,
                        Some(SubscriptionUsagePeriod::ProviderPeriod {
                            kind: "USAGE_PERIOD_TYPE_WEEKLY".to_owned(),
                            starts_at: None,
                        }),
                        Some(Timestamp::new(1_788_300_000, 0)?),
                    )?],
                )?],
            )?
            .with_balances(vec![
                SubscriptionUsageBalance::new(
                    SUBSCRIPTION_BALANCE_PREPAID_CREDITS.to_owned(),
                    Some(9999),
                    None,
                    None,
                )?,
                SubscriptionUsageBalance::new(
                    SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
                    None,
                    Some(355),
                    Some(5000),
                )?,
            ])?,
        ))
    }

    fn openrouter_usage() -> Result<VerticalObservation, Box<dyn std::error::Error>> {
        let usage = decode_openrouter_key_usage(
            br#"{"data":{"limit":10.0,"limit_remaining":6.42,"usage":3.58,"usage_daily":0.4,
                 "usage_weekly":1.1,"usage_monthly":2.9,"is_free_tier":true}}"#,
            Timestamp::new(1_788_000_000, 0)?,
        )?;
        Ok(VerticalObservation::OpenRouterKey(
            Box::new(usage),
            "OPENROUTER_API_KEY".to_owned(),
        ))
    }

    #[test]
    fn all_four_verticals_are_always_listed_in_order() {
        let view = project_limits([], SessionLedgerView::default());
        assert_eq!(view.verticals.len(), 4);
        let order: Vec<_> = view
            .verticals
            .iter()
            .map(|vertical| vertical.vertical.clone())
            .collect();
        assert_eq!(order, product_verticals().to_vec());
        let text = render_limits_text(&view);
        for vertical in product_verticals() {
            assert!(
                text.contains(vertical.provider_label()),
                "{} missing from:\n{text}",
                vertical.provider_label()
            );
        }
    }

    #[test]
    fn claude_ready_meters_include_fable_as_a_grouped_weekly_window()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = |name: &str, used: f64, seconds: u64, reset: u64| {
            SubscriptionUsageWindow::new(
                Some(name.to_owned()),
                SubscriptionUsagePercent::new(used).expect("percent"),
                Some(SubscriptionUsagePeriod::Rolling {
                    duration_seconds: seconds,
                }),
                Some(Timestamp::new(reset, 0).expect("timestamp")),
            )
            .expect("window")
        };
        let snapshot = SubscriptionUsageSnapshot::new(
            Timestamp::new(1_788_100_000, 0)?,
            None,
            vec![
                SubscriptionUsageMeter::new(
                    "claude".to_owned(),
                    None,
                    vec![
                        window("five_hour", 5.0, 18_000, 1_788_110_000),
                        window("seven_day", 72.0, 604_800, 1_788_392_640),
                    ],
                )?,
                SubscriptionUsageMeter::new(
                    "fable".to_owned(),
                    Some("Fable".to_owned()),
                    vec![window("seven_day", 96.0, 604_800, 1_788_392_640)],
                )?,
            ],
        )?;
        let view = project_limits(
            [(
                claude_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        let claude = view
            .verticals
            .iter()
            .find(|vertical| vertical.vertical == claude_vertical())
            .expect("Claude is always listed");
        let VerticalUsageState::Ready { meters } = &claude.state else {
            panic!("Claude is Ready");
        };
        assert_eq!(meters[0].label, "Weekly limit");
        assert_eq!(meters[1].label, "5h limit");
        assert_eq!(meters[2].group.as_deref(), Some("Fable limit"));
        assert_eq!(meters[2].label, "Weekly limit");
        let text = render_limits_text(&view);
        assert!(text.contains("Fable limit:"), "{text}");
        assert!(text.contains("  95% left"), "{text}");
        assert!(text.contains("  28% left"), "{text}");
        assert!(text.contains("   4% left"), "{text}");
        Ok(())
    }

    #[test]
    fn claude_account_email_is_projected_onto_the_account_line()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = SubscriptionUsageSnapshot::new(
            Timestamp::new(1_788_000_000, 0)?,
            Some("Claude Max".to_owned()),
            Vec::new(),
        )?
        .with_account(Some("user@example.com".to_owned()))?;
        let view = project_limits(
            [(
                claude_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        let claude = &view.verticals[2];
        assert_eq!(claude.account_email.as_deref(), Some("user@example.com"));
        assert_eq!(claude.plan_label.as_deref(), Some("Claude Max"));
        let text = render_limits_text(&view);
        assert!(
            text.contains("\nClaude\n  Account: user@example.com (Claude Max)\n"),
            "{text}"
        );
        Ok(())
    }

    #[test]
    fn a_non_ready_vertical_carries_no_numbers_and_no_bar() {
        let view = project_limits(
            [(codex_vertical(), VerticalObservation::NotConnected)],
            SessionLedgerView::default(),
        );
        let codex = &view.verticals[0];
        assert_eq!(codex.state, VerticalUsageState::NotConnected);
        assert_eq!(codex.plan_label, None);
        let text = render_limits_text(&view);
        let block = text
            .split("\n\n")
            .find(|block| block.starts_with("Codex"))
            .expect("Codex block");
        assert!(block.contains("not connected"), "{block}");
        assert!(!block.contains(LIMIT_BAR_FILL), "{block}");
    }

    #[test]
    fn plan_and_tier_labels_survive_from_the_wire() -> Result<(), Box<dyn std::error::Error>> {
        let view = project_limits(
            [
                (
                    codex_vertical(),
                    VerticalObservation::Native(codex_snapshot()?),
                ),
                (
                    supergrok_vertical(),
                    VerticalObservation::Native(grok_snapshot()?),
                ),
            ],
            SessionLedgerView::default(),
        );
        assert_eq!(view.verticals[0].plan_label.as_deref(), Some("ChatGPT Pro"));
        assert_eq!(
            view.verticals[1].plan_label.as_deref(),
            Some("SuperGrok Heavy")
        );
        let text = render_limits_text(&view);
        // The vertical names the header; the plan is its own account line.
        assert!(text.contains("Codex\n  Account: ChatGPT Pro"), "{text}");
        assert!(
            text.contains("SuperGrok\n  Account: SuperGrok Heavy"),
            "{text}"
        );
        Ok(())
    }

    #[test]
    fn codex_reports_weekly_before_five_hour_with_floored_percentages()
    -> Result<(), Box<dyn std::error::Error>> {
        let view = project_limits(
            [(
                codex_vertical(),
                VerticalObservation::Native(codex_snapshot()?),
            )],
            SessionLedgerView::default(),
        );
        let VerticalUsageState::Ready { meters } = &view.verticals[0].state else {
            panic!("Codex is Ready");
        };
        assert_eq!(meters[0].label, "Weekly limit");
        assert_eq!(meters[1].label, "5h limit");
        assert_eq!(
            meters[0].reading,
            UsageMeterReading::Percent {
                floored_percent: 50,
                exact_percent: "50.7".to_owned(),
                percent_left: Some(49),
            }
        );
        assert_eq!(
            meters[1].reading,
            UsageMeterReading::Percent {
                floored_percent: 22,
                exact_percent: "22.9".to_owned(),
                percent_left: Some(77),
            }
        );
        Ok(())
    }

    #[test]
    fn supergrok_names_its_one_window_from_the_reported_period_and_keeps_balances()
    -> Result<(), Box<dyn std::error::Error>> {
        let view = project_limits(
            [(
                supergrok_vertical(),
                VerticalObservation::Native(grok_snapshot()?),
            )],
            SessionLedgerView::default(),
        );
        let vertical = &view.verticals[1];
        let VerticalUsageState::Ready { meters } = &vertical.state else {
            panic!("SuperGrok is Ready");
        };
        assert_eq!(meters.len(), 1);
        // The cadence is the provider's own period type, never guessed.
        assert_eq!(meters[0].label, "Weekly limit");
        assert_eq!(
            vertical
                .balances
                .iter()
                .map(|balance| (balance.label.as_str(), balance.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("Credits", "$99.99"),
                ("Pay as you go", "$3.55 used of $50.00 cap"),
            ]
        );
        let text = render_limits_text(&view);
        assert!(
            text.contains("Credits") && text.contains("$99.99"),
            "{text}"
        );
        assert!(
            text.contains("Pay as you go") && text.contains("$3.55 used of $50.00 cap"),
            "{text}"
        );
        Ok(())
    }

    /// proto3 omits a true 0.0 `creditUsagePercent`. A named weekly SuperGrok
    /// period with no percentage is 0% used (a full bar), and zero prepaid /
    /// disabled pay-as-you-go lines are omitted.
    #[test]
    fn a_supergrok_period_without_a_usage_figure_is_an_untouched_weekly_limit()
    -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1_788_000_000, 0)?;
        let snapshot = SubscriptionUsageSnapshot::new(
            observed,
            None,
            vec![SubscriptionUsageMeter::new(
                "included-credits".to_owned(),
                None,
                vec![SubscriptionUsageWindow::new(
                    None,
                    SubscriptionUsagePercent::new(0.0)?,
                    Some(SubscriptionUsagePeriod::ProviderPeriod {
                        kind: "USAGE_PERIOD_TYPE_WEEKLY".to_owned(),
                        starts_at: Some(observed),
                    }),
                    Some(Timestamp::new(1_789_046_548, 0)?),
                )?],
            )?],
        )?
        .with_balances(vec![
            SubscriptionUsageBalance::new(
                SUBSCRIPTION_BALANCE_PREPAID_CREDITS.to_owned(),
                Some(0),
                None,
                None,
            )?,
            SubscriptionUsageBalance::new(
                SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO.to_owned(),
                None,
                Some(0),
                Some(0),
            )?,
        ])?;
        let view = project_limits(
            [(
                supergrok_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        let vertical = &view.verticals[1];
        let VerticalUsageState::Ready { meters } = &vertical.state else {
            panic!("SuperGrok is Ready");
        };
        assert_eq!(meters.len(), 1);
        assert_eq!(meters[0].label, "Weekly limit");
        assert_eq!(
            meters[0].reading,
            UsageMeterReading::Percent {
                floored_percent: 0,
                exact_percent: "0".to_owned(),
                percent_left: Some(100),
            }
        );
        let text = render_limits_text(&view);
        assert!(text.contains("100% left"), "{text}");
        assert!(text.contains(&limit_bar(100)), "{text}");
        assert!(!text.contains("$0.00"), "{text}");
        assert!(!text.contains("Pay as you go"), "{text}");
        assert!(!text.contains("Credits"), "{text}");
        Ok(())
    }

    #[test]
    fn a_supergrok_period_with_no_reported_cadence_is_not_named_weekly()
    -> Result<(), Box<dyn std::error::Error>> {
        let observed = Timestamp::new(1_788_000_000, 0)?;
        let snapshot = SubscriptionUsageSnapshot::new(
            observed,
            None,
            vec![SubscriptionUsageMeter::new(
                "included-credits".to_owned(),
                None,
                vec![SubscriptionUsageWindow::new(
                    None,
                    SubscriptionUsagePercent::new(10.0)?,
                    None,
                    Some(Timestamp::new(1_788_300_000, 0)?),
                )?],
            )?],
        )?;
        let view = project_limits(
            [(
                supergrok_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        let VerticalUsageState::Ready { meters } = &view.verticals[1].state else {
            panic!("SuperGrok is Ready");
        };
        assert_eq!(meters[0].label, "Usage limit");
        Ok(())
    }

    #[test]
    fn openrouter_amounts_render_and_a_null_limit_is_no_cap()
    -> Result<(), Box<dyn std::error::Error>> {
        let view = project_limits(
            [(openrouter_vertical(), openrouter_usage()?)],
            SessionLedgerView::default(),
        );
        let text = render_limits_text(&view);
        // The winning source labels the key, not the provider's masked label.
        // Only an explicit `is_free_tier: true` becomes a tier line.
        assert!(
            text.contains("OpenRouter\n  Account: OPENROUTER_API_KEY · free tier"),
            "{text}"
        );
        assert!(!text.contains("API key"), "{text}");
        assert!(text.contains("$10.00 cap"), "{text}");
        assert!(text.contains("$6.42 remaining"), "{text}");
        assert!(text.contains("$0.40 today"), "{text}");

        let uncapped = decode_openrouter_key_usage(
            br#"{"data":{"limit":null,"usage":1.0}}"#,
            Timestamp::new(1, 0)?,
        )?;
        let view = project_limits(
            [(
                openrouter_vertical(),
                VerticalObservation::OpenRouterKey(
                    Box::new(uncapped),
                    "sha256:1234abcd".to_owned(),
                ),
            )],
            SessionLedgerView::default(),
        );
        let text = render_limits_text(&view);
        assert!(text.contains("no cap"), "{text}");
        assert!(!text.contains("$0.00 cap"), "{text}");
        assert!(text.contains("Account: sha256:1234abcd"), "{text}");
        // A file fingerprint never implies a paid tier.
        assert!(!text.contains("paid tier"), "{text}");
        assert!(!text.contains("free tier"), "{text}");
        Ok(())
    }

    #[test]
    fn openrouter_usage_never_renders_provider_key_material()
    -> Result<(), Box<dyn std::error::Error>> {
        let usage = decode_openrouter_key_usage(
            br#"{"data":{"label":"sk-or-masked-secret-suffix","usage":1.0}}"#,
            Timestamp::new(1, 0)?,
        )?;
        let view = project_limits(
            [(
                openrouter_vertical(),
                VerticalObservation::OpenRouterKey(
                    Box::new(usage),
                    "OPENROUTER_API_KEY".to_owned(),
                ),
            )],
            SessionLedgerView::default(),
        );
        let text = render_limits_text(&view);
        assert!(!text.contains("sk-or-masked-secret-suffix"));
        Ok(())
    }

    /// Spec 02 §7.2: a reset reads as the time first and then the day it falls
    /// on, in the operator's own wall clock.
    #[test]
    fn a_reset_names_the_time_then_the_day() {
        let rendered = format_reset_wall_clock(1_788_018_000);
        assert!(!rendered.contains('T'), "{rendered}");
        assert!(!rendered.contains("2026"), "{rendered}");
        let (clock, day) = rendered.split_once(" on ").expect("time then day");
        let (hour, minute) = clock.split_once(':').expect("a wall clock time");
        assert!(hour.len() == 2 && minute.len() == 2, "{rendered}");
        // `6 Sep`, or `6 Sep UTC` when the local offset is indeterminate.
        assert!(day.split(' ').count() <= 3, "{rendered}");
    }

    /// Spec 02 §7.2: an observation time is wall clock, and never an unlabelled
    /// instant in a zone the operator does not use.
    #[test]
    fn reset_times_render_as_wall_clock_and_name_utc_when_local_is_unknown() {
        let rendered = format_unix_seconds(1_788_018_000);
        // Never RFC 3339: no `T` separator and no bare `Z`.
        assert!(!rendered.contains('T'), "{rendered}");
        assert!(!rendered.ends_with('Z'), "{rendered}");
        assert!(rendered.contains("2026-"), "{rendered}");
        match time::UtcOffset::current_local_offset() {
            // The local offset is known: the value is local, and says nothing
            // about UTC because it is not UTC.
            Ok(offset) if offset != time::UtcOffset::UTC => {
                assert!(!rendered.ends_with(" UTC"), "{rendered}");
            }
            // Indeterminate (or genuinely UTC): the zone is named rather than
            // presented as if it were the operator's own.
            _ => assert!(
                rendered.ends_with(" UTC") || !rendered.ends_with('Z'),
                "{rendered}"
            ),
        }
    }

    #[test]
    fn cost_is_never_rendered_as_zero_when_it_is_unknown() {
        let ledger = project_session_ledger(120, 100, 30, 10, 4, 3, Some(Duration::from_secs(192)));
        assert!(matches!(ledger.cost, UsageCostView::NotAvailable { .. }));
        let view = project_limits([], ledger);
        let text = render_limits_text(&view);
        assert!(text.contains("Cost:"), "{text}");
        assert!(text.contains("not available"), "{text}");
        assert!(!text.contains("$0"), "{text}");
        assert!(
            text.contains("turn wall time (host-measured): 3m12s"),
            "{text}"
        );
        assert!(!text.contains("API time"), "{text}");
    }

    #[test]
    fn a_stale_projection_reports_its_age_and_never_claims_unavailable()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut view = project_limits(
            [(
                codex_vertical(),
                VerticalObservation::Native(codex_snapshot()?),
            )],
            SessionLedgerView::default(),
        );
        view.stale_age_seconds = Some(75);
        let text = render_limits_text(&view);
        assert!(text.contains("1m15s old"), "{text}");
        // The held Codex numbers are served, not replaced by an unavailable
        // claim; only Claude, which has no transport at all, says unavailable.
        let codex = text
            .split("\n\n")
            .find(|block| block.starts_with("Codex"))
            .expect("Codex block");
        assert!(codex.contains("Weekly limit"), "{codex}");
        // 50.7% used is 49% left, floored the same way.
        assert!(codex.contains("49% left"), "{codex}");
        assert!(!codex.contains("unavailable"), "{codex}");
        assert!(!codex.contains("not connected"), "{codex}");
        Ok(())
    }

    fn strip_sgr(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
            out.push(ch);
        }
        out
    }

    #[test]
    /// The fill is the percent LEFT over exactly twenty floored cells: a full
    /// bar is an untouched limit, an empty bar a reached one (spec 02 §7.2).
    fn the_bar_fill_is_the_percent_left_over_twenty_floored_cells() {
        for (percent_left, filled) in [(0_u8, 0_usize), (1, 0), (50, 10), (99, 19), (100, 20)] {
            let bar = limit_bar(percent_left);
            let visible: String = strip_sgr(&bar);
            assert_eq!(
                visible.chars().count(),
                LIMIT_BAR_CELLS + 2,
                "brackets included"
            );
            assert_eq!(
                visible
                    .chars()
                    .filter(|cell| *cell == LIMIT_BAR_FILL)
                    .count(),
                LIMIT_BAR_CELLS
            );
            let empty = LIMIT_BAR_CELLS - filled;
            if empty == 0 {
                assert!(!bar.contains('\u{1b}'));
            } else {
                assert!(bar.contains(LIMIT_BAR_EMPTY_SGR), "{bar:?}");
            }
            assert!(bar.starts_with('[') && bar.ends_with(']'));
        }
    }

    /// A lowercase wire `plan_type` such as `pro` is shown as `Pro`, and a wire
    /// e-mail (when one is carried) joins it on the Account line.
    #[test]
    fn plan_is_title_cased_and_email_joins_the_account_line()
    -> Result<(), Box<dyn std::error::Error>> {
        let snapshot = SubscriptionUsageSnapshot::new(
            Timestamp::new(1_788_000_000, 0)?,
            Some("pro".to_owned()),
            Vec::new(),
        )?
        .with_account(Some("operator@example.test".to_owned()))?;
        let view = project_limits(
            [(
                codex_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        assert_eq!(view.verticals[0].plan_label.as_deref(), Some("Pro"));
        assert_eq!(
            view.verticals[0].account_email.as_deref(),
            Some("operator@example.test")
        );
        let text = render_limits_text(&view);
        assert!(
            text.contains("\nCodex\n  Account: operator@example.test (Pro)\n"),
            "{text}"
        );
        assert!(
            !text.contains("Pro (Codex)") && !text.contains("Codex (Pro)"),
            "{text}"
        );
        Ok(())
    }

    /// Shaped like the operator's Codex account: the plan's own weekly window
    /// at 0.5% used, plus a `GPT-5.3-Codex-Spark` feature whose 5h window is
    /// untouched and whose weekly window is exhausted. The feature's windows
    /// arrive weekly-first, so labels must follow the seconds, never the
    /// primary/secondary position.
    #[test]
    fn codex_spark_feature_is_grouped_and_labelled_by_window_duration()
    -> Result<(), Box<dyn std::error::Error>> {
        let window = |name: &str, used: f64, seconds: u64, reset: u64| {
            SubscriptionUsageWindow::new(
                Some(name.to_owned()),
                SubscriptionUsagePercent::new(used).expect("percent"),
                Some(SubscriptionUsagePeriod::Rolling {
                    duration_seconds: seconds,
                }),
                Some(Timestamp::new(reset, 0).expect("timestamp")),
            )
            .expect("window")
        };
        let snapshot = SubscriptionUsageSnapshot::new(
            Timestamp::new(1_788_100_000, 0)?,
            Some("pro".to_owned()),
            vec![
                SubscriptionUsageMeter::new(
                    "codex".to_owned(),
                    None,
                    vec![window("primary", 0.5, 604_800, 1_788_392_640)],
                )?,
                SubscriptionUsageMeter::new(
                    "codex_spark".to_owned(),
                    Some("GPT-5.3-Codex-Spark".to_owned()),
                    vec![
                        window("primary", 100.0, 604_800, 1_788_391_380),
                        window("secondary", 0.0, 18_000, 1_788_110_940),
                    ],
                )?,
            ],
        )?;
        let view = project_limits(
            [(
                codex_vertical(),
                VerticalObservation::Native(SubscriptionUsageRead::Snapshot(snapshot)),
            )],
            SessionLedgerView::default(),
        );
        let codex = &view.verticals[0];
        assert_eq!(codex.plan_label.as_deref(), Some("Pro"));
        assert_eq!(codex.account_email, None);
        let VerticalUsageState::Ready { meters } = &codex.state else {
            panic!("Codex is Ready");
        };
        assert_eq!(meters.len(), 3);
        assert_eq!(meters[0].group, None);
        assert_eq!(meters[0].label, "Weekly limit");
        assert_eq!(
            meters[0].reading,
            UsageMeterReading::Percent {
                floored_percent: 0,
                exact_percent: "0.5".to_owned(),
                percent_left: Some(99),
            }
        );
        for meter in &meters[1..] {
            assert_eq!(meter.group.as_deref(), Some("GPT-5.3-Codex-Spark limit"));
        }
        assert_eq!(meters[1].label, "Weekly limit");
        assert_eq!(
            meters[1].reading,
            UsageMeterReading::Percent {
                floored_percent: 100,
                exact_percent: "100".to_owned(),
                percent_left: Some(0),
            }
        );
        assert_eq!(meters[2].label, "5h limit");
        assert_eq!(
            meters[2].reading,
            UsageMeterReading::Percent {
                floored_percent: 0,
                exact_percent: "0".to_owned(),
                percent_left: Some(100),
            }
        );
        let text = render_limits_text(&view);
        assert!(text.contains("\nCodex\n  Account: Pro\n"), "{text}");
        assert!(text.contains("GPT-5.3-Codex-Spark limit:"), "{text}");
        assert!(text.contains(" 99% left"), "{text}");
        assert!(text.contains("  0% left"), "{text}");
        assert!(text.contains("100% left"), "{text}");
        let bar_at = |needle: &str| {
            text.lines()
                .find(|line| line.contains(needle))
                .and_then(|line| line.find('['))
        };
        assert_eq!(
            bar_at("99% left"),
            bar_at("  0% left"),
            "grouped Spark bars must share a column with the plan meters:\n{text}"
        );
        assert_eq!(bar_at("99% left"), bar_at("100% left"), "{text}");
        Ok(())
    }

    /// A local lock miss or a fenced credential is named as exactly that,
    /// never as a failed transport; only `Transport` says the HTTP call failed.
    #[test]
    fn local_refusals_are_never_called_a_transport_failure() {
        let busy = unavailable_reason(SubscriptionUsageUnavailable::Busy, "grok");
        assert_eq!(busy, USAGE_BUSY_REASON);
        assert!(!busy.contains("transport"), "{busy}");
        for (state, expected, remedy) in [
            (
                SubscriptionUsageCredentialState::RefreshRequired,
                "credential needs a token refresh",
                "the next turn refreshes it, or run `plak refresh grok`",
            ),
            (
                SubscriptionUsageCredentialState::Quarantined,
                "credential is quarantined",
                "plak login grok",
            ),
            (
                SubscriptionUsageCredentialState::Revoked,
                "credential is revoked",
                "plak login grok",
            ),
        ] {
            let text =
                unavailable_reason(SubscriptionUsageUnavailable::CredentialState(state), "grok");
            assert!(text.contains(expected), "{text}");
            assert!(text.contains(remedy), "{text}");
            assert!(!text.contains("transport"), "{text}");
        }
        let grok_quarantine = unavailable_reason(
            SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ),
            "grok",
        );
        assert!(
            grok_quarantine.contains("plak refresh grok"),
            "{grok_quarantine}"
        );
        let codex_quarantine = unavailable_reason(
            SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined,
            ),
            "codex",
        );
        assert!(
            codex_quarantine.contains("run `plak login codex` to recover"),
            "{codex_quarantine}"
        );
        assert!(
            !codex_quarantine.contains("plak refresh"),
            "{codex_quarantine}"
        );
        let transport = unavailable_reason(SubscriptionUsageUnavailable::Transport(None), "grok");
        assert!(transport.contains("HTTP request"), "{transport}");
        assert_eq!(
            unavailable_reason(SubscriptionUsageUnavailable::RateLimited, "claude"),
            USAGE_RATE_LIMITED_REASON
        );
    }

    fn ready_claude_view() -> UsageLimitsView {
        let mut view = project_limits([], SessionLedgerView::default());
        view.verticals[2].observed_at = Some("2026-09-04 12:00".to_owned());
        view.verticals[2].state = VerticalUsageState::Ready {
            meters: vec![UsageMeterView {
                label: "Weekly limit".to_owned(),
                group: None,
                reading: UsageMeterReading::Percent {
                    floored_percent: 10,
                    exact_percent: "10".to_owned(),
                    percent_left: Some(90),
                },
                resets_at: None,
            }],
        };
        view
    }

    fn claude_unavailable(reason: &str) -> UsageLimitsView {
        let mut view = project_limits([], SessionLedgerView::default());
        view.verticals[2].state = VerticalUsageState::Unavailable {
            reason: reason.to_owned(),
        };
        view
    }

    #[test]
    fn a_fresh_held_projection_is_served_as_current() {
        let mut view = ready_claude_view();
        view.stale_age_seconds = Some(12);
        let served = cached_if_fresh(&view, Duration::from_secs(59)).expect("still fresh");
        assert_eq!(served.stale_age_seconds, None);
        assert_eq!(served.verticals[2].state, view.verticals[2].state);
        assert!(cached_if_fresh(&view, USAGE_PROJECTION_FRESHNESS).is_none());
        assert!(cached_if_fresh(&view, Duration::from_secs(61)).is_none());
    }

    #[test]
    fn a_rate_limited_claude_vertical_keeps_the_last_ready_meters() {
        let previous = ready_claude_view();
        let incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
        let merged = preserve_ready_across_rate_limit(incoming, Some(&previous));
        assert_eq!(merged.verticals[2], previous.verticals[2]);
        assert!(matches!(
            merged.verticals[2].state,
            VerticalUsageState::Ready { .. }
        ));
        assert_eq!(
            merged.verticals[2].observed_at.as_deref(),
            Some("2026-09-04 12:00")
        );
    }

    #[test]
    fn a_busy_claude_vertical_keeps_the_last_ready_meters() {
        let previous = ready_claude_view();
        let incoming = claude_unavailable(USAGE_BUSY_REASON);
        let merged = preserve_ready_across_rate_limit(incoming, Some(&previous));
        assert_eq!(merged.verticals[2], previous.verticals[2]);
    }

    #[test]
    fn a_busy_only_projection_is_not_served_as_fresh() {
        let view = claude_unavailable(USAGE_BUSY_REASON);
        assert!(projection_is_lock_contention(&view));
        assert!(cached_if_fresh(&view, Duration::from_secs(1)).is_none());
        let ready = ready_claude_view();
        assert!(!projection_is_lock_contention(&ready));
        assert!(cached_if_fresh(&ready, Duration::from_secs(1)).is_some());
    }

    #[test]
    fn a_transport_failure_does_not_keep_stale_ready_meters() {
        let previous = ready_claude_view();
        let incoming = claude_unavailable("usage HTTP request to the provider failed");
        let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
        assert_eq!(merged.verticals[2], incoming.verticals[2]);
        assert!(matches!(
            merged.verticals[2].state,
            VerticalUsageState::Unavailable { .. }
        ));
    }

    #[test]
    fn a_rate_limited_read_without_a_prior_ready_stays_unavailable() {
        let incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
        let merged = preserve_ready_across_rate_limit(incoming.clone(), None);
        assert_eq!(merged, incoming);
        let previous = project_limits([], SessionLedgerView::default());
        let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
        assert_eq!(merged.verticals[2], incoming.verticals[2]);
    }

    #[test]
    fn a_successful_sibling_vertical_still_updates_across_a_claude_429() {
        let previous = ready_claude_view();
        let mut incoming = claude_unavailable(USAGE_RATE_LIMITED_REASON);
        incoming.verticals[0].state = VerticalUsageState::Ready {
            meters: vec![UsageMeterView {
                label: "Weekly limit".to_owned(),
                group: None,
                reading: UsageMeterReading::Percent {
                    floored_percent: 1,
                    exact_percent: "1".to_owned(),
                    percent_left: Some(99),
                },
                resets_at: None,
            }],
        };
        let merged = preserve_ready_across_rate_limit(incoming.clone(), Some(&previous));
        assert_eq!(merged.verticals[0], incoming.verticals[0]);
        assert_eq!(merged.verticals[2], previous.verticals[2]);
    }

    #[test]
    fn the_rate_limit_sentence_is_the_exact_operator_text() {
        assert!(is_rate_limited_reason(USAGE_RATE_LIMITED_REASON));
        assert!(!is_rate_limited_reason(
            "usage HTTP request to the provider failed"
        ));
        assert!(is_busy_reason(USAGE_BUSY_REASON));
        assert!(!is_busy_reason(USAGE_RATE_LIMITED_REASON));
    }
}
