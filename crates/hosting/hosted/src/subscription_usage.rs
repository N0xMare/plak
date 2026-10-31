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
//! - while the Usage tab is open during a turn, allowances are re-read on that
//!   same interval over the coalesced request path, and not while it is closed;
//! - a rate-limited or Busy vertical keeps the last Ready meters rather than replacing them;
//! - a projection that is only Busy is not treated as fresh.

use std::fmt::Write as _;
use std::time::Duration;

use crate::openrouter::OpenRouterKeyUsage;
use plakous::model::{
    SUBSCRIPTION_BALANCE_PAY_AS_YOU_GO, SUBSCRIPTION_BALANCE_PREPAID_CREDITS,
    SubscriptionUsageBalance, SubscriptionUsageCredentialState, SubscriptionUsagePeriod,
    SubscriptionUsageRead, SubscriptionUsageSnapshot, SubscriptionUsageUnavailable,
};
#[cfg(feature = "openrouter")]
#[path = "subscription_usage/openrouter.rs"]
mod openrouter_usage;
#[cfg(not(feature = "openrouter"))]
#[path = "subscription_usage/openrouter_unavailable.rs"]
mod openrouter_usage;
use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::views::presentation::{
    SessionLedgerView, UsageBalanceView, UsageCostView, UsageLimitsView, UsageMeterReading,
    UsageMeterView, UsageVertical, VerticalUsageState, VerticalUsageView,
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

/// Exact operator sentence for [`SubscriptionUsageUnavailable::Busy`]. Every
/// credential may be busy in this process or in a peer; neither is assumed.
pub const USAGE_BUSY_REASON: &str = "the subscription credential is busy (usage read, login, or refresh); nothing was sent, retry in a moment";

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
    let verticals: Vec<_> = crate::capabilities::ROUTES
        .iter()
        .filter_map(|route| {
            route
                .usage_label
                .map(|label| UsageVertical::new(route.account, label))
        })
        .collect();
    // Preserve the existing fixed-array API without a second route roster.
    match verticals.try_into() {
        Ok(verticals) => verticals,
        Err(_) => unreachable!("Usage registry must contain four verticals"),
    }
}

/// The ChatGPT/Codex OAuth subscription vertical.
#[must_use]
pub fn codex_vertical() -> UsageVertical {
    vertical_for_route(&crate::capabilities::CODEX)
}

/// The `SuperGrok` OAuth subscription vertical.
#[must_use]
pub fn supergrok_vertical() -> UsageVertical {
    vertical_for_route(&crate::capabilities::GROK)
}

/// The Claude OAuth subscription vertical.
#[must_use]
pub fn claude_vertical() -> UsageVertical {
    vertical_for_route(&crate::capabilities::CLAUDE)
}

/// The `OpenRouter` inference-key vertical.
#[must_use]
pub fn openrouter_vertical() -> UsageVertical {
    vertical_for_route(&crate::capabilities::OPENROUTER)
}

fn vertical_for_route(route: &crate::capabilities::RouteCapability) -> UsageVertical {
    match route.usage_label {
        Some(label) => UsageVertical::new(route.account, label),
        None => unreachable!("route must have a Usage vertical"),
    }
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
    /// The selected inference key was rejected, with its original source retained.
    RejectedKey {
        /// Neutral failure facts.
        reason: String,
        /// Selected key source.
        source: plakous_host_protocol::views::KeySource,
    },
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
            repair: None,
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
            repair: None,
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
            let repair = match reason {
                SubscriptionUsageUnavailable::CredentialState(state) => {
                    Some(plakous_host_protocol::views::UsageRepair::Credential(
                        usage_credential_state(*state),
                    ))
                }
                _ => None,
            };
            VerticalUsageView {
                vertical,
                account_email: None,
                plan_label: None,
                observed_at: Some(format_unix_seconds(observed_at.seconds())),
                state,
                balances: Vec::new(),
                repair,
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
                repair: None,
            }
        }
        VerticalObservation::RejectedKey { reason, source } => VerticalUsageView {
            vertical,
            account_email: None,
            plan_label: None,
            observed_at: None,
            state: VerticalUsageState::Unavailable {
                reason: reason.clone(),
            },
            balances: Vec::new(),
            repair: Some(plakous_host_protocol::views::UsageRepair::RejectedKey(
                *source,
            )),
        },
        VerticalObservation::OpenRouterKey(usage, label) => {
            openrouter_usage::project(vertical, usage, label)
        }
    }
}

fn usage_credential_state(
    state: SubscriptionUsageCredentialState,
) -> plakous_host_protocol::views::UsageCredentialState {
    use plakous_host_protocol::views::UsageCredentialState as S;
    match state {
        SubscriptionUsageCredentialState::RefreshRequired => S::RefreshRequired,
        SubscriptionUsageCredentialState::Quarantined => S::Quarantined,
        SubscriptionUsageCredentialState::Revoked => S::Revoked,
        SubscriptionUsageCredentialState::LoginRequired => S::LoginRequired,
        SubscriptionUsageCredentialState::WrittenByNewerBuild => S::WrittenByNewerBuild,
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
                let _ = writeln!(out, "not connected");
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
    let route = match crate::capabilities::for_native(provider) {
        Some(route) => route,
        None => unreachable!(),
    };
    vertical_for_route(route)
}

/// Returns the operator-facing label for one native subscription vertical.
#[must_use]
pub const fn subscription_provider_label(provider: NativeProvider) -> &'static str {
    let route = match crate::capabilities::for_native(provider) {
        Some(route) => route,
        None => unreachable!(),
    };
    match route.subscription_label {
        Some(label) => label,
        None => unreachable!(),
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
            SubscriptionUsageCredentialState::RefreshRequired => "credential needs a token refresh; the next turn refreshes it, or refresh explicitly".to_owned(),
            // Grok reclaims an ambiguous refresh on its next refresh, so a
            // re-login is the fallback rather than the first remedy there.
            SubscriptionUsageCredentialState::Quarantined if account == "grok" => {
                "credential is quarantined after an ambiguous refresh; the next turn retries it, or refresh explicitly; replace the login if that fails"
                    .to_owned()
            }
            SubscriptionUsageCredentialState::Quarantined => "credential is quarantined after an ambiguous refresh; replace the login to recover".to_owned(),
            SubscriptionUsageCredentialState::Revoked => {
                "credential is revoked; replace the login".to_owned()
            }
            SubscriptionUsageCredentialState::LoginRequired => "the stored login was made by an earlier build and cannot be used; replace the login".to_owned(),
            SubscriptionUsageCredentialState::WrittenByNewerBuild => "the stored login was written by a newer build; use that build or replace the login; the writing build must then log in again".to_owned(),
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
mod tests;
