//! Credential roster and readiness shared by CLI admission and the host console.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
use plakous::gateway::protocol::facts::CredentialStatus;
use plakous::subscriptions::{
    NativeProvider, NativeSubscriptionError, NativeSubscriptions, SubscriptionPaths,
};
use plakous_hosted::account::{AccountRef, Vertical};

use super::ReadyAccount;
use crate::gateway::{AccountRoute, AdminLink, HostCustody};
use crate::home::{paths::PlakPaths, secrets};

const SCAN_ENTRIES: usize = 512;

pub(crate) fn subscriptions_at(
    state: &Path,
    custody: &HostCustody,
) -> Result<NativeSubscriptions, NativeSubscriptionError> {
    let paths = SubscriptionPaths::from_xdg_state_home(state)?;
    Ok(custody.subscriptions(NativeSubscriptions::new(paths)))
}

pub(crate) fn subscriptions_for(
    subscriptions: &NativeSubscriptions,
    provider: NativeProvider,
    account: &AccountRef,
) -> Result<NativeSubscriptions, NativeSubscriptionError> {
    match account.name() {
        Some(name) => subscriptions.for_named_account(provider, name.as_str()),
        None => Ok(subscriptions.clone()),
    }
}

pub(crate) fn parse_exact(value: &str) -> Option<AccountRef> {
    value
        .parse::<AccountRef>()
        .ok()
        .filter(|account| value.len() <= 64 && account.as_str() == value)
}

pub(crate) fn with_default_accounts(
    accounts: impl IntoIterator<Item = AccountRef>,
) -> Vec<AccountRef> {
    Vertical::ALL
        .into_iter()
        .map(AccountRef::default_for)
        .chain(accounts)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Returns the accounts, verticals whose enumeration failed, and truncation.
pub(crate) async fn local_roster(
    subscriptions: &NativeSubscriptions,
    paths: &PlakPaths,
) -> (Vec<AccountRef>, Vec<Vertical>, bool) {
    let mut accounts = Vec::new();
    let mut unavailable = Vec::new();
    let mut remaining = SCAN_ENTRIES;
    let mut truncated = false;
    for vertical in Vertical::ALL {
        let names = if let Some(provider) = vertical.capability().native_provider() {
            if !vertical.capability().compiled {
                continue;
            }
            subscriptions
                .named_accounts_bounded(provider, remaining)
                .await
                .ok()
        } else {
            secrets::list_shelf_secrets_bounded(
                paths,
                crate::home::paths::SecretShelf::Api(vertical),
                remaining,
            )
            .ok()
        };
        match names {
            Some((names, scanned, cut)) => {
                remaining = remaining.saturating_sub(scanned);
                truncated |= cut;
                accounts.extend(
                    names
                        .iter()
                        .filter_map(|name| parse_exact(&format!("{}/{name}", vertical.as_str()))),
                );
            }
            None => unavailable.push(vertical),
        }
    }
    (with_default_accounts(accounts), unavailable, truncated)
}

/// The actual custodian answers roster and readiness, without a local read
/// when a configured or local service gateway holds the credentials.
pub(super) async fn ready_accounts(
    state: &Path,
    paths: &PlakPaths,
    custody: &HostCustody,
) -> Result<Vec<ReadyAccount>, String> {
    if let Some(link) = custody.admin() {
        return gateway_ready_accounts(link);
    }
    if let AccountRoute::Service(address) = crate::gateway::route_for(paths, custody)? {
        return gateway_ready_accounts(&AdminLink::local_service(address));
    }
    let _holder = crate::gateway::acquire_shared(state)?;
    let subscriptions = subscriptions_at(state, custody)
        .map_err(|_| "the account roster could not be read".to_owned())?;
    let (accounts, _, _) = local_roster(&subscriptions, paths).await;
    let mut rows = Vec::with_capacity(accounts.len());
    for account in accounts {
        let vertical = account.vertical();
        let ready = if !vertical.capability().compiled {
            false
        } else if vertical.is_api() {
            secrets::key_status(paths, account.as_str())
                .is_ok_and(|key| key.source != secrets::KeySource::Missing)
        } else if let Some(provider) = vertical.capability().native_provider()
            && let Ok(selected) = subscriptions_for(&subscriptions, provider, &account)
        {
            crate::commands::support::offline_subscription_status(&selected, provider)
                .await
                .is_ok_and(|status| {
                    status.compatibility == crate::presentation::CompatibilityState::Current
                        && status.credential == crate::presentation::CredentialState::Ready
                })
        } else {
            false
        };
        rows.push(ReadyAccount {
            account: account.to_string(),
            ready,
            label: None,
        });
    }
    Ok(rows)
}

fn gateway_ready_accounts(link: &AdminLink) -> Result<Vec<ReadyAccount>, String> {
    let names = link.accounts()?;
    let listed_accounts = || names.iter().filter_map(|name| parse_exact(name));
    let expected = listed_accounts()
        .filter(|account| account.vertical().capability().compiled)
        .map(|account| account.to_string());
    let roster = with_default_accounts(listed_accounts());
    let accounts = roster
        .iter()
        .filter(|account| account.vertical().capability().compiled)
        .map(|account| GatewayAccount::new(account.as_str()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut readiness = BTreeMap::new();
    if !accounts.is_empty() {
        match link.call(&AdminRequest::Status { accounts })? {
            AdminResponse::Status { rows } => {
                for row in rows {
                    readiness.insert(
                        row.facts.account.as_str().to_owned(),
                        row.facts.credential.status == CredentialStatus::Ready,
                    );
                }
            }
            AdminResponse::Refused { refusal } => {
                return Err(format!(
                    "the gateway refused the account status read: {refusal:?}"
                ));
            }
            _ => return Err("the gateway answered the account status read unexpectedly".to_owned()),
        }
    }
    if let Some(missing) = expected
        .filter(|account| !readiness.contains_key(account))
        .min()
    {
        return Err(format!(
            "the gateway returned partial account status: missing {missing}"
        ));
    }
    Ok(roster
        .into_iter()
        .map(|account| ReadyAccount {
            account: account.to_string(),
            ready: readiness.get(account.as_str()).copied().unwrap_or(false),
            label: None,
        })
        .collect())
}
