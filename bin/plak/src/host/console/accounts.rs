//! Local account observations, without product roots or session admission.

use super::{diagnostic, unavailable, validation};
use crate::home::{paths::PlakPaths, secrets};
use plakd_wire_types::console::{
    AccountStatus, Compatibility, Credential, Diagnostic, KeySource, KeyStatus, NativeStatus,
    ProfileNotice, ReadAction, ReadRefusal, ReadRemedy, ReadScope, UnavailableCode, ValidationCode,
    Vertical,
};
use plakous_hosted::account::{AccountRef, Vertical as AccountVertical};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub(super) struct Roster {
    pub accounts: Vec<AccountRef>,
    pub diagnostics: Vec<Diagnostic>,
    pub truncated: bool,
}

pub(super) struct Reader {
    pub subscriptions: plakous::subscriptions::NativeSubscriptions,
}

pub(super) fn reader(state: &std::path::Path) -> Result<Reader, ReadRefusal> {
    let paths = plakous::subscriptions::SubscriptionPaths::from_xdg_state_home(state.to_path_buf())
        .map_err(|_| unavailable(ReadScope::Accounts, UnavailableCode::Storage))?;
    Ok(Reader {
        subscriptions: plakous::subscriptions::NativeSubscriptions::new(paths),
    })
}
impl Reader {
    pub fn subscriptions_for(
        &self,
        account: &AccountRef,
    ) -> Result<plakous::subscriptions::NativeSubscriptions, ReadRefusal> {
        if let Some(name) = account.name() {
            let provider = account
                .vertical()
                .capability()
                .native_provider()
                .ok_or_else(|| unavailable(ReadScope::Accounts, UnavailableCode::NotCompiled))?;
            self.subscriptions
                .for_named_account(provider, name.as_str())
                .map_err(|_| unavailable(ReadScope::Accounts, UnavailableCode::Storage))
        } else {
            Ok(self.subscriptions.clone())
        }
    }
}

pub(super) fn exact(value: &str) -> Result<AccountRef, ReadRefusal> {
    value
        .parse::<AccountRef>()
        .ok()
        .filter(|account| value.len() <= 64 && account.as_str() == value)
        .ok_or_else(|| validation("account_id", ValidationCode::InvalidAccount))
}

pub(super) async fn roster(
    application: &Reader,
    paths: &PlakPaths,
    selected: Option<&str>,
) -> Result<Roster, ReadRefusal> {
    if let Some(selected) = selected {
        // Exact selection deliberately bypasses enumeration. A missing named
        // selection still has truthful missing-credential/historical facts.
        return Ok(Roster {
            accounts: vec![exact(selected)?],
            diagnostics: Vec::new(),
            truncated: false,
        });
    }
    let mut accounts = BTreeSet::new();
    let mut diagnostics = Vec::new();
    let mut remaining = super::limits::SCAN_ENTRIES;
    let mut truncated = false;
    for vertical in AccountVertical::ALL {
        accounts.insert(AccountRef::default_for(vertical));
        let names = if let Some(provider) = vertical.capability().native_provider() {
            if !vertical.capability().compiled {
                continue;
            }
            application
                .subscriptions
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
                for name in names {
                    if let Ok(account) = exact(&format!("{}/{name}", vertical.as_str())) {
                        accounts.insert(account);
                    }
                }
            }
            None => diagnostics.push(diagnostic(
                "enumeration_unavailable",
                Some(vertical.as_str()),
                "account names could not be read",
            )),
        }
    }
    Ok(Roster {
        accounts: accounts.into_iter().collect(),
        diagnostics,
        truncated,
    })
}

pub(super) async fn status(
    application: &Reader,
    paths: &PlakPaths,
    account: &AccountRef,
    as_of: &str,
) -> AccountStatus {
    let vertical = account.vertical();
    let mut row = AccountStatus {
        account_id: account.to_string(),
        vertical: wire_vertical(vertical),
        name: account.name().map(|name| name.as_str().to_owned()),
        label: account.to_string(),
        compiled: vertical.capability().compiled,
        observed_at: as_of.to_owned(),
        native: None,
        key: None,
        same_upstream_as: None,
        last_refresh: None,
        remedies: Vec::new(),
        diagnostics: Vec::new(),
    };
    if vertical.is_api() {
        match secrets::key_status(paths, account.as_str()) {
            Ok(key) => {
                if key.source == secrets::KeySource::Missing {
                    row.remedies.push(remedy(ReadAction::SetKey, account));
                }
                row.key = Some(KeyStatus {
                    source: match key.source {
                        secrets::KeySource::Env => KeySource::Environment,
                        secrets::KeySource::File => KeySource::File,
                        secrets::KeySource::Missing => KeySource::Missing,
                    },
                    fingerprint: key.fingerprint,
                    environment_override: key.env_var.is_some(),
                    shadowed_file_fingerprint: key.shadowed_file_fingerprint,
                });
            }
            Err(_) => row.diagnostics.push(diagnostic(
                "key_status_unavailable",
                Some(account.as_str()),
                "key status could not be read",
            )),
        }
    } else if let Some(provider) = vertical.capability().native_provider()
        && let Ok(subscriptions) = application.subscriptions_for(account)
        && let Ok(status) =
            crate::commands::support::offline_subscription_status(&subscriptions, provider).await
    {
        let mut native = native_status(status);
        native.profile = plakous::subscriptions::NativeSubscriptions::notice(provider)
            .ok()
            .map(|notice| ProfileNotice {
                id: notice.profile_id().to_owned(),
                notice_revision: notice.notice_revision().to_owned(),
                reviewed_at: date(notice.reviewed_at()),
                expires_at: date(notice.expires_at()),
            });
        row.native = Some(native);
        if status.credential != crate::presentation::CredentialState::Ready {
            row.remedies.push(remedy(ReadAction::Login, account));
        }
    } else {
        row.diagnostics.push(diagnostic(
            "native_status_unavailable",
            Some(account.as_str()),
            "native status could not be read",
        ));
    }
    if !vertical.is_api() {
        match crate::telemetry_store::TelemetryStore::latest_account_refresh_outcome(paths, account)
        {
            Ok(Some(refresh)) => row.last_refresh = refresh_observation(refresh),
            Ok(None) => {}
            Err(_) => row.diagnostics.push(diagnostic(
                "refresh_history_unavailable",
                Some(account.as_str()),
                "retained refresh history could not be read",
            )),
        }
    }
    if !row.compiled {
        row.remedies
            .push(remedy(ReadAction::UseMatchingBuild, account));
    }
    row
}

fn refresh_observation(
    row: crate::telemetry_store::CredentialRefreshRow,
) -> Option<plakd_wire_types::console::CredentialRefreshObservation> {
    use plakd_wire_types::console::{CredentialRefreshObservation, CredentialRefreshOutcome};
    let outcome = match row.phase.as_str() {
        "completed" => CredentialRefreshOutcome::Completed,
        "restored" => CredentialRefreshOutcome::Restored,
        "outcome_unknown" => CredentialRefreshOutcome::OutcomeUnknown,
        "quarantined" => CredentialRefreshOutcome::Quarantined,
        _ => return None,
    };
    let failure_class = row.failure_class.filter(|class| {
        matches!(
            class.as_str(),
            "transport"
                | "timeout"
                | "http_4xx"
                | "http_5xx"
                | "http_other"
                | "validation"
                | "store"
                | "cancelled"
                | "not_ready"
        )
    });
    Some(CredentialRefreshObservation {
        outcome,
        observed_at: jiff::Timestamp::from_nanosecond(i128::from(row.at_ns))
            .ok()?
            .to_string(),
        failure_class,
    })
}

pub(super) async fn status_rows(
    application: &Reader,
    paths: &PlakPaths,
    accounts: Vec<AccountRef>,
    as_of: &str,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<AccountStatus>, ReadRefusal> {
    let mut rows = Vec::new();
    let mut fingerprints = Vec::new();
    for account in accounts {
        check(deadline, cancelled)?;
        rows.push(status(application, paths, &account, as_of).await);
        if let Some(provider) = account.vertical().capability().native_provider()
            && let Ok(subscriptions) = application.subscriptions_for(&account)
            && let Ok(Some(fingerprint)) =
                subscriptions.upstream_account_fingerprint(provider).await
        {
            fingerprints.push((account, fingerprint));
        }
    }
    for row in &mut rows {
        if let Some((account, fingerprint)) = fingerprints
            .iter()
            .find(|(account, _)| account.as_str() == row.account_id)
        {
            let matches: Vec<_> = fingerprints
                .iter()
                .filter(|(other, candidate)| {
                    other != account
                        && other.vertical() == account.vertical()
                        && candidate == fingerprint
                })
                .map(|(other, _)| other.to_string())
                .collect();
            if !matches.is_empty() {
                row.same_upstream_as = Some(matches);
            }
        }
    }
    Ok(rows)
}

fn native_status(status: crate::presentation::CliStatus) -> NativeStatus {
    use crate::presentation::{CompatibilityState as C, CredentialState as S};
    NativeStatus {
        compatibility: match status.compatibility {
            C::Current => Compatibility::Current,
            C::Required => Compatibility::Required,
            C::Inactive => Compatibility::Inactive,
            C::Unsupported => Compatibility::Unsupported,
        },
        credential: match status.credential {
            S::SignedOut => Credential::SignedOut,
            S::Ready => Credential::Ready,
            S::RefreshRequired => Credential::RefreshRequired,
            S::Refreshing => Credential::Refreshing,
            S::Quarantined => Credential::Quarantined,
            S::Revoked => Credential::Revoked,
            S::LoginRequired => Credential::LoginRequired,
            S::ProfileChanged => Credential::ProfileChanged,
            S::WrittenByNewerBuild => Credential::WrittenByNewerBuild,
            S::Unsupported => Credential::Unsupported,
        },
        profile: None,
        identity: None,
    }
}

pub(super) fn remedy(action: ReadAction, account: &AccountRef) -> ReadRemedy {
    ReadRemedy {
        action,
        account_id: Some(account.to_string()),
    }
}

fn date(seconds: u64) -> Option<String> {
    super::clock::date(seconds)
}

pub(super) async fn exists(
    application: &Reader,
    paths: &PlakPaths,
    account: &AccountRef,
) -> Result<bool, ReadRefusal> {
    if !account.is_named() || !account.vertical().capability().compiled {
        return Ok(true);
    }
    if let Some(provider) = account.vertical().capability().native_provider() {
        let subscriptions = application
            .subscriptions_for(account)
            .map_err(|_| unavailable(ReadScope::Accounts, UnavailableCode::Storage))?;
        subscriptions
            .account_exists(provider)
            .await
            .map_err(|_| unavailable(ReadScope::Accounts, UnavailableCode::Storage))
    } else {
        secrets::key_status(paths, account.as_str())
            .map(|key| key.source != secrets::KeySource::Missing)
            .map_err(|_| unavailable(ReadScope::Accounts, UnavailableCode::Storage))
    }
}

fn wire_vertical(vertical: AccountVertical) -> Vertical {
    match vertical {
        AccountVertical::Claude => Vertical::Claude,
        AccountVertical::Codex => Vertical::Codex,
        AccountVertical::Grok => Vertical::Grok,
        AccountVertical::Anthropic => Vertical::Anthropic,
        AccountVertical::OpenAi => Vertical::Openai,
        AccountVertical::XaiApi => Vertical::XaiApi,
        AccountVertical::OpenRouter => Vertical::Openrouter,
    }
}

pub(super) fn check(deadline: Instant, cancelled: &AtomicBool) -> Result<(), ReadRefusal> {
    if Instant::now() >= deadline || cancelled.load(Ordering::Acquire) {
        Err(unavailable(ReadScope::Accounts, UnavailableCode::Deadline))
    } else {
        Ok(())
    }
}

impl super::Console {
    pub(super) async fn status(
        &self,
        selected: Option<String>,
        at: String,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<plakd_wire_types::console::HostResponse, ReadRefusal> {
        let reader = reader(&self.state)?;
        if let Some(selected) = selected.as_deref()
            && !exists(&reader, &self.paths, &exact(selected)?).await?
        {
            return Err(validation("account_id", ValidationCode::UnknownAccount));
        }
        let roster = roster(&reader, &self.paths, selected.as_deref()).await?;
        let rows = status_rows(
            &reader,
            &self.paths,
            roster.accounts,
            &at,
            deadline,
            cancelled,
        )
        .await?;
        Ok(plakd_wire_types::console::HostResponse::AccountsStatus {
            as_of: at,
            rows,
            diagnostics: roster.diagnostics,
            page: plakd_wire_types::console::Page {
                next: None,
                truncated: roster.truncated,
            },
        })
    }
}
