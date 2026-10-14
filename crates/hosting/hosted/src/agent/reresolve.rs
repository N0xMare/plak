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
    let (label, account) = match provider {
        NativeProvider::Codex => ("Codex", "codex"),
        _ => ("xAI", "grok"),
    };
    ModelReresolutionError::new(match kind {
        NativeSubscriptionError::ReresolutionBindingChanged => format!(
            "fresh {label} resolution changed the selected login, account or model semantics; \
             rebind with Fork now in the Agents tab, or start a new session"
        ),
        NativeSubscriptionError::CredentialUnavailable
        | NativeSubscriptionError::Authentication
        | NativeSubscriptionError::AuthenticationBinding
        | NativeSubscriptionError::AuthenticationAccountChanged => format!(
            "{label} credential is unavailable (signed out, quarantined, or a login made by an earlier build); run `plak login {account}`"
        ),
        kind => format!(
            "exact {label} resolution failed: {kind}; retry the prompt, or run `plak login {account}` if it persists"
        ),
    })
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn at(seconds: u64) -> TestResult<Timestamp> {
        Ok(Timestamp::new(seconds, 0)?)
    }

    #[test]
    fn a_changed_binding_names_the_fork_now_remedy() {
        let refusal = refusal(
            NativeProvider::Codex,
            NativeSubscriptionError::ReresolutionBindingChanged,
        );
        assert!(
            refusal
                .message()
                .ends_with("rebind with Fork now in the Agents tab, or start a new session"),
            "{refusal}"
        );
        assert!(!refusal.message().contains("/model"), "{refusal}");
    }

    #[tokio::test]
    async fn concurrent_callers_past_one_window_share_one_refresh() -> TestResult {
        let flight = Arc::new(SingleFlight::new(Arc::new(at(100)?)));
        let refreshes = Arc::new(AtomicU32::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let callers = (0..4)
            .map(|_| {
                let (flight, refreshes, gate) = (
                    Arc::clone(&flight),
                    Arc::clone(&refreshes),
                    Arc::clone(&gate),
                );
                tokio::spawn(async move {
                    flight
                        .refreshed(
                            Timestamp::new(100, 0).map_err(|_| ())?,
                            |until: &Timestamp| Some(*until),
                            |_| async move {
                                refreshes.fetch_add(1, Ordering::SeqCst);
                                gate.notified().await;
                                Timestamp::new(200, 0).map_err(|_| ())
                            },
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        while refreshes.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Every caller is waiting on the refresh in flight.
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        gate.notify_one();
        for caller in callers {
            assert_eq!(*caller.await?.map_err(|()| "refresh failed")?, at(200)?);
        }
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        flight.accept(at(200)?, |until: &Timestamp| Some(*until));
        // A later expiry of the fresh window refreshes again.
        let again = flight
            .refreshed(
                at(200)?,
                |until: &Timestamp| Some(*until),
                |_| async { Timestamp::new(300, 0).map_err(|_| ()) },
            )
            .await
            .map_err(|()| "refresh failed")?;
        assert_eq!(*again, at(300)?);
        Ok(())
    }

    #[tokio::test]
    async fn a_failed_refresh_keeps_the_current_value_and_installs_nothing() -> TestResult {
        let flight = SingleFlight::new(Arc::new(at(100)?));
        let failed = flight
            .refreshed(
                at(100)?,
                |until: &Timestamp| Some(*until),
                |_| async { Err::<Timestamp, _>("binding changed") },
            )
            .await;
        assert_eq!(failed.err(), Some("binding changed"));
        let retried = flight
            .refreshed(
                at(100)?,
                |until: &Timestamp| Some(*until),
                |current| async move {
                    assert_eq!(*current, Timestamp::new(100, 0).map_err(|_| "time")?);
                    Timestamp::new(150, 0).map_err(|_| "time")
                },
            )
            .await?;
        assert_eq!(*retried, at(150)?);
        Ok(())
    }

    /// A resolution the runtime refused (or whose compile failed) is never
    /// handed out again: the next caller past the same window refreshes from
    /// the accepted value, and only an accepted resolution is shared.
    #[tokio::test]
    async fn a_refused_resolution_is_refreshed_not_reused() -> TestResult {
        let flight = SingleFlight::new(Arc::new(at(100)?));
        let window = |until: &Timestamp| Some(*until);
        let refreshes = AtomicU32::new(0);
        let refresh = |next: u64| {
            let refreshes = &refreshes;
            move |current: Arc<Timestamp>| async move {
                refreshes.fetch_add(1, Ordering::SeqCst);
                assert_eq!(*current, Timestamp::new(100, 0).map_err(|_| "time")?);
                Timestamp::new(next, 0).map_err(|_| "time")
            }
        };
        let offered = flight.refreshed(at(100)?, window, refresh(200)).await?;
        assert_eq!(*offered, at(200)?);
        // The runtime refused it: nothing was accepted.
        let again = flight.refreshed(at(100)?, window, refresh(300)).await?;
        assert_eq!(*again, at(300)?);
        assert_eq!(refreshes.load(Ordering::SeqCst), 2);
        // A stale acceptance installs nothing; the current offer's does.
        flight.accept(at(200)?, window);
        flight.accept(at(300)?, window);
        let shared = flight.refreshed(at(100)?, window, refresh(400)).await?;
        assert_eq!(*shared, at(300)?);
        assert_eq!(refreshes.load(Ordering::SeqCst), 2);
        Ok(())
    }
}
