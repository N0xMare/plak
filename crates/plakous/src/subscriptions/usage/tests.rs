use super::*;
use std::sync::Arc;
use std::time::Duration;

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
    let (codex, grok, claude) = tokio::time::timeout(Duration::from_secs(5), async {
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
