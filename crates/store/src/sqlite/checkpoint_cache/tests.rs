use super::super::{bundle_ref, gc_tests};
use super::*;

#[test]
fn pending_seed_never_displaces_ready_entries() -> Result<(), Box<dyn std::error::Error>> {
    let a = gc_tests::seed(1_190_000)?;
    let b = gc_tests::seed(1_200_000)?;
    let ready =
        StoredCheckpoint::from_store(bundle_ref(a.initial()), a.initial().current_root_closure()?)?;
    let ready_weight = weight(&ready);
    let pending_weight = bundle_weight(b.initial()) + size_of::<(SessionRef, CheckpointId)>();
    let mut cache = CheckpointCache::new(ready_weight + pending_weight - 1);
    cache.insert(ready.clone());
    cache.seed(b.session(), bundle_ref(b.initial()), b.initial(), None);
    assert!(!cache.has_pending(b.session()));
    assert_eq!(cache.bytes, ready_weight);
    assert_eq!(cache.get(ready.reference().checkpoint_id()), Some(ready));
    assert_eq!(cache.normalizations, 0);
    Ok(())
}

#[test]
fn a_mid_size_fork_target_takes_only_its_source_heads_slot()
-> Result<(), Box<dyn std::error::Error>> {
    let source = gc_tests::seed(1_210_000)?;
    let other = gc_tests::seed(1_220_000)?;
    let target = gc_tests::seed(1_230_000)?;
    let ready =
        |bundle: &CheckpointObjectBundle| -> Result<StoredCheckpoint, Box<dyn std::error::Error>> {
            Ok(StoredCheckpoint::from_store(
                bundle_ref(bundle),
                bundle.current_root_closure()?,
            )?)
        };
    let source_ready = ready(source.initial())?;
    let source_id = source_ready.reference().checkpoint_id();
    let target_ref = bundle_ref(target.initial());
    let charge = pending_charge(target.initial());
    let source_weight = weight(&source_ready);

    // Fits beside the source head: the head stays retained.
    let mut roomy = CheckpointCache::new(charge + source_weight);
    roomy.insert(source_ready.clone());
    roomy.seed_fork(target.session(), target_ref, target.initial(), source_id);
    assert!(roomy.has_pending(target.session()));
    assert_eq!(roomy.bytes, charge + source_weight);
    assert_eq!(roomy.get(source_id), Some(source_ready.clone()));

    // Fits alone but not beside it: the target takes the source's slot.
    let mut tight = CheckpointCache::new(charge + source_weight - 1);
    tight.insert(source_ready.clone());
    tight.seed_fork(target.session(), target_ref, target.initial(), source_id);
    assert!(tight.has_pending(target.session()));
    assert_eq!(tight.bytes, charge);
    assert!(tight.get(source_id).is_none());
    let normalized = tight
        .get(target_ref.checkpoint_id())
        .ok_or("pending fork target")?;
    assert_eq!(normalized, ready(target.initial())?);
    assert_eq!(tight.normalizations, 1);

    // Another branch's ready entry stays protected: nothing is seeded and
    // the source head is not given up for a seed that still cannot fit.
    let other_ready = ready(other.initial())?;
    assert!(charge > source_weight);
    let mut protected = CheckpointCache::new(weight(&other_ready) + source_weight);
    protected.insert(other_ready.clone());
    protected.insert(source_ready.clone());
    let before = protected.bytes;
    protected.seed_fork(target.session(), target_ref, target.initial(), source_id);
    assert!(!protected.has_pending(target.session()));
    assert_eq!(protected.bytes, before);
    Ok(())
}

#[test]
fn failed_normalization_releases_all_accounted_bytes() -> Result<(), Box<dyn std::error::Error>> {
    let request = gc_tests::seed(1_180_000)?;
    let reference = bundle_ref(request.initial());
    let mut cache = CheckpointCache::new(usize::MAX);
    cache.seed(request.session(), reference, request.initial(), None);
    cache.fail_normalization = true;
    assert!(cache.bytes > 0);
    assert!(cache.get(reference.checkpoint_id()).is_none());
    assert_eq!(cache.bytes, 0);
    assert!(!cache.has_pending(request.session()));
    assert_eq!(cache.normalizations, 1);
    assert!(cache.get(reference.checkpoint_id()).is_none());
    assert_eq!(cache.normalizations, 1);
    Ok(())
}

#[test]
fn pending_entries_share_the_budget_and_lru_without_normalizing()
-> Result<(), Box<dyn std::error::Error>> {
    let a = gc_tests::seed(950_000)?;
    let b = gc_tests::seed(960_000)?;
    let a_ref = bundle_ref(a.initial());
    let b_ref = bundle_ref(b.initial());
    let a_weight = bundle_weight(a.initial()) + size_of::<(SessionRef, CheckpointId)>();
    let b_weight = bundle_weight(b.initial()) + size_of::<(SessionRef, CheckpointId)>();
    let mut cache = CheckpointCache::new(a_weight.max(b_weight));
    cache.seed(a.session(), a_ref, a.initial(), None);
    assert_eq!(cache.bytes, a_weight);
    cache.seed(b.session(), b_ref, b.initial(), None);
    assert_eq!(cache.bytes, b_weight);
    assert!(!cache.has_pending(a.session()));
    assert!(cache.get(a_ref.checkpoint_id()).is_none());
    assert_eq!(cache.normalizations, 0);
    let normalized = cache
        .get(b_ref.checkpoint_id())
        .ok_or("pending cache hit")?;
    assert!(normalized.bundle().base_root().is_none());
    assert_eq!(cache.bytes, weight(&normalized));
    assert!(!cache.has_pending(b.session()));
    assert_eq!(cache.normalizations, 1);
    cache.clear();
    cache.seed(a.session(), a_ref, a.initial(), None);
    cache.remove(a_ref.checkpoint_id());
    assert!(!cache.has_pending(a.session()));
    assert_eq!(cache.bytes, 0);
    cache.seed(b.session(), b_ref, b.initial(), None);
    cache.clear();
    assert!(!cache.has_pending(b.session()));
    assert_eq!(cache.bytes, 0);
    assert_eq!(cache.normalizations, 1);
    Ok(())
}
