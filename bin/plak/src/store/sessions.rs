//! Where a product session record is persisted.
//!
//! The record itself is `plakous_hosted::sessions`, re-exported here so a
//! consumer's `crate::store::sessions::…` path does not change twice.
//! Path-based helpers below are fixture access only; product commands use the
//! composition root's `SessionRecordStore`.

use std::fs;
use std::path::{Path, PathBuf};

pub use plakous_hosted::sessions::*;

#[cfg(any(test, feature = "test-support"))]
#[cfg(any(test, feature = "test-support"))]
use crate::home::paths::PlakPaths;

#[cfg(any(test, feature = "test-support"))]
pub fn save_session(paths: &PlakPaths, session: &mut SessionRecord) -> Result<(), String> {
    with_product_store(paths, |store| store.commit(session).map_err(String::from))
}

#[cfg(any(test, feature = "test-support"))]
pub fn load_session(paths: &PlakPaths, id: &str) -> Result<SessionRecord, String> {
    canonical_product_session_id(id)?;
    with_product_store(paths, |store| store.load(id))
}

#[cfg(any(test, feature = "test-support"))]
pub fn list_sessions_for_cwd(paths: &PlakPaths, cwd: &Path) -> Result<Vec<SessionRecord>, String> {
    with_product_store(paths, |store| store.list_for_workspace(cwd))
}

#[cfg(any(test, feature = "test-support"))]
fn with_product_store<T>(
    paths: &PlakPaths,
    operation: impl FnOnce(&mut crate::store::ProductStore) -> Result<T, String>,
) -> Result<T, String> {
    let mut store = crate::store::ProductStore::open(paths).map_err(|error| error.to_string())?;
    operation(&mut store)
}

#[must_use]
pub fn workspace_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
