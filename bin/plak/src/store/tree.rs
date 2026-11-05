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

/// Names this process's custody host before its first take: the home's
/// stable host id (`<home>/host-id`, created on first use, the identity
/// runners pair with), this machine, and this binary's build id when it is
/// known without hashing the image (an install manifest or the home's
/// identity cache names it), so opening the tree store never pays for a hash.
/// Every process of a home presents the same host id; the custody instance
/// tells them apart, and liveness compares only the machine and the
/// instance. A process binds the host id of the first home it opens a tree
/// store for.
///
/// # Errors
///
/// A host id that cannot be read or created, with its remedy; the process
/// then has no host to take custody as, so its tree store does not open. A
/// process that already took custody as the unbound default `local` (a take
/// through a store opened some other way) keeps it, since the identity is
/// fixed for its life, and says so in the log.
pub(crate) fn bind_custody_host(paths: &PlakPaths) -> Result<(), String> {
    static BOUND: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    if BOUND.get().is_some() {
        return Ok(());
    }
    let file = paths.home().join("host-id");
    let host_id = crate::commands::runner::host_id(paths).map_err(|error| {
        format!(
            "{}; fix or remove {} (a new host id is made at the next use), then pair this \
             home's runners again with `plak runner pair`",
            error.message(),
            file.display()
        )
    })?;
    let bound = plakous_recursive_subagents::tree::bind_custody_host(
        host_id.clone(),
        plakous_recursive_subagents::tree::machine_name(),
        crate::build_identity::known_binary_id().filter(|binary| !binary.is_empty()),
    );
    match bound {
        // Another home's id bound first: the first home this process opens
        // a tree store for names its host.
        Ok(()) => {}
        Err(bound) if bound.host_id != UNBOUND_HOST_ID => {}
        Err(_) => {
            tracing::warn!(
                target: "plak::custody",
                host_id = %host_id,
                "tree custody was taken as `local` before this process bound the home's host id"
            );
        }
    }
    let _ = BOUND.set(());
    Ok(())
}

/// The custody host id a take uses when nothing bound one.
const UNBOUND_HOST_ID: &str = "local";

/// The tree store for this home, opened on its own file.
pub fn open_tree_store(paths: &PlakPaths) -> Result<SqliteTreeStore, String> {
    paths.ensure_layout()?;
    bind_custody_host(paths)?;
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
