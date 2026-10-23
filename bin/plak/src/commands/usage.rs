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

/// Every connected subscription credential under a current profile offers an
/// explicit Usage refresh; states whose remedy is `plak login` (or another
/// build) do not. SuperGrok's quarantine is reclaimable by its next refresh.
pub(crate) fn can_refresh_usage(
    provider: NativeProvider,
    status: crate::presentation::CliStatus,
) -> bool {
    status.compatibility == CompatibilityState::Current
        && match status.credential {
            CredentialState::Ready
            | CredentialState::RefreshRequired
            | CredentialState::Refreshing => true,
            CredentialState::Quarantined => provider == NativeProvider::Xai,
            _ => false,
        }
}

/// The subscription accounts whose Usage vertical offers `[refresh]`.
pub(crate) fn usage_refresh_accounts(backend: &mut impl AccountOps) -> Vec<String> {
    [
        NativeProvider::Codex,
        NativeProvider::Xai,
        NativeProvider::Claude,
    ]
    .into_iter()
    .filter(|provider| {
        backend
            .offline_status(*provider)
            .is_ok_and(|status| can_refresh_usage(*provider, status))
    })
    .map(|provider| super::support::cli_provider_name(provider).to_owned())
    .collect()
}

/// Whether the credential must be refreshed before Usage re-reads it, decided
/// from local state only. The offline posture stays `Ready` for an expired but
/// refreshable access token, so a `Ready` posture also checks the stored
/// expiry. A `Refreshing` credential is renewed by its claim holder.
fn usage_needs_refresh(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
    credential: CredentialState,
) -> CliResult<bool> {
    match credential {
        CredentialState::RefreshRequired => Ok(true),
        CredentialState::Quarantined => Ok(provider == NativeProvider::Xai),
        CredentialState::Ready => backend.access_token_expired(provider),
        _ => Ok(false),
    }
}

/// The Usage action re-reads every offered account; when the credential needs
/// a refresh it first runs the same forced, claimed refresh as `plak refresh`.
/// Revalidate the offline posture because a button may outlive its snapshot.
pub(crate) fn refresh_usage_credential(
    backend: &mut impl AccountOps,
    provider: NativeProvider,
) -> CliResult<()> {
    let status = backend.offline_status(provider)?;
    if !can_refresh_usage(provider, status) {
        return Err(plakous_hosted::error::CliFailure::new(
            "credential can no longer be refreshed from Usage; reopen Usage",
        ));
    }
    if !usage_needs_refresh(backend, provider, status.credential)? {
        return Ok(());
    }
    let (state, _) = backend.refresh_now(provider)?;
    if state != CredentialState::Ready {
        return Err(plakous_hosted::error::CliFailure::new(
            "refresh did not produce a ready credential; reopen Usage for its remedy",
        ));
    }
    Ok(())
}
