//! Subscription allowance collection for `plak usage`.
//!
//! Every vertical is collected through the one host-owned Usage projection, so
//! the CLI and the TUI Limits surface can never disagree.

use plakous::subscriptions::NativeProvider;
use plakous_host_protocol::views::presentation::UsageVertical;

use super::application::AccountOps;
use crate::presentation::{CompatibilityState, CredentialState};
use plakous_hosted::error::CliResult;
use plakous_hosted::subscription_usage as product_usage;

/// Collects the four Usage verticals for the CLI.
///
/// With no selection every vertical is collected. A selection returns only
/// that vertical, so unrelated accounts are not falsely presented as
/// disconnected merely because they were deliberately not probed.
pub fn collect_usage_limits(
    selected: Option<UsageVertical>,
    backend: &mut impl AccountOps,
) -> CliResult<plakous_host_protocol::views::presentation::UsageLimitsView> {
    let mut observations = Vec::new();
    for (vertical, provider) in [
        (product_usage::codex_vertical(), NativeProvider::Codex),
        (product_usage::supergrok_vertical(), NativeProvider::Xai),
        (product_usage::claude_vertical(), NativeProvider::Claude),
    ] {
        if selected
            .as_ref()
            .is_some_and(|selected| *selected != vertical)
        {
            continue;
        }
        let status = backend.offline_status(provider)?;
        if status.compatibility != CompatibilityState::Current
            || status.credential != CredentialState::Ready
        {
            observations.push((vertical, product_usage::VerticalObservation::NotConnected));
            continue;
        }
        let usage = backend.subscription_usage(provider)?;
        observations.push((vertical, product_usage::VerticalObservation::Native(usage)));
    }
    if selected
        .as_ref()
        .is_none_or(|selected| *selected == product_usage::openrouter_vertical())
    {
        observations.push((
            product_usage::openrouter_vertical(),
            backend.openrouter_key_usage()?,
        ));
    }
    let mut view = product_usage::project_limits(
        observations,
        // No session is attached on this path, so no turn ledger exists yet.
        plakous_host_protocol::views::presentation::SessionLedgerView::default(),
    );
    if let Some(selected) = selected {
        view.verticals
            .retain(|vertical| vertical.vertical == selected);
    }
    Ok(view)
}

/// The closed account-name spelling accepted by `plak usage --account`.
pub fn usage_vertical_for_account(account: &str) -> Option<UsageVertical> {
    match account {
        "claude" => Some(product_usage::claude_vertical()),
        "codex" => Some(product_usage::codex_vertical()),
        "grok" | "supergrok" => Some(product_usage::supergrok_vertical()),
        "openrouter" => Some(product_usage::openrouter_vertical()),
        _ => None,
    }
}
