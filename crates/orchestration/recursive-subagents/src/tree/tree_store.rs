//! The tree's persistence port.
//!
//! One durable tree per root session, saved whole: the revision is tree-wide,
//! so the atom is the tree. `save` is a compare-and-swap on that revision —
//! the tree carries the revision the store last acknowledged
//! (`stored_revision`), and a store publishes only over exactly that
//! revision, so two handles of one tree cannot overwrite each other — and
//! `load` hands back raw rows
//! that [`ProductAgentTree::reopen`] applies the reopen law to, so the law
//! lives in the tree, not in SQL. The SQLite implementation lives beside the
//! product store today and becomes `plakous-recursive-subagents-store` with
//! its own `subagents.sqlite3` at the crate cut; [`MemoryTreeStore`] is the
//! in-process implementation the conformance suite runs alongside it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use plakous_host_protocol::topology::CandidateId;

use super::{ProductAgentTree, TopologyError, TopologyErrorCode};

/// Why a [`TreeStore`] call failed. The three kinds are distinguished because
/// they need different answers: retry, fail closed, or reload and re-apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeStoreErrorKind {
    /// The store could not be opened, read, or written.
    Unavailable,
    /// The store holds a revision of this tree other than the one this
    /// handle last acknowledged: another writer published in between, so
    /// the in-memory copy is stale and must not overwrite it. `stored` is
    /// what the store holds (0 when the row is gone), `offered` the revision
    /// the refused save carried.
    StaleRevision {
        /// The revision the store holds now; 0 when the row is gone.
        stored: u64,
        /// The revision the refused save carried.
        offered: u64,
    },
    /// Rows came back that do not decode as a tree.
    Corrupt,
}

/// One store failure: the kind a caller decides on, and a sentence naming
/// what actually happened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeStoreError {
    /// Which failure this is.
    pub kind: TreeStoreErrorKind,
    /// The operator-facing detail; for a stale revision it names both.
    pub detail: String,
}

impl TreeStoreError {
    /// The store could not be reached at all.
    #[must_use]
    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            kind: TreeStoreErrorKind::Unavailable,
            detail: detail.into(),
        }
    }

    /// Rows came back that do not decode as a tree; fail closed rather than
    /// reopen a tree the store cannot reproduce.
    #[must_use]
    pub fn corrupt(detail: impl Into<String>) -> Self {
        Self {
            kind: TreeStoreErrorKind::Corrupt,
            detail: detail.into(),
        }
    }

    /// Another handle published in between: the caller must reload and
    /// re-apply rather than overwrite.
    #[must_use]
    pub fn stale(stored: u64, offered: u64) -> Self {
        Self {
            kind: TreeStoreErrorKind::StaleRevision { stored, offered },
            detail: format!(
                "the store holds revision {stored} of this tree, not the revision this handle \
                 loaded; refusing to overwrite it with revision {offered}"
            ),
        }
    }
}

impl std::fmt::Display for TreeStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.detail)
    }
}

impl From<TreeStoreError> for TopologyError {
    fn from(error: TreeStoreError) -> Self {
        Self::new(TopologyErrorCode::OperationRecoveryRequired, error.detail)
    }
}

/// A tree as its rows came back: no reopen law applied yet. Only
/// [`ProductAgentTree::reopen`] turns one into a live tree, so a caller
/// cannot forget the law.
#[derive(Debug)]
pub struct PersistedTree(pub(super) ProductAgentTree);

impl PersistedTree {
    /// Builds the rows a store read back. Exact reconstruction: nothing is
    /// derived here that the reopen law owns.
    #[allow(clippy::too_many_arguments, reason = "exact persisted tree rows")]
    #[must_use]
    pub fn from_rows(
        tree_id: String,
        root_agent_id: String,
        revision: u64,
        host_generation: u64,
        state: super::TreeAdmissionState,
        writer_mode: crate::protocol::WriterMode,
        selected_tuple: String,
        launch_checkout: std::path::PathBuf,
        snapshot: crate::epoch::OrchestrationSnapshot,
        nodes: BTreeMap<String, super::NodeRecord>,
        grants: BTreeMap<String, crate::protocol::TopologyGrant>,
        attempts: BTreeMap<crate::protocol::AttemptId, super::AttemptRecord>,
        candidates: BTreeMap<CandidateId, super::CandidateRecord>,
        workspaces: BTreeMap<String, super::WorkspaceRecord>,
        messages: BTreeMap<String, super::MailRecord>,
        operations: BTreeMap<String, super::OperationRecord>,
    ) -> Self {
        Self(ProductAgentTree::from_persisted(
            tree_id,
            root_agent_id,
            revision,
            host_generation,
            state,
            writer_mode,
            selected_tuple,
            launch_checkout,
            snapshot,
            nodes,
            grants,
            attempts,
            candidates,
            workspaces,
            messages,
            operations,
        ))
    }

    /// Attaches the `agent_commands` and `agent_feed_cursors` rows a store
    /// read back.
    #[must_use]
    pub fn with_feed_rows(
        mut self,
        commands: BTreeMap<String, super::AgentCommand>,
        feed_cursors: BTreeMap<String, u64>,
    ) -> Self {
        self.0.commands = commands;
        self.0.feed_cursors = feed_cursors;
        self
    }

    /// The raw rows, for a store that needs to inspect them (a revision
    /// fence, a conformance check) without reopening.
    #[must_use]
    pub fn rows(&self) -> &ProductAgentTree {
        &self.0
    }
}

impl ProductAgentTree {
    /// The reopen law: a new host generation, and
    /// `restore_paused` to fence work that was in flight when the last
    /// process ended.
    #[must_use]
    pub fn reopen(persisted: PersistedTree) -> Self {
        let mut tree = persisted.0;
        tree.host_generation = tree.host_generation.saturating_add(1);
        tree.restore_paused();
        tree
    }

    /// The durable rows of this tree, exactly as a store would hand them
    /// back: process-local state (queued effects, fault reasons, the VCS
    /// port, the wait notifier) is not part of them.
    #[must_use]
    pub fn to_persisted(&self) -> PersistedTree {
        let mut rows = Self::from_persisted(
            self.tree_id.clone(),
            self.root_agent_id.clone(),
            self.revision,
            self.host_generation,
            self.state,
            self.writer_mode,
            self.selected_tuple.clone(),
            self.launch_checkout.clone(),
            self.snapshot.clone(),
            self.nodes.clone(),
            self.grants.clone(),
            self.attempts.clone(),
            self.candidates.clone(),
            self.workspaces.clone(),
            self.messages.clone(),
            self.operations.clone(),
        );
        rows.commands.clone_from(&self.commands);
        rows.feed_cursors.clone_from(&self.feed_cursors);
        PersistedTree(rows)
    }
}

/// Durability for one tree. The tree knows nothing about where
/// its rows live; this is the whole surface between them.
pub trait TreeStore: Send + Sync {
    /// Whether the store already holds a tree under this id, without paying
    /// to decode it.
    ///
    /// # Errors
    ///
    /// Returns [`TreeStoreErrorKind::Unavailable`] when the store cannot be
    /// reached.
    fn exists(&self, tree_id: &str) -> Result<bool, TreeStoreError>;
    /// Every persisted tree id, in ascending id order.
    fn ids(&self) -> Result<Vec<String>, TreeStoreError>;
    /// Atomic whole-tree publish as a compare-and-swap on the revision: the
    /// store must hold exactly `tree.stored_revision` (no row at all when
    /// that is 0) or the save is refused with `StaleRevision` and the rows
    /// are left alone. See [`fence_revision`].
    ///
    /// The published revision comes back rather than being written into the
    /// argument: a durability port persists a tree, it does not edit one.
    /// Applying it is [`ProductAgentTree::acknowledge_revision`]'s job, and a
    /// caller that forgets leaves its handle looking stale, which the next
    /// save refuses — the failure mode is loud.
    ///
    /// # Errors
    ///
    /// Refuses a stale revision, an unreachable store, and rows that do not
    /// encode.
    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError>;
    /// Raw rows; the caller applies the reopen law.
    fn load(&self, tree_id: &str) -> Result<Option<PersistedTree>, TreeStoreError>;
}

/// The revision a store acknowledged for one published tree.
///
/// Only a [`TreeStore::save`] mints one, so the only way a handle's
/// `stored_revision` can move is a publish that actually happened.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[must_use = "a published revision the caller does not acknowledge leaves its handle stale"]
pub struct Revision(u64);

impl Revision {
    /// The revision a store just published. Called by a store implementation
    /// reporting what it wrote.
    pub const fn published(value: u64) -> Self {
        Self(value)
    }

    /// The acknowledged revision as a number, for a store's own fence.
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Revision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// The compare-and-swap every store applies: `stored` is the
/// revision the store holds now (`None` for no row), `expected` the one the
/// saving handle last acknowledged (0 for a tree it has never published).
/// Anything else means another handle published in between.
pub fn fence_revision(
    stored: Option<u64>,
    expected: u64,
    offered: u64,
) -> Result<(), TreeStoreError> {
    match stored {
        None if expected == 0 => Ok(()),
        Some(stored) if stored == expected => Ok(()),
        _ => Err(TreeStoreError::stale(stored.unwrap_or(0), offered)),
    }
}

/// An in-process [`TreeStore`]: the rows of each tree, keyed by id. Runs the
/// same conformance suite as the SQLite store.
#[derive(Default)]
pub struct MemoryTreeStore {
    trees: Mutex<BTreeMap<String, ProductAgentTree>>,
}

impl MemoryTreeStore {
    /// An empty in-process store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl TreeStore for MemoryTreeStore {
    fn exists(&self, tree_id: &str) -> Result<bool, TreeStoreError> {
        let trees = self
            .trees
            .lock()
            .map_err(|_| TreeStoreError::unavailable("memory tree store is poisoned"))?;
        Ok(trees.contains_key(tree_id))
    }

    fn ids(&self) -> Result<Vec<String>, TreeStoreError> {
        let trees = self
            .trees
            .lock()
            .map_err(|_| TreeStoreError::unavailable("memory tree store is poisoned"))?;
        Ok(trees.keys().cloned().collect())
    }

    fn save(&self, tree: &ProductAgentTree) -> Result<Revision, TreeStoreError> {
        let mut trees = self
            .trees
            .lock()
            .map_err(|_| TreeStoreError::unavailable("memory tree store is poisoned"))?;
        fence_revision(
            trees.get(&tree.tree_id).map(|stored| stored.revision),
            tree.stored_revision,
            tree.revision,
        )?;
        trees.insert(tree.tree_id.clone(), tree.to_persisted().0);
        Ok(Revision::published(tree.revision))
    }

    fn load(&self, tree_id: &str) -> Result<Option<PersistedTree>, TreeStoreError> {
        let trees = self
            .trees
            .lock()
            .map_err(|_| TreeStoreError::unavailable("memory tree store is poisoned"))?;
        Ok(trees.get(tree_id).map(ProductAgentTree::to_persisted))
    }
}
