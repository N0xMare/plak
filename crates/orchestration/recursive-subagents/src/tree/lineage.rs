//! Tree-schema-10 lineage rows: the L0 line a node maps to, the epochs a
//! tree carries, where a forked tree came from, where a node's tools run, and
//! which commits each runner already holds (spec 03 §1.2, §14).
//!
//! Today's flows write only the defaults (no line, epoch 0, the local runner,
//! no provenance, no ancestors) and read none of them; the switch, the line
//! integration and the runner steps fill them in.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::{OrchestrationSnapshot, ProductAgentTree};

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

impl ProductAgentTree {
    /// The tree's epochs: the current one and the successor rows.
    #[must_use]
    pub fn epochs(&self) -> &TreeEpochs {
        &self.lineage.epochs
    }

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
