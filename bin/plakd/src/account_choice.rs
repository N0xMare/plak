//! Session account choice (spec 02 §4.4.1): the fill rule, its refusals and
//! the CLI's `--account <kind>=<ref>`.
//!
//! Templates name a provider kind. The root kind's account is the root line
//! head's and is never a map entry; every other kind the profile uses is.
//! Creation, `ApplyProfile` and the CLI fill with [`FillMode::Require`];
//! import and carry fill [`FillMode::BestEffort`] and never refuse.

use std::collections::{BTreeMap, BTreeSet};

use plakous_host_protocol::wire::{AccountChoice, AccountOption, KindAccounts, RequestRefusal};
use plakous_hosted::account::{AccountRef, Vertical};

use crate::home::paths::PlakPaths;
use crate::product_config::{AgentTemplate, ProductConfig};
use crate::store::sessions::SessionRecord;

/// One account the fill rule may select.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadyAccount {
    /// Full account reference.
    pub account: String,
    /// The credential can serve a new dispatch now.
    pub ready: bool,
    /// Operator-facing label, when the catalog has one.
    pub label: Option<String>,
}

/// Whether an unfilled kind refuses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FillMode {
    /// Creation, `ApplyProfile` and the CLI run and agent paths.
    Require,
    /// Import and carry. Spawn still fails closed for a kind left unset.
    BestEffort,
}

/// What a fill chose.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FilledChoices {
    /// The root kind's account: the line head binds it, it is not stored.
    pub root_account: Option<String>,
    /// Every other used kind.
    pub choices: BTreeMap<String, String>,
}

/// Every known account with its readiness, as the gateway or this home's
/// stores report it. Runs off the caller's thread, so it is safe inside an
/// async runtime.
pub fn ready_catalog(paths: &PlakPaths) -> Vec<ReadyAccount> {
    if let Some(accounts) = crate::test_hooks::ready_accounts() {
        return accounts
            .into_iter()
            .map(|account| ReadyAccount {
                account,
                ready: true,
                label: None,
            })
            .collect();
    }
    let Ok(state) = crate::home::paths::application_state_home() else {
        return Vec::new();
    };
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return Vec::new();
                };
                runtime.block_on(crate::host::ready_accounts(&state, paths))
            })
            .join()
            .unwrap_or_default()
    })
}

/// The kinds a profile's root and working-set cards name.
pub fn profile_kinds(config: &ProductConfig) -> BTreeSet<String> {
    let mut kinds = BTreeSet::new();
    let root = config
        .root_agent
        .as_ref()
        .and_then(|name| config.templates.get(name));
    let cards = config
        .working_set
        .names()
        .iter()
        .filter_map(|name| config.templates.get(name.as_str()));
    for template in root.into_iter().chain(cards) {
        if let Some(kind) = template_kind(template) {
            kinds.insert(kind);
        }
    }
    kinds
}

fn template_kind(template: &AgentTemplate) -> Option<String> {
    template
        .account
        .parse::<AccountRef>()
        .ok()
        .map(|account| account.vertical().as_str().to_owned())
}

/// The root template's kind, when the profile has a root template.
pub fn root_kind(config: &ProductConfig) -> Option<String> {
    template_kind(config.templates.get(config.root_agent.as_ref()?)?)
}

/// Group accounts into picker rows, one per kind.
pub fn kind_accounts(catalog: &[ReadyAccount]) -> Vec<KindAccounts> {
    let mut grouped: BTreeMap<String, Vec<AccountOption>> = BTreeMap::new();
    for row in catalog {
        let Ok(account) = row.account.parse::<AccountRef>() else {
            continue;
        };
        grouped
            .entry(account.vertical().as_str().to_owned())
            .or_default()
            .push(AccountOption {
                account: account.to_string(),
                ready: row.ready,
                label: row.label.clone(),
            });
    }
    grouped
        .into_iter()
        .map(|(kind, rows)| KindAccounts { kind, rows })
        .collect()
}

/// Fill `kinds`: an explicit choice wins (its kind must be its account's),
/// else the kind's bare default slot if Ready, else its single Ready account.
/// `Require` refuses the kinds left unfilled, listing their accounts. The root
/// kind's account is returned apart from the stored choices.
///
/// # Errors
/// `AccountChoiceInvalid` for a malformed, repeated, unused or unready
/// explicit choice; `AccountChoiceRequired` for unfilled kinds under `Require`.
pub fn fill(
    kinds: &BTreeSet<String>,
    root_kind: Option<&str>,
    explicit: &[AccountChoice],
    catalog: &[ReadyAccount],
    mode: FillMode,
) -> Result<FilledChoices, RequestRefusal> {
    let mut chosen: BTreeMap<String, String> = BTreeMap::new();
    for choice in explicit {
        let account = admit_choice(&choice.kind, &choice.account, catalog)?;
        if chosen.insert(choice.kind.clone(), account).is_some() {
            return Err(invalid(&choice.kind, &choice.account, "repeated choice"));
        }
    }
    let mut missing = Vec::new();
    let mut filled = FilledChoices::default();
    for kind in kinds {
        let account = chosen.remove(kind).or_else(|| automatic(kind, catalog));
        match account {
            Some(account) if Some(kind.as_str()) == root_kind => {
                filled.root_account = Some(account);
            }
            Some(account) => {
                filled.choices.insert(kind.clone(), account);
            }
            None if mode == FillMode::Require => missing.push(kind.clone()),
            None => {}
        }
    }
    if let Some((kind, account)) = chosen.into_iter().next() {
        return Err(invalid(&kind, &account, "kind is not used by this profile"));
    }
    if missing.is_empty() {
        return Ok(filled);
    }
    let rows = kind_accounts(catalog);
    let kinds = missing
        .into_iter()
        .map(|kind| {
            rows.iter()
                .find(|row| row.kind == kind)
                .cloned()
                .unwrap_or(KindAccounts {
                    kind,
                    rows: Vec::new(),
                })
        })
        .collect();
    Err(RequestRefusal::AccountChoiceRequired { kinds })
}

fn invalid(kind: &str, account: &str, reason: &str) -> RequestRefusal {
    RequestRefusal::AccountChoiceInvalid {
        kind: kind.to_owned(),
        account: account.to_owned(),
        reason: reason.to_owned(),
    }
}

/// An explicit choice: a full reference of a known kind, of that kind, and
/// Ready now.
///
/// # Errors
/// `AccountChoiceInvalid` naming why.
pub fn admit_choice(
    kind: &str,
    account: &str,
    catalog: &[ReadyAccount],
) -> Result<String, RequestRefusal> {
    if !is_kind(kind) {
        return Err(invalid(kind, account, "not a provider kind"));
    }
    let parsed = account
        .parse::<AccountRef>()
        .map_err(|_| invalid(kind, account, "not an account reference"))?;
    if parsed.vertical().as_str() != kind {
        return Err(invalid(kind, account, "not an account of this kind"));
    }
    if !catalog
        .iter()
        .any(|row| row.ready && row.account == parsed.as_str())
    {
        return Err(invalid(kind, parsed.as_str(), "not ready"));
    }
    Ok(parsed.to_string())
}

/// The fill rule without an explicit choice: the kind's bare default slot if
/// Ready, else its single Ready account.
pub fn automatic(kind: &str, catalog: &[ReadyAccount]) -> Option<String> {
    let ready: Vec<&ReadyAccount> = catalog
        .iter()
        .filter(|row| {
            row.ready
                && row
                    .account
                    .parse::<AccountRef>()
                    .is_ok_and(|account| account.vertical().as_str() == kind)
        })
        .collect();
    if ready.iter().any(|row| row.account == kind) {
        return Some(kind.to_owned());
    }
    match ready.as_slice() {
        [only] => Some(only.account.clone()),
        _ => None,
    }
}

fn is_kind(kind: &str) -> bool {
    Vertical::ALL
        .iter()
        .any(|vertical| vertical.as_str() == kind)
}

/// Import and carry: the root keeps the imported binding's account; every
/// other kind the profile uses is filled without refusing.
pub fn fill_imported(
    record: &mut SessionRecord,
    kinds: &BTreeSet<String>,
    catalog: &[ReadyAccount],
) {
    let root = record
        .account
        .as_deref()
        .and_then(|account| account.parse::<AccountRef>().ok())
        .map(|account| account.vertical().as_str().to_owned());
    if let Ok(filled) = fill(kinds, root.as_deref(), &[], catalog, FillMode::BestEffort) {
        record.account_choices = filled.choices;
    }
}

/// The session's account for every kind it holds one for, the root kind
/// first: what `AccountChoices` reports.
pub fn session_choices(
    root: Option<&str>,
    choices: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let root = root
        .and_then(|account| account.parse::<AccountRef>().ok())
        .map(|account| (account.vertical().as_str().to_owned(), account.to_string()));
    root.clone()
        .into_iter()
        .chain(
            choices
                .iter()
                .filter(|(kind, _)| root.as_ref().is_none_or(|(root, _)| root != *kind))
                .map(|(kind, account)| (kind.clone(), account.clone())),
        )
        .collect()
}

/// Parse one `--account <kind>=<ref>` flag.
///
/// # Errors
/// An operator-facing usage message.
pub fn parse_kind_choice(value: &str) -> Result<AccountChoice, String> {
    let Some((kind, account)) = value.split_once('=') else {
        return Err(format!(
            "--account {value} must be <kind>=<ref>, for example codex=codex/work"
        ));
    };
    if kind.is_empty() || account.is_empty() || !is_kind(kind) {
        return Err(format!(
            "--account {value} must name a provider kind and an account of that kind"
        ));
    }
    let parsed = account
        .parse::<AccountRef>()
        .map_err(|error: plakous_hosted::capabilities::AccountAdmissionError| error.to_string())?;
    if parsed.vertical().as_str() != kind {
        return Err(format!(
            "--account {value} names an account of a different kind"
        ));
    }
    Ok(AccountChoice {
        kind: kind.to_owned(),
        account: parsed.to_string(),
    })
}

/// Split repeatable `--account` values into one bare root account and kind
/// choices.
///
/// # Errors
/// A second bare account, or a malformed kind choice.
pub fn split_account_flags(
    flags: Vec<String>,
) -> Result<(Option<String>, Vec<AccountChoice>), String> {
    let mut bare = None;
    let mut choices = Vec::new();
    for flag in flags {
        if flag.contains('=') {
            choices.push(parse_kind_choice(&flag)?);
        } else if bare.is_some() {
            return Err(
                "repeat --account as <kind>=<ref>; only one bare account selects the root"
                    .to_owned(),
            );
        } else {
            bare = Some(flag);
        }
    }
    Ok((bare, choices))
}

/// The CLI fill for a session whose root account is already chosen: every
/// other kind the profile uses, from `--account <kind>=<ref>` and the fill
/// rule.
///
/// # Errors
/// The typed refusal, as operator text.
pub fn fill_cli(
    config: &ProductConfig,
    paths: &PlakPaths,
    root_account: Option<&str>,
    explicit: &[AccountChoice],
) -> Result<BTreeMap<String, String>, crate::CliFailure> {
    let root = root_account
        .and_then(|account| account.parse::<AccountRef>().ok())
        .map(|account| account.vertical().as_str().to_owned());
    let mut kinds = profile_kinds(config);
    if let Some(root) = &root {
        kinds.remove(root);
    }
    if explicit.is_empty() && kinds.is_empty() {
        return Ok(BTreeMap::new());
    }
    fill(
        &kinds,
        None,
        explicit,
        &ready_catalog(paths),
        FillMode::Require,
    )
    .map(|filled| filled.choices)
    .map_err(|refusal| crate::CliFailure::new(refusal_text(&refusal)))
}

/// Operator text for a fill refusal.
pub fn refusal_text(refusal: &RequestRefusal) -> String {
    match refusal {
        RequestRefusal::AccountChoiceRequired { kinds } => format!(
            "account_choice_required: choose an account for {}; pass --account <kind>=<ref>",
            kinds
                .iter()
                .map(|row| row.kind.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        RequestRefusal::AccountChoiceInvalid {
            kind,
            account,
            reason,
        } => format!("account_choice_invalid: {kind}={account}: {reason}"),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests;
