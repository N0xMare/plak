use super::*;
use std::sync::Arc;
use std::time::Duration;

#[cfg(all(feature = "codex", feature = "grok", feature = "claude"))]
mod http;

#[tokio::test]
async fn console_joins_a_legacy_read_and_reuses_only_its_exact_fenced_observation()
-> Result<(), Box<dyn std::error::Error>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir()?;
    let paths = super::super::SubscriptionPaths::from_xdg_state_home(root.path())?;
    let subscriptions = NativeSubscriptions::new(paths.clone());
    let provider = NativeProvider::Codex;
    let entry = cache::entry(&paths, provider).ok_or("bounded fixture scope")?;
    let gets = Arc::new(AtomicUsize::new(0));
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let legacy = {
        let subscriptions = subscriptions.clone();
        let gets = Arc::clone(&gets);
        tokio::spawn(async move {
            let lock = subscriptions.acquire_compatibility_lock(provider).await?;
            Ok::<_, NativeSubscriptionError>(
                subscriptions
                    .observe_usage_unlocked(provider, lock, async {
                        gets.fetch_add(1, Ordering::SeqCst);
                        let _ = started.send(());
                        let _ = finished.await;
                        subscriptions.usage_unavailable(SubscriptionUsageUnavailable::NotConnected)
                    })
                    .await,
            )
        })
    };
    waiting.await?;
    let mut changed = entry
        .pending()
        .ok_or("legacy GET registered before unlocking")?;
    assert!(entry.cached(&(None, None)).is_none());
    finish.send(()).map_err(|()| "legacy stopped early")?;
    changed.changed().await?;
    let observation = legacy.await??;
    assert_eq!(entry.cached(&(None, None)), Some(observation.clone()));
    assert_eq!(
        gets.load(Ordering::SeqCst),
        1,
        "console joined, then reused the legacy observation"
    );
    assert!(
        entry
            .cached(&(Some("another-consent".into()), None))
            .is_none()
    );
    let named = paths.for_named_account(provider, "other")?;
    assert!(
        cache::entry(&named, provider)
            .ok_or("named scope")?
            .cached(&(None, None))
            .is_none()
    );
    // Legacy never consults the populated console cache: its next read still
    // polls a fresh reader, and its per-session 60 s aging remains elsewhere.
    let lock = subscriptions.acquire_compatibility_lock(provider).await?;
    subscriptions
        .observe_usage_unlocked(provider, lock, async {
            gets.fetch_add(1, Ordering::SeqCst);
            observation
        })
        .await;
    assert_eq!(gets.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn console_read_of_an_absent_namespace_does_not_create_it()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let state = root.path().join("missing");
    let paths = super::super::SubscriptionPaths::from_xdg_state_home(&state)?;
    let subscriptions = NativeSubscriptions::new(paths);
    assert!(matches!(
        subscriptions
            .subscription_usage_read_only(NativeProvider::Codex)
            .await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::NotConnected,
            ..
        }
    ));
    assert!(!state.exists());
    Ok(())
}

#[tokio::test]
async fn three_slow_usage_reads_do_not_hold_the_shared_compatibility_guard()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = super::super::SubscriptionPaths::from_xdg_state_home(root.path())?;
    let subscriptions = NativeSubscriptions::new(paths);
    // Every fake provider waits until all three network reads are in flight.
    // Holding the directory guard across any read makes this time out.
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let run = |provider| {
        let subscriptions = subscriptions.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            let lock = subscriptions.acquire_compatibility_lock(provider).await?;
            let read = async {
                barrier.wait().await;
                subscriptions.usage_unavailable(SubscriptionUsageUnavailable::NotConnected)
            };
            Ok::<_, NativeSubscriptionError>(
                subscriptions
                    .observe_usage_unlocked(provider, lock, read)
                    .await,
            )
        }
    };
    // A liveness bound only: generous, so load never trips it.
    let (codex, grok, claude) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(
            run(NativeProvider::Codex),
            run(NativeProvider::Xai),
            run(NativeProvider::Claude)
        )
    })
    .await?;
    for observation in [codex?, grok?, claude?] {
        assert!(matches!(
            observation,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::NotConnected,
                ..
            }
        ));
    }
    Ok(())
}

#[tokio::test]
async fn consent_change_during_usage_discards_the_observation()
-> Result<(), Box<dyn std::error::Error>> {
    let root = tempfile::tempdir()?;
    let paths = super::super::SubscriptionPaths::from_xdg_state_home(root.path())?;
    let subscriptions = NativeSubscriptions::new(paths);
    let provider = NativeProvider::Codex;
    let lock = subscriptions.acquire_compatibility_lock(provider).await?;
    let read = async {
        subscriptions
            .acknowledge_exact(
                provider,
                NativeSubscriptions::notice(provider)?.notice_revision(),
            )
            .await?;
        Ok::<_, NativeSubscriptionError>(
            subscriptions.usage_unavailable(SubscriptionUsageUnavailable::NotConnected),
        )
    };
    let observation = subscriptions
        .observe_usage_unlocked(provider, lock, async {
            read.await.unwrap_or_else(|_| {
                subscriptions.usage_unavailable(SubscriptionUsageUnavailable::LocalState)
            })
        })
        .await;
    assert!(matches!(
        observation,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::AccountChanged,
            ..
        }
    ));
    Ok(())
}
