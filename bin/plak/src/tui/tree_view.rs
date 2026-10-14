//! The canvas's own tree rows, adapted from the protocol's typed [`TreeView`].
//!
//! `plakous-host-protocol` projects one tree model: `TreeView`/`NodeView`,
//! whose every lifecycle field is an enum. The v1 JSONL wire chooses its
//! spellings from those enums in `plakous_host_protocol::jsonl`; this module is
//! where *the TUI* chooses its own. Nothing else in `bin/plak/src/tui` sees a
//! protocol enum, and nothing outside this module turns one into a label — so a
//! canvas spelling is decided in exactly one place, and the two readers cannot
//! drift into each other's vocabulary.
//!
//! Canvas rows are product presentation types, separate from wire serialization.

use plakous_host_protocol::views::{AttemptUsage, BlockedObservation, NodeView, TreeView};

/// One preorder canvas row: the typed node with its labels already chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasNode {
    /// Exact durable agent id.
    pub agent_id: String,
    /// Parent agent id; `None` for the tree root.
    pub parent_agent_id: Option<String>,
    /// Depth from the root (`0` at root).
    pub depth: u8,
    /// Lifecycle label (`running`, `paused`, …); a `status_glyph` key.
    pub status: String,
    /// Current attempt's status label. `None` until the node has an attempt
    /// row; the root never has one.
    pub attempt_status: Option<String>,
    /// Sequence of the node's current attempt.
    pub attempt_seq: Option<u32>,
    /// Every attempt sequence this node has had, ascending.
    pub attempt_id: Option<String>,
    pub attempts: Vec<u32>,
    /// Effective spawn tier label after the template clip.
    pub tier: Option<String>,
    /// Description string from the node record.
    pub description: String,
    /// Prompt string from the node record.
    pub prompt: String,
    /// Workspace path display.
    pub workspace_path: String,
    /// Workspace mode label (`inherit`, `snapshot`, `fork`).
    pub workspace_mode: String,
    /// Workspace row status label; `None` when no row backs the node.
    pub workspace_status: Option<String>,
    /// Candidate this node's current attempt sealed, when it has one.
    pub candidate_id: Option<String>,
    /// Quota refusal on the current attempt, when there is one.
    pub blocked: Option<BlockedObservation>,
    /// Per-attempt usage; the canvas renders unknown rather than zero on `None`.
    pub usage: Option<AttemptUsage>,
    /// Whether the node currently holds an active turn.
    pub active_turn: bool,
    /// Best-effort current-turn activity from the host.
    pub live_progress: Option<plakous_host_protocol::views::ChildProgress>,
    /// Unread inbound mail.
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
    /// Admitted turns on this node; `None` renders as unknown, never zero.
    pub turns: Option<u32>,
    /// Wall-clock start, when the host has a durable stamp. `None` renders as
    /// unknown: the canvas never invents a duration.
    pub started_at_unix: Option<u64>,
    pub ended_at_unix: Option<u64>,
    /// `is_finished`, derived by the view from `status`.
    pub finished: bool,
    /// Why the node cannot make progress, when the host knows.
    pub fault: Option<String>,
    /// What a parked child waits on, present exactly while it is parked
    /// (empty when the wait is not named yet).
    pub waiting_on: Option<Vec<String>>,
}

/// A revisioned complete canvas tree. Replace, never patch locally.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanvasTree {
    /// Durable tree id.
    pub tree_id: String,
    /// Tree revision; intents echo this so the host can resnapshot on mismatch.
    pub revision: u64,
    /// Admission state label (`live`, `restored_paused`, `closed`, …).
    pub state: String,
    /// True when mutating spawn/mail/merge is blocked until `ResumeTree`.
    pub restored_paused: bool,
    /// True when this snapshot answers an explicit `/tree` or `r`.
    pub solicited: bool,
    /// Deterministic preorder rows.
    pub nodes: Vec<CanvasNode>,
}

impl From<&NodeView> for CanvasNode {
    fn from(node: &NodeView) -> Self {
        Self {
            agent_id: node.agent_id.clone(),
            parent_agent_id: node.parent_agent_id.clone(),
            depth: node.depth,
            status: node.status.as_str().to_owned(),
            attempt_status: node.attempt_status.map(|status| status.as_str().to_owned()),
            attempt_seq: node.attempt_seq,
            attempt_id: node.attempt_id.clone(),
            attempts: node.attempts.clone(),
            tier: node.tier.map(|tier| tier.as_str().to_owned()),
            description: node.description.clone(),
            prompt: node.prompt.clone(),
            workspace_path: node.workspace_path.clone(),
            workspace_mode: node.workspace_mode.as_str().to_owned(),
            workspace_status: node
                .workspace_status
                .map(|status| status.as_str().to_owned()),
            candidate_id: node.candidate_id.as_ref().map(ToString::to_string),
            blocked: node.blocked.clone(),
            usage: node.usage,
            active_turn: node.active_turn,
            live_progress: node.live_progress.clone(),
            unread_mail: node.unread_mail,
            has_session: node.has_session,
            template: node.template.clone(),
            model_account: node.model_account.clone(),
            model_tuple: node.model_tuple.clone(),
            model_effort: node.model_effort.clone(),
            turns: node.turns,
            started_at_unix: node.started_at_unix,
            ended_at_unix: node.ended_at_unix,
            finished: node.finished(),
            fault: node.fault.clone(),
            waiting_on: node.waiting_on.clone(),
        }
    }
}

impl From<&TreeView> for CanvasTree {
    fn from(view: &TreeView) -> Self {
        Self {
            tree_id: view.tree_id.clone(),
            revision: view.revision,
            state: view.state.as_str().to_owned(),
            restored_paused: view.restored_paused(),
            solicited: view.solicited,
            nodes: view.nodes.iter().map(CanvasNode::from).collect(),
        }
    }
}

impl From<TreeView> for CanvasTree {
    fn from(view: TreeView) -> Self {
        Self::from(&view)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::{CanvasNode, CanvasTree};
    use plakous_host_protocol::topology::{
        AttemptStatus, CandidateId, NodeStatus, Tier, TreeAdmissionState, WorkspaceMode,
        WorkspaceStatus,
    };
    use plakous_host_protocol::views::{NodeView, PROJECTED_STATUS_LABELS, TreeView, status_glyph};

    fn node(status: NodeStatus, attempt: Option<AttemptStatus>) -> NodeView {
        NodeView {
            agent_id: "c1".to_owned(),
            parent_agent_id: Some("root".to_owned()),
            depth: 1,
            status,
            attempt_status: attempt,
            attempt_seq: Some(2),
            attempt_id: Some("c1-a2".to_owned()),
            attempts: vec![1, 2],
            tier: Some(Tier::ReadOnly),
            description: "the child".to_owned(),
            prompt: "read it".to_owned(),
            workspace_path: "/w/c1".to_owned(),
            workspace_mode: WorkspaceMode::Fork,
            workspace_status: Some(WorkspaceStatus::Adopting),
            candidate_id: Some(CandidateId::from_persisted("cand-c1-a2".to_owned())),
            blocked: None,
            usage: None,
            active_turn: false,
            live_progress: None,
            unread_mail: 2,
            has_session: false,
            template: Some("review".to_owned()),
            model_account: "grok".to_owned(),
            model_tuple: "grok-4.6".to_owned(),
            model_effort: Some("high".to_owned()),
            turns: Some(3),
            started_at_unix: Some(1_699_999_999),
            ended_at_unix: Some(1_700_000_009),
            fault: None,
            waiting_on: None,
        }
    }

    #[test]
    fn the_canvas_row_carries_the_label_of_every_typed_field() {
        let view = TreeView {
            tree_id: "tree-1".to_owned(),
            revision: 7,
            state: TreeAdmissionState::RestoredPaused,
            solicited: true,
            nodes: vec![node(
                NodeStatus::RecoveryRequired,
                Some(AttemptStatus::CompletedWithoutSubmission),
            )],
        };
        let tree = CanvasTree::from(&view);
        assert_eq!(tree.state, "restored_paused");
        assert!(tree.restored_paused, "derived from the state, not carried");
        assert!(tree.solicited);
        let row = &tree.nodes[0];
        assert_eq!(row.status, "recovery_required");
        assert_eq!(
            row.attempt_status.as_deref(),
            Some("completed_without_submission")
        );
        assert_eq!(row.tier.as_deref(), Some("read_only"));
        assert_eq!(row.workspace_mode, "fork");
        assert_eq!(row.workspace_status.as_deref(), Some("adopting"));
        assert!(row.finished, "recovery_required is finished");
        assert_eq!(CanvasNode::from(&view.nodes[0]), *row);
    }

    /// Spec 02 §5.7 paints a glyph per label, and `UNKNOWN_STATUS_GLYPH` is a
    /// silent degradation. Every label *this adapter* produces must therefore
    /// be one the glyph table knows — the same contract the host asserts for
    /// its own projections, restated on the reader that paints them.
    #[test]
    fn every_label_this_adapter_produces_has_a_glyph() {
        for status in [
            NodeStatus::Pending,
            NodeStatus::Running,
            NodeStatus::Paused,
            NodeStatus::Sealing,
            NodeStatus::Completed,
            NodeStatus::Interrupted,
            NodeStatus::Failed,
            NodeStatus::Closed,
            NodeStatus::RecoveryRequired,
        ] {
            let row = CanvasNode::from(&node(status, None));
            assert!(
                PROJECTED_STATUS_LABELS.contains(&row.status.as_str()),
                "node status `{}` is not a painted label",
                row.status
            );
            assert_ne!(
                status_glyph(&row.status),
                plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
                "node status `{}` falls through to the unknown glyph",
                row.status
            );
        }
        for attempt in [
            AttemptStatus::Running,
            AttemptStatus::Submitted,
            AttemptStatus::Completed,
            AttemptStatus::CompletedWithoutSubmission,
            AttemptStatus::Failed,
            AttemptStatus::Cancelled,
            AttemptStatus::RecoveryRequired,
            AttemptStatus::Closed,
            AttemptStatus::Blocked,
            AttemptStatus::Adopted,
        ] {
            let row = CanvasNode::from(&node(NodeStatus::Running, Some(attempt)));
            let label = row.attempt_status.expect("an attempt status");
            assert!(
                PROJECTED_STATUS_LABELS.contains(&label.as_str()),
                "attempt status `{label}` is not a painted label"
            );
            assert_ne!(
                status_glyph(&label),
                plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
                "attempt status `{label}` falls through to the unknown glyph"
            );
        }
    }
}
