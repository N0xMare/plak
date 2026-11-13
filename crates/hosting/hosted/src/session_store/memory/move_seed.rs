//! Match SQLite's once-only semantic install without holding a driver or process grant.
use super::{MemoryStore, RecordStoreError, SessionViews, bind, commit_locked};
use crate::sessions::host_move::{DormantSession, DormantSessionDocument};

pub(super) fn claims(
    store: &MemoryStore,
    id: &str,
) -> Result<Vec<crate::sessions::host_move::MoveIdentityClaim>, String> {
    use crate::sessions::host_move::{MoveIdentityClaim, MoveIdentityKind};
    let state = store
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .bindings
        .iter()
        .filter(|(_, binding)| binding.owner == id)
        .map(|(identity, binding)| {
            let kind =
                MoveIdentityKind::parse(binding.kind).ok_or("invalid move identity domain")?;
            Ok(MoveIdentityClaim {
                identity: identity.clone(),
                kind,
                owner: binding.owner.clone(),
                binding: binding.digest.clone(),
                disposition: binding
                    .terminal
                    .as_ref()
                    .map(|(disposition, _)| disposition.clone()),
                outcome: binding
                    .terminal
                    .as_ref()
                    .and_then(|(_, outcome)| outcome.clone()),
            })
        })
        .collect()
}

pub(super) fn base(store: &MemoryStore, id: &str) -> Result<Option<SessionViews>, String> {
    let state = store
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .move_seeds
        .get(id)
        .map(|(_, _, text)| {
            let document: DormantSessionDocument =
                serde_json::from_str(text).map_err(|_| "invalid semantic move base")?;
            let seed = document.into_cut()?;
            seed.validate_identity_claims()?;
            Ok(seed.views().clone())
        })
        .transpose()
}

pub(super) fn plan_authority(store: &MemoryStore, id: &str) -> Result<Option<String>, String> {
    let state = store
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state
        .move_seeds
        .get(id)
        .map(|(_, _, text)| {
            let document: DormantSessionDocument =
                serde_json::from_str(text).map_err(|_| "invalid semantic move base")?;
            let authority = document.plan_authority().map(str::to_owned);
            document.into_cut()?.validate_identity_claims()?;
            Ok(authority)
        })
        .transpose()
        .map(Option::flatten)
}

pub(super) fn install(
    store: &MemoryStore,
    operation: &str,
    digest: &str,
    seed: &DormantSession,
) -> Result<(), RecordStoreError> {
    seed.validate_identity_claims()?;
    let document = seed.document()?;
    let id = document.id().to_owned();
    let text =
        serde_json::to_string(&document).map_err(|_| "could not encode semantic move seed")?;
    let mut state = store
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((prior_operation, prior_digest, prior_text)) = state.move_seeds.get(&id) {
        return if prior_operation == operation && prior_digest == digest && prior_text == &text {
            Ok(())
        } else {
            Err("semantic move seed conflicts with a reserved product identity".into())
        };
    }
    let mut next = state.clone();
    let (mut record, views, intents) = document.into_cut()?.into_parts();
    if next.bindings.contains_key(&record.create_operation_id) {
        return Err("semantic move cannot recreate an already used product identity".into());
    }
    record.transcript.clear();
    commit_locked(&mut next, &mut record)?;
    for claim in seed.identity_claims() {
        bind(
            &mut next.bindings,
            claim.kind.as_str(),
            &claim.identity,
            &claim.owner,
            claim.binding.clone(),
            claim
                .disposition
                .as_ref()
                .map(|disposition| (disposition.clone(), claim.outcome.clone())),
        )?;
    }
    next.intents.insert(id.clone(), intents);
    next.views.insert(id.clone(), views);
    next.move_seeds
        .insert(id, (operation.into(), digest.into(), text));
    *state = next;
    Ok(())
}
