use super::{
    BTreeMap, CheckpointId, CheckpointObjectBundle, CheckpointObjectRef, SessionRef,
    StoredCheckpoint,
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
                #[cfg(test)]
                {
                    self.normalizations += 1;
                    if self.fail_normalization {
                        return None;
                    }
                }
                // Failure discards this optional seed; the caller performs a cold load.
                let closure = bundle.current_root_closure().ok()?;
                let checkpoint = StoredCheckpoint::from_store(reference, closure).ok()?;
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
    ) {
        #[cfg(test)]
        if !self.seed_enabled {
            return;
        }
        if let Some(id) = self.pending.get(&branch).copied() {
            self.remove(id);
        }
        let bytes = bundle_weight(bundle).saturating_add(size_of::<(SessionRef, CheckpointId)>());
        let ready_bytes: usize = self
            .entries
            .values()
            .filter(|entry| matches!(entry.value, Value::Ready(_)))
            .map(|entry| entry.bytes)
            .sum();
        // Optional publication seeds must never displace validated read results.
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
mod tests {
    use super::super::{bundle_ref, gc_tests};
    use super::*;

    #[test]
    fn pending_seed_never_displaces_ready_entries() -> Result<(), Box<dyn std::error::Error>> {
        let a = gc_tests::seed(1_190_000)?;
        let b = gc_tests::seed(1_200_000)?;
        let ready = StoredCheckpoint::from_store(
            bundle_ref(a.initial()),
            a.initial().current_root_closure()?,
        )?;
        let ready_weight = weight(&ready);
        let pending_weight = bundle_weight(b.initial()) + size_of::<(SessionRef, CheckpointId)>();
        let mut cache = CheckpointCache::new(ready_weight + pending_weight - 1);
        cache.insert(ready.clone());
        cache.seed(b.session(), bundle_ref(b.initial()), b.initial());
        assert!(!cache.has_pending(b.session()));
        assert_eq!(cache.bytes, ready_weight);
        assert_eq!(cache.get(ready.reference().checkpoint_id()), Some(ready));
        assert_eq!(cache.normalizations, 0);
        Ok(())
    }

    #[test]
    fn failed_normalization_releases_all_accounted_bytes() -> Result<(), Box<dyn std::error::Error>>
    {
        let request = gc_tests::seed(1_180_000)?;
        let reference = bundle_ref(request.initial());
        let mut cache = CheckpointCache::new(usize::MAX);
        cache.seed(request.session(), reference, request.initial());
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
        cache.seed(a.session(), a_ref, a.initial());
        assert_eq!(cache.bytes, a_weight);
        cache.seed(b.session(), b_ref, b.initial());
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
        cache.seed(a.session(), a_ref, a.initial());
        cache.remove(a_ref.checkpoint_id());
        assert!(!cache.has_pending(a.session()));
        assert_eq!(cache.bytes, 0);
        cache.seed(b.session(), b_ref, b.initial());
        cache.clear();
        assert!(!cache.has_pending(b.session()));
        assert_eq!(cache.bytes, 0);
        assert_eq!(cache.normalizations, 1);
        Ok(())
    }
}
