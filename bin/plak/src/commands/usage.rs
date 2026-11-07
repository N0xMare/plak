//! Subscription allowance collection for `plak usage`.
//!
//! Every vertical is collected through the one host-owned Usage projection, so
//! the CLI and the TUI Limits surface can never disagree.

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
    let roster = if let Some(selected) = &selected {
        vec![selected.clone()]
    } else {
        backend
            .accounts()?
            .into_iter()
            .filter_map(|account| usage_vertical_for_account(account.as_str()))
            .collect()
    };
    for vertical in roster {
        let account: plakous_hosted::account::AccountRef = vertical.account().parse()?;
        if account.vertical() == plakous_hosted::account::Vertical::OpenRouter {
            observations.push((vertical, backend.openrouter_key_usage(&account)?));
            continue;
        }
        let status = match backend.offline_status(&account) {
            Ok(status) => status,
            Err(failure) if selected.is_none() => {
                observations.push((
                    vertical,
                    product_usage::VerticalObservation::Unavailable(failure.message().to_owned()),
                ));
                continue;
            }
            Err(failure) => return Err(failure),
        };
        if status.compatibility != CompatibilityState::Current
            || status.credential != CredentialState::Ready
        {
            observations.push((vertical, product_usage::VerticalObservation::NotConnected));
            continue;
        }
        let observation = match backend.subscription_usage(&account) {
            Ok(usage) => product_usage::VerticalObservation::Native(usage),
            Err(failure) if selected.is_none() => {
                product_usage::VerticalObservation::Unavailable(failure.message().to_owned())
            }
            Err(failure) => return Err(failure),
        };
        observations.push((vertical, observation));
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
    let account: plakous_hosted::account::AccountRef = account.parse().ok()?;
    let label = account.vertical().capability().usage_label?;
    Some(UsageVertical::new(
        account.as_str(),
        if account.is_named() {
            account.as_str()
        } else {
            label
        },
    ))
}

/// Whether a Usage refresh may run for this posture: a connected
/// subscription credential under a current profile. States whose remedy is
/// `plak login` (or another build) never refresh from Usage. SuperGrok's
/// quarantine is reclaimable by its next refresh.
fn can_refresh_usage(
    provider: &plakous_hosted::account::AccountRef,
    status: crate::presentation::CliStatus,
) -> bool {
    status.compatibility == CompatibilityState::Current
        && match status.credential {
            CredentialState::Ready
            | CredentialState::RefreshRequired
            | CredentialState::Refreshing => true,
            CredentialState::Quarantined => {
                provider.vertical() == plakous_hosted::account::Vertical::Grok
            }
            _ => false,
        }
}

/// The subscription accounts whose Usage vertical offers `[refresh]`: only
/// those whose credential needs one, decided offline.
pub(crate) fn usage_refresh_accounts(backend: &mut impl AccountOps) -> Vec<String> {
    backend
        .accounts()
        .unwrap_or_default()
        .into_iter()
        .filter(|account| !account.vertical().is_api())
        .filter(|account| {
            backend.offline_status(account).is_ok_and(|status| {
                can_refresh_usage(account, status)
                    && usage_needs_refresh(backend, account, status.credential).unwrap_or(false)
            })
        })
        .map(|account| account.to_string())
        .collect()
}

/// Whether the credential needs a refresh, decided from local state only.
/// The offline posture stays `Ready` for an expired but refreshable access
/// token, so a `Ready` posture also checks the stored expiry. A `Refreshing`
/// credential is renewed by its claim holder.
fn usage_needs_refresh(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,
    credential: CredentialState,
) -> CliResult<bool> {
    match credential {
        CredentialState::RefreshRequired => Ok(true),
        CredentialState::Quarantined => {
            Ok(provider.vertical() == plakous_hosted::account::Vertical::Grok)
        }
        CredentialState::Ready => backend.access_token_expired(provider),
        _ => Ok(false),
    }
}

/// The Usage action runs the same forced, claimed refresh as `plak refresh`
/// when the credential still needs one; the caller then re-reads Usage.
/// Revalidate the offline posture because a button may outlive its snapshot:
/// a credential renewed meanwhile is only re-read.
pub(crate) fn refresh_usage_credential(
    backend: &mut impl AccountOps,
    provider: &plakous_hosted::account::AccountRef,
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
