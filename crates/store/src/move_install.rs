//! Re-encode semantic checkpoints in dependency order using only the current codec.
use plakous_agent::internal::host_move::MoveInstall;
use plakous_agent::internal::{CheckpointObjectBundle, RuntimeCheckpointCodec, SessionStoreError};
use std::collections::BTreeMap;

pub(crate) fn bundles(
    install: &MoveInstall,
) -> Result<Vec<CheckpointObjectBundle>, SessionStoreError> {
    let mut pending = install
        .sessions()
        .iter()
        .flat_map(|s| &s.checkpoints)
        .map(|c| (c.id(), c))
        .collect::<BTreeMap<_, _>>();
    let mut encoded = BTreeMap::new();
    let mut result = Vec::new();
    while !pending.is_empty() {
        let id = pending
            .iter()
            .find(|(_, c)| {
                c.manifest()
                    .topology()
                    .parent_checkpoint_id
                    .is_none_or(|p| !pending.contains_key(&p))
            })
            .map(|(id, _)| *id)
            .ok_or(SessionStoreError::IdentityConflict)?;
        let checkpoint = pending.remove(&id).ok_or(SessionStoreError::corrupt())?;
        let parent = checkpoint
            .manifest()
            .topology()
            .parent_checkpoint_id
            .and_then(|p| encoded.get(&p));
        let bundle = RuntimeCheckpointCodec::bundle(checkpoint, parent)
            .map_err(|_| SessionStoreError::corrupt())?;
        encoded.insert(id, bundle.clone());
        result.push(bundle);
    }
    Ok(result)
}
