use super::{
    BTreeMap, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef, CheckpointRow, Connection,
    SessionRef, SessionStoreError, StoredCheckpoint, load_checkpoint_contents,
};

enum Value {
    Ready(StoredCheckpoint),
    Pending {
        branch: SessionRef,
        reference: CheckpointObjectRef,
        bundle: CheckpointObjectBundle,
    },
}

struct Entry {
    value: Value,
    bytes: usize,
    age: u64,
}

pub(super) struct CheckpointCache {
    entries: BTreeMap<CheckpointId, Entry>,
    recency: BTreeMap<u64, CheckpointId>,
    pending: BTreeMap<SessionRef, CheckpointId>,
    #[cfg(test)]
    pub(super) normalizations: usize,
    /// Cold loads: full decodes and verification of a checkpoint's graph.
    #[cfg(test)]
    pub(super) decodes: usize,
    #[cfg(test)]
    pub(super) seed_enabled: bool,
    #[cfg(test)]
    pub(super) fail_normalization: bool,
    bytes: usize,
    limit: usize,
    clock: u64,
}

impl CheckpointCache {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            recency: BTreeMap::new(),
            pending: BTreeMap::new(),
            #[cfg(test)]
            normalizations: 0,
            #[cfg(test)]
            decodes: 0,
            #[cfg(test)]
            seed_enabled: true,
            #[cfg(test)]
            fail_normalization: false,
            bytes: 0,
            limit,
            clock: 0,
        }
    }

    #[cfg(test)]
    pub(super) fn has_pending(&self, branch: SessionRef) -> bool {
        self.pending.contains_key(&branch)
    }

    fn next_age(&mut self) -> u64 {
        if self.clock == u64::MAX {
            self.clear();
        }
        self.clock += 1;
        self.clock
    }

    /// The validated checkpoint `row` names: the retained one when present
    /// (`Corrupt` if its reference differs from the row's), else the verified
    /// cold load, which is then retained. The caller has probed `row` in its
    /// own read snapshot.
    pub(super) fn load(
        &mut self,
        c: &Connection,
        id: CheckpointId,
        row: &CheckpointRow,
    ) -> Result<StoredCheckpoint, SessionStoreError> {
        if let Some(checkpoint) = self.get(id) {
            if checkpoint.reference() != row.reference {
                return Err(SessionStoreError::corrupt());
            }
            return Ok(checkpoint);
        }
        #[cfg(test)]
        {
            self.decodes += 1;
        }
        let checkpoint = load_checkpoint_contents(c, id, row)?;
        self.insert(checkpoint.clone());
        Ok(checkpoint)
    }

    pub(super) fn get(&mut self, id: CheckpointId) -> Option<StoredCheckpoint> {
        let entry = self.entries.remove(&id)?;
        self.recency.remove(&entry.age);
        self.bytes -= entry.bytes;
        match entry.value {
            Value::Ready(checkpoint) => {
                let age = self.next_age();
                self.bytes += entry.bytes;
                self.recency.insert(age, id);
                self.entries.insert(
                    id,
                    Entry {
                        value: Value::Ready(checkpoint.clone()),
                        bytes: entry.bytes,
                        age,
                    },
                );
                Some(checkpoint)
            }
            Value::Pending {
                branch,
                reference,
                bundle,
            } => {
                self.pending.remove(&branch);
                // Failure discards this optional seed; the caller performs a cold load.
                let checkpoint = self.normalize(reference, &bundle)?;
                self.insert(checkpoint.clone());
                Some(checkpoint)
            }
        }
    }

    pub(super) fn seed(
        &mut self,
        branch: SessionRef,
        reference: CheckpointObjectRef,
        bundle: &CheckpointObjectBundle,
        superseded: Option<CheckpointId>,
    ) {
        #[cfg(test)]
        if !self.seed_enabled {
            return;
        }
        if let Some(id) = self.pending.get(&branch).copied() {
            self.remove(id);
        }
        let bytes = pending_charge(bundle);
        let ready_bytes: usize = self
            .entries
            .iter()
            .filter(|(id, entry)| {
                Some(**id) != superseded && matches!(entry.value, Value::Ready(_))
            })
            .map(|(_, entry)| entry.bytes)
            .sum();
        // Only this branch's superseded head may yield its ready slot to a seed.
        if bytes > self.limit.saturating_sub(ready_bytes)
            || matches!(
                self.entries
                    .get(&reference.checkpoint_id())
                    .map(|e| &e.value),
                Some(Value::Ready(_))
            )
        {
            return;
        }
        if let Some(id) = superseded {
            self.remove(id);
        }
        while self.bytes > self.limit.saturating_sub(bytes) {
            let oldest = self.recency.values().copied().find(|id| {
                self.entries
                    .get(id)
                    .is_some_and(|entry| matches!(entry.value, Value::Pending { .. }))
            });
            let Some(oldest) = oldest else {
                return;
            };
            self.remove(oldest);
        }
        self.insert_value(
            reference.checkpoint_id(),
            Value::Pending {
                branch,
                reference,
                bundle: bundle.clone(),
            },
            bytes,
        );
        self.pending.insert(branch, reference.checkpoint_id());
    }

    /// Seeds a fresh fork's target. A target whose pending charge exceeds the
    /// whole budget can never be a pending seed, so it is normalized now and
    /// retained as the ready entry under the over-budget rule, evicting every
    /// other entry including the source head: otherwise its first read would
    /// verify again every object it shares with the source just read. A target
    /// that fits the budget alone but not beside its source head's ready entry
    /// may take that slot, as a commit seed may for the head it supersedes; the
    /// source head stays retained whenever both fit.
    pub(super) fn seed_fork(
        &mut self,
        branch: SessionRef,
        reference: CheckpointObjectRef,
        bundle: &CheckpointObjectBundle,
        source: CheckpointId,
    ) {
        #[cfg(test)]
        if !self.seed_enabled {
            return;
        }
        if pending_charge(bundle) <= self.limit {
            self.seed(branch, reference, bundle, None);
            if !self.pending.contains_key(&branch) {
                self.seed(branch, reference, bundle, Some(source));
            }
            return;
        }
        // Failure leaves nothing retained; the first read performs a cold load.
        if let Some(checkpoint) = self.normalize(reference, bundle) {
            self.insert(checkpoint);
        }
    }

    /// The ready form of a validated publication: its current-root closure
    /// with a rebuilt CAS index, or `None` when the closure cannot be built.
    #[allow(
        clippy::unused_self,
        reason = "test builds count normalizations and can make them fail"
    )]
    fn normalize(
        &mut self,
        reference: CheckpointObjectRef,
        bundle: &CheckpointObjectBundle,
    ) -> Option<StoredCheckpoint> {
        #[cfg(test)]
        {
            self.normalizations += 1;
            if self.fail_normalization {
                return None;
            }
        }
        let closure = bundle.current_root_closure().ok()?;
        StoredCheckpoint::from_store(reference, closure).ok()
    }

    pub(super) fn insert(&mut self, checkpoint: StoredCheckpoint) {
        let id = checkpoint.reference().checkpoint_id();
        let bytes = weight(&checkpoint);
        self.insert_value(id, Value::Ready(checkpoint), bytes);
    }

    fn insert_value(&mut self, id: CheckpointId, value: Value, bytes: usize) {
        self.remove(id);
        // A validated graph is already bounded to 1 GiB of encoded objects.
        // Retain an over-budget head alone rather than reload it on every read.
        while self.bytes > self.limit.saturating_sub(bytes) {
            let Some((&age, &oldest)) = self.recency.first_key_value() else {
                break;
            };
            self.recency.remove(&age);
            self.remove(oldest);
        }
        let age = self.next_age();
        self.bytes += bytes;
        self.recency.insert(age, id);
        self.entries.insert(id, Entry { value, bytes, age });
    }

    pub(super) fn remove(&mut self, id: CheckpointId) {
        if let Some(entry) = self.entries.remove(&id) {
            self.recency.remove(&entry.age);
            self.bytes -= entry.bytes;
            if let Value::Pending { branch, .. } = entry.value {
                self.pending.remove(&branch);
            }
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
        self.pending.clear();
        self.bytes = 0;
        self.clock = 0;
    }
}

pub(super) fn weight(checkpoint: &StoredCheckpoint) -> usize {
    bundle_weight(checkpoint.bundle())
}

/// What a pending seed of `bundle` charges against the budget.
pub(super) fn pending_charge(bundle: &CheckpointObjectBundle) -> usize {
    bundle_weight(bundle).saturating_add(size_of::<(SessionRef, CheckpointId)>())
}

pub(super) fn bundle_weight(bundle: &CheckpointObjectBundle) -> usize {
    // Pending bundles charge all retained ancestry, without walking it.
    // Charge encoded bytes twice to account for both the graph and decoded state,
    // plus the known per-object/entry allocations. This is not a process RSS limit.
    bundle
        .stats()
        .retained_bytes()
        .saturating_mul(2)
        .saturating_add(
            bundle
                .stats()
                .object_count()
                .saturating_mul(size_of::<super::RuntimeObject>()),
        )
        .saturating_add(size_of::<Entry>())
}

#[cfg(test)]
mod tests;
