use super::*;
use crate::subscriptions::{
    SubscriptionPaths,
    state::tests::child_catalog::{seed, usage_server},
};
use std::sync::atomic::Ordering;
type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn now() -> Result<u64, Box<dyn std::error::Error + Send + Sync>> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs())
}

#[tokio::test]
async fn read_only_ready_recheck_precedes_a_fresh_exact_fenced_cache() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let store = plakous_auth::PrivateCredentialStore::open_slot(
        paths
            .state_root
            .join("plak/subscriptions/codex/credential.json"),
        plakous_auth::SubscriptionIntegrationProfileId::new(
            plakous_openai::OPENAI_CODEX_COMPATIBILITY_PROFILE_ID,
        )?,
        plakous_auth::CredentialSlotId::new("primary")?,
    )?;
    store.logout(plakous_auth::CredentialGeneration::new(1)?)?;
    let subscriptions = NativeSubscriptions::new(paths.clone());
    let provider = NativeProvider::Codex;
    let revision = subscriptions.usage_snapshot(provider).await?;
    let observation = subscriptions.usage_unavailable(SubscriptionUsageUnavailable::Unauthorized);
    let flight = cache::Flight::start(&paths, provider);
    flight.publish(revision, &observation);
    drop(flight);
    let result = subscriptions
        .subscription_usage_read_only(provider, std::future::pending::<()>())
        .await;
    assert!(
        matches!(
            result,
            SubscriptionUsageRead::Unavailable {
                reason: SubscriptionUsageUnavailable::CredentialState(
                    SubscriptionUsageCredentialState::Quarantined
                ),
                ..
            }
        ),
        "{result:?}"
    );
    Ok(())
}

#[tokio::test]
async fn public_console_read_joins_an_active_legacy_get_without_starting_another() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let fixture = usage_server(vec![401]).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    let provider = NativeProvider::Codex;
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (finish, finished) = tokio::sync::oneshot::channel();
    let legacy = {
        let subscriptions = subscriptions.clone();
        tokio::spawn(async move {
            let lock = subscriptions
                .usage_lock(provider)
                .await
                .map_err(|_| "usage lock")?;
            Ok::<_, &'static str>(
                subscriptions
                    .observe_usage_unlocked(provider, lock, async {
                        let _ = started.send(());
                        let _ = finished.await;
                        subscriptions.usage_unavailable(SubscriptionUsageUnavailable::Unauthorized)
                    })
                    .await,
            )
        })
    };
    waiting.await?;
    let mut console =
        Box::pin(subscriptions.subscription_usage_read_only(provider, std::future::ready(())));
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut console)
            .await
            .is_err(),
        "console must wait for the registered legacy GET"
    );
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 0);
    finish.send(()).map_err(|()| "legacy exited")?;
    let expected = legacy.await??;
    assert_eq!(console.await, expected);
    fixture.server.abort();
    Ok(())
}

#[tokio::test]
async fn a_cached_native_observation_does_not_acquire_the_host_provider_permit() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let fixture = usage_server(vec![401]).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    let provider = NativeProvider::Codex;
    let expected = subscriptions
        .subscription_usage_read_only(provider, std::future::ready(()))
        .await;
    let cached = tokio::time::timeout(
        Duration::from_secs(5),
        subscriptions.subscription_usage_read_only(provider, std::future::pending::<()>()),
    )
    .await?;
    assert_eq!(cached, expected);
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 1);
    fixture.server.await??;
    Ok(())
}

#[tokio::test]
async fn read_only_rejection_is_one_get_and_never_a_refresh_or_retry() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let fixture = usage_server(vec![401]).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    let first = subscriptions
        .subscription_usage_read_only(NativeProvider::Codex, std::future::ready(()))
        .await;
    assert!(matches!(
        first,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Unauthorized,
            ..
        }
    ));
    let again = subscriptions
        .subscription_usage_read_only(NativeProvider::Codex, std::future::ready(()))
        .await;
    assert_eq!(first, again);
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(30), fixture.server).await???;
    Ok(())
}

#[tokio::test]
async fn legacy_still_gets_fresh_data_while_console_uses_its_fenced_observations() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let fixture = usage_server(vec![200, 200]).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    for expected in [1, 2] {
        let fresh = subscriptions
            .subscription_usage(NativeProvider::Codex)
            .await;
        assert!(matches!(fresh, SubscriptionUsageRead::Snapshot(_)));
        let console = subscriptions
            .subscription_usage_read_only(NativeProvider::Codex, std::future::ready(()))
            .await;
        assert_eq!(console, fresh);
        assert_eq!(fixture.gets.load(Ordering::SeqCst), expected);
    }
    tokio::time::timeout(Duration::from_secs(30), fixture.server).await???;
    Ok(())
}

#[tokio::test]
async fn read_only_expired_token_makes_no_request_and_keeps_its_generation() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?.checked_sub(7200).ok_or("clock range")?)?;
    let fixture = usage_server(Vec::new()).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    fixture.server.await??;
    let before = subscriptions
        .account_observation_revision(NativeProvider::Codex)
        .await
        .map_err(|_| "fixture revision unavailable")?;
    assert!(matches!(
        subscriptions
            .subscription_usage_read_only(NativeProvider::Codex, std::future::ready(()))
            .await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired
            ),
            ..
        }
    ));
    assert_eq!(
        subscriptions
            .account_observation_revision(NativeProvider::Codex)
            .await
            .map_err(|_| "fixture revision unavailable")?,
        before
    );
    Ok(())
}
