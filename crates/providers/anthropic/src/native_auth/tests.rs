use super::*;
use plakous_auth::CredentialRecord;

#[tokio::test]
async fn usage_expired_access_reports_refresh_required_without_claiming_refresh() {
    use plakous_auth::subscription_limit::{
        InMemorySubscriptionRequestGate, SubscriptionRequestLimits, SystemSubscriptionGateClock,
    };
    let temp = tempfile::tempdir().unwrap();
    let mut manager = manager(private_store(&temp));
    // Fail closed with no network: a regressed refresh finds no permit,
    // and any request at all would connect to this local listener.
    manager.request_gate = Some(Arc::new(InMemorySubscriptionRequestGate::new(
        SubscriptionRequestLimits::new(0, 0, 0, 0, None, 0).unwrap(),
        Arc::new(SystemSubscriptionGateClock::default()),
    )));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let core = Arc::get_mut(&mut manager.core).unwrap();
    core.token_origin = origin.clone();
    core.control_origin = Some(origin);
    manager
        .persist_test_grant("offline-access", "offline-refresh", 3600, "user:inference")
        .unwrap();
    let fence = manager.core.store.load().unwrap().unwrap();
    let mut stored = manager.core.read_stored(&fence).unwrap();
    stored.expires_at = 1;
    manager
        .core
        .store
        .replace(
            fence.generation(),
            &credential_record(fence.generation().next().unwrap(), &stored).unwrap(),
        )
        .unwrap();
    let before = manager.core.store.load().unwrap().unwrap();
    // Dropped after its first poll, as the hosted usage timeout drops a
    // slow read; then read to completion.
    let _ = tokio::time::timeout(Duration::ZERO, manager.subscription_usage()).await;
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::RefreshRequired
            ),
            ..
        }
    ));
    let after = manager.core.store.load().unwrap().unwrap();
    assert_eq!(after, before);
    assert_eq!(after.status(), CredentialStatus::Ready);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "a usage read with an expired token must attempt no request"
    );
}

/// Another process's in-flight refresh claim makes the durable fence
/// Unknown only transiently; it is retryable contention, not a quarantine.
#[tokio::test]
async fn usage_reports_peer_refresh_claim_as_busy_not_quarantined() {
    let temp = tempfile::tempdir().unwrap();
    let path = private_store(&temp);
    let mut manager = manager(path.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("https://{}", listener.local_addr().unwrap());
    let core = Arc::get_mut(&mut manager.core).unwrap();
    core.token_origin = origin.clone();
    core.control_origin = Some(origin);
    manager
        .persist_test_grant("offline-access", "offline-refresh", 3600, "user:inference")
        .unwrap();
    let peer = PrivateCredentialStore::open_slot(
        path,
        auth_profile_id().unwrap(),
        CredentialSlotId::new("primary").unwrap(),
    )
    .unwrap();
    let fence = peer.load().unwrap().unwrap();
    let claim = peer.begin_refresh(&fence).unwrap().start();
    assert_eq!(
        manager.core.store.load().unwrap().unwrap().status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::Busy,
            ..
        }
    ));
    drop(claim);
    assert!(matches!(
        manager.subscription_usage().await,
        SubscriptionUsageRead::Unavailable {
            reason: SubscriptionUsageUnavailable::CredentialState(
                SubscriptionUsageCredentialState::Quarantined
            ),
            ..
        }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[test]
fn grant_lineage_binding_is_stable_and_not_the_route_name() {
    let first = oauth_grant_binding("refresh-token-lineage-one").expect("first binding");
    let same = oauth_grant_binding("refresh-token-lineage-one").expect("same binding");
    let other = oauth_grant_binding("refresh-token-lineage-two").expect("other binding");
    assert_eq!(first, same);
    assert_ne!(first, other);
    assert_ne!(first.as_str(), "claude-oauth");
    assert!(!first.as_str().contains("refresh-token"));
}
use plakous_http::Limits;

pub(super) fn private_store(temp: &tempfile::TempDir) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = temp.path().join("credentials");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    directory.join("claude.json")
}

pub(super) fn manager(path: PathBuf) -> ClaudeNativeAuthManager {
    let now = Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX, 0).unwrap();
    let acceptance = ClaudeCompatibilityAcceptance::accept_exact(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
        CLAUDE_OAUTH_SOURCE_REVISION,
        CLAUDE_OAUTH_NOTICE_REVISION,
        CLAUDE_OAUTH_KILL_SWITCH_REVISION,
        now,
    )
    .unwrap();
    let slot = CredentialSlotId::new("primary").unwrap();
    let client = Client::new_http1(Limits::default()).unwrap();
    ClaudeNativeAuthManager::open_selected_slot(
        path,
        slot,
        client,
        acceptance,
        crate::ClaudeClientDeviceId::new("ab".repeat(32)).unwrap(),
    )
    .unwrap()
}

fn write_retired(path: PathBuf, payload: Vec<u8>) -> PrivateCredentialStore {
    let store = PrivateCredentialStore::open_slot(
        path,
        auth_profile_id().unwrap(),
        CredentialSlotId::new("primary").unwrap(),
    )
    .unwrap();
    let record = CredentialRecord::new(
        auth_profile_id().unwrap(),
        AccountBinding::new("claude-oauth").unwrap(),
        CredentialGeneration::new(1).unwrap(),
        CredentialStatus::Ready,
        SecretBytes::new(payload),
    )
    .unwrap();
    store.initialize_selected(&record).unwrap();
    store
}

#[test]
fn acceptance_binds_the_contract_and_expires_ninety_days_after_review() {
    assert_eq!(
        CLAUDE_OAUTH_EXPIRES_AT_UNIX,
        CLAUDE_OAUTH_REVIEWED_AT_UNIX + 90 * 86_400
    );
    let before_review = ClaudeCompatibilityAcceptance::accept_exact(
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID,
        CLAUDE_OAUTH_SOURCE_REVISION,
        CLAUDE_OAUTH_NOTICE_REVISION,
        CLAUDE_OAUTH_KILL_SWITCH_REVISION,
        Timestamp::new(CLAUDE_OAUTH_REVIEWED_AT_UNIX - 1, 0).unwrap(),
    )
    .unwrap();
    assert!(validate_acceptance(&before_review, CLAUDE_OAUTH_REVIEWED_AT_UNIX).is_ok());
    assert!(validate_acceptance(&before_review, CLAUDE_OAUTH_EXPIRES_AT_UNIX - 1).is_ok());
    for now in [
        CLAUDE_OAUTH_REVIEWED_AT_UNIX - 1,
        CLAUDE_OAUTH_EXPIRES_AT_UNIX,
        CLAUDE_OAUTH_REVIEWED_AT_UNIX - 2,
    ] {
        assert!(matches!(
            validate_acceptance(&before_review, now),
            Err(ClaudeNativeAuthError::CompatibilityInactive)
        ));
    }
}

#[test]
fn begin_login_uses_cai_authorize_and_independent_state() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    let login = manager.begin_login().unwrap();
    let url = login.authorization_url();
    assert!(url.starts_with("https://claude.com/cai/oauth/authorize?"));
    assert!(url.contains("code=true"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("state="));
    assert!(url.ends_with(login.state.expose_secret()));
    assert_ne!(
        login.state.expose_secret(),
        login.pkce.verifier().expose_secret()
    );
    assert!(!url.contains(login.pkce.verifier().expose_secret()));
    assert!(!url.contains("platform.claude.com/oauth/authorize"));
    assert_eq!(
        format!("{login:?}"),
        "ClaudeOAuthLogin(<redacted non-cloneable login>)"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn invalid_paste_fails_before_token_exchange() {
    let temp = tempfile::tempdir().unwrap();
    let manager = manager(private_store(&temp));
    let login = manager.begin_login().unwrap();
    assert!(matches!(
        login.complete("nope").await,
        Err(ClaudeNativeAuthError::InvalidCallback)
    ));
    assert!(manager.status().unwrap().is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn mismatched_state_fails_before_token_exchange() {
    let temp = tempfile::tempdir().unwrap();
    let mut manager = manager(private_store(&temp));
    // If validation regresses, request construction fails without any network.
    Arc::get_mut(&mut manager.core).unwrap().token_origin = "invalid-test-origin".to_owned();
    let login = manager.begin_login().unwrap();
    let input = format!(
        "{}#{}",
        "fixture-code".repeat(3),
        login.pkce.verifier().expose_secret()
    );
    assert!(matches!(
        login.validate_authorization_input(&input),
        Err(ClaudeNativeAuthError::InvalidCallback)
    ));
    assert!(matches!(
        login.validate_authorization_input("nope"),
        Err(ClaudeNativeAuthError::InvalidCallback)
    ));
    assert!(
        login
            .validate_authorization_input(&format!(
                "{}#{}",
                "fixture-code".repeat(3),
                login.state.expose_secret()
            ))
            .is_ok(),
        "a rejected paste must retain the same pending authority"
    );
    assert!(manager.status().unwrap().is_none());
    assert!(matches!(
        login.complete(&input).await,
        Err(ClaudeNativeAuthError::InvalidCallback)
    ));
    assert!(manager.status().unwrap().is_none());
}

#[test]
fn retired_ready_records_are_refused_and_left_unchanged() {
    for version in [1, 2] {
        let temp = tempfile::tempdir().unwrap();
        let path = private_store(&temp);
        let retired = serde_json::to_vec(&serde_json::json!({
            "version": version,
            "client_id": CLAUDE_OAUTH_CLIENT_ID,
            "scope": CLAUDE_OAUTH_INFERENCE_SCOPE,
            "access_token": "retired-access-token",
            "refresh_token": "retired-refresh-token",
            "expires_at": system_unix_seconds().unwrap() + 3_600,
        }))
        .unwrap();
        let store = write_retired(path.clone(), retired);
        let manager = manager(path.clone());
        assert!(matches!(
            manager.status(),
            Err(ClaudeNativeAuthError::StoredCredentialFormatRetired)
        ));
        let unchanged = store.load().unwrap().expect("retired fence remains");
        assert_eq!(unchanged.generation().get(), 1);
        assert_eq!(unchanged.account_binding().as_str(), "claude-oauth");
        let offline = ClaudeOfflineSelectedSlot::open_explicit(
            path,
            CredentialSlotId::new("primary").unwrap(),
        )
        .unwrap();
        assert!(matches!(
            offline.status(),
            Err(ClaudeNativeAuthError::StoredCredentialFormatRetired)
        ));
    }
}

#[test]
fn persist_and_logout_round_trip_offline() {
    let temp = tempfile::tempdir().unwrap();
    let path = private_store(&temp);
    let manager = manager(path.clone());
    let status = manager
        .persist_test_grant(
            "access-token-value",
            "refresh-token-value",
            3600,
            "user:inference",
        )
        .unwrap();
    assert_eq!(status.status(), CredentialStatus::Ready);
    assert_eq!(manager.enrolled_catalog().unwrap().len(), 6);
    assert_eq!(
        manager.enrolled_catalog().unwrap()[0].exact_id(),
        "claude-sonnet-5"
    );
    assert!(
        !manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-fable-5")
    );
    assert!(
        manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-fable-5-1")
    );
    assert!(
        manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-opus-5")
    );
    assert!(
        manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-opus-5-5")
    );
    assert!(
        manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-sonnet-5-5")
    );
    assert!(
        manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-haiku-5-5")
    );
    assert!(
        !manager
            .enrolled_catalog()
            .unwrap()
            .iter()
            .any(|entry| entry.exact_id() == "claude-haiku-4-5")
    );
    manager.logout_local().unwrap();
    let slot =
        ClaudeOfflineSelectedSlot::open_explicit(path, CredentialSlotId::new("primary").unwrap())
            .unwrap();
    assert_eq!(slot.status().unwrap(), OfflineCredentialLifecycle::Revoked);
}
