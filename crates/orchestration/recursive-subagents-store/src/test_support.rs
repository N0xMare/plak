//! Repository-only tree schema fixture, selected by the product test harness.

use std::sync::atomic::{AtomicBool, Ordering};

pub use crate::admission::test_registry::check_revision_paths;
use crate::admission::{Revision, Schema};

static ENABLED: AtomicBool = AtomicBool::new(false);
const DDL: &str = "CREATE INDEX subagents_revision_fixture ON subagents_schema(fingerprint)";

/// Use the synthetic additive step in this process.
pub fn enable_store_revision_fixture() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub(super) fn schema(base: Schema) -> Schema {
    if ENABLED.load(Ordering::Relaxed) {
        Schema {
            revisions: &[Revision {
                sql: DDL,
                objects: &[("subagents_revision_fixture", DDL)],
                fingerprint: "09cee8161887c4c8f18e5d12fa2a6b858cb5955d959bcb744928b88fa33c9f4d",
            }],
            ..base
        }
    } else {
        base
    }
}
