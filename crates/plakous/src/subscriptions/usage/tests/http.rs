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
async fn read_only_rejection_is_one_get_and_never_a_refresh_or_retry() -> TestResult {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path())?;
    seed(&paths, now()?)?;
    let fixture = usage_server(vec![401]).await?;
    let mut subscriptions = NativeSubscriptions::new(paths);
    subscriptions.codex_test_transport = Some((fixture.client, fixture.origin));
    let first = subscriptions
        .subscription_usage_read_only(NativeProvider::Codex)
        .await;
    assert!(matches!(
        first,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Unauthorized,
            ..
        }
    ));
    let again = subscriptions
        .subscription_usage_read_only(NativeProvider::Codex)
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
            .subscription_usage_read_only(NativeProvider::Codex)
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
            .subscription_usage_read_only(NativeProvider::Codex)
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
