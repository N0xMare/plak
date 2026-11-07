//! Hosted continuation of Codex and SuperGrok sessions past their admitted
//! catalog window (spec 01 §7.4).
//!
//! The agent runtime asks for a fresh resolution between model operations once
//! the current one's window closed. This resolves the same exact selection
//! through the facade, recompiles the node's session configuration from it the
//! way construction did, and hands the result back for the runtime's own
//! binding checks. Roots and children install it alike; routes without a
//! window (Claude OAuth, API keys, OpenRouter) install nothing.

use std::future::Future;
use std::sync::Arc;

use plakous::agent::{
    ModelReresolutionError, ModelReresolutionOperation, ModelReresolver, ReresolvedModel,
};
use plakous::model::{ConfiguredLanguageModel, Timestamp};
use plakous::subscriptions::{NativeProvider, NativeSubscriptionError, ResolvedProvider};

use crate::CliResult;

/// Recompiles the node's session configuration from a fresh resolution.
pub(super) type SessionCompile =
    dyn Fn(ConfiguredLanguageModel) -> CliResult<ConfiguredLanguageModel> + Send + Sync;

/// One shared current value refreshed by at most one caller at a time.
///
/// A fresh value is only offered: callers that waited while it was refreshed
/// share it, but it becomes the current value only once a runtime reports it
/// accepted. A caller arriving after a refresh, past the same window, means
/// that runtime refused the offer (or its compile failed), so it refreshes
/// anew instead of receiving the refused value again.
pub(super) struct SingleFlight<T> {
    refreshing: tokio::sync::Mutex<()>,
    state: std::sync::Mutex<Flight<T>>,
}

struct Flight<T> {
    current: Arc<T>,
    offered: Option<Arc<T>>,
    refreshes: u64,
}

impl<T> SingleFlight<T> {
    pub(super) fn new(current: Arc<T>) -> Self {
        Self {
            refreshing: tokio::sync::Mutex::new(()),
            state: std::sync::Mutex::new(Flight {
                current,
                offered: None,
                refreshes: 0,
            }),
        }
    }

    fn flight(&self) -> std::sync::MutexGuard<'_, Flight<T>> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) async fn refreshed<E, F, Fut>(
        &self,
        expired_until: Timestamp,
        window: impl Fn(&T) -> Option<Timestamp>,
        refresh: F,
    ) -> Result<Arc<T>, E>
    where
        F: FnOnce(Arc<T>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let arrived_after = self.flight().refreshes;
        let _refreshing = self.refreshing.lock().await;
        let current = {
            let flight = self.flight();
            let later = |value: &Arc<T>| window(value).is_some_and(|until| until > expired_until);
            if flight.refreshes != arrived_after
                && let Some(offered) = flight.offered.as_ref().filter(|offered| later(offered))
            {
                return Ok(Arc::clone(offered));
            }
            if later(&flight.current) {
                return Ok(Arc::clone(&flight.current));
            }
            Arc::clone(&flight.current)
        };
        let fresh = Arc::new(refresh(current).await?);
        let mut flight = self.flight();
        flight.offered = Some(Arc::clone(&fresh));
        flight.refreshes = flight.refreshes.wrapping_add(1);
        Ok(fresh)
    }

    /// Installs the offered value whose window ends at `admitted_until`.
    pub(super) fn accept(
        &self,
        admitted_until: Timestamp,
        window: impl Fn(&T) -> Option<Timestamp>,
    ) {
        let mut flight = self.flight();
        if let Some(offered) = flight
            .offered
            .take_if(|offered| window(offered) == Some(admitted_until))
        {
            flight.current = offered;
        }
    }
}

pub(super) struct NativeModelReresolver {
    provider: NativeProvider,
    resolved: SingleFlight<ResolvedProvider>,
    compile: Box<SessionCompile>,
}

impl NativeModelReresolver {
    /// Returns the hook and the current window's end for a windowed
    /// resolution, `None` for a route without a window.
    pub(super) fn for_resolution(
        provider: NativeProvider,
        resolved: &Arc<ResolvedProvider>,
        compile: Box<SessionCompile>,
    ) -> Option<(Arc<dyn ModelReresolver>, Timestamp)> {
        let admitted_until = resolved.admitted_until()?;
        let reresolver: Arc<dyn ModelReresolver> = Arc::new(Self {
            provider,
            resolved: SingleFlight::new(Arc::clone(resolved)),
            compile,
        });
        Some((reresolver, admitted_until))
    }
}

fn refusal(provider: NativeProvider, kind: NativeSubscriptionError) -> ModelReresolutionError {
    // Drift is a pinned-contract mismatch, not a missing login. A login
    // remedy would send the operator through `plak login` for a condition
    // that login cannot repair. The variant exists only with a native route.
    #[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
    if let NativeSubscriptionError::CompatibilityDrift { .. } = &kind {
        return ModelReresolutionError::new(kind.to_string());
    }
    let target = crate::binding::account_target(provider);
    let (label, account) = match &target {
        plakous_host_protocol::notice_remedy::AccountTarget::Codex => ("Codex", "codex"),
        plakous_host_protocol::notice_remedy::AccountTarget::Grok => ("xAI", "grok"),
        plakous_host_protocol::notice_remedy::AccountTarget::Claude => ("Claude", "claude"),
        plakous_host_protocol::notice_remedy::AccountTarget::OpenRouter => {
            ("OpenRouter", "openrouter")
        }
        plakous_host_protocol::notice_remedy::AccountTarget::Unspecified => {
            ("subscription", "unspecified")
        }
        plakous_host_protocol::notice_remedy::AccountTarget::Named(account) => {
            (account.as_str(), account.as_str())
        }
    };
    let reason = match &kind {
        NativeSubscriptionError::ReresolutionBindingChanged => {
            plakous::agent::ModelReresolutionRefusal::BindingChanged
        }
        NativeSubscriptionError::CredentialUnavailable
        | NativeSubscriptionError::Authentication
        | NativeSubscriptionError::AuthenticationBinding
        | NativeSubscriptionError::AuthenticationAccountChanged => {
            plakous::agent::ModelReresolutionRefusal::CredentialUnavailable {
                account: account.to_owned(),
            }
        }
        _ => plakous::agent::ModelReresolutionRefusal::ResolutionFailed {
            account: account.to_owned(),
        },
    };
    ModelReresolutionError::new(match kind {
        NativeSubscriptionError::ReresolutionBindingChanged => format!(
            "fresh {label} resolution changed the selected login, account or model semantics; explicitly rebind by forking, or start a new session"
        ),
        NativeSubscriptionError::CredentialUnavailable
        | NativeSubscriptionError::Authentication
        | NativeSubscriptionError::AuthenticationBinding
        | NativeSubscriptionError::AuthenticationAccountChanged => format!(
            "{label} credential is unavailable (signed out, quarantined, or a login made by an earlier build); restore usable credentials"
        ),
        kind => format!(
            "exact {label} resolution failed: {kind}; retry the prompt, then restore credentials if it persists"
        ),
    }).with_refusal(reason)
}

impl ModelReresolver for NativeModelReresolver {
    fn reresolve(self: Arc<Self>, expired_until: Timestamp) -> ModelReresolutionOperation {
        Box::pin(async move {
            let fresh = self
                .resolved
                .refreshed(
                    expired_until,
                    ResolvedProvider::admitted_until,
                    |current| async move { current.reresolve().await },
                )
                .await
                .map_err(|error| refusal(self.provider, error.kind()))?;
            let admitted_until = fresh
                .admitted_until()
                .ok_or_else(|| refusal(self.provider, NativeSubscriptionError::Internal))?;
            let configured = (self.compile)(fresh.configured_model())
                .map_err(|error| ModelReresolutionError::new(error.message().to_owned()))?;
            Ok(ReresolvedModel::new(configured, admitted_until))
        })
    }

    fn accepted(&self, admitted_until: Timestamp) {
        self.resolved
            .accept(admitted_until, ResolvedProvider::admitted_until);
    }
}

#[cfg(test)]
mod tests;
