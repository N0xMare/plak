//! Credential roster and readiness shared by CLI admission and the host console.

use std::collections::{BTreeMap, BTreeSet};

use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
use plakous::gateway::protocol::facts::CredentialStatus;
use plakous_hosted::account::{AccountRef, Vertical};

use super::ReadyAccount;
use crate::gateway::{AdminLink, HostCustody};

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

pub(super) fn ready_accounts(custody: &HostCustody) -> Result<Vec<ReadyAccount>, String> {
    gateway_ready_accounts(
        custody
            .admin()
            .ok_or_else(|| crate::gateway::GatewayUnavailable::MissingAddress.to_string())?,
    )
}

fn gateway_ready_accounts(link: &AdminLink) -> Result<Vec<ReadyAccount>, String> {
    let names = link.accounts()?;
    let listed_accounts = || names.iter().filter_map(|name| parse_exact(name));
    let expected = listed_accounts().map(|account| account.to_string());
    let roster = with_default_accounts(listed_accounts());
    let accounts = roster
        .iter()
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
