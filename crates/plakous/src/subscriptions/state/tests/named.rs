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
        assert!(native.remove_signed_out_account(provider).await.is_err());
        assert!(native.state.provider_record(provider).is_file());
        drop(store);
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
