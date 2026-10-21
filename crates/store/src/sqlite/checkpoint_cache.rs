use super::{BTreeMap, CheckpointId, StoredCheckpoint};

struct Entry {
    checkpoint: StoredCheckpoint,
    bytes: usize,
    age: u64,
}

pub(super) struct CheckpointCache {
    entries: BTreeMap<CheckpointId, Entry>,
    recency: BTreeMap<u64, CheckpointId>,
    bytes: usize,
    limit: usize,
    clock: u64,
}

impl CheckpointCache {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            recency: BTreeMap::new(),
            bytes: 0,
            limit,
            clock: 0,
        }
    }

    fn next_age(&mut self) -> u64 {
        if self.clock == u64::MAX {
            self.clear();
        }
        self.clock += 1;
        self.clock
    }

    pub(super) fn get(&mut self, id: CheckpointId) -> Option<StoredCheckpoint> {
        let age = self.next_age();
        let entry = self.entries.get_mut(&id)?;
        self.recency.remove(&entry.age);
        entry.age = age;
        self.recency.insert(age, id);
        Some(entry.checkpoint.clone())
    }

    pub(super) fn insert(&mut self, checkpoint: StoredCheckpoint) {
        let id = checkpoint.reference().checkpoint_id();
        self.remove(id);
        let bytes = weight(&checkpoint);
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
        self.entries.insert(
            id,
            Entry {
                checkpoint,
                bytes,
                age,
            },
        );
    }

    pub(super) fn remove(&mut self, id: CheckpointId) {
        if let Some(entry) = self.entries.remove(&id) {
            self.recency.remove(&entry.age);
            self.bytes -= entry.bytes;
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.recency.clear();
        self.bytes = 0;
        self.clock = 0;
    }
}

pub(super) fn weight(checkpoint: &StoredCheckpoint) -> usize {
    // Charge encoded bytes twice to account for both the graph and decoded state,
    // plus the known per-object/entry allocations. This is not a process RSS limit.
    checkpoint
        .bundle()
        .stats()
        .retained_bytes()
        .saturating_mul(2)
        .saturating_add(
            checkpoint
                .bundle()
                .stats()
                .object_count()
                .saturating_mul(size_of::<super::RuntimeObject>()),
        )
        .saturating_add(size_of::<Entry>())
}
