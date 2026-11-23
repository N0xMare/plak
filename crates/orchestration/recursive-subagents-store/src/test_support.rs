//! Repository-only tree schema fixture, selected by the product test harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{
    LazyLock, Mutex, PoisonError,
    atomic::{AtomicBool, Ordering},
};

pub use crate::admission::test_registry::check_revision_paths;
use crate::admission::{Revision, Schema};

static ENABLED: AtomicBool = AtomicBool::new(false);
const DDL: &str = "CREATE INDEX subagents_revision_fixture ON subagents_schema(fingerprint)";

static TREE_LOADS: LazyLock<Mutex<BTreeMap<PathBuf, usize>>> = LazyLock::new(Mutex::default);

pub(super) fn record_tree_load(path: &Path) {
    *TREE_LOADS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .entry(path.to_owned())
        .or_default() += 1;
}

/// Number of whole-tree reads against this test database.
#[must_use]
pub fn tree_load_count(path: &Path) -> usize {
    TREE_LOADS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(path)
        .copied()
        .unwrap_or(0)
}

/// Use the synthetic additive step in this process.
pub fn enable_store_revision_fixture() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(super) fn schema(base: Schema) -> Schema {
    if ENABLED.load(Ordering::Relaxed) {
        Schema {
            revisions: &[
                crate::database::RETIREMENT_REVISION,
                Revision {
                    sql: DDL,
                    objects: &[("subagents_revision_fixture", DDL)],
                    fingerprint: "c5724458b476c2e24d662fb726937128f0f270c7df434b0c817bdc5ed02069fe",
                },
            ],
            ..base
        }
    } else {
        base
    }
}
