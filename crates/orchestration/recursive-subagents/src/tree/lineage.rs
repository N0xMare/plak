//! Tree-schema-10 lineage rows: the L0 line a node maps to, the epochs a
//! tree carries, where a forked tree came from, where a node's tools run, and
//! which commits each runner already holds (spec 03 §1.2, §14).
//!
//! Every snapshot read goes through the epoch accessors here: spawn authority
//! by the caller's epoch, a node's bind, reopen and lifecycle by its own,
//! housekeeping and the root's surfaces by the current one. Today's flows
//! write only the defaults (no line, epoch 0, the local runner, no
//! provenance, no ancestors) and never append an epoch; the switch, the line
//! integration and the runner steps fill them in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{
    CellAgentBudget, OrchestrationSnapshot, ProductAgentTree, TopologyError, TopologyErrorCode,
};

/// The L0 agent-line identity a node maps to for life: the canonical
/// lowercase hyphenated UUID of the branch that started the line, the same
/// text `plakous` writes for a line id. Opaque here: the tree compares and
/// stores it, and never derives one.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NodeLineId(String);

impl NodeLineId {
    /// Admits the canonical text of a non-nil UUID.
    ///
    /// # Errors
    ///
    /// Refuses anything else, including upper case and the nil UUID.
    pub fn parse(text: &str) -> Result<Self, String> {
        let canonical = text.len() == 36
            && text.char_indices().all(|(index, character)| match index {
                8 | 13 | 18 | 23 => character == '-',
                _ => character.is_ascii_digit() || ('a'..='f').contains(&character),
            });
        if !canonical || text.bytes().all(|byte| byte == b'0' || byte == b'-') {
            return Err("agent line id is not a canonical non-nil UUID".to_owned());
        }
        Ok(Self(text.to_owned()))
    }

    /// The canonical text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NodeLineId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<NodeLineId> for String {
    fn from(value: NodeLineId) -> Self {
        value.0
    }
}

impl std::fmt::Display for NodeLineId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Where a node's tools run. The default is the host's own local runner and
/// the node's `path`, which is every node today.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NodePlacement {
    /// The runner that hosts the node's workspace; `None` is the local one.
    pub runner_id: Option<String>,
    /// The workspace on that runner; `None` means the node's `path`.
    pub workspace: Option<NodeWorkspace>,
}

impl NodePlacement {
    /// Whether this is today's placement: the local runner, at `path`.
    #[must_use]
    pub fn is_local(&self) -> bool {
        self.runner_id.is_none() && self.workspace.is_none()
    }
}

/// A node's workspace on its runner: the registered root and the working
/// directory inside it, both as the runner names them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeWorkspace {
    /// The workspace root the runner registered.
    pub root: PathBuf,
    /// The node's working directory, inside `root`.
    pub cwd: PathBuf,
}

/// Where a tree's root came from, recorded once when the tree is created.
/// Frozen as complete: unknown fields are refused, and any later field is a
/// tree schema change.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ForkedFrom {
    /// A fork of a node in this state directory: its tree, its agent, its
    /// line and the exact branch the fork copied.
    Node {
        /// The source node's tree.
        tree_id: String,
        /// The source node.
        agent_id: String,
        /// The source node's line.
        line_id: NodeLineId,
        /// The source branch the fork copied, as its canonical UUID text.
        branch_id: String,
        /// The source epoch whose snapshot bytes seeded the fork's genesis.
        epoch_no: u64,
    },
    /// A cross-state import: line ids are per runtime store, so the source
    /// is provenance text only.
    Import {
        /// Operator-facing provenance of the imported document.
        provenance: String,
    },
}

/// A tree's epochs. Epoch 0 is the header snapshot every tree has; successor
/// epochs are rows numbered from 1. `current` is the epoch new spawns bind.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TreeEpochs {
    /// The epoch new spawns bind; 0 is the header snapshot.
    pub current: u64,
    /// Successor epochs by number, each a frozen snapshot with its digest.
    pub successors: BTreeMap<u64, OrchestrationSnapshot>,
}

impl TreeEpochs {
    /// Whether `epoch_no` names an epoch this tree holds.
    #[must_use]
    pub fn holds(&self, epoch_no: u64) -> bool {
        epoch_no == 0 || self.successors.contains_key(&epoch_no)
    }

    /// Whether the rows are coherent: successors are numbered from 1 and
    /// each snapshot's digest matches its contents, and `current` names a
    /// held epoch.
    #[must_use]
    pub fn is_coherent(&self) -> bool {
        self.holds(self.current)
            && self
                .successors
                .iter()
                .all(|(number, snapshot)| *number >= 1 && snapshot.digest_matches())
    }
}

/// A tree's lineage rows, held together on the tree.
#[derive(Clone, Debug, Default)]
pub(super) struct TreeLineage {
    pub(super) epochs: TreeEpochs,
    pub(super) forked_from: Option<ForkedFrom>,
    pub(super) runner_ancestors: BTreeMap<String, BTreeSet<String>>,
}

/// Epochs one tree may reference at once: the current epoch plus every
/// epoch a non-root node was spawned under. An append past it is refused.
pub const MAX_REFERENCED_EPOCHS: usize = 8;

impl ProductAgentTree {
    /// The tree's epochs: the current one and the successor rows.
    #[must_use]
    pub fn epochs(&self) -> &TreeEpochs {
        &self.lineage.epochs
    }

    /// The snapshot of epoch `epoch_no`, when this tree holds it.
    #[must_use]
    pub fn epoch_snapshot(&self, epoch_no: u64) -> Option<&OrchestrationSnapshot> {
        if epoch_no == 0 {
            return Some(&self.snapshot);
        }
        self.lineage.epochs.successors.get(&epoch_no)
    }

    /// Epoch 0, the snapshot the tree row itself carries. Only a store
    /// writing that row reads it; every other reader names whose epoch it
    /// wants.
    #[must_use]
    pub fn header_snapshot(&self) -> &OrchestrationSnapshot {
        &self.snapshot
    }

    /// The current epoch: what the root spawns from, the root's own
    /// surfaces, and the policy tree-wide housekeeping applies.
    #[must_use]
    pub fn current_epoch(&self) -> &OrchestrationSnapshot {
        // A load refuses a current epoch the tree does not hold and
        // `append_epoch` only ever names one it inserted, so the header is
        // never reached here while the rows are coherent.
        self.epoch_snapshot(self.lineage.epochs.current)
            .unwrap_or(&self.snapshot)
    }

    /// The epoch number `node_id` lives under. The root follows the current
    /// epoch; every other node keeps the epoch it was spawned under.
    pub(super) fn epoch_no_of(&self, node_id: &str) -> Option<u64> {
        if node_id == self.root_agent_id {
            return Some(self.lineage.epochs.current);
        }
        self.nodes.get(node_id).map(|node| node.epoch_no)
    }

    /// The epoch a node binds and reopens under, and whose per-node policy
    /// (turn ceiling, mint hook, transient resumes, cell budget) applies to
    /// it. `None` for an unknown node.
    #[must_use]
    pub fn epoch_of(&self, node_id: &str) -> Option<&OrchestrationSnapshot> {
        self.epoch_snapshot(self.epoch_no_of(node_id)?)
    }

    /// The epoch whose roster is `caller_id`'s spawn authority: its menu,
    /// its cards, and the bind a spawn from it freezes.
    ///
    /// # Errors
    ///
    /// Refuses an unknown caller as `grant_denied`, and a caller whose
    /// epoch the tree does not hold.
    pub fn caller_epoch(&self, caller_id: &str) -> Result<&OrchestrationSnapshot, TopologyError> {
        let epoch_no = self.caller_epoch_no(caller_id)?;
        self.epoch_snapshot(epoch_no).ok_or_else(epoch_not_held)
    }

    pub(super) fn caller_epoch_no(&self, caller_id: &str) -> Result<u64, TopologyError> {
        self.node(caller_id)?;
        self.epoch_no_of(caller_id).ok_or_else(epoch_not_held)
    }

    /// Every epoch the tree holds, header first.
    pub fn retained_epochs(&self) -> impl Iterator<Item = (u64, &OrchestrationSnapshot)> + '_ {
        std::iter::once((0, &self.snapshot)).chain(
            self.lineage
                .epochs
                .successors
                .iter()
                .map(|(number, snapshot)| (*number, snapshot)),
        )
    }

    /// Whether every retained epoch's digest still matches its contents:
    /// the reopen gate, so an edited roster in any epoch is caught.
    #[must_use]
    pub fn epoch_digests_match(&self) -> bool {
        self.retained_epochs()
            .all(|(_, snapshot)| snapshot.digest_matches())
    }

    /// The current epoch, for an in-place install on a quiescent tree.
    pub(super) fn current_epoch_mut(&mut self) -> &mut OrchestrationSnapshot {
        let current = self.lineage.epochs.current;
        if current == 0 {
            return &mut self.snapshot;
        }
        match self.lineage.epochs.successors.get_mut(&current) {
            Some(snapshot) => snapshot,
            None => &mut self.snapshot,
        }
    }

    /// Epochs a non-root node still references.
    fn node_epochs(&self) -> BTreeSet<u64> {
        self.nodes
            .values()
            .filter(|node| node.agent_id != self.root_agent_id)
            .map(|node| node.epoch_no)
            .collect()
    }

    /// Appends `snapshot` as the tree's next epoch and makes it current.
    ///
    /// The root moves to it: its bind, its spawn ceilings and the
    /// per-prompt attempt bound come from the new epoch, and future root
    /// spawns bind its roster. Every other node keeps the epoch it was
    /// spawned under, with its cards, grant ceilings and cell budget, so
    /// live descendants do not block an append. A spawn mid-reservation
    /// does, because its node would otherwise bind an epoch chosen before
    /// the append. The admission state is kept: an append is not a resume
    /// and does not lift a stop. Successor epochs no node references any
    /// more are folded away first; epoch 0 is the header and stays.
    ///
    /// Returns the new epoch's number.
    ///
    /// # Errors
    ///
    /// Refuses a snapshot whose digest does not match its contents, a
    /// pending spawn, and an append that would reference more than
    /// [`MAX_REFERENCED_EPOCHS`] epochs.
    pub fn append_epoch(&mut self, snapshot: OrchestrationSnapshot) -> Result<u64, TopologyError> {
        if !snapshot.digest_matches() {
            return Err(TopologyError::new(
                TopologyErrorCode::SchemaMismatch,
                "a tree epoch's digest does not match its roster",
            ));
        }
        if self.pending_spawns > 0 || !self.minting.is_empty() {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                Self::TREE_EPOCH_REBIND_PENDING_SPAWNS,
            ));
        }
        let next = self
            .lineage
            .epochs
            .current
            .checked_add(1)
            .ok_or_else(epoch_not_held)?;
        let mut referenced = self.node_epochs();
        referenced.insert(next);
        if referenced.len() > MAX_REFERENCED_EPOCHS {
            return Err(TopologyError::new(
                TopologyErrorCode::BoundExceeded,
                format!(
                    "this tree already references {MAX_REFERENCED_EPOCHS} epochs; close agents \
                     spawned under older profiles before switching again"
                ),
            ));
        }
        self.lineage
            .epochs
            .successors
            .retain(|number, _| referenced.contains(number));
        self.cell_budgets
            .retain(|number, _| *number == 0 || referenced.contains(number));
        self.cell_budgets
            .insert(next, Arc::new(CellAgentBudget::for_snapshot(&snapshot)));
        self.selected_tuple.clone_from(&snapshot.root_tuple);
        let root_id = self.root_agent_id.clone();
        if let Some(root) = self.nodes.get_mut(&root_id) {
            if !snapshot.root_account.is_empty() {
                root.account.clone_from(&snapshot.root_account);
            }
            root.tuple_id.clone_from(&snapshot.root_tuple);
            root.effort.clone_from(&snapshot.root_effort);
            root.epoch_no = next;
        }
        self.clamp_root_ceilings(&snapshot);
        self.lineage.epochs.successors.insert(next, snapshot);
        self.lineage.epochs.current = next;
        self.bump();
        Ok(next)
    }

    /// The per-cell agent budget of `caller_id`'s epoch. Shared by every
    /// topology tool built for that caller: the extensions are rebuilt per
    /// caller and per turn, so a counter owned by one set would enforce
    /// nothing, and a cell keeps the budget its epoch gave it.
    ///
    /// # Errors
    ///
    /// Refuses an unknown caller and a caller whose epoch the tree does not
    /// hold.
    pub fn cell_budget(&self, caller_id: &str) -> Result<Arc<CellAgentBudget>, TopologyError> {
        let epoch_no = self.caller_epoch_no(caller_id)?;
        self.cell_budgets
            .get(&epoch_no)
            .map(Arc::clone)
            .ok_or_else(epoch_not_held)
    }

    /// Gives every held epoch a cell budget, keeping the ones already built.
    pub(super) fn ensure_cell_budgets(&mut self) {
        for (number, snapshot) in &self.lineage.epochs.successors {
            self.cell_budgets
                .entry(*number)
                .or_insert_with(|| Arc::new(CellAgentBudget::for_snapshot(snapshot)));
        }
    }
}

fn epoch_not_held() -> TopologyError {
    TopologyError::new(
        TopologyErrorCode::OperationRecoveryRequired,
        "this node's tree epoch is not held",
    )
}

impl ProductAgentTree {
    /// Where this tree came from, when it was forked or imported.
    #[must_use]
    pub fn forked_from(&self) -> Option<&ForkedFrom> {
        self.lineage.forked_from.as_ref()
    }

    /// Per runner id, the commits that runner already holds for this tree,
    /// so an incremental bundle can be cut against the best one.
    #[must_use]
    pub fn runner_ancestors(&self) -> &BTreeMap<String, BTreeSet<String>> {
        &self.lineage.runner_ancestors
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
