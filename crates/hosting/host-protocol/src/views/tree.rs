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
#[derive(Clone, Debug, Eq, PartialEq)]
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
    /// What a parked child waits on (`p3`, `c2`), present exactly while it
    /// is parked: an empty list is the parked marker for a child whose wait
    /// is not yet named (an arrival already woke it, or a reopen holds it).
    pub waiting_on: Option<Vec<String>>,
}

/// Non-authoritative, bounded observation of one running child's turn.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ChildProgress {
    /// Exact turn identity; never inferred from a tree revision.
    pub turn_id: String,
    /// Current activity verb, shared with the root HUD's classification.
    pub activity: Option<String>,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeView {
    /// Durable tree id.
    pub tree_id: String,
    /// Tree revision; intents echo this so the host can resnapshot on mismatch.
    pub revision: u64,
    /// What the tree admits right now.
    pub state: TreeAdmissionState,
    /// True when this view is the echo of an explicit `tree` / `resume_tree`.
    pub solicited: bool,
    /// Deterministic preorder rows.
    pub nodes: Vec<NodeView>,
}

impl TreeView {
    /// mutating spawn, mail, and merge are blocked until an
    /// explicit resume. Derived from `state` rather than carried beside it.
    #[must_use]
    pub const fn restored_paused(&self) -> bool {
        matches!(self.state, TreeAdmissionState::RestoredPaused)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
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
}
