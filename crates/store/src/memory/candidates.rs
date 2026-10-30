use super::{
    BTreeSet, Candidate, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef,
    CheckpointRecord, MemoryStoreLimits, ObjectSweep, RuntimeCheckpoint, RuntimeObject, SessionRef,
    SessionStoreError, StoreAdmission, StoreCapacityBound, StoreInvalidReason, StoreState, admit,
    capacity,
};
use std::sync::Arc;

pub(super) fn publication_candidate(
    bundle: &CheckpointObjectBundle,
) -> Result<Candidate, SessionStoreError> {
    let checkpoint = validated_checkpoint(bundle)?;
    Ok(Candidate {
        reference: bundle_reference(bundle),
        base_root: bundle.base_root(),
        checkpoint,
        bundle: bundle.clone(),
        objects: bundle.supplied_objects().to_vec(),
    })
}

pub(super) fn validated_checkpoint(
    bundle: &CheckpointObjectBundle,
) -> Result<Arc<RuntimeCheckpoint>, SessionStoreError> {
    let checkpoint = bundle
        .validated_checkpoint()
        .map_err(|_| SessionStoreError::Invalid(StoreInvalidReason::CheckpointObjectBundle))?;
    let reference = bundle_reference(bundle);
    if checkpoint.manifest().topology().checkpoint_id != reference.checkpoint_id()
        || checkpoint.integrity() != reference.integrity()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointObjectBinding,
        ));
    }
    Ok(checkpoint)
}

pub(super) fn bundle_reference(bundle: &CheckpointObjectBundle) -> CheckpointObjectRef {
    CheckpointObjectRef::from_store(
        bundle.checkpoint_id(),
        bundle.root(),
        bundle.checkpoint_integrity(),
    )
}

pub(super) fn install_candidate(
    state: &mut StoreState,
    candidate: Candidate,
    limits: MemoryStoreLimits,
    admission: StoreAdmission,
) -> Result<CheckpointObjectBundle, SessionStoreError> {
    if let Some(existing) = state.checkpoints.get(&candidate.reference.checkpoint_id()) {
        return if existing.reference == candidate.reference {
            Ok(candidate.bundle)
        } else {
            Err(SessionStoreError::IdentityConflict)
        };
    }
    admit(
        StoreCapacityBound::Checkpoints,
        state.checkpoints.len().saturating_add(1),
        limits.checkpoints.get(),
        admission,
    )?;
    if candidate
        .base_root
        .is_some_and(|base| !state.objects.contains_key(&base))
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingBase,
        ));
    }
    if candidate
        .objects
        .windows(2)
        .any(|pair| pair[0].address() >= pair[1].address())
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::NoncanonicalCheckpointObjects,
        ));
    }
    let mut missing_objects = 0_usize;
    let mut missing_bytes = 0_usize;
    let supplied = candidate
        .objects
        .iter()
        .map(RuntimeObject::address)
        .collect::<BTreeSet<_>>();
    if !state.objects.contains_key(&candidate.reference.root())
        && !supplied.contains(&candidate.reference.root())
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingRoot,
        ));
    }
    for object in &candidate.objects {
        if object
            .children()
            .iter()
            .any(|child| !state.objects.contains_key(child) && !supplied.contains(child))
        {
            return Err(SessionStoreError::Corrupt(
                plakous_agent::internal::StoreIntegrityReason::MissingChild,
            ));
        }
        if let Some(existing) = state.objects.get(&object.address()) {
            if !same_object(existing, object) {
                return Err(SessionStoreError::corrupt());
            }
        } else {
            missing_objects = missing_objects
                .checked_add(1)
                .ok_or_else(|| capacity(StoreCapacityBound::Objects, state.objects.len(), 0))?;
            missing_bytes = missing_bytes
                .checked_add(object.retained_bytes())
                .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
        }
    }
    let object_count = state
        .objects
        .len()
        .checked_add(missing_objects)
        .ok_or_else(|| capacity(StoreCapacityBound::Objects, state.objects.len(), 0))?;
    let object_bytes = state
        .object_bytes
        .checked_add(missing_bytes)
        .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
    admit(
        StoreCapacityBound::Objects,
        object_count,
        limits.objects.get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::ObjectBytes,
        object_bytes,
        limits.object_bytes.get(),
        admission,
    )?;
    for object in candidate.objects {
        state.objects.entry(object.address()).or_insert(object);
    }
    state.object_bytes = object_bytes;
    state.checkpoints.insert(
        candidate.reference.checkpoint_id(),
        CheckpointRecord {
            reference: candidate.reference,
            checkpoint: candidate.checkpoint,
        },
    );
    Ok(candidate.bundle)
}

/// Checkpoints of `selector` that a create, a fork or another branch names.
pub(super) fn pinned_checkpoints(
    state: &StoreState,
    selector: SessionRef,
) -> BTreeSet<CheckpointId> {
    let mut pinned = state
        .creates
        .values()
        .map(|receipt| receipt.operation().initial().checkpoint_id())
        .chain(state.forks.values().flat_map(|receipt| {
            [
                receipt.source().checkpoint().checkpoint_id(),
                receipt.target().checkpoint().checkpoint_id(),
            ]
        }))
        .collect::<BTreeSet<_>>();
    for record in state.checkpoints.values() {
        let topology = record.checkpoint.manifest().topology();
        if let Some(fork) = topology.fork {
            pinned.insert(fork.checkpoint_id);
        }
        if (topology.session_id != selector.session_id()
            || topology.branch_id != selector.branch_id())
            && let Some(parent) = topology.parent_checkpoint_id
        {
            pinned.insert(parent);
        }
    }
    pinned
}

/// Whether retention reclaimed this checkpoint's commit receipt.
pub(super) fn commit_history_pruned(state: &StoreState, checkpoint: CheckpointId) -> bool {
    if state.pruned.contains_key(&checkpoint) {
        return true;
    }
    state.checkpoints.get(&checkpoint).is_some_and(|record| {
        let topology = record.checkpoint.manifest().topology();
        topology.branch_revision > 0
            && state
                .horizons
                .get(&SessionRef::new(topology.session_id, topology.branch_id))
                .is_some_and(|horizon| topology.branch_revision <= horizon.get())
    })
}

/// Removes every object no retained checkpoint reaches; returns the removed
/// objects and their retained bytes.
pub(super) fn sweep_unreachable(
    state: &mut StoreState,
) -> Result<(usize, usize), SessionStoreError> {
    let sweep = plan_object_sweep(state, &BTreeSet::new())?;
    let objects = sweep.objects.len();
    for address in sweep.objects {
        state.objects.remove(&address);
    }
    state.object_bytes = sweep.remaining_bytes;
    Ok((objects, sweep.bytes))
}

pub(super) fn plan_object_sweep(
    state: &StoreState,
    removed_checkpoints: &BTreeSet<CheckpointId>,
) -> Result<ObjectSweep, SessionStoreError> {
    let mut reachable = BTreeSet::new();
    let mut pending = state
        .checkpoints
        .iter()
        .filter(|(id, _)| !removed_checkpoints.contains(id))
        .map(|(_, record)| record.reference.root())
        .collect::<Vec<_>>();
    while let Some(address) = pending.pop() {
        if !reachable.insert(address) {
            continue;
        }
        let object = state
            .objects
            .get(&address)
            .ok_or(SessionStoreError::corrupt())?;
        pending.extend(object.children().iter().copied());
    }
    let swept = state
        .objects
        .iter()
        .filter_map(|(address, object)| {
            (!reachable.contains(address)).then_some((*address, object.retained_bytes()))
        })
        .collect::<Vec<_>>();
    let bytes = swept
        .iter()
        .try_fold(0_usize, |sum, (_, bytes)| sum.checked_add(*bytes))
        .ok_or_else(|| capacity(StoreCapacityBound::ObjectBytes, state.object_bytes, 0))?;
    let remaining_bytes = state
        .object_bytes
        .checked_sub(bytes)
        .ok_or(SessionStoreError::corrupt())?;
    Ok(ObjectSweep {
        objects: swept.into_iter().map(|(address, _)| address).collect(),
        bytes,
        remaining_bytes,
    })
}

pub(super) fn retain_head_bundle(state: &mut StoreState, bundle: CheckpointObjectBundle) {
    state.hot_bundles.insert(bundle.checkpoint_id(), bundle);
    prune_hot_bundles(state);
}

pub(super) fn prune_hot_bundles(state: &mut StoreState) {
    let heads = state
        .sessions
        .values()
        .filter(|session| !session.head.is_archived())
        .flat_map(|session| session.branches.values())
        .map(|branch| branch.head.checkpoint().checkpoint_id())
        .collect::<BTreeSet<_>>();
    state
        .hot_bundles
        .retain(|checkpoint_id, _| heads.contains(checkpoint_id));
}

fn same_object(left: &RuntimeObject, right: &RuntimeObject) -> bool {
    left.address() == right.address()
        && left.canonical_bytes() == right.canonical_bytes()
        && left.children() == right.children()
}
