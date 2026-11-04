//! The typed tree view: what `plakous-recursive-subagents` projects.
//!
//! Every field that names a lifecycle is the protocol's own enum, not a label.
//! This is the only tree row the protocol has: the v1 JSONL `tree` frame is
//! projected from it in [`crate::jsonl`], and the product's canvas reads it
//! through an adapter of its own in `bin/plak/src/tui`. The stringly
//! `TopologyNavigatorNode` that used to sit between them is gone, so a
//! spelling is chosen in exactly one place per reader.

use super::navigator::{AttemptUsage, BlockedObservation};
use crate::topology::{
    AttemptStatus, CandidateId, NodeDisposition, NodeStatus, Tier, TreeAdmissionState,
    WorkspaceMode, WorkspaceStatus,
};

/// One preorder row of a tree, typed.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct NodeView {
    /// Exact durable agent id.
    pub agent_id: String,
    /// Parent agent id; `None` for the tree root.
    pub parent_agent_id: Option<String>,
    /// Depth from the root (`0` at root).
    pub depth: u8,
    /// Lifecycle state of the node.
    pub status: NodeStatus,
    /// The current attempt's status; `None` until the node has an attempt row,
    /// which the root never does.
    pub attempt_status: Option<AttemptStatus>,
    /// Sequence of the node's current attempt: `a1`, `a2` after a `followup`
    /// re-task. `None` when the attempt id carries no parsable sequence.
    pub attempt_seq: Option<u32>,
    /// Durable id of the node's current attempt; `None` until the node has an
    /// attempt row. A `followup` re-task moves it, so a product that keys
    /// operator state by attempt sees the superseded attempt drop out.
    pub attempt_id: Option<String>,
    /// Every attempt sequence this node has had, ascending.
    pub attempts: Vec<u32>,
    /// Effective spawn tier after the template clip. `None` on a row the host
    /// could not tier.
    pub tier: Option<Tier>,
    /// Description string from the node record.
    pub description: String,
    /// Prompt string from the node record.
    pub prompt: String,
    /// Workspace path display.
    pub workspace_path: String,
    /// The copy shape backing the node.
    pub workspace_mode: WorkspaceMode,
    /// The workspace row's lifecycle; `None` when no row backs the node.
    pub workspace_status: Option<WorkspaceStatus>,
    /// Candidate this node's current attempt sealed, when it has one.
    pub candidate_id: Option<CandidateId>,
    /// Quota refusal on the current attempt, when there is one.
    pub blocked: Option<BlockedObservation>,
    /// Per-attempt usage when the host has a row for it.
    pub usage: Option<AttemptUsage>,
    /// Whether the node currently holds an active turn.
    pub active_turn: bool,
    /// Best-effort current-turn activity, absent after the terminal boundary.
    pub live_progress: Option<ChildProgress>,
    /// Unread inbound mail (accepted or acknowledged, not delivered/terminal).
    pub unread_mail: u32,
    /// Whether a session locator is bound (transcript paging is possible).
    pub has_session: bool,
    /// Template the node was spawned from; `None` on the root.
    pub template: Option<String>,
    /// Bound account of the model tuple.
    pub model_account: String,
    /// Bound tuple id of the model tuple.
    pub model_tuple: String,
    /// Bound effort of the model tuple, when the tuple carries one.
    pub model_effort: Option<String>,
    /// Admitted turns on this node; `None` when no attempt row exists.
    pub turns: Option<u32>,
    /// Wall-clock start of the node, when the host has a durable stamp.
    pub started_at_unix: Option<u64>,
    /// Wall-clock end of the current attempt, once it has stopped.
    pub ended_at_unix: Option<u64>,
    /// Why the node cannot make progress, when the host knows.
    pub fault: Option<String>,
    /// The current failure records an operator's session-deletion abandonment.
    pub recovery_abandoned: bool,
    /// What a parked child waits on (`p3`, `c2`), present exactly while it
    /// is parked: an empty list is the parked marker for a child whose wait
    /// is not yet named (an arrival already woke it, or a reopen holds it).
    pub waiting_on: Option<Vec<String>>,
    /// Observational dormancy. Empty on the root and when no fact is true.
    pub dormancy: NodeDormancy,
    /// Why an interrupted node was interrupted, when its interrupt recorded
    /// a cause; `None` on any other node.
    pub interrupt_cause: Option<InterruptCauseView>,
    /// The node's continuation after its host was lost: held for the
    /// operator's Resume, pending dispatch, or running; `None` otherwise.
    pub continuation: Option<LossContinuationView>,
}

/// Why a node's turn was interrupted.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum InterruptCauseView {
    /// The operator interrupted it.
    Operator,
    /// Its managing parent interrupted it.
    Parent,
    /// The root's interrupt cascaded to it.
    Cascade,
    /// The host running its turn was lost.
    OwnerLost,
    /// A planned handoff released it to another host.
    Handoff,
    /// An explicit stop.
    Stopped,
}

/// Where a node's continuation after a lost host stands.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum LossContinuationView {
    /// Waiting for the operator's Resume.
    Held,
    /// Consent is recorded; its turn has not started yet.
    Pending,
    /// Its continuation turn is running.
    Running,
}

/// Independent dormancy facts. Several may be true at once. This is not a
/// lifecycle state, a timer, or recovery authority: retirement does not delete
/// work, and a reclaimed checkout can be revived.
#[allow(
    clippy::struct_excessive_bools,
    reason = "dormancy facts are independent and several may be true at once"
)]
#[derive(
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    PartialEq,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct NodeDormancy {
    /// Parked, including a wait that is not named yet.
    pub parked: bool,
    /// The conversation was retired.
    pub conversation_retired: bool,
    /// The checkout was reclaimed.
    pub workspace_reclaimed: bool,
    /// The node is closed.
    pub closed: bool,
}

impl NodeDormancy {
    /// No fact is set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        !self.parked && !self.conversation_retired && !self.workspace_reclaimed && !self.closed
    }

    /// Followup can revive a reclaimed checkout, and cannot revive a retired
    /// conversation. `None` when neither fact applies.
    #[must_use]
    pub const fn followup_revives(self) -> Option<bool> {
        if self.conversation_retired {
            Some(false)
        } else if self.workspace_reclaimed {
            Some(true)
        } else {
            None
        }
    }
}

/// Non-authoritative, bounded observation of one running child's turn.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ChildProgress {
    /// Exact turn identity; never inferred from a tree revision.
    pub turn_id: String,
    /// Current activity verb, shared with the root HUD's classification.
    pub activity: Option<String>,
    /// Original account while waiting for its login; absent outside that wait.
    pub credential_wait: Option<String>,
    /// Tool calls dispatched during this turn.
    pub tool_calls: u64,
    /// Unix milliseconds of the most recent observed activity.
    pub last_activity_unix_ms: Option<u64>,
    /// Provider-reported live input tokens.
    pub input_tokens: u64,
    /// Provider-reported live output tokens.
    pub output_tokens: u64,
    /// Provider-reported live reasoning tokens.
    pub reasoning_tokens: u64,
    /// Provider-reported cached input tokens.
    pub cached_input_tokens: u64,
    /// Provider-reported cache-write tokens.
    pub cache_write_tokens: u64,
}

impl NodeView {
    /// What the node is doing. The row carries `status` — the
    /// nine values a row can hold — and this is the five-value reading every
    /// caller actually wants; both derive from the same one mapping.
    #[must_use]
    pub const fn disposition(&self) -> NodeDisposition {
        self.status.disposition()
    }

    /// No turn is running and none starts without a re-task. Derived through
    /// the disposition rather than carried, so it cannot disagree with
    /// `status` and cannot read it differently from the tree's own filters.
    #[must_use]
    pub const fn finished(&self) -> bool {
        matches!(
            self.disposition(),
            NodeDisposition::Idle | NodeDisposition::Dead
        )
    }
}

/// A revisioned complete tree, typed. Replace, never patch locally.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct TreeView {
    /// Durable tree id.
    pub tree_id: String,
    /// Tree revision; intents echo this so the host can resnapshot on mismatch.
    pub revision: u64,
    /// What the tree admits right now.
    pub state: TreeAdmissionState,
    /// True when this view is the echo of an explicit `tree`.
    pub solicited: bool,
    /// Deterministic preorder rows.
    pub nodes: Vec<NodeView>,
}

impl TreeView {
    /// Whether `state` is the retained `restored_paused` spelling. A tree
    /// reopens live, so a current host never projects it. Derived from
    /// `state` rather than carried beside it.
    #[must_use]
    pub const fn restored_paused(&self) -> bool {
        matches!(self.state, TreeAdmissionState::RestoredPaused)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
