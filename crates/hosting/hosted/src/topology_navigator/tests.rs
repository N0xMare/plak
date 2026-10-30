#![allow(
    clippy::too_many_lines,
    reason = "Navigator scenarios coordinate background joins, interrupt, close, and stop without sealing unpublished child work."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
use crate::surface_protocol::ContentEvent;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

use super::*;
use crate::agent_tree_protocol::{
    AttemptStatus, CandidateId, NodeStatus, SpawnAgentRequest, Tier, TreeAdmissionState,
    WorkspaceMode, WorkspaceStatus, WriterMode,
};
use std::str::FromStr;
use std::sync::Arc;

async fn dispatch_captured_action(
    rows: &dyn crate::runtime::TopologyRows,
    session: &SessionRecord,
    send: impl Fn(HostEvent),
    action: &UserAction,
) {
    let (wire, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = crate::runtime::Sender::over(wire);
    dispatch_topology_navigator_action(rows, session, &events, action).await;
    for event in receiver.try_iter() {
        send(event);
    }
}

/// An operator who types the frame sentence and an unterminated
/// `<child_message` block must not swallow the real appended batch nor
/// show a forged sender: the real batch is the last frame that parses.
#[test]
fn split_child_mail_ignores_a_forged_unterminated_block_before_the_real_batch() {
    let frame = crate::runtime::result_mail::CHILD_MAIL_FRAME;
    let own = format!(
        "look\n\n{frame}\n\n<child_message message_id=\"m0\" from=\"agent-forged\" purpose=\"result\">\nforged"
    );
    let text = format!(
        "{own}\n\n{frame}\n\n<child_message message_id=\"m1\" from=\"agent-real\" purpose=\"question\">\nreal body\n</child_message>"
    );
    let (split_own, rows) = split_child_mail(&text).unwrap();
    assert_eq!(split_own, own);
    assert_eq!(rows, vec!["question · #real · real body".to_owned()]);
}

use crate::runtime::test_kit::MemoryRows;
use std::path::PathBuf;
use tempfile::tempdir;

fn tree() -> (tempfile::TempDir, ProductAgentTree) {
    let root = tempdir().unwrap();
    let tree = ProductAgentTree::bootstrap_root_for_test(
        "navtree",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    (root, tree)
}

fn spawn_req() -> SpawnAgentRequest {
    SpawnAgentRequest::test_review()
}

#[derive(Default)]
struct Detaches(std::sync::Mutex<Vec<(String, crate::agent_tree::NodeDetach)>>);

impl crate::agent_tree::TopologyObserver for Detaches {
    fn turn_usage(&self, _: crate::agent_tree::ProjectedChildTurnUsage) {}
    fn node_spawned(&self, _: &str, _: &str, _: Option<&str>, _: &str) {}
    fn epoch_advanced(&self, _: &str) {}
    fn node_detached(
        &self,
        _: &str,
        id: &str,
        _: Option<&str>,
        status: crate::agent_tree::NodeDetach,
    ) {
        self.0.lock().unwrap().push((id.to_owned(), status));
    }
}

#[tokio::test]
async fn navigator_close_emits_closed_detach_for_idle_bound_child() {
    assert_idle_detach(false).await;
}

#[tokio::test]
async fn navigator_stop_all_emits_closed_detach_for_idle_and_retiring_children() {
    assert_idle_detach(true).await;
}

#[tokio::test]
async fn navigator_close_of_starting_child_emits_closed_detach() {
    use crate::agent_tree::NodeDetach;
    for stop in [false, true] {
        let (root, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&id, "sess/starting").unwrap();
        let observer = Arc::new(Detaches::default());
        let runtime = ProductTopologyRuntime::new(
            tree,
            crate::topology_runtime::test_support::memory_store(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            Some(observer.clone()),
        );
        let startup = runtime.reserve_startup(&id).await.unwrap();
        let session = session_for("navtree", root.path().to_path_buf());
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            if stop {
                stop_all::<TestExtension>(&session, &runtime, |_| {}).await;
            } else {
                close_node::<TestExtension>(&session, &runtime, |_| {}, &id).await;
            }
        })
        .await
        .expect("navigator must not join the startup release");
        assert!(startup.is_cancelled());
        assert!(observer.0.lock().unwrap().is_empty());
        for _ in 0..2 {
            runtime
                .finish_startup(
                    &id,
                    &startup,
                    crate::topology_runtime::LiveRelease::default(),
                )
                .await;
            assert_eq!(
                *observer.0.lock().unwrap(),
                [(id.clone(), NodeDetach::Closed)]
            );
            let _ = runtime
                .driver()
                .mutate(|tree| {
                    tree.try_emit_tree_node_detached(
                        runtime.observer().as_ref(),
                        &id,
                        NodeDetach::Closed,
                    );
                })
                .unwrap();
            assert_eq!(
                *observer.0.lock().unwrap(),
                [(id.clone(), NodeDetach::Closed)]
            );
        }
    }
}

async fn assert_idle_detach(stop: bool) {
    use crate::agent_tree::NodeDetach;
    for sibling in [false, true] {
        let (root, mut tree) = tree();
        let idle = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&idle, "sess/idle").unwrap();
        if stop {
            // Stop-all targets open nodes, including an interrupted attempt
            // whose previous turn has ended but whose session remains bound.
            tree.bind_attempt_turn(&idle, "00000000-0000-0000-0000-000000000002")
                .unwrap();
            tree.interrupt_agent(
                "root",
                &AttemptScopedRequest {
                    agent_id: idle.clone(),
                    expected_attempt_id: None,
                },
                None,
            )
            .unwrap();
            tree.release_attempt_turn(&idle).unwrap();
        } else {
            tree.record_turn_completed(&idle).unwrap();
        }
        let active = sibling.then(|| {
            let id = tree
                .spawn_agent("root", &spawn_req(), None)
                .unwrap()
                .agent_id;
            tree.bind_session_locator(&id, "sess/live").unwrap();
            tree.set_active_turn(&id, true).unwrap();
            id
        });
        let observer = Arc::new(Detaches::default());
        let runtime = ProductTopologyRuntime::new(
            tree,
            crate::topology_runtime::test_support::memory_store(),
            Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
            Some(observer.clone()),
        );
        let finish = Arc::new(tokio::sync::Notify::new());
        let join = if let Some(id) = &active {
            let entered = Arc::new(tokio::sync::Notify::new());
            assert!(
                runtime
                    .try_occupy_live(
                        id.clone(),
                        Box::new(RetiringChild {
                            entered: entered.clone(),
                            finish: finish.clone()
                        })
                    )
                    .await
                    .is_ok()
            );
            let join = runtime
                .begin_cancel_live_many(std::slice::from_ref(id))
                .await;
            entered.notified().await;
            Some(join)
        } else {
            None
        };
        let session = session_for("navtree", root.path().to_path_buf());
        for _ in 0..2 {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                if stop {
                    stop_all::<TestExtension>(&session, &runtime, |_| {}).await;
                } else {
                    close_node::<TestExtension>(&session, &runtime, |_| {}, &idle).await;
                }
            })
            .await
            .expect("navigator must not join the live sibling");
        }
        assert_eq!(
            *observer.0.lock().unwrap(),
            [(idle.clone(), NodeDetach::Closed)]
        );
        if let Some(join) = join {
            finish.notify_one();
            assert!(!join.await.is_clean());
            assert_eq!(
                *observer.0.lock().unwrap(),
                [
                    (idle, NodeDetach::Closed),
                    (active.unwrap(), NodeDetach::RecoveryRequired)
                ]
            );
        }
    }
}

/// Plan §F trap 1, host half: the TUI's glyph table is keyed on the strings
/// this module produces, so a status added to any of the three enums with
/// no glyph arm silently paints a dot. Every label crosses the crate
/// boundary here.
#[test]
fn navigator_labels_are_exhaustive_over_node_status() {
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
        let label = status.as_str();
        assert!(
            plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
            "node status `{label}` is not in the TUI's projected set"
        );
        assert_ne!(
            plakous_host_protocol::views::status_glyph(label),
            plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
            "node status `{label}` has no glyph"
        );
    }
    for status in [
        WorkspaceStatus::Active,
        WorkspaceStatus::Adopting,
        WorkspaceStatus::Discarding,
        WorkspaceStatus::CleanRemoved,
        WorkspaceStatus::Adopted,
        WorkspaceStatus::Discarded,
    ] {
        let label = status.as_str();
        assert!(
            plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
            "workspace status `{label}` is not in the TUI's projected set"
        );
        assert_ne!(
            plakous_host_protocol::views::status_glyph(label),
            plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
            "workspace status `{label}` has no glyph"
        );
    }
    for status in [
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
        let label = status.as_str();
        assert!(
            plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&label),
            "attempt status `{label}` is not in the TUI's projected set"
        );
        assert_ne!(
            plakous_host_protocol::views::status_glyph(label),
            plakous_host_protocol::views::UNKNOWN_STATUS_GLYPH,
            "attempt status `{label}` has no glyph"
        );
    }
    for mode in [
        WorkspaceMode::Inherit,
        WorkspaceMode::Snapshot,
        WorkspaceMode::Fork,
    ] {
        assert!(
            ["inherit", "snapshot", "fork"].contains(&mode.as_str()),
            "workspace modes are inherit|snapshot|fork"
        );
    }
    for tier in [Tier::ReadOnly, Tier::Execute, Tier::Mutate] {
        assert!(
            ["read_only", "execute", "mutate"].contains(&tier.as_str()),
            "tier labels are the wire spelling"
        );
    }
}

/// a pushed `result` lands in the parent's session as one
/// entry retaining a bounded body; the surface owns compact presentation.
#[test]
fn a_pushed_result_retains_its_body_in_one_notice() {
    let envelope = serde_json::json!({
        "purpose": "result",
        "from": "navtree-c1",
        "attempt_id": "navtree-c1-a2",
        "candidate": "cand-0f3a91b2",
        "output": "{\"verdict\":\"ok\"}\nsecond line\nthird line",
    })
    .to_string();
    let row = mail_row(&envelope).unwrap();
    assert_eq!(
        row,
        "result · #c1 · a2 · #0f3a91b2 · {\"verdict\":\"ok\"}\nsecond line\nthird line"
    );
    assert!(row.contains("second line"));
}

#[test]
fn delivered_mail_body_has_a_utf8_safe_two_kib_bound_and_cut_marker() {
    let body = "界".repeat(2000);
    let command = format!(
        "prompt\n\n{}\n\n<child_message message_id=\"m\" from=\"tree-c7\" purpose=\"finding\">\n{body}\n</child_message>",
        crate::runtime::result_mail::CHILD_MAIL_FRAME
    );
    let (_, rows) = split_child_mail(&command).unwrap();
    let carried = rows[0].strip_prefix("finding · #c7 · ").unwrap();
    assert!(carried.len() <= 2048);
    assert!(carried.ends_with("… cut at 2 KiB of 6 KiB"));
    let envelope =
        serde_json::json!({"purpose":"finding", "from":"tree-c7", "body":body}).to_string();
    assert_eq!(mail_row(&envelope).unwrap(), rows[0]);
}

/// A `followup` is mail like any other and says so; it is not a user turn
/// the operator typed.
#[test]
fn a_followup_renders_as_mail_with_its_purpose() {
    let envelope = serde_json::json!({
        "purpose": "followup",
        "from": "navtree-root",
        "body": "also cover the error path",
    })
    .to_string();
    assert_eq!(
        mail_row(&envelope).unwrap(),
        "followup · #root · also cover the error path"
    );
}

/// A body the host did not author is never reshaped: ordinary JSON the
/// child happened to print stays exactly the lines it was.
#[test]
fn an_ordinary_entry_is_never_reshaped_into_a_mail_row() {
    assert!(mail_row("{\"purpose\":\"unrelated\"}").is_none());
    assert!(mail_row("plain text").is_none());
}

/// The reset time and the usage counters live on the
/// attempt, so the row reads them from there or reports nothing.
#[test]
fn a_blocked_attempt_projects_its_reset_and_usage() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    {
        let attempt = tree.attempt_mut_for_test(&spawned.agent_id);
        attempt.status = AttemptStatus::Blocked;
        attempt.blocked = Some(crate::agent_tree::BlockedReason {
            quota: "weekly".to_owned(),
            account: "codex".to_owned(),
            resets_at: Some(1_700_000_000),
        });
        attempt.candidate_id = Some(CandidateId::from_persisted("cand-0f3a91b2".to_owned()));
        attempt.usage = crate::agent_tree::UsageRow {
            elapsed_ms: 91_000,
            turns: 3,
            tool_calls: 11,
            input_tokens: 4_200,
            output_tokens: 800,
        };
    }
    let snap = project_snapshot(&tree);
    let child = snap
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .unwrap();
    assert_eq!(
        child.attempt_status.map(AttemptStatus::as_str),
        Some("blocked")
    );
    assert_eq!(child.attempt_seq, Some(1));
    assert_eq!(child.attempts, vec![1]);
    assert_eq!(
        child.candidate_id.as_ref().map(CandidateId::as_str),
        Some("cand-0f3a91b2")
    );
    let blocked = child.blocked.as_ref().expect("the attempt is blocked");
    assert_eq!(blocked.quota, "weekly");
    assert_eq!(blocked.account, "codex");
    assert_eq!(blocked.resets_at_unix, Some(1_700_000_000));
    let usage = child.usage.expect("the attempt carries usage");
    assert_eq!(usage.elapsed_ms, 91_000);
    assert_eq!(usage.turns, 3);
    assert_eq!(usage.tool_calls, 11);
    assert_eq!(usage.input_tokens, 4_200);
    assert_eq!(usage.output_tokens, 800);
    // The root has no attempt, so it reports nothing rather than zeros.
    let root = &snap.nodes[0];
    assert!(root.usage.is_none());
    assert!(root.blocked.is_none());
    assert_eq!(root.attempt_seq, None);
    assert_eq!(root.attempts, [] as [u32; 0]);
}

/// The row carries the template and the tier, and the
/// workspace mode is one of `inherit|snapshot|fork`.
#[test]
fn the_row_carries_the_tier_and_the_workspace_status() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let snap = project_snapshot(&tree);
    let child = snap
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .unwrap();
    assert_eq!(child.template.as_deref(), Some("review"));
    assert!(
        matches!(
            child.tier.map(Tier::as_str),
            Some("read_only" | "execute" | "mutate")
        ),
        "{:?}",
        child.tier
    );
    assert!(
        ["inherit", "snapshot", "fork"].contains(&child.workspace_mode.as_str()),
        "{:?}",
        child.workspace_mode
    );
    assert!(
        child
            .workspace_status
            .map(WorkspaceStatus::as_str)
            .is_some_and(|status| {
                plakous_host_protocol::views::PROJECTED_STATUS_LABELS.contains(&status)
            }),
        "{:?}",
        child.workspace_status
    );
}

#[test]
fn snapshot_is_preorder_with_revision_and_unread() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let snap = project_snapshot(&tree);
    assert_eq!(snap.tree_id, "navtree");
    assert_eq!(snap.revision, tree.revision);
    assert!(!snap.restored_paused());
    assert_eq!(
        snap.nodes
            .iter()
            .map(|node| node.agent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["root", spawned.agent_id.as_str()]
    );
    assert_eq!(snap.nodes[1].parent_agent_id.as_deref(), Some("root"));
    assert_eq!(snap.nodes[1].depth, 1);
    assert!(!snap.nodes[0].has_session);
    assert_eq!(snap.nodes[0].unread_mail, 0);
}

#[test]
fn projection_carries_class_role_model_turns_and_terminal_state() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let snap = project_snapshot(&tree);
    let child = snap
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .unwrap();
    assert_eq!(child.template.as_deref(), Some("review"));
    assert_eq!(
        child.description,
        tree.node(&spawned.agent_id).unwrap().description
    );
    assert_ne!(child.model_tuple, "");
    assert_eq!(child.turns, Some(1));
    assert!(!child.finished());
    let started = tree.attempt(&spawned.agent_id).unwrap().started_at / 1_000;
    assert_eq!(child.started_at_unix, Some(started));
    assert_eq!(
        child.attempt_id.as_deref(),
        Some(spawned.attempt_id.as_str()),
        "the current attempt is named, so operator state can key on it"
    );
    assert_eq!(child.ended_at_unix, None, "a running attempt has no end");
}

#[test]
fn terminal_nodes_project_terminal_and_survive_the_snapshot() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    tree.close_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: spawned.agent_id.clone(),
            expected_attempt_id: None,
        },
        Some("op-close"),
    )
    .unwrap();
    let snap = project_snapshot(&tree);
    let child = snap
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .expect("a closed node stays on the canvas");
    assert!(child.finished());
    let ended = tree
        .attempt(&spawned.agent_id)
        .unwrap()
        .ended_at
        .map(|ended_at| ended_at / 1_000);
    assert!(ended.is_some(), "closing ends the attempt");
    assert_eq!(child.ended_at_unix, ended);
}

#[test]
fn restored_paused_is_visible() {
    let (_dir, mut tree) = tree();
    tree.spawn_agent("root", &spawn_req(), None).unwrap();
    tree.restore_paused();
    let snap = project_snapshot(&tree);
    assert!(snap.restored_paused());
    assert_eq!(snap.state.as_str(), "restored_paused");
    assert_eq!(snap.nodes[0].status.as_str(), "paused");
}

/// A reopened singleton has nothing to resume, so the canvas never paints
/// it paused and never offers `r`.
#[test]
fn a_reopened_singleton_is_not_painted_paused() {
    let (_dir, mut tree) = tree();
    tree.restore_paused();
    let snap = project_snapshot(&tree);
    assert!(!snap.restored_paused());
    assert_eq!(snap.state.as_str(), "live");
}

/// The canvas `Bind` line read
/// `claude-opus-5 · low` while the header read `codex · gpt-5.6-luna · low`.
/// A durable tree is shared across a rebind lineage, so its root record
/// carried the successor's binding; the canvas is showing *this* session.
#[test]
fn the_canvas_root_bind_follows_the_attached_session() {
    let (_dir, mut tree) = tree();
    // A rebind installed a successor epoch, so the shared tree records the
    // successor's opus binding — correct for the tree, wrong for a resumed
    // source session.
    tree.begin_successor_epoch(crate::orchestration::OrchestrationSnapshot::freeze(
        crate::orchestration::RosterInput {
            workspace_idle_seconds: 600,
            workspace_budget_mib: 4096,
            child_retention_days: 7,
            child_retention_count: 128,
            root: crate::orchestration::RootBind {
                delegation: crate::agent_tree_protocol::Delegation::default(),
                template: None,
                account: "claude".to_owned(),
                tuple_id: "claude-opus-5".to_owned(),
                effort: Some("low".to_owned()),
            },
            bind_hints: crate::orchestration::BindHints::from_names(
                &[
                    "codex",
                    "chatgpt",
                    "grok",
                    "supergrok",
                    "xai",
                    "xai-oauth",
                    "claude",
                    "openrouter",
                ],
                &["openrouter"],
            ),
            ..crate::orchestration::RosterInput::unpinned("claude-opus-5")
        },
    ))
    .expect("a quiescent tree admits a successor epoch");
    let mut binding = crate::orchestration::RosterBinding::leaf("codex", "gpt-5.6-luna");
    binding.effort = Some("low".to_owned());
    binding.max_tier = Tier::ReadOnly;
    tree.pin_template("review", binding);
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let mut snapshot = project_snapshot(&tree);
    assert_eq!(snapshot.nodes[0].model_tuple, "claude-opus-5");

    let mut session = SessionRecord::new(PathBuf::from("/work/exact"));
    session.account = Some("codex".into());
    session.model = Some("gpt-5.6-luna".into());
    session.effort = Some("low".into());
    let child_before = snapshot
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .cloned()
        .expect("child");
    apply_session_root_binding(&mut snapshot, &session);

    assert_eq!(snapshot.nodes[0].model_account, "codex");
    assert_eq!(snapshot.nodes[0].model_tuple, "gpt-5.6-luna");
    assert_eq!(snapshot.nodes[0].model_effort.as_deref(), Some("low"));
    // A child's binding is its frozen roster pin and is never rewritten.
    let child_after = snapshot
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .expect("child");
    assert_eq!(child_after.model_tuple, child_before.model_tuple);
    assert_eq!(child_after.model_account, child_before.model_account);

    // An unbound session leaves the tree's record alone rather than
    // blanking the row.
    let mut snapshot = project_snapshot(&tree);
    apply_session_root_binding(&mut snapshot, &SessionRecord::new(PathBuf::from("/w")));
    assert_eq!(snapshot.nodes[0].model_tuple, "claude-opus-5");
}

#[test]
fn missing_locator_projects_has_session_false() {
    let (_dir, mut tree) = tree();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let snap = project_snapshot(&tree);
    let child = snap
        .nodes
        .iter()
        .find(|node| node.agent_id == spawned.agent_id)
        .unwrap();
    assert!(!child.has_session);
}

fn session_for(tree_id: &str, workspace: PathBuf) -> SessionRecord {
    let mut session = SessionRecord::new(workspace);
    session.id = tree_id.to_owned();
    session
}

#[tokio::test]
async fn missing_hub_fails_closed_without_load() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let session = session_for(&tree_id, root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::new());
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::OpenTopologyNavigator,
    )
    .await;
    let events = events.lock().unwrap();
    assert!(matches!(
        events.as_slice(),
        [HostEvent::TopologyNavigatorUnavailable { .. }]
    ));
    assert!(crate::topology_runtime::lookup(&tree_id).is_none());
}

/// `ResumeTopologyTree` is an explicit intent, so a hub miss answers it
/// like any other navigator action. It was silent only because attach used
/// to auto-send it, which the topology contract forbids.
#[tokio::test]
async fn missing_hub_resume_tree_reports_unavailable_like_any_other_intent() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let session = session_for(&tree_id, root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::new());
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::ResumeTopologyTree,
    )
    .await;
    let events = events.lock().unwrap();
    assert!(matches!(
        events.as_slice(),
        [HostEvent::TopologyNavigatorUnavailable { .. }]
    ));
}

/// Opening the canvas over a `RestoredPaused` tree paints it paused.
///
/// Spec 02 §5.7: "`RestoredPaused` never auto-resumes", `r` is "the only
/// exit from `RestoredPaused`", §10.4 "do not auto-resume on reattach",
/// Every intent an attach or a reattach can issue leaves the
/// tree paused; only the explicit `ResumeTopologyTree` moves it.
#[tokio::test]
async fn attach_intents_paint_a_restored_paused_tree_and_never_resume_it() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    tree.restore_paused();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let session = session_for(&tree_id, root.path().to_path_buf());

    for action in [
        UserAction::OpenTopologyNavigator,
        UserAction::RefreshTopologyTree,
    ] {
        let events = std::sync::Mutex::new(Vec::new());
        dispatch_captured_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &action,
        )
        .await;
        let captured = events.lock().unwrap();
        let snapshot = captured
            .iter()
            .find_map(|event| match event {
                HostEvent::TopologyNavigatorSnapshot(snapshot) => Some(snapshot),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{action:?} must paint the tree"));
        assert!(
            snapshot.restored_paused(),
            "{action:?} auto-resumed the tree"
        );
        assert_eq!(snapshot.state.as_str(), "restored_paused", "{action:?}");
        assert_eq!(
            crate::topology_runtime::lookup(&tree_id)
                .unwrap()
                .tree_handle_for_test()
                .lock()
                .unwrap()
                .state,
            TreeAdmissionState::RestoredPaused,
            "{action:?} must leave the persisted epoch paused"
        );
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

#[tokio::test]
async fn hub_open_emits_snapshot() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    tree.spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    let revision = tree.revision;
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let session = session_for(&tree_id, root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::new());
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::OpenTopologyNavigator,
    )
    .await;
    {
        let captured = events.lock().unwrap();
        let HostEvent::TopologyNavigatorSnapshot(snap) = &captured[0] else {
            panic!("expected snapshot, got {:?}", captured[0]);
        };
        assert_eq!(snap.tree_id, tree_id);
        assert_eq!(snap.revision, revision);
        assert!(!snap.restored_paused());
    }
    crate::topology_runtime::force_release(&tree_id).await;
}

#[tokio::test]
async fn resume_and_interrupt_resnapshot() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let spawned = tree
        .spawn_agent("root", &spawn_req(), Some("op-spawn"))
        .unwrap();
    tree.restore_paused();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let session = session_for(&tree_id, root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::new());
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::ResumeTopologyTree,
    )
    .await;
    {
        let captured = events.lock().unwrap();
        let snap = captured
            .iter()
            .find_map(|event| match event {
                HostEvent::TopologyNavigatorSnapshot(snap) => Some(snap),
                _ => None,
            })
            .unwrap();
        assert!(!snap.restored_paused());
        assert_eq!(snap.state.as_str(), "live");
        assert!(
            !captured
                .iter()
                .any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(_)))),
            "resuming a paused tree is not a no-op: {captured:?}"
        );
    }
    events.lock().unwrap().clear();
    // A second resume finds the tree live and says so instead of
    // returning silently.
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::ResumeTopologyTree,
    )
    .await;
    {
        let captured = events.lock().unwrap();
        assert!(
            captured.iter().any(|event| matches!(
                event,
                HostEvent::Row(row) if row.text() == Some("agent tree is not paused; nothing to resume")
            )),
            "{captured:?}"
        );
        assert!(
            captured
                .iter()
                .any(|event| matches!(event, HostEvent::TopologyNavigatorSnapshot(_)))
        );
    }
    events.lock().unwrap().clear();
    let revision = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .tree_handle_for_test()
        .lock()
        .unwrap()
        .revision;
    dispatch_captured_action(
        &paths,
        &session,
        |event| events.lock().unwrap().push(event),
        &UserAction::InterruptTopologyNode {
            agent_id: spawned.agent_id.clone(),
        },
    )
    .await;
    {
        let captured = events.lock().unwrap();
        assert!(
            captured
                .iter()
                .any(|event| matches!(event, HostEvent::TopologyNavigatorSnapshot(_))),
            "interrupt must resnapshot, got {captured:?}"
        );
    }
    let _ = revision;
    crate::topology_runtime::force_release(&tree_id).await;
}

#[tokio::test]
async fn refresh_re_emits_the_current_revision_even_when_it_did_not_advance() {
    let root = tempdir().unwrap();
    let tree_id = format!("nav-{}", uuid::Uuid::new_v4().simple());
    let paths = MemoryRows::new();
    let tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let revision = tree.revision;
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let session = session_for(&tree_id, root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::new());
    for _ in 0..2 {
        dispatch_captured_action(
            &paths,
            &session,
            |event| events.lock().unwrap().push(event),
            &UserAction::RefreshTopologyTree,
        )
        .await;
    }
    {
        let captured = events.lock().unwrap();
        let snaps: Vec<&TreeView> = captured
            .iter()
            .filter_map(|event| match event {
                HostEvent::TopologyNavigatorSnapshot(snap) => Some(snap),
                _ => None,
            })
            .collect();
        assert_eq!(snaps.len(), 2, "refresh always answers");
        assert!(snaps.iter().all(|snap| snap.revision == revision));
        assert!(snaps.iter().all(|snap| snap.solicited));
    }
    crate::topology_runtime::force_release(&tree_id).await;
}
struct RetiringChild {
    entered: Arc<tokio::sync::Notify>,
    finish: Arc<tokio::sync::Notify>,
}
impl crate::topology_runtime::LiveChildHandle for RetiringChild {
    fn agent(&self) -> Option<plakous::agent::Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        plakous::model::TurnId::from_str("00000000-0000-0000-0000-000000000001").unwrap()
    }
    fn turn_control(&self) -> plakous::agent::TurnControl {
        panic!("fixture")
    }
    fn follow_up(
        &self,
        _: plakous::agent::CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Err("fixture".into()) })
    }

    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: plakous::agent::CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("fixture".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
        }
        Box::pin(async move {
            self.entered.notify_one();
            self.finish.notified().await;
            crate::topology_runtime::LiveRelease {
                feed: None,
                locator: None,
                failure: Some("release refused".into()),
            }
        })
    }
}

#[tokio::test(start_paused = true)]
async fn navigator_background_join_does_not_refault_a_settled_release() {
    use std::future::Future;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    let root = tempdir().unwrap();
    let mut tree = crate::topology_runtime::test_support::bootstrap(
        "navigator-joiner-race",
        root.path().to_path_buf(),
    );
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.set_active_turn(&child, true).unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        tree,
        crate::topology_runtime::test_support::memory_store(),
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                child.clone(),
                Box::new(RetiringChild {
                    entered: entered.clone(),
                    finish: finish.clone()
                })
            )
            .await
            .is_ok()
    );
    let session = session_for("navigator-joiner-race", root.path().to_path_buf());
    let events = Arc::new(Mutex::new(Vec::new()));
    let owner = runtime.clone();
    let id = child.clone();
    let notes = events.clone();
    let pending: std::pin::Pin<Box<dyn Future<Output = ()> + Send>> = Box::pin(async move {
        close_node(
            &session,
            &owner,
            |event| notes.lock().unwrap().push(event),
            &id,
        )
        .await;
    });
    let pending = Arc::new(Mutex::new(Some(pending)));
    let close = pending.clone();
    let weak = Arc::downgrade(&runtime);
    let id = child.clone();
    runtime
        .set_retirement_recovery(Arc::new(move || {
            let owner = weak.upgrade().unwrap();
            // The deadline's early recovery request cannot reopen a live owner.
            if owner.live.try_lock().unwrap().contains_key(&id) {
                return;
            }
            owner
                .mutate_then_emit(false, |tree| {
                    let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
                    tree.recover_host_decision(
                        &id,
                        &attempt,
                        crate::agent_tree::RecoveryResolution::RecoveredWithoutTurn {
                            message: "late release is quiescent".into(),
                        },
                    )
                    .map_err(|error| error.message)?;
                    Ok(())
                })
                .unwrap();
            let pending = close.lock().unwrap().take();
            if let Some(mut close) = pending {
                let mut cx = Context::from_waker(std::task::Waker::noop());
                assert!(matches!(close.as_mut().poll(&mut cx), Poll::Ready(())));
            }
        }))
        .await;
    {
        let mut close = pending.lock().unwrap();
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(close.as_mut().unwrap().as_mut().poll(&mut cx).is_pending());
    }
    entered.notified().await;
    tokio::time::advance(
        plakous_host_protocol::limits::HOST_CANCEL_SETTLE_TIMEOUT
            + plakous_host_protocol::limits::HOST_SESSION_RELEASE_TIMEOUT
            + std::time::Duration::from_secs(1),
    )
    .await;
    tokio::task::yield_now().await;
    finish.notify_one();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(pending.lock().unwrap().is_none());
    runtime
        .read(|tree| {
            assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
            assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Closed);
            assert_eq!(
                tree.recovery_need(&child).unwrap(),
                crate::agent_tree::RecoveryNeed::NotRequired
            );
            assert_eq!(
                tree.pending_result_mail("root"),
                [] as [plakous_recursive_subagents::tree::MailRecord; 0]
            );
        })
        .unwrap();
    assert!(!events.lock().unwrap().iter().any(|event| matches!(event, ContentEvent::<TestExtension>::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: message, .. })) if message.contains("owner retained"))));
}

#[tokio::test]
async fn interrupt_then_overlapping_close_and_stop_all_return_before_retirement_without_sealing() {
    use crate::occupancy_vcs::fake::{FakeOp, FakeVcs};
    use std::sync::Arc;
    let root = tempdir().unwrap();
    let vcs = Arc::new(FakeVcs::new());
    let mut tree = ProductAgentTree::bootstrap_root_for_test_with(
        vcs.clone(),
        "close-retiring",
        "root",
        "owner",
        root.path().to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let mut binding = crate::orchestration::RosterBinding::leaf("test-account", "selected-tuple");
    binding.max_tier = Tier::Mutate;
    tree.pin_template("implement", binding);
    let child = tree
        .spawn_agent(
            "root",
            &SpawnAgentRequest::test_named(
                "implement",
                Tier::Mutate,
                "implementer",
                "edit the parser",
            ),
            None,
        )
        .unwrap()
        .agent_id;
    tree.set_active_turn(&child, true).unwrap();
    tree.interrupt_agent(
        "root",
        &AttemptScopedRequest {
            agent_id: child.clone(),
            expected_attempt_id: None,
        },
        None,
    )
    .unwrap();
    let runtime = Arc::new(ProductTopologyRuntime::new(
        tree,
        crate::topology_runtime::test_support::memory_store(),
        vcs.clone(),
        None,
    ));
    let entered = Arc::new(tokio::sync::Notify::new());
    let finish = Arc::new(tokio::sync::Notify::new());
    assert!(
        runtime
            .try_occupy_live(
                child.clone(),
                Box::new(RetiringChild {
                    entered: entered.clone(),
                    finish: finish.clone(),
                })
            )
            .await
            .is_ok()
    );
    let owner = runtime.clone();
    let id = child.clone();
    let cancel = tokio::spawn(async move { owner.cancel_live(&id).await });
    entered.notified().await;
    let session = session_for("close-retiring", root.path().to_path_buf());
    let events = std::sync::Mutex::new(Vec::<ContentEvent<TestExtension>>::new());
    let send = |event| events.lock().unwrap().push(event);
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        close_node(&session, &runtime, send, &child),
    )
    .await
    .unwrap();
    close_node(&session, &runtime, send, &child).await;
    stop_all(&session, &runtime, send).await;
    assert!(
        !cancel.is_finished(),
        "navigator must return while release is blocked"
    );
    assert!(vcs.calls_of(FakeOp::Seal).is_empty());
    assert!(
        runtime
            .read(|tree| tree.node(&child).unwrap().active_turn)
            .unwrap()
    );
    finish.notify_one();
    assert_eq!(
        cancel.await.unwrap().unwrap_err().to_string(),
        "release refused"
    );
    runtime
        .read(|tree| {
            assert_eq!(
                tree.node(&child).unwrap().status,
                NodeStatus::RecoveryRequired
            );
            assert_eq!(tree.attempt(&child).unwrap().status, AttemptStatus::Closed);
            assert!(matches!(
                tree.recovery_need(&child).unwrap(),
                crate::agent_tree::RecoveryNeed::Session { .. }
            ));
            assert_eq!(tree.fault_reason(&child), Some("release refused"));
            assert_eq!(tree.revision, tree.stored_revision);
        })
        .unwrap();
    assert!(vcs.calls_of(FakeOp::Seal).is_empty());
}
