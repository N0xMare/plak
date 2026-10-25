//! Credential-store refresh regressions.

use crate::private_store::PrivateStoreError;
use crate::private_store::platform;
use crate::private_store::{Fixture, load_raw_for_test};
use crate::{CredentialFence, CredentialGeneration, CredentialStatus};
use std::fs;
use std::sync::Arc;
use std::sync::Barrier;
use std::thread;

#[test]
fn refresh_intent_is_crash_safe_quarantine() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    let generation = CredentialGeneration::new(1)?;
    store.initialize(&fixture.record(1, CredentialStatus::RefreshRequired, b"refresh")?)?;

    let intent = store.mark_refresh_intent(generation)?;
    let claim_generation = CredentialGeneration::new(2)?;
    assert_eq!(intent.generation(), claim_generation);
    assert_eq!(intent.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert!(matches!(
        store.mark_refresh_intent(generation),
        Err(PrivateStoreError::StaleWriter)
    ));
    assert!(matches!(
        store.mark_refresh_intent(claim_generation),
        Err(PrivateStoreError::CredentialQuarantined)
    ));
    assert_eq!(
        store.replace(
            generation,
            &fixture.record(3, CredentialStatus::Ready, b"stale-success")?,
        ),
        Err(PrivateStoreError::StaleWriter)
    );
    drop(store);

    let restarted = fixture.store()?;
    let quarantined = restarted.load()?.ok_or("missing quarantine")?;
    assert_eq!(quarantined.generation(), claim_generation);
    assert_eq!(
        quarantined.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert_eq!(
        restarted
            .quarantine_refresh_outcome(claim_generation)?
            .status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    assert_eq!(
        restarted
            .quarantine_refresh_outcome(claim_generation)?
            .generation(),
        claim_generation
    );
    Ok(())
}

#[test]
fn generic_cas_cannot_clear_a_dropped_claim_after_restart() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready: CredentialFence = store.load()?.ok_or("missing ready fence")?;
    let claim = store.begin_refresh(&ready)?;
    let unknown = claim.fence();
    drop(claim);
    drop(store);

    let restarted = fixture.store()?;
    let replacement = fixture.record(3, CredentialStatus::Ready, b"new")?;
    assert_eq!(
        restarted.replace(unknown.generation(), &replacement),
        Err(PrivateStoreError::CredentialQuarantined)
    );
    assert_eq!(
        restarted.replace(unknown.generation(), &replacement),
        Err(PrivateStoreError::CredentialQuarantined)
    );
    assert_eq!(restarted.load()?.ok_or("quarantine was lost")?, unknown);
    Ok(())
}

#[test]
fn dropping_a_started_claim_during_unwind_leaves_unknown() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready fence")?;
    let claim = store.begin_refresh(&ready)?;
    let unknown = claim.fence();
    let unwind = std::panic::catch_unwind(move || {
        let started = claim.start();
        assert_eq!(started.payload().expose_secret(), b"old");
        panic!("simulated provider panic");
    });
    assert!(unwind.is_err());
    drop(store);
    assert_eq!(
        fixture.store()?.load()?.ok_or("quarantine was lost")?,
        unknown
    );
    Ok(())
}

#[test]
fn concurrent_refresh_intent_has_one_generation_claimant() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    fixture.store()?.initialize(&fixture.record(
        1,
        CredentialStatus::RefreshRequired,
        b"refresh",
    )?)?;
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for store in [fixture.store()?, fixture.store()?] {
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            store.mark_refresh_intent(
                CredentialGeneration::new(1).map_err(|_| PrivateStoreError::InvalidRecord)?,
            )
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().map_err(|_| "refresh claimant panicked"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    // The loser either observed the winner's claim (stale generation) or
    // found the execution lock owned (busy, nothing written); both are
    // local facts and neither is a second claimant.
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(PrivateStoreError::StaleWriter | PrivateStoreError::StoreBusy)
            ))
            .count(),
        1
    );
    let claimed = fixture.store()?.load()?.ok_or("missing claim")?;
    assert_eq!(claimed.generation(), CredentialGeneration::new(2)?);
    assert_eq!(claimed.status(), CredentialStatus::RefreshOutcomeUnknown);
    Ok(())
}

#[test]
fn refresh_claim_in_flight_probe_tracks_the_claim_without_writing()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"probe-secret")?)?;
    let execution_lock = store.path().with_file_name(&store.execution_lock_name);
    let _ = fs::remove_file(&execution_lock);
    assert!(!store.refresh_claim_in_flight()?);
    assert!(
        !execution_lock.exists(),
        "the probe must not create a lock file"
    );
    let fence = store.load()?.ok_or("missing ready")?;
    let claim = fixture.store()?.begin_refresh(&fence)?.start();
    let unknown = store.load()?.ok_or("missing unknown")?;
    assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert!(store.refresh_claim_in_flight()?);
    assert!(!store.refresh_outcome_settled(&unknown)?);
    assert_eq!(store.load()?, Some(unknown.clone()));
    drop(claim);
    assert!(!store.refresh_claim_in_flight()?);
    assert!(store.refresh_outcome_settled(&unknown)?);
    assert!(!store.refresh_outcome_settled(&fence)?);
    assert_eq!(store.load()?, Some(unknown));
    Ok(())
}

#[test]
fn offline_lifecycle_reads_a_live_claim_as_refreshing_not_quarantined()
-> Result<(), Box<dyn std::error::Error>> {
    use crate::OfflineCredentialLifecycle as Lifecycle;
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"lifecycle-secret")?)?;
    assert_eq!(store.offline_lifecycle()?, Lifecycle::Ready);
    let fence = store.load()?.ok_or("missing ready")?;
    let claim = fixture.store()?.begin_refresh(&fence)?.start();
    assert_eq!(store.offline_lifecycle()?, Lifecycle::Refreshing);
    fixture.store()?.complete_refresh(
        claim,
        fixture.record(3, CredentialStatus::Ready, b"lifecycle-rotated")?,
    )?;
    assert_eq!(store.offline_lifecycle()?, Lifecycle::Ready);
    let fence = store.load()?.ok_or("missing rotated")?;
    drop(fixture.store()?.begin_refresh(&fence)?.start());
    assert_eq!(store.offline_lifecycle()?, Lifecycle::Quarantined);
    Ok(())
}

#[test]
fn try_restore_unknown_to_ready_never_waits_and_restores_only_the_exact_fence()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"restore-secret")?)?;
    let ready = store.load()?.ok_or("missing ready")?;
    let claim = fixture.store()?.begin_refresh(&ready)?.start();
    let unknown = claim.fence();
    assert_eq!(store.try_restore_unknown_to_ready(&unknown)?, None);
    assert_eq!(store.load()?, Some(unknown.clone()));
    drop(claim);
    assert!(matches!(
        store.try_restore_unknown_to_ready(&ready),
        Err(PrivateStoreError::FenceMismatch)
    ));
    let restored = store
        .try_restore_unknown_to_ready(&unknown)?
        .ok_or("uncontended restore")?;
    assert_eq!(restored.status(), CredentialStatus::Ready);
    assert_eq!(restored.generation(), unknown.generation().next()?);
    assert_eq!(store.load()?, Some(restored));
    Ok(())
}

#[test]
fn refresh_claim_completion_restore_drop_and_privacy() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old-secret")?)?;
    let ready = store.load()?.ok_or("missing ready")?;
    let claim = store.begin_refresh(&ready)?;
    assert_eq!(claim.previous_generation(), CredentialGeneration::new(1)?);
    assert_eq!(claim.previous_status(), CredentialStatus::Ready);
    assert_eq!(claim.payload().expose_secret(), b"old-secret");
    assert!(!format!("{claim:?}").contains("old-secret"));
    let claim = claim.start();
    assert_eq!(claim.payload().expose_secret(), b"old-secret");
    assert!(!format!("{claim:?}").contains("old-secret"));
    let completed = store.complete_refresh(
        claim,
        fixture.record(3, CredentialStatus::Ready, b"new-secret")?,
    )?;
    assert_eq!(completed.generation(), CredentialGeneration::new(3)?);
    assert_eq!(
        store.acquire_ready(&completed)?.payload().expose_secret(),
        b"new-secret"
    );

    store.replace(
        CredentialGeneration::new(3)?,
        &fixture.record(4, CredentialStatus::RefreshRequired, b"restore-secret")?,
    )?;
    let refresh_required = store.load()?.ok_or("missing refresh record")?;
    let restore_claim = store.begin_refresh(&refresh_required)?;
    let restored = store.restore_refresh_definitely_not_started(restore_claim)?;
    assert_eq!(restored.generation(), CredentialGeneration::new(6)?);
    let restored_record = store.load()?.ok_or("missing restored record")?;
    assert_eq!(restored_record.status(), CredentialStatus::RefreshRequired);
    assert_eq!(
        load_raw_for_test(&store)?
            .ok_or("missing restored record")?
            .payload()
            .expose_secret(),
        b"restore-secret"
    );

    let dropped_claim = store.begin_refresh(&restored_record)?;
    let dropped_fence = dropped_claim.fence();
    drop(dropped_claim);
    let quarantined = store.load()?.ok_or("missing dropped claim")?;
    assert_eq!(quarantined, dropped_fence);
    assert_eq!(
        quarantined.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    Ok(())
}

#[test]
fn begin_refresh_reserves_unknown_and_completion_generations_before_writing()
-> Result<(), Box<dyn std::error::Error>> {
    for generation in [u64::MAX - 1, u64::MAX] {
        let fixture = Fixture::new()?;
        let store = fixture.store()?;
        store.initialize(&fixture.record(
            generation,
            CredentialStatus::RefreshRequired,
            b"boundary-old",
        )?)?;
        let before = store.load()?.ok_or("missing boundary record")?;
        assert_eq!(
            store.begin_refresh(&before).err(),
            Some(PrivateStoreError::GenerationExhausted)
        );
        let unchanged = load_raw_for_test(&store)?.ok_or("boundary record was lost")?;
        assert_eq!(unchanged.fence(), before);
        assert_eq!(unchanged.status(), CredentialStatus::RefreshRequired);
        assert_eq!(unchanged.payload().expose_secret(), b"boundary-old");
    }

    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(
        u64::MAX - 2,
        CredentialStatus::RefreshRequired,
        b"last-refresh",
    )?)?;
    let before = store.load()?.ok_or("missing final refresh record")?;
    let claim = store.begin_refresh(&before)?;
    assert_eq!(claim.fence().generation().get(), u64::MAX - 1);
    let completed = store.complete_refresh(
        claim.start(),
        fixture.record(u64::MAX, CredentialStatus::Ready, b"last-ready")?,
    )?;
    assert_eq!(completed.generation().get(), u64::MAX);
    Ok(())
}

#[test]
fn complete_refresh_rejects_skipped_generation_and_leaves_unknown()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(11, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready record")?;
    let claim = store.begin_refresh(&ready)?;
    let unknown = claim.fence();
    assert_eq!(
        store.complete_refresh(
            claim.start(),
            fixture.record(14, CredentialStatus::Ready, b"skipped")?,
        ),
        Err(PrivateStoreError::GenerationNotSequential)
    );
    let unchanged = load_raw_for_test(&store)?.ok_or("missing quarantine")?;
    assert_eq!(unchanged.fence(), unknown);
    assert_eq!(unchanged.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(unchanged.payload().expose_secret(), b"old");
    Ok(())
}

#[test]
fn begin_refresh_rejects_unknown_by_default() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready record")?;
    let claim = store.begin_refresh(&ready)?;
    drop(claim);
    let unknown = store.load()?.ok_or("missing unknown record")?;
    assert_eq!(unknown.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(
        store.begin_refresh(&unknown).err(),
        Some(PrivateStoreError::CredentialQuarantined)
    );
    Ok(())
}

#[test]
fn begin_refresh_reclaiming_unknown_claims_from_unknown() -> Result<(), Box<dyn std::error::Error>>
{
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready record")?;
    let claim = store.begin_refresh(&ready)?;
    let unknown = claim.fence();
    drop(claim);
    let reclaim = store.begin_refresh_reclaiming_unknown(&unknown)?;
    assert_eq!(reclaim.fence().generation().get(), 3);
    assert_eq!(reclaim.payload().expose_secret(), b"old");
    let loaded = store.load()?.ok_or("missing reclaimed unknown")?;
    assert_eq!(loaded.status(), CredentialStatus::RefreshOutcomeUnknown);
    assert_eq!(loaded.generation().get(), 3);
    Ok(())
}

#[test]
fn restore_unknown_to_ready_writes_ready_next_generation_same_payload()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new()?;
    let store = fixture.store()?;
    store.initialize(&fixture.record(1, CredentialStatus::Ready, b"old")?)?;
    let ready = store.load()?.ok_or("missing ready record")?;
    let claim = store.begin_refresh(&ready)?;
    let unknown = claim.fence();
    drop(claim);
    let restored = store.restore_unknown_to_ready(&unknown)?;
    assert_eq!(restored.status(), CredentialStatus::Ready);
    assert_eq!(restored.generation().get(), 3);
    let loaded = store.load()?.ok_or("missing restored ready")?;
    assert_eq!(loaded, restored);
    let raw = load_raw_for_test(&store)?.ok_or("missing restored record")?;
    assert_eq!(raw.payload().expose_secret(), b"old");
    Ok(())
}

#[test]
fn stale_and_wrong_refresh_claims_cannot_clear_unknown() -> Result<(), Box<dyn std::error::Error>> {
    let first = Fixture::new()?;
    let second = Fixture::new()?;
    for fixture in [&first, &second] {
        fixture
            .store()?
            .initialize(&fixture.record(1, CredentialStatus::Ready, b"same-old")?)?;
    }
    let first_store = first.store()?;
    let second_store = second.store()?;
    let first_fence = first_store.load()?.ok_or("missing first")?;
    let second_fence = second_store.load()?.ok_or("missing second")?;
    let wrong_claim = first_store.begin_refresh(&first_fence)?;
    let second_claim = second_store.begin_refresh(&second_fence)?;
    assert!(matches!(
        second_store.complete_refresh(
            wrong_claim.start(),
            second.record(3, CredentialStatus::Ready, b"wrong")?,
        ),
        Err(PrivateStoreError::RefreshClaimMismatch)
    ));
    assert_eq!(
        second_store.load()?.ok_or("second lost")?.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );

    // A live claim owns the private directory, so no writer can replace
    // the record through the API; inject the changed Unknown directly to
    // prove the claim's own record verification still refuses it.
    platform::write_record(
        &second_store,
        &second.record(3, CredentialStatus::RefreshOutcomeUnknown, b"other-old")?,
    )?;
    assert!(matches!(
        second_store.complete_refresh(
            second_claim.start(),
            second.record(3, CredentialStatus::Ready, b"new")?,
        ),
        Err(PrivateStoreError::RefreshClaimMismatch)
    ));
    let still_unknown = second_store.load()?.ok_or("stale claim cleared")?;
    assert_eq!(still_unknown.generation(), CredentialGeneration::new(3)?);
    assert_eq!(
        still_unknown.status(),
        CredentialStatus::RefreshOutcomeUnknown
    );
    Ok(())
}
