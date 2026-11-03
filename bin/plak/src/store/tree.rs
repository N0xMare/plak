//! The product's adapter onto the agent tree's own store.
//!
//! The tree's rows live in
//! `subagents.sqlite3`, with their own schema version and their own DDL
//! fingerprint, and `plakous-recursive-subagents-store` owns the file and the
//! codecs. What is left here is the product opening it by `PlakPaths`, and the
//! `String`-typed convenience the CLI flows read.

use std::sync::Arc;

use plakous_recursive_subagents::tree::{ProductAgentTree, TreeStore, TreeStoreError};
use plakous_recursive_subagents_store::SqliteTreeStore;

use crate::home::paths::PlakPaths;
use plakous_hosted::runtime::TreeWriteError;
use plakous_hosted::{CliFailure, CliResult};

/// Names this process's custody host before its first take: the local
/// per-user host on this machine, with this binary's build id when it is
/// known without hashing the image (an install manifest or the home's
/// identity cache names it), so opening the tree store never pays for a hash.
fn bind_custody_host() {
    static BOUND: std::sync::Once = std::sync::Once::new();
    BOUND.call_once(|| {
        let _ = plakous_recursive_subagents::tree::bind_custody_host(
            "local",
            plakous_recursive_subagents::tree::machine_name(),
            crate::build_identity::known_binary_id().filter(|binary| !binary.is_empty()),
        );
    });
}

/// The tree store for this home, opened on its own file.
pub fn open_tree_store(paths: &PlakPaths) -> Result<SqliteTreeStore, String> {
    bind_custody_host();
    paths.ensure_layout()?;
    SqliteTreeStore::open(&paths.subagents_database()).map_err(|error| error.message().to_owned())
}

/// The tree store as the port, for a caller that holds it for a runtime's
/// life.
pub fn tree_store(paths: &PlakPaths) -> Result<Arc<dyn TreeStore>, String> {
    Ok(Arc::new(open_tree_store(paths)?))
}

/// Every persisted tree id. A headless `plak config set` fences a roster or
/// orchestration edit against all of them, because no session names one.
pub fn tree_ids(paths: &PlakPaths) -> Result<Vec<String>, String> {
    open_tree_store(paths)?
        .ids()
        .map_err(|error| error.detail.clone())
}

/// Whether the tree is persisted at all.
pub fn tree_exists(paths: &PlakPaths, tree_id: &str) -> Result<bool, String> {
    open_tree_store(paths)?
        .exists(tree_id)
        .map_err(|error| error.detail.clone())
}

/// The tree's rows with the reopen law applied.
pub fn load_tree(paths: &PlakPaths, tree_id: &str) -> Result<Option<ProductAgentTree>, String> {
    Ok(open_tree_store(paths)?
        .load(tree_id)
        .map_err(|error| error.detail.clone())?
        .map(ProductAgentTree::reopen))
}

/// A whole-tree publish through the port's compare-and-swap.
pub fn save_tree(paths: &PlakPaths, tree: &mut ProductAgentTree) -> Result<(), TreeWriteError> {
    let store = open_tree_store(paths).map_err(TreeStoreError::unavailable)?;
    let acknowledged = store.save_in_custody(tree)?;
    tree.acknowledge_revision(acknowledged);
    Ok(())
}

/// Plak's tree store: `subagents.sqlite3` under one product home.
///
/// The product side of `agent::TreeStoreFactory`; the roots hold the port and
/// never learn where the file is.
pub(crate) struct ProductTreeStoreFactory {
    paths: PlakPaths,
}

impl ProductTreeStoreFactory {
    pub(crate) const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }
}

impl plakous_hosted::agent::TreeStoreFactory for ProductTreeStoreFactory {
    fn open(&self) -> CliResult<Arc<dyn TreeStore>> {
        tree_store(&self.paths).map_err(CliFailure::new)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
