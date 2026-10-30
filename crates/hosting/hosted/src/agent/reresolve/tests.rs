use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn at(seconds: u64) -> TestResult<Timestamp> {
    Ok(Timestamp::new(seconds, 0)?)
}

#[cfg(any(feature = "codex", feature = "grok", feature = "claude"))]
#[test]
fn compatibility_drift_is_not_a_login_remedy() {
    let refusal = refusal(
        NativeProvider::Codex,
        NativeSubscriptionError::CompatibilityDrift {
            provider: NativeProvider::Codex,
            reason: plakous::subscriptions::CompatibilityDrift::GrantedScope,
        },
    );
    assert!(refusal.refusal().is_none(), "{refusal}");
    let agent =
        plakous::agent::AgentError::new(plakous::agent::AgentErrorKind::Model, refusal.message())
            .with_reresolution_error(Some(refusal));
    assert!(crate::runtime::agent_error_remedy(&agent).is_none());
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
            .ends_with("explicitly rebind by forking, or start a new session"),
        "{refusal}"
    );
    assert!(matches!(
        refusal.refusal(),
        Some(plakous::agent::ModelReresolutionRefusal::BindingChanged)
    ));
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
