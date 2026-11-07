//! Account-local directory lifecycle and writer-lock regressions.

use super::*;
use plakous_auth::{
    AccountBinding, CredentialGeneration, CredentialRecord, CredentialStatus,
    PrivateCredentialStore, SecretBytes, SubscriptionIntegrationProfileId,
};

const PROVIDERS: [NativeProvider; 3] = [
    NativeProvider::Codex,
    NativeProvider::Claude,
    NativeProvider::Xai,
];

#[tokio::test]
async fn named_inventory_is_local_and_keeps_verticals_separate() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    let native = subscriptions(paths);
    assert_eq!(
        native.named_accounts(NativeProvider::Claude).await?,
        [] as [String; 0]
    );
    assert!(!root.path().join("state").exists());
    let claude = native
        .for_named_account(NativeProvider::Claude, "work")?
        .create_account_for_login(NativeProvider::Claude)
        .await?;
    let grok = native
        .for_named_account(NativeProvider::Xai, "personal")?
        .create_account_for_login(NativeProvider::Xai)
        .await?;
    assert_eq!(
        native.named_accounts(NativeProvider::Claude).await?,
        ["work"]
    );
    assert_eq!(
        native.named_accounts(NativeProvider::Xai).await?,
        ["personal"]
    );
    assert_eq!(
        native.named_accounts(NativeProvider::Codex).await?,
        [] as [String; 0]
    );
    assert!(claude.account_exists(NativeProvider::Claude).await?);
    assert!(claude.named_accounts(NativeProvider::Xai).await.is_err());
    assert_eq!(
        claude
            .upstream_account_fingerprint(NativeProvider::Claude)
            .await?,
        None
    );
    drop((claude, grok));
    assert_eq!(
        native.named_accounts(NativeProvider::Claude).await?,
        [] as [String; 0]
    );
    Ok(())
}

fn subscriptions(paths: SubscriptionPaths) -> NativeSubscriptions {
    NativeSubscriptions::new(paths).with_clock(Arc::new(plakous_model::FixedClock::new(
        Timestamp::new(test_now(), 0).unwrap(),
    )))
}

#[tokio::test]
async fn named_native_paths_preserve_defaults_and_reads_never_create_accounts()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let native = subscriptions(paths.clone());
        assert_eq!(
            paths.provider_record(provider),
            root.path()
                .join("state/plak/subscriptions")
                .join(provider.as_str())
                .join("credential.json")
        );
        assert_eq!(native.state.slot_id()?.as_str(), PRIMARY_SLOT);
        let named = native.for_named_account(provider, "work")?;
        let vertical = if provider == NativeProvider::Xai {
            "grok"
        } else {
            provider.as_str()
        };
        assert_eq!(
            named.state.provider_record(provider),
            root.path()
                .join("state/plak/subscriptions/accounts")
                .join(vertical)
                .join("work/credential.json")
        );
        assert_eq!(named.state.slot_id()?.as_str(), "named-work");
        assert_eq!(
            named.status(provider).await?.credential(),
            NativeAuthStatus::SignedOut
        );
        assert_eq!(
            named.refresh(provider).await,
            Err(NativeSubscriptionError::CredentialUnavailable)
        );
        assert!(!root.path().join("state").exists());
        for invalid in [
            "default",
            "",
            "../work",
            "work/team",
            "Work",
            "a@b",
            "work ",
        ] {
            assert!(native.for_named_account(provider, invalid).is_err());
        }
        let other = PROVIDERS
            .into_iter()
            .find(|other| *other != provider)
            .unwrap();
        assert_eq!(
            named.status(other).await,
            Err(NativeSubscriptionError::AuthenticationBinding)
        );
    }
    Ok(())
}

#[tokio::test]
async fn abandoned_named_login_removes_only_a_new_directory() -> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let native = subscriptions(paths.clone()).for_named_account(provider, "work")?;
        let created = native.create_account_for_login(provider).await?;
        let directory = created.state.paths.layout_root();
        let notice = NativeSubscriptions::notice(provider)?;
        created
            .acknowledge_exact(provider, notice.notice_revision())
            .await?;
        assert!(directory.is_dir());
        drop(created);
        assert!(!directory.exists());
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let existing = native.create_account_for_login(provider).await?;
        drop(existing);
        assert!(directory.is_dir());
        native.remove_signed_out_account(provider).await?;
        assert!(!directory.exists());
    }
    Ok(())
}

#[tokio::test]
async fn abandoned_creation_cleanup_waits_off_executor_for_a_busy_reader()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    let provider = NativeProvider::Codex;
    let selected = subscriptions(paths).for_named_account(provider, "work")?;
    let creator = selected.create_account_for_login(provider).await?;
    let directory = creator.state.paths.layout_root();
    let lock = selected.acquire_compatibility_lock(provider).await?;
    drop(creator);
    assert!(directory.is_dir());
    lock.verify()?;
    drop(lock);
    tokio::time::timeout(Duration::from_secs(10), async {
        while directory.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test]
async fn abandoned_creator_preserves_a_published_record_without_consent_changes()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let creator = subscriptions(paths.clone())
            .for_named_account(provider, "work")?
            .create_account_for_login(provider)
            .await?;
        let path = creator.state.provider_record(provider);
        let consent = stored_acceptance_generation(&creator.state.paths, provider)?;
        let profile = SubscriptionIntegrationProfileId::new(
            NativeSubscriptions::notice(provider)?.profile_id(),
        )?;
        let store = PrivateCredentialStore::open_slot(
            path.clone(),
            profile.clone(),
            creator.state.slot_id()?,
        )?;
        store.initialize_selected(&record(profile, 1)?)?;
        let bytes = fs::read(&path)?;
        assert!(!creator.state.paths.login_recovery_path(provider).exists());
        assert_eq!(
            stored_acceptance_generation(&creator.state.paths, provider)?,
            consent
        );
        drop(store);
        drop(creator);
        assert!(path.parent().unwrap().is_dir());
        assert_eq!(fs::read(path)?, bytes);
    }
    Ok(())
}

#[tokio::test]
async fn an_abandoned_creator_cannot_remove_a_later_writer_or_recreated_name()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let native = subscriptions(paths.clone()).for_named_account(provider, "work")?;
        let creator = native.create_account_for_login(provider).await?;
        let directory = creator.state.paths.layout_root();
        let later = native.create_account_for_login(provider).await?;
        later
            .acknowledge_exact(
                provider,
                NativeSubscriptions::notice(provider)?.notice_revision(),
            )
            .await?;
        drop(creator);
        drop(later);
        assert!(directory.is_dir());
        native.remove_signed_out_account(provider).await?;
        let old = native.create_account_for_login(provider).await?;
        let retired = directory.with_file_name("retired");
        fs::rename(&directory, &retired)?;
        let recreated = native.create_account_for_login(provider).await?;
        drop(old);
        assert!(directory.is_dir());
        drop(recreated);
        assert!(!directory.exists());
        fs::remove_dir_all(retired)?;
    }
    Ok(())
}

#[tokio::test]
async fn named_removal_refuses_defaults_live_credentials_and_busy_writers()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let defaults = subscriptions(paths.clone());
        assert_eq!(
            defaults.remove_signed_out_account(provider).await,
            Err(NativeSubscriptionError::DefaultAccountNotRemovable)
        );
        let native = defaults
            .for_named_account(provider, "work")?
            .create_account_for_login(provider)
            .await?;
        let lock = native.acquire_compatibility_lock(provider).await?;
        assert_eq!(
            native.remove_signed_out_account(provider).await,
            Err(NativeSubscriptionError::AccountBusy)
        );
        lock.verify()?;
        drop(lock);
        let profile = SubscriptionIntegrationProfileId::new(
            NativeSubscriptions::notice(provider)?.profile_id(),
        )?;
        let store = PrivateCredentialStore::open_slot(
            native.state.provider_record(provider),
            profile.clone(),
            native.state.slot_id()?,
        )?;
        store.initialize_selected(&record(profile, 1)?)?;
        assert_eq!(
            native.remove_signed_out_account(provider).await,
            Err(NativeSubscriptionError::AccountSignedIn)
        );
        assert!(native.state.provider_record(provider).is_file());
        let path = native.state.provider_record(provider);
        let ready = fs::read(&path)?;
        for (field, value) in [
            ("status", serde_json::json!("refresh_required")),
            ("status", serde_json::json!("refresh_outcome_unknown")),
            ("format_version", serde_json::json!(0)),
            ("format_version", serde_json::json!(999)),
            ("profile_id", serde_json::json!("foreign-profile")),
            ("slot_id", serde_json::json!("named-other")),
        ] {
            let mut document: serde_json::Value = serde_json::from_slice(&ready)?;
            document[field] = value;
            let bytes = serde_json::to_vec(&document)?;
            fs::write(&path, &bytes)?;
            assert_eq!(
                native.remove_signed_out_account(provider).await,
                Err(NativeSubscriptionError::AccountSignedIn),
                "{provider:?}: {field}"
            );
            assert_eq!(fs::read(&path)?, bytes);
        }
        fs::write(&path, &ready)?;
        store.logout_local()?;
        assert_eq!(store.status()?, Some(CredentialStatus::Revoked));
        drop(store);
        native.remove_signed_out_account(provider).await?;
        assert!(!path.exists());
    }
    Ok(())
}

fn record(
    profile: SubscriptionIntegrationProfileId,
    generation: u64,
) -> Result<CredentialRecord, plakous_auth::CredentialValueError> {
    CredentialRecord::new(
        profile,
        AccountBinding::new("test-upstream")?,
        CredentialGeneration::new(generation)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"opaque-test-credential".to_vec()),
    )
}

#[tokio::test]
async fn sibling_refresh_claims_and_quarantine_never_cross_account_boundaries()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let native = subscriptions(paths.clone());
        let work = native
            .for_named_account(provider, "work")?
            .create_account_for_login(provider)
            .await?;
        let other = native
            .for_named_account(provider, "other")?
            .create_account_for_login(provider)
            .await?;
        let profile = SubscriptionIntegrationProfileId::new(
            NativeSubscriptions::notice(provider)?.profile_id(),
        )?;
        let open = |selected: &NativeSubscriptions| {
            PrivateCredentialStore::open_slot(
                selected.state.provider_record(provider),
                profile.clone(),
                selected.state.slot_id().unwrap(),
            )
        };
        let work_store = open(&work)?;
        let other_store = open(&other)?;
        work_store.initialize_selected(&record(profile.clone(), 1)?)?;
        other_store.initialize_selected(&record(profile.clone(), 1)?)?;
        let work_fence = work_store.load()?.unwrap();
        let other_fence = other_store.load()?.unwrap();
        let work_lock = work.acquire_compatibility_lock(provider).await?;
        let other_lock = other
            .try_acquire_compatibility_lock(provider)
            .await?
            .ok_or("sibling writer lock was shared")?;
        let work_claim = work_store.begin_refresh(&work_fence)?.start();
        assert_eq!(
            other_store.load()?.unwrap().status(),
            CredentialStatus::Ready
        );
        assert!(other_store.try_acquire_ready(&other_fence)?.is_some());
        let other_claim = other_store.begin_refresh(&other_fence)?.start();
        drop(work_claim);
        other_store.complete_refresh(other_claim, record(profile.clone(), 3)?)?;
        assert_eq!(
            work_store.load()?.unwrap().status(),
            CredentialStatus::RefreshOutcomeUnknown
        );
        assert_eq!(
            other_store.load()?.unwrap().status(),
            CredentialStatus::Ready
        );
        assert_eq!(other_store.load()?.unwrap().generation().get(), 3);
        work_lock.verify()?;
        other_lock.verify()?;
    }
    Ok(())
}

#[tokio::test]
async fn named_compatibility_lock_allows_same_process_login_and_refresh_commits()
-> Result<(), Box<dyn Error>> {
    let root = tempfile::tempdir()?;
    let paths = SubscriptionPaths::from_xdg_state_home(root.path().join("state"))?;
    for provider in PROVIDERS {
        let native = subscriptions(paths.clone())
            .for_named_account(provider, "work")?
            .create_account_for_login(provider)
            .await?;
        let notice = NativeSubscriptions::notice(provider)?;
        native
            .acknowledge_exact(provider, notice.notice_revision())
            .await?;
        let accepted = native.state.load_acceptance_exact(notice, test_now())?;
        let consent = PendingCompatibilityConsent {
            state: native.state.clone(),
            notice,
            exact_receipt: accepted.exact_bytes,
            generation: accepted.generation,
        };
        let state = native.state.clone();
        complete_pending_with_consent(
            &consent,
            || Ok(test_now()),
            move || async move {
                run_blocking(move || {
                    let path = state.provider_record(provider);
                    let (directory, _) = open_private_parent(&path)?;
                    rustix::fs::flock(&directory, FlockOperation::NonBlockingLockExclusive)
                        .map_err(|_| NativeSubscriptionError::AccountBusy)?;
                    rustix::fs::flock(&directory, FlockOperation::Unlock)
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let profile = SubscriptionIntegrationProfileId::new(notice.profile_id())
                        .map_err(|_| NativeSubscriptionError::Internal)?;
                    let store =
                        PrivateCredentialStore::open_slot(path, profile.clone(), state.slot_id()?)
                            .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
                    store
                        .initialize_selected(
                            &record(profile, 1).map_err(|_| NativeSubscriptionError::Internal)?,
                        )
                        .map_err(|_| NativeSubscriptionError::UnsafeStorage)?;
                    Ok(NativeAuthStatus::Ready)
                })
                .await?
            },
        )
        .await?;
        let lock = native.acquire_compatibility_lock(provider).await?;
        let profile = SubscriptionIntegrationProfileId::new(notice.profile_id())?;
        let store = PrivateCredentialStore::open_slot(
            native.state.provider_record(provider),
            profile.clone(),
            native.state.slot_id()?,
        )?;
        let before = store.load()?.ok_or("missing login record")?;
        let claim = store.begin_refresh(&before)?.start();
        store.complete_refresh(claim, record(profile, 3)?)?;
        assert_eq!(
            store
                .load()?
                .ok_or("missing refreshed record")?
                .generation()
                .get(),
            3
        );
        lock.verify()?;
        drop(lock);
    }
    Ok(())
}
