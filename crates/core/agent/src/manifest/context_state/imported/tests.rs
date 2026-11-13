use super::*;
use plakous_model::{Actor, CompactionBlock, ContentBlock, ConversationItem, Provenance};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn history() -> TestResult<(CanonicalHistory, EntryId)> {
    let entry = EntryId::try_from(uuid::Uuid::from_u128(9))?;
    Ok((
        CanonicalHistory::new(vec![HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![ConversationItem::new(
                entry,
                Actor::Assistant,
                vec![ContentBlock::Compaction(CompactionBlock::new(
                    "committed summary",
                )?)],
                Provenance::ModelOutput,
            )?],
        )?])?,
        entry,
    ))
}

fn context(entropy: u8) -> TestResult<RuntimeContextState> {
    Ok(RuntimeContextState::genesis(
        plakous_model::SessionId::try_from(uuid::Uuid::from_u128(1))?,
        plakous_model::BranchId::try_from(uuid::Uuid::from_u128(2))?,
        [entropy; 32],
    )?)
}

#[test]
fn imported_compaction_recomputes_identities_without_inventing_audit_or_carrying_grants()
-> TestResult {
    let (history, entry) = history()?;
    let source = context(1)?.with_occupancy_anchor(plakous_model::ContextAnchor::new(100, 90))?;
    let facts = source
        .export_compaction_selection(CheckpointId::try_from(uuid::Uuid::from_u128(3))?, &history)?
        .ok_or("missing selection")?;
    assert_eq!(facts.summary_entry, entry);
    assert_eq!(
        facts.source_operation, None,
        "a portable seed has no historical paid-dispatch proof"
    );
    let target = context(2)?.with_imported_compaction(facts.clone(), &history)?;
    assert_eq!(
        target.imported_compaction().ok_or("selection")?.facts(),
        &facts
    );
    assert_ne!(target.affinity().identity(), source.affinity().identity());
    assert!(target.occupancy_anchor().is_none());
    assert!(target.request_anchor().is_none());
    assert!(target.compaction_provenance().is_none());
    target.validate_history(&history)?;
    assert_eq!(RuntimeContextState::decode(&target.encode()?)?, target);
    assert_eq!(
        target.with_occupancy_anchor(None)?.imported_compaction(),
        target.imported_compaction()
    );
    let fork = target.ordinary_fork(
        CheckpointId::try_from(uuid::Uuid::from_u128(5))?,
        target.affinity().session(),
        plakous_model::BranchId::try_from(uuid::Uuid::from_u128(6))?,
        [3; 32],
    )?;
    assert_eq!(fork.imported_compaction(), target.imported_compaction());
    fork.validate_history(&history)?;
    let carried_again = target
        .export_compaction_selection(CheckpointId::try_from(uuid::Uuid::from_u128(8))?, &history)?
        .ok_or("selection")?;
    assert_eq!(carried_again, facts);
    assert_eq!(carried_again.source_checkpoint, facts.source_checkpoint);
    Ok(())
}

#[test]
fn imported_compaction_refuses_missing_summaries_bounds_and_native_hash_drift() -> TestResult {
    let (history, _) = history()?;
    let source = context(1)?;
    let facts = source
        .export_compaction_selection(CheckpointId::try_from(uuid::Uuid::from_u128(3))?, &history)?
        .ok_or("selection")?;
    for bad in [0, u32::MAX] {
        let mut changed = facts.clone();
        changed.prefix_segments = bad;
        assert!(
            context(2)?
                .with_imported_compaction(changed, &history)
                .is_err()
        );
    }
    let mut changed = facts.clone();
    changed.summary_entry = EntryId::try_from(uuid::Uuid::from_u128(99))?;
    assert!(
        context(2)?
            .with_imported_compaction(changed, &history)
            .is_err()
    );
    let mut changed = facts.clone();
    changed.trailing_segments = 1;
    assert!(
        context(2)?
            .with_imported_compaction(changed, &history)
            .is_err()
    );
    let target = context(2)?.with_imported_compaction(facts, &history)?;
    let mut forged = target.imported_compaction().ok_or("selection")?.clone();
    forged.prefix = Digest::sha256(DigestDomain::CanonicalHistory, 1, &[b"foreign"])?;
    assert!(forged.validate_history(&history).is_err());
    assert!(
        source
            .export_compaction_selection(
                CheckpointId::try_from(uuid::Uuid::from_u128(3))?,
                &CanonicalHistory::default(),
            )?
            .is_none()
    );
    Ok(())
}
