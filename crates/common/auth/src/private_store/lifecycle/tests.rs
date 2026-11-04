//! Credential-store lifecycle regressions.

use crate::private_store::PrivateCredentialStore;
use crate::private_store::PrivateStoreError;
use crate::private_store::platform;
use crate::private_store::{Fixture, load_raw_for_test};
use crate::{
    AccountBinding, CredentialGeneration, CredentialRecord, CredentialSlotId, CredentialStatus,
    SecretBytes, SubscriptionIntegrationProfileId,
};
use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::fs::symlink;
use std::sync::Arc;
use std::sync::Barrier;
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

#[test]
fn lifecycle_is_monotonic_and_logout_is_a_tombstone() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"one")?)?;
    let loaded = store.load()?.ok_or("missing record")?;
    assert_eq!(
        store.acquire_ready(&loaded)?.payload().expose_secret(),
        b"one"
    );

    store.replace(
        CredentialGeneration::new(1)?,
        &fixture.record(2, CredentialStatus::RefreshRequired, b"two")?,
    )?;
    assert_eq!(
        store.replace(
            CredentialGeneration::new(2)?,
            &fixture.record(2, CredentialStatus::Ready, b"rollback")?,
        ),
        Err(PrivateStoreError::GenerationRollback)
    );

    let revoked_generation = store.logout(CredentialGeneration::new(2)?)?;
    let revoked = store.load()?.ok_or("missing tombstone")?;
    assert_eq!(revoked_generation.get(), 3);
    assert_eq!(revoked.status(), CredentialStatus::Revoked);
    assert!(
        load_raw_for_test(&store)?
            .ok_or("missing tombstone")?
            .payload()
            .is_empty()
    );
    assert_eq!(
        store.replace(
            CredentialGeneration::new(2)?,
            &fixture.record(4, CredentialStatus::Ready, b"stale")?,
        ),
        Err(PrivateStoreError::StaleWriter)
    );
    Ok(())
}

#[test]
fn ready_leases_share_and_block_logout_until_all_drop() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"lease-secret")?)?;
    let fence = store.load()?.ok_or("missing record")?;
    let first = store.acquire_ready(&fence)?;
    let second = fixture.store()?.try_acquire_ready(&fence)?.ok_or("busy")?;
    assert_eq!(fixture.store()?.try_load()?, Some(fence.clone()));
    assert_eq!(first.payload().expose_secret(), b"lease-secret");
    assert!(!format!("{fence:?}").contains("opaque-account"));
    assert!(!format!("{first:?}").contains("lease-secret"));

    let mutation_store = fixture.store()?;
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let _ =
            sender.send(mutation_store.logout(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
            ));
        Ok::<(), PrivateStoreError>(())
    });
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    drop(first);
    assert!(matches!(
        receiver.recv_timeout(Duration::from_millis(50)),
        Err(RecvTimeoutError::Timeout)
    ));
    drop(second);
    // A liveness bound only: generous, so load never trips it.
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(30))??,
        CredentialGeneration::new(2)?
    );
    worker.join().map_err(|_| "logout worker panicked")??;
    Ok(())
}

#[test]
fn nonblocking_fence_read_names_writer_contention() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"fixture-secret")?)?;
    let writer = platform::lock(&store)?;
    assert_eq!(store.try_load(), Err(PrivateStoreError::StoreBusy));
    drop(writer);
    assert!(store.try_load()?.is_some());
    Ok(())
}

#[test]
fn concurrent_initialization_has_one_explicit_winner() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for payload in [b"first".as_slice(), b"second".as_slice()] {
        let store = fixture.store()?;
        let barrier = Arc::clone(&barrier);
        let profile = fixture.profile.clone();
        let account = fixture.account.clone();
        workers.push(thread::spawn(move || {
            let candidate = CredentialRecord::new(
                profile,
                account,
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                CredentialStatus::Ready,
                SecretBytes::new(payload.to_vec()),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            barrier.wait();
            store.initialize_selected(&candidate)
        }));
    }
    barrier.wait();
    let outcomes = workers
        .into_iter()
        .map(|worker| worker.join().map_err(|_| "initialization worker panicked"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, Err(PrivateStoreError::AlreadyInitialized)))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn independent_handles_reject_a_cas_race() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    fixture
        .store()?
        .initialize(&fixture.record(1, CredentialStatus::Ready, b"initial")?)?;
    let first = fixture.store()?;
    let second = fixture.store()?;
    let profile = fixture.profile.clone();
    let account = fixture.account.clone();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for (store, payload) in [(first, b"first".as_slice()), (second, b"second".as_slice())] {
        let barrier = Arc::clone(&barrier);
        let profile = profile.clone();
        let account = account.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            let record = CredentialRecord::new(
                profile,
                account,
                CredentialGeneration::new(2).map_err(|_| PrivateStoreError::InvalidRecord)?,
                CredentialStatus::Ready,
                SecretBytes::new(payload.to_vec()),
            )
            .map_err(|_| PrivateStoreError::InvalidRecord)?;
            store.replace(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
                &record,
            )
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().map_err(|_| "CAS worker panicked"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| **result == Err(PrivateStoreError::StaleWriter))
            .count(),
        1
    );
    assert_eq!(
        fixture
            .store()?
            .load()?
            .ok_or("missing winner")?
            .generation(),
        CredentialGeneration::new(2)?
    );
    Ok(())
}

#[test]
fn selected_slot_switches_accounts_only_through_exact_plus_one()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let slot = CredentialSlotId::new("primary")?;
    let store =
        PrivateCredentialStore::open_slot(&fixture.path, fixture.profile.clone(), slot.clone())?;
    store.initialize_selected(&fixture.record(1, CredentialStatus::Ready, b"account-a")?)?;
    let bytes = fs::read(&fixture.path)?;
    let document: serde_json::Value = serde_json::from_slice(&bytes)?;
    assert_eq!(document["format_version"], 2);
    assert_eq!(document["slot_id"], "primary");
    assert_eq!(document["account_binding"], "opaque-account");

    let account_b = AccountBinding::new("actual-account-b")?;
    let before_switch = store.load()?.ok_or("missing selected account")?;
    let switched_record = CredentialRecord::new(
        fixture.profile.clone(),
        account_b.clone(),
        CredentialGeneration::new(2)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"account-b".to_vec()),
    )?;
    let switched = store.replace_selected_login(&before_switch, switched_record)?;
    assert_eq!(switched.account_binding(), &account_b);
    let jumped = CredentialRecord::new(
        fixture.profile.clone(),
        AccountBinding::new("account-c")?,
        CredentialGeneration::new(4)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"account-c".to_vec()),
    )?;
    assert_eq!(
        store.replace_selected_login(&switched, jumped),
        Err(PrivateStoreError::GenerationNotSequential)
    );

    let claim = store.begin_refresh(&switched)?;
    let wrong_account_completion = fixture.record(4, CredentialStatus::Ready, b"wrong")?;
    assert!(matches!(
        store.complete_refresh(claim.start(), wrong_account_completion),
        Err(PrivateStoreError::ForeignAccount)
    ));
    let unknown = store.load()?.ok_or("claim did not persist")?;
    assert_eq!(unknown.account_binding(), &account_b);
    assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(
        store.logout(unknown.generation())?,
        CredentialGeneration::new(4)?
    );
    let tombstone = store.load()?.ok_or("missing slot tombstone")?;
    assert_eq!(tombstone.account_binding(), &account_b);
    assert_eq!(tombstone.status(), CredentialStatus::Revoked);
    Ok(())
}

#[test]
fn explicit_local_logout_retires_foreign_selected_profile_without_reusing_payload()
-> Result<(), Box<dyn std::error::Error>> {
    for state in [
        CredentialStatus::Ready,
        CredentialStatus::RefreshOutcomeUnknown,
        CredentialStatus::Revoked,
    ] {
        let fixture = Fixture::new()?;
        let slot = CredentialSlotId::new("primary")?;
        let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
        let old =
            PrivateCredentialStore::open_slot(&fixture.path, old_profile.clone(), slot.clone())?;
        old.initialize_selected(&CredentialRecord::new(
            old_profile,
            fixture.account.clone(),
            CredentialGeneration::new(1)?,
            CredentialStatus::Ready,
            SecretBytes::new(b"old-secret".to_vec()),
        )?)?;
        if state == CredentialStatus::RefreshOutcomeUnknown {
            let fence = old.load()?.ok_or("missing old credential")?;
            drop(old.begin_refresh(&fence)?);
        } else if state == CredentialStatus::Revoked {
            old.logout_local()?;
        }
        let current =
            PrivateCredentialStore::open_slot(&fixture.path, fixture.profile.clone(), slot)?;
        assert_eq!(
            current.load().err(),
            Some(PrivateStoreError::ForeignProfile)
        );
        let before = old.load()?.ok_or("missing old credential")?.generation();
        assert_eq!(
            current.logout_local()?,
            crate::OfflineCredentialLifecycle::Revoked
        );
        let retired = current.load()?.ok_or("missing retired credential")?;
        assert_eq!(retired.generation(), before.next()?);
        assert_eq!(retired.status(), CredentialStatus::Revoked);
        assert_eq!(retired.profile_id(), &fixture.profile);
        assert_eq!(
            load_raw_for_test(&current)?
                .ok_or("missing retired record")?
                .payload()
                .expose_secret(),
            b""
        );
        assert_eq!(old.load().err(), Some(PrivateStoreError::ForeignProfile));
        assert_eq!(
            old.logout(before).err(),
            Some(PrivateStoreError::ForeignProfile)
        );
        assert_eq!(
            current.logout_local()?,
            crate::OfflineCredentialLifecycle::Revoked
        );
        assert_eq!(
            current.load()?.ok_or("missing tombstone")?.generation(),
            retired.generation()
        );
    }
    Ok(())
}

#[test]
fn local_logout_refuses_foreign_slot_malformed_record_and_exhausted_generation()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
    let foreign_slot = PrivateCredentialStore::open_slot(
        &fixture.path,
        old_profile.clone(),
        CredentialSlotId::new("other-slot")?,
    )?;
    foreign_slot.initialize_selected(&CredentialRecord::new(
        old_profile,
        fixture.account.clone(),
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"old-secret".to_vec()),
    )?)?;
    let current = PrivateCredentialStore::open_slot(
        &fixture.path,
        fixture.profile.clone(),
        CredentialSlotId::new("primary")?,
    )?;
    let original = fs::read(&fixture.path)?;
    assert_eq!(
        current.logout_local().err(),
        Some(PrivateStoreError::ForeignSlot)
    );
    assert_eq!(fs::read(&fixture.path)?, original);

    fs::write(&fixture.path, b"invalid")?;
    fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o600))?;
    assert_eq!(
        current.logout_local().err(),
        Some(PrivateStoreError::MalformedRecord)
    );
    assert_eq!(fs::read(&fixture.path)?, b"invalid");

    fs::remove_file(&fixture.path)?;
    current.initialize_selected(&CredentialRecord::new(
        fixture.profile.clone(),
        fixture.account.clone(),
        CredentialGeneration::new(u64::MAX)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"current-secret".to_vec()),
    )?)?;
    let original = fs::read(&fixture.path)?;
    assert_eq!(
        current.logout_local().err(),
        Some(PrivateStoreError::GenerationExhausted)
    );
    assert_eq!(fs::read(&fixture.path)?, original);
    Ok(())
}

#[test]
fn local_logout_never_retires_linked_foreign_profile_record()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let slot = CredentialSlotId::new("primary")?;
    let old_profile = SubscriptionIntegrationProfileId::new("previous-profile")?;
    let old = PrivateCredentialStore::open_slot(&fixture.path, old_profile.clone(), slot.clone())?;
    old.initialize_selected(&CredentialRecord::new(
        old_profile,
        fixture.account.clone(),
        CredentialGeneration::new(1)?,
        CredentialStatus::Ready,
        SecretBytes::new(b"old-secret".to_vec()),
    )?)?;
    let current = PrivateCredentialStore::open_slot(&fixture.path, fixture.profile.clone(), slot)?;
    let displaced = fixture.directory.path().join("displaced-record");
    fs::rename(&fixture.path, &displaced)?;
    let original = fs::read(&displaced)?;

    symlink(&displaced, &fixture.path)?;
    assert_eq!(
        current.logout_local().err(),
        Some(PrivateStoreError::UnsafeRecord)
    );
    assert_eq!(fs::read(&displaced)?, original);
    fs::remove_file(&fixture.path)?;

    fs::hard_link(&displaced, &fixture.path)?;
    assert_eq!(
        current.logout_local().err(),
        Some(PrivateStoreError::UnsafeRecord)
    );
    assert_eq!(fs::read(&displaced)?, original);
    Ok(())
}
