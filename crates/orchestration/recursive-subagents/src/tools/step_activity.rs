//! Joined effect work is distinct from the lifetime of retained child owners.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Default)]
pub(super) struct EffectActivity(Arc<AtomicUsize>);

impl EffectActivity {
    pub(super) fn enter(&self) -> EffectGuard {
        self.0.fetch_add(1, Ordering::AcqRel);
        EffectGuard(Arc::clone(&self.0))
    }

    pub(super) fn is_idle(&self) -> bool {
        self.0.load(Ordering::Acquire) == 0
    }
}

pub(super) struct EffectGuard(Arc<AtomicUsize>);

impl Drop for EffectGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
