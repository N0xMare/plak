use super::{CanvasNode, CanvasTree};
use crate::tui::status_glyph::status_glyph;
use plakous_host_protocol::topology::{
    AttemptStatus, CandidateId, NodeStatus, Tier, TreeAdmissionState, WorkspaceMode,
    WorkspaceStatus,
};
use plakous_host_protocol::views::{NodeView, PROJECTED_STATUS_LABELS, TreeView};

fn node(status: NodeStatus, attempt: Option<AttemptStatus>) -> NodeView {
    NodeView {
        runner: plakous_host_protocol::wire::RunnerLabel::local(),
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
        recovery_abandoned: false,
        dormancy: plakous_host_protocol::views::NodeDormancy::default(),
        interrupt_cause: None,
        continuation: None,
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
    assert_eq!(
        tree.state, "restored_paused",
        "a retained stored spelling still reads"
    );
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
            crate::tui::status_glyph::UNKNOWN_STATUS_GLYPH,
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
            crate::tui::status_glyph::UNKNOWN_STATUS_GLYPH,
            "attempt status `{label}` falls through to the unknown glyph"
        );
    }
}
