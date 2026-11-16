use super::{Console, accounts, diagnostic, recorded, unavailable};
use plakd_wire_types::console::{
    AccountUsage, Allowance, CoverageReason, HostResponse, Page, ReadAction, ReadRefusal,
    ReadRemedy, ReadScope, UnavailableCode, UsageWindow,
};
use plakous_host_protocol::views::presentation::SessionLedgerView;
use plakous_hosted::account::AccountRef;
use plakous_hosted::subscription_usage::{self, VerticalObservation};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

impl Console {
    pub(super) async fn usage(
        &self,
        selected: Option<String>,
        window: UsageWindow,
        as_of: String,
        deadline: Instant,
        cancelled: Arc<AtomicBool>,
    ) -> Result<HostResponse, ReadRefusal> {
        let to = super::clock::milliseconds(&as_of)
            .ok_or_else(|| unavailable(ReadScope::Usage, UnavailableCode::Storage))?;
        let from = to - i64::try_from(window.seconds()).unwrap_or(0) * 1_000;
        let paths = self.paths.clone();
        let reader = accounts::reader(&self.state)?;
        let roster = accounts::roster(&reader, &paths, selected.as_deref()).await?;
        let selection = selected.clone();
        let cancel = Arc::clone(&cancelled);
        let recorded = tokio::task::spawn_blocking(move || {
            recorded::read(
                &paths,
                from,
                to,
                selection.as_deref(),
                super::limits::SCAN_ENTRIES,
                deadline,
                Arc::clone(&cancel),
            )
        })
        .await
        .map_err(|_| unavailable(ReadScope::Usage, UnavailableCode::Storage))?;
        let mut accounts = roster
            .accounts
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        let (mut totals, unattributed, cut, empty) = match recorded {
            Ok(recorded) => {
                for account in recorded.accounts.keys() {
                    if let Ok(account) = accounts::exact(account) {
                        accounts.insert(account);
                    }
                }
                (
                    recorded.accounts,
                    recorded.unattributed,
                    recorded.truncated,
                    recorded.empty,
                )
            }
            Err(reason) => {
                let reason = match reason {
                    recorded::Unavailable::LegacySchema => CoverageReason::LegacySchema,
                    recorded::Unavailable::Storage | recorded::Unavailable::Deadline => {
                        CoverageReason::StoreUnavailable
                    }
                };
                let unknown = recorded::unknown(reason);
                (
                    std::collections::BTreeMap::new(),
                    unknown.clone(),
                    false,
                    unknown,
                )
            }
        };
        let mut rows = Vec::new();
        for account in accounts {
            let recorded = totals
                .remove(account.as_str())
                .unwrap_or_else(|| empty.clone());
            rows.push(AccountUsage {
                allowance: self.allowance(&account, deadline).await,
                account_id: account.to_string(),
                recorded,
            });
        }
        Ok(HostResponse::AccountsUsage {
            as_of: as_of.clone(),
            window,
            from: utc(from),
            to: as_of,
            rows,
            unattributed: Box::new(unattributed),
            diagnostics: roster.diagnostics,
            page: Page {
                next: None,
                truncated: roster.truncated || cut,
            },
        })
    }

    async fn allowance(&self, account: &AccountRef, deadline: Instant) -> Allowance {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Some(budget) = remaining
            .checked_sub(super::limits::PROVIDER_RESERVE)
            .filter(|budget| !budget.is_zero())
        else {
            return missing(
                account,
                "deadline",
                "provider allowance budget exhausted; retry",
            );
        };
        let timeout = budget.min(super::limits::PROVIDER_DEADLINE);
        if !account.vertical().capability().compiled {
            return missing(
                account,
                "not_compiled",
                "this account route was not compiled",
            );
        }
        if account.vertical() == plakous_hosted::account::Vertical::OpenRouter {
            return match tokio::time::timeout(
                timeout,
                crate::openrouter_product::usage::console::read(
                    &self.paths,
                    account,
                    self.provider.clone().acquire_owned(),
                ),
            )
            .await
            {
                Ok(observation) => project(account, observation),
                Err(_) => missing(
                    account,
                    "deadline",
                    "provider allowance read took too long; retry",
                ),
            };
        }
        let Some(provider) = account.vertical().capability().native_provider() else {
            return missing(
                account,
                "not_supported",
                "this API account has no provider allowance endpoint",
            );
        };
        let subscriptions =
            accounts::reader(&self.state).and_then(|reader| reader.subscriptions_for(account));
        let Ok(subscriptions) = subscriptions else {
            return missing(account, "local_state", "allowance state could not be read");
        };
        #[cfg(test)]
        let subscriptions = self
            .native_fixture
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
            .unwrap_or(subscriptions);
        let Ok(observation) = tokio::time::timeout(
            timeout,
            subscriptions
                .subscription_usage_read_only(provider, self.provider.clone().acquire_owned()),
        )
        .await
        else {
            return missing(
                account,
                "deadline",
                "provider allowance read took too long; retry",
            );
        };
        project(account, VerticalObservation::Native(observation))
    }
}

fn project(account: &AccountRef, observation: VerticalObservation) -> Allowance {
    let remedies = remedies(account, &observation);
    let Some(vertical) = crate::commands::usage::usage_vertical_for_account(account.as_str())
    else {
        return missing(
            account,
            "not_supported",
            "allowance is not available for this account",
        );
    };
    let view = subscription_usage::project_limits(
        [(vertical.clone(), observation)],
        SessionLedgerView::default(),
    )
    .verticals
    .into_iter()
    .find(|view| view.vertical == vertical);
    let Some(view) = view else {
        return missing(
            account,
            "not_supported",
            "allowance is not available for this account",
        );
    };
    let observed_at = view.observed_at.clone();
    let stale_age_ms = observed_at
        .as_deref()
        .and_then(super::clock::milliseconds)
        .and_then(|at| u64::try_from(super::clock::now_ms() - at).ok());
    Allowance {
        observed_at,
        stale_age_ms,
        view: Some(view),
        unavailable: None,
        remedies,
    }
}

fn remedies(account: &AccountRef, observation: &VerticalObservation) -> Vec<ReadRemedy> {
    use plakous::model::{
        SubscriptionUsageCredentialState as C, SubscriptionUsageRead as R,
        SubscriptionUsageUnavailable as U,
    };
    let action = match observation {
        VerticalObservation::Native(R::Unavailable {
            reason: U::Unauthorized | U::CredentialState(C::RefreshRequired),
            ..
        }) => ReadAction::Refresh,
        VerticalObservation::Native(R::Unavailable {
            reason:
                U::NotConnected | U::CredentialState(C::LoginRequired | C::Revoked | C::Quarantined),
            ..
        }) => ReadAction::Login,
        VerticalObservation::RejectedKey { .. } | VerticalObservation::NotConnected
            if account.vertical() == plakous_hosted::account::Vertical::OpenRouter =>
        {
            ReadAction::SetKey
        }
        _ => return Vec::new(),
    };
    vec![ReadRemedy {
        action,
        account_id: Some(account.to_string()),
    }]
}

fn missing(account: &AccountRef, code: &str, detail: &str) -> Allowance {
    Allowance {
        observed_at: None,
        stale_age_ms: None,
        view: None,
        unavailable: Some(diagnostic(code, Some(account.as_str()), detail)),
        remedies: Vec::new(),
    }
}
fn utc(at: i64) -> String {
    super::clock::utc(at).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::model::{
        SubscriptionUsageCredentialState as C, SubscriptionUsageRead as R,
        SubscriptionUsageUnavailable as U, Timestamp,
    };

    #[test]
    fn allowance_repairs_name_the_exact_account_and_credential_action() {
        let native: AccountRef = "codex/work".parse().unwrap();
        for (reason, action) in [
            (U::Unauthorized, ReadAction::Refresh),
            (U::CredentialState(C::RefreshRequired), ReadAction::Refresh),
            (U::NotConnected, ReadAction::Login),
        ] {
            let observation = VerticalObservation::Native(R::Unavailable {
                observed_at: Timestamp::new(1, 0).unwrap(),
                reason,
            });
            assert_eq!(
                project(&native, observation).remedies,
                vec![ReadRemedy {
                    action,
                    account_id: Some("codex/work".into())
                }]
            );
        }
        let router: AccountRef = "openrouter/work".parse().unwrap();
        for observation in [
            VerticalObservation::NotConnected,
            VerticalObservation::RejectedKey {
                reason: "fixture".into(),
                source: plakous_host_protocol::views::KeySource::Environment,
            },
        ] {
            assert_eq!(
                project(&router, observation).remedies,
                vec![ReadRemedy {
                    action: ReadAction::SetKey,
                    account_id: Some("openrouter/work".into())
                }]
            );
        }
    }
}
