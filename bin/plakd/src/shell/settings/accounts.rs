//! `ChooseAccount`, the account fill `ApplyProfile` applies, and the session
//! account observations (spec 02 §4.4.1).

use std::collections::BTreeMap;

use plakd_wire_types::console::Vertical;
use plakd_wire_types::{AccountChoice, AccountChoiceRefusal};
use plakous_host_protocol::wire::RequestRefusal;
use plakous_hosted::runtime::{
    AccountChoiceOutcome, Sender, SurfaceAccountChoice, SurfaceAccountEffect, SurfaceEffect,
    SurfaceFacts, SurfacePhase, SurfaceSession,
};

use crate::account_choice::{self, FillMode};
use crate::home::paths::PlakPaths;
use crate::surface_protocol::{ContentEvent, PlakExtension, SurfaceObservation};

/// The provider kind id a wire vertical names.
pub(super) const fn kind_id(kind: Vertical) -> &'static str {
    match kind {
        Vertical::Claude => "claude",
        Vertical::Codex => "codex",
        Vertical::Grok => "grok",
        Vertical::Anthropic => "anthropic",
        Vertical::Openai => "openai",
        Vertical::XaiApi => "xai-api",
        Vertical::Openrouter => "openrouter",
    }
}

fn wire_kind(kind: &str) -> Option<Vertical> {
    [
        Vertical::Claude,
        Vertical::Codex,
        Vertical::Grok,
        Vertical::Anthropic,
        Vertical::Openai,
        Vertical::XaiApi,
        Vertical::Openrouter,
    ]
    .into_iter()
    .find(|vertical| kind_id(*vertical) == kind)
}

/// The kinds a session's root and frozen cards use.
pub(super) fn used_kinds<'a>(
    root: Option<&'a str>,
    cards: impl Iterator<Item = &'a str>,
) -> Vec<String> {
    let mut kinds = std::collections::BTreeSet::new();
    for account in root.into_iter().chain(cards) {
        if let Ok(parsed) = account.parse::<plakous_hosted::account::AccountRef>() {
            kinds.insert(parsed.vertical().as_str().to_owned());
        }
    }
    kinds.into_iter().collect()
}

fn refuse(events: &Sender<PlakExtension>, refusal: AccountChoiceRefusal) {
    let _ = events.send(ContentEvent::Extension(
        SurfaceObservation::AccountChoiceRefused(refusal),
    ));
}

fn from_request(refusal: RequestRefusal) -> AccountChoiceRefusal {
    match refusal {
        RequestRefusal::AccountChoiceRequired { kinds } => {
            AccountChoiceRefusal::AccountChoiceRequired { kinds }
        }
        RequestRefusal::AccountChoiceInvalid {
            kind,
            account,
            reason,
        } => AccountChoiceRefusal::AccountChoiceInvalid {
            kind,
            account,
            reason,
        },
        other => AccountChoiceRefusal::Failed {
            reason: format!("{other:?}"),
        },
    }
}

/// Publishes the session's account for every kind it holds one for.
pub(super) fn send_choices(
    root: Option<&str>,
    choices: &BTreeMap<String, String>,
    events: &Sender<PlakExtension>,
) {
    let choices = account_choice::session_choices(root, choices)
        .into_iter()
        .filter_map(|(kind, account)| {
            Some(AccountChoice {
                kind: wire_kind(&kind)?,
                account,
            })
        })
        .collect();
    let _ = events.send(ContentEvent::Extension(
        SurfaceObservation::AccountChoices { choices },
    ));
}

/// `ApplyProfile`'s fill for a fork, a switch or a bind: the root's account
/// and the stored choices for every card kind, before the host resolves the
/// root's route. `None` after publishing the typed refusal and the profile
/// refusal.
pub(super) fn fill_profile(
    prepared: &crate::profiles::Prepared,
    accounts: &[AccountChoice],
    facts: &SurfaceFacts<'_>,
    events: &Sender<PlakExtension>,
) -> Option<(String, BTreeMap<String, String>)> {
    let config = &prepared.config;
    let explicit = accounts
        .iter()
        .map(|choice| plakous_host_protocol::wire::AccountChoice {
            kind: kind_id(choice.kind).to_owned(),
            account: choice.account.clone(),
        })
        .collect::<Vec<_>>();
    let catalog = match host_catalog(facts) {
        Ok(catalog) => catalog,
        Err(refusal) => {
            refuse(events, refusal);
            super::refuse_profile(
                &prepared.id,
                "account readiness could not be read".to_owned(),
                events,
            );
            return None;
        }
    };
    let mut explicit = explicit;
    keep_current_choices(&mut explicit, config, facts.session.as_ref(), &catalog);
    let filled = account_choice::fill(
        &account_choice::profile_kinds(config),
        account_choice::root_kind(config).as_deref(),
        &explicit,
        &catalog,
        FillMode::Require,
    );
    match filled {
        Ok(filled) => Some((
            filled
                .root_account
                .unwrap_or_else(|| prepared.root.account.clone()),
            filled.choices,
        )),
        Err(refusal) => {
            let reason = account_choice::refusal_text(&refusal);
            refuse(events, from_request(refusal));
            super::refuse_profile(&prepared.id, reason, events);
            None
        }
    }
}

/// Before the fill's default slot, each kind the profile uses keeps the
/// session's current account when it is still Ready: the line head's for the
/// root kind, the stored choice for the others.
fn keep_current_choices(
    explicit: &mut Vec<plakous_host_protocol::wire::AccountChoice>,
    config: &crate::product_config::ProductConfig,
    session: Option<&SurfaceSession>,
    catalog: &[account_choice::ReadyAccount],
) {
    let Some(session) = session else {
        return;
    };
    let current =
        account_choice::session_choices(session.account.as_deref(), &session.account_choices);
    for kind in account_choice::profile_kinds(config) {
        if explicit.iter().any(|choice| choice.kind == kind) {
            continue;
        }
        let Some((_, account)) = current.iter().find(|(current, _)| *current == kind) else {
            continue;
        };
        if catalog
            .iter()
            .any(|row| row.ready && row.account == *account)
        {
            explicit.push(plakous_host_protocol::wire::AccountChoice {
                kind,
                account: account.clone(),
            });
        }
    }
}

/// The account readiness read for `ChooseAccount` and `ApplyProfile`. The
/// host runs it on its own thread before the dispatch
/// ([`SurfaceFacts::accounts`]), so the host loop never waits on a gateway.
pub(super) fn catalog_read(
    paths: PlakPaths,
    custody: crate::gateway::HostCustody,
) -> plakous_hosted::runtime::AccountsRead {
    Box::new(move || account_choice::ready_catalog(&paths, &custody))
}

/// The readiness a surface fill reads: every known account the host read
/// before the dispatch, and the host's own Ready catalog for the default
/// slots it serves. An unread or unreadable catalog is a refusal, never an
/// empty catalog.
fn host_catalog(
    facts: &SurfaceFacts<'_>,
) -> Result<Vec<account_choice::ReadyAccount>, AccountChoiceRefusal> {
    let mut catalog = match facts.accounts {
        Some(Ok(catalog)) => catalog.clone(),
        Some(Err(reason)) => {
            return Err(AccountChoiceRefusal::Failed {
                reason: reason.clone(),
            });
        }
        None => {
            return Err(AccountChoiceRefusal::Failed {
                reason: "the host did not read account readiness".to_owned(),
            });
        }
    };
    for section in facts.catalog.iter().filter(|section| section.ready) {
        match catalog
            .iter_mut()
            .find(|row| row.account == section.account)
        {
            Some(row) => row.ready = true,
            None => catalog.push(account_choice::ReadyAccount {
                account: section.account.clone(),
                ready: true,
                label: Some(section.label.clone()),
            }),
        }
    }
    Ok(catalog)
}

/// Independent of readiness: whether the choice can reach admission and
/// whether it would move the root.
pub(super) fn choice_scope(
    kind: Vertical,
    account: &str,
    facts: &SurfaceFacts<'_>,
) -> Result<bool, AccountChoiceRefusal> {
    if facts.phase == SurfacePhase::Detached {
        return Err(AccountChoiceRefusal::Failed {
            reason: "this session is not attached; attach it before choosing an account".to_owned(),
        });
    }
    let session = facts
        .session
        .as_ref()
        .ok_or_else(|| AccountChoiceRefusal::Failed {
            reason: "choose an account in an open session".to_owned(),
        })?;
    let kind = kind_id(kind);
    let root = session
        .account
        .as_deref()
        .and_then(|value| value.parse::<plakous_hosted::account::AccountRef>().ok())
        .is_some_and(|root| root.vertical().as_str() == kind);
    if root && facts.phase == SurfacePhase::Busy {
        return Err(AccountChoiceRefusal::RootBusy);
    }
    let used = used_kinds(
        session.account.as_deref(),
        session
            .roster
            .iter()
            .flat_map(|roster| roster.roster.values())
            .map(|card| card.account.as_str()),
    );
    if !used.iter().any(|used| used == kind) {
        return Err(AccountChoiceRefusal::AccountChoiceInvalid {
            kind: kind.to_owned(),
            account: account.to_owned(),
            reason: "kind is not used by this session".to_owned(),
        });
    }
    Ok(root)
}

/// `ChooseAccount`: admits the exact account, then stores a non-root
/// choice or continues the idle root on it.
pub(super) fn choose_account(
    kind: Vertical,
    account: String,
    continue_failed: bool,
    facts: &SurfaceFacts<'_>,
    events: &Sender<PlakExtension>,
) -> SurfaceEffect {
    let admitted = choice_scope(kind, &account, facts).and_then(|root| {
        let catalog = host_catalog(facts)?;
        account_choice::admit_choice(kind_id(kind), &account, &catalog)
            .map(|account| (root, account))
            .map_err(from_request)
    });
    let (root, account) = match admitted {
        Ok(admitted) => admitted,
        Err(refusal) => {
            refuse(events, refusal);
            return SurfaceEffect::default();
        }
    };
    let choice = if root {
        SurfaceAccountChoice::Root {
            account,
            continue_failed,
        }
    } else {
        SurfaceAccountChoice::Store {
            kind: kind_id(kind).to_owned(),
            account,
        }
    };
    let events = events.answering();
    SurfaceEffect {
        account: Some(SurfaceAccountEffect {
            choice,
            finish: Box::new(move |outcome| match outcome {
                AccountChoiceOutcome::Chosen { root, choices } => {
                    send_choices(root.as_deref(), &choices, &events);
                }
                AccountChoiceOutcome::Refused(refusal) => refuse(&events, host_refusal(refusal)),
            }),
        }),
        ..SurfaceEffect::default()
    }
}

fn host_refusal(refusal: plakous_hosted::runtime::AccountChoiceRefusal) -> AccountChoiceRefusal {
    use plakous_hosted::runtime::AccountChoiceRefusal as Host;
    match refusal {
        Host::RootBusy => AccountChoiceRefusal::RootBusy,
        Host::NoFailedTurn => AccountChoiceRefusal::NoFailedTurn,
        Host::AlreadyContinued => AccountChoiceRefusal::AlreadyContinued,
        Host::Failed(failure) => AccountChoiceRefusal::Failed {
            reason: failure.diagnostic.as_str().to_owned(),
        },
    }
}

/// The read of the other Ready accounts of `account`'s kind, which the host
/// runs off its loop for a failed turn's usage-limit notice.
pub(super) fn alternatives_read(
    paths: PlakPaths,
    custody: crate::gateway::HostCustody,
    account: String,
) -> plakous_hosted::runtime::AlternativesRead {
    Box::new(move || ready_alternatives(&paths, &custody, &account))
}

/// The other Ready accounts of `account`'s kind.
fn ready_alternatives(
    paths: &PlakPaths,
    custody: &crate::gateway::HostCustody,
    account: &str,
) -> Vec<String> {
    let Ok(current) = account.parse::<plakous_hosted::account::AccountRef>() else {
        return Vec::new();
    };
    account_choice::ready_catalog(paths, custody)
        .unwrap_or_default()
        .into_iter()
        .filter(|row| {
            row.ready
                && row.account != current.as_str()
                && row
                    .account
                    .parse::<plakous_hosted::account::AccountRef>()
                    .is_ok_and(|other| other.vertical() == current.vertical())
        })
        .map(|row| row.account)
        .collect()
}

/// Every session the host serves publishes its account choices, so an
/// attach carries them as state.
pub(super) fn serving(session: &SurfaceSession, events: &Sender<PlakExtension>) {
    send_choices(session.account.as_deref(), &session.account_choices, events);
}
