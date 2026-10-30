use super::{NodeView, TreeView};
use crate::envelope::JsonlCorrelation;
use crate::jsonl::project_host_event;
use crate::observation::SessionObservation;
use crate::topology::{
    AttemptStatus, CandidateId, NodeDisposition, NodeStatus, Tier, TreeAdmissionState,
    WorkspaceMode, WorkspaceStatus,
};
use crate::views::navigator::{AttemptUsage, BlockedObservation};

/// Every field the JSONL `tree` frame reads, set to a value that would show
/// a wrong spelling: a non-default status, a non-default tier, a workspace
/// row mid-adopt, and a quota refusal with a reset stamp.
fn view() -> TreeView {
    TreeView {
        tree_id: "tree-view".to_owned(),
        revision: 7,
        state: TreeAdmissionState::RestoredPaused,
        solicited: true,
        nodes: vec![
            NodeView {
                agent_id: "root".to_owned(),
                parent_agent_id: None,
                depth: 0,
                status: NodeStatus::Running,
                attempt_status: None,
                attempt_seq: None,
                attempt_id: None,
                attempts: Vec::new(),
                tier: Some(Tier::Mutate),
                description: "the root".to_owned(),
                prompt: "do the thing".to_owned(),
                workspace_path: "/w".to_owned(),
                workspace_mode: WorkspaceMode::Inherit,
                workspace_status: Some(WorkspaceStatus::Active),
                candidate_id: None,
                blocked: None,
                usage: None,
                active_turn: true,
                live_progress: None,
                unread_mail: 0,
                has_session: true,
                template: None,
                model_account: "codex".to_owned(),
                model_tuple: "gpt".to_owned(),
                model_effort: None,
                turns: Some(1),
                started_at_unix: None,
                ended_at_unix: None,
                fault: None,
                waiting_on: None,
            },
            NodeView {
                agent_id: "c1".to_owned(),
                parent_agent_id: Some("root".to_owned()),
                depth: 1,
                status: NodeStatus::RecoveryRequired,
                attempt_status: Some(AttemptStatus::CompletedWithoutSubmission),
                attempt_seq: Some(2),
                attempt_id: Some("c1-a2".to_owned()),
                attempts: vec![1, 2],
                tier: Some(Tier::ReadOnly),
                description: "the child".to_owned(),
                prompt: "read the thing".to_owned(),
                workspace_path: "/w/.plak/worktrees/c1".to_owned(),
                workspace_mode: WorkspaceMode::Fork,
                workspace_status: Some(WorkspaceStatus::Adopting),
                candidate_id: Some(CandidateId::from_persisted("cand-c1-a2".to_owned())),
                blocked: Some(BlockedObservation {
                    quota: "weekly".to_owned(),
                    account: "codex".to_owned(),
                    resets_at_unix: Some(1_700_000_000),
                }),
                usage: Some(AttemptUsage {
                    elapsed_ms: 12,
                    turns: 3,
                    tool_calls: 4,
                    input_tokens: 5,
                    output_tokens: 6,
                    compaction: None,
                }),
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
                ended_at_unix: Some(1_700_000_011),
                fault: Some("bind failed".to_owned()),
                waiting_on: None,
            },
        ],
    }
}

fn projected(view: TreeView) -> String {
    project_host_event::<crate::intent::Core>(
        &SessionObservation::TopologyNavigatorSnapshot(view),
        JsonlCorrelation {
            prompt_id: None,
            omit_unknown_turn_id: false,
            command_id: None,
            pending_tree_snapshot_id: Some("cmd-1"),
        },
    )
    .expect("a tree snapshot projects")
}

/// identity golden: the exact `tree` frame bytes the typed view projects.
///
/// These are the bytes the stringly `TopologyNavigatorNode` used to
/// produce, unchanged by its deletion: the v1 wire did not move when its
/// only remaining model became the typed one.
#[test]
fn the_typed_tree_view_projects_the_pinned_wire_bytes() {
    assert_eq!(
        projected(view()),
        r#"{"id":"cmd-1","kind":"tree","nodes":[{"account":"codex","active_turn":true,"agent_id":"root","depth":0,"description":"the root","prompt":"do the thing","status":"running","tier":"mutate","workspace_mode":"inherit","workspace_path":"/w","workspace_status":"active"},{"account":"grok","active_turn":false,"agent_id":"c1","attempt":"completed_without_submission","attempt_seq":2,"blocked":{"account":"codex","quota":"weekly","resets_at":1700000000},"candidate":"cand-c1-a2","depth":1,"description":"the child","parent_agent_id":"root","prompt":"read the thing","status":"recovery_required","template":"review","tier":"read_only","workspace_mode":"fork","workspace_path":"/w/.plak/worktrees/c1","workspace_status":"adopting"}],"restored_paused":true,"revision":7,"state":"restored_paused","tree_id":"tree-view","v":1}"#
    );
}

#[test]
fn live_child_progress_is_additive_on_jsonl_tree_rows() {
    let mut tree = view();
    tree.nodes[1].live_progress = Some(super::ChildProgress {
        turn_id: "turn-c1".into(),
        activity: Some("reading".into()),
        tool_calls: 4,
        last_activity_unix_ms: Some(1_700_000_000_123),
        input_tokens: 11,
        output_tokens: 3,
        reasoning_tokens: 2,
        cached_input_tokens: 7,
        cache_write_tokens: 0,
    });
    let wire: serde_json::Value = serde_json::from_str(&projected(tree)).unwrap();
    assert_eq!(wire["v"], 1);
    assert!(wire["nodes"][0].get("live_progress").is_none());
    assert_eq!(
        wire["nodes"][1]["live_progress"],
        serde_json::json!({
            "turn_id":"turn-c1", "activity":"reading", "tool_calls":4,
            "last_activity_unix_ms":1_700_000_000_123_u64, "input_tokens":11,
            "output_tokens":3,"reasoning_tokens":2,"cached_input_tokens":7,"cache_write_tokens":0
        })
    );
}

#[test]
fn a_parked_childs_waiting_on_is_additive_on_jsonl_tree_rows() {
    let mut tree = view();
    tree.nodes[1].waiting_on = Some(vec!["p3".to_owned(), "c2".to_owned()]);
    let wire: serde_json::Value = serde_json::from_str(&projected(tree.clone())).unwrap();
    assert!(wire["nodes"][0].get("waiting_on").is_none());
    assert_eq!(
        wire["nodes"][1]["waiting_on"],
        serde_json::json!(["p3", "c2"])
    );
    // Parked on nothing named yet: the empty list is the parked marker.
    tree.nodes[1].waiting_on = Some(Vec::new());
    let wire: serde_json::Value = serde_json::from_str(&projected(tree)).unwrap();
    assert_eq!(wire["nodes"][1]["waiting_on"], serde_json::json!([]));
}

/// The two facts the view derives rather than stores.
/// the view's disposition is the tree's own, not a second
/// reading of the same status.
#[test]
fn the_view_reports_the_status_disposition() {
    let view = view();
    assert_eq!(view.nodes[0].disposition(), NodeDisposition::Live);
    assert_eq!(view.nodes[1].disposition(), NodeDisposition::Dead);
    for node in &view.nodes {
        assert_eq!(node.disposition(), node.status.disposition());
        assert_eq!(node.finished(), node.status.is_finished());
    }
}

#[test]
fn the_view_derives_its_paused_and_finished_facts_from_its_enums() {
    let view = view();
    assert!(view.restored_paused(), "the state says so");
    assert!(view.nodes[1].finished(), "recovery_required is finished");
    assert!(!view.nodes[0].finished(), "a running root is not finished");
}
