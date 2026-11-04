use super::rows::{attempts_strip, dormancy_fact, live_fact};
use super::*;
use crate::tui::SurfaceIntent;
use crate::tui::status_glyph::UNKNOWN_STATUS_GLYPH;
use plakous_host_protocol::views::{BlockedObservation, PROJECTED_STATUS_LABELS};

fn node(id: &str, parent: Option<&str>, depth: u8) -> CanvasNode {
    CanvasNode {
        agent_id: id.to_owned(),
        parent_agent_id: parent.map(str::to_owned),
        depth,
        status: "running".into(),
        attempt_status: None,
        attempt_seq: None,
        attempt_id: None,
        attempts: Vec::new(),
        tier: None,
        description: "description".into(),
        prompt: "prompt".into(),
        workspace_path: "/tmp".into(),
        workspace_mode: "inherit".into(),
        workspace_status: None,
        candidate_id: None,
        blocked: None,
        usage: None,
        active_turn: false,
        live_progress: None,
        unread_mail: 0,
        has_session: true,
        template: Some("implement".into()),
        model_account: "grok".into(),
        model_tuple: "grok-4.6".into(),
        model_effort: Some("low".into()),
        turns: Some(1),
        started_at_unix: None,
        ended_at_unix: None,
        finished: false,
        fault: None,
        waiting_on: None,
        dormancy: plakous_host_protocol::views::NodeDormancy::default(),
    }
}

#[test]
fn dormancy_reasons_keep_priority_and_the_inspector_lists_every_fact() {
    use plakous_host_protocol::views::NodeDormancy;
    let mut node = node("c1", Some("root"), 1);
    node.dormancy = NodeDormancy {
        parked: true,
        conversation_retired: true,
        workspace_reclaimed: true,
        closed: true,
    };
    node.waiting_on = Some(vec!["p3".into(), "c2".into()]);
    node.live_progress = Some(plakous_host_protocol::views::ChildProgress {
        activity: Some("reading".into()),
        ..plakous_host_protocol::views::ChildProgress::default()
    });
    node.active_turn = true;
    assert_eq!(live_fact(&node, 0), "reading", "in-flight work wins");
    node.live_progress = None;
    assert_eq!(live_fact(&node, 0), "closed");
    node.dormancy.closed = false;
    assert_eq!(live_fact(&node, 0), "retired conversation");
    node.dormancy.conversation_retired = false;
    assert_eq!(live_fact(&node, 0), "reclaimed checkout");
    node.dormancy.workspace_reclaimed = false;
    assert_eq!(live_fact(&node, 0), "waiting on p3, c2");
    node.dormancy = NodeDormancy {
        conversation_retired: true,
        workspace_reclaimed: true,
        ..NodeDormancy::default()
    };
    let inspector = dormancy_inspector(&node);
    assert!(inspector.contains("retired conversation"), "{inspector}");
    assert!(inspector.contains("reclaimed checkout"), "{inspector}");
    assert!(inspector.contains("cannot revive"), "{inspector}");
    node.dormancy = NodeDormancy {
        workspace_reclaimed: true,
        ..NodeDormancy::default()
    };
    assert!(dormancy_inspector(&node).contains("can revive"));
    let mut missing = node.clone();
    missing.has_session = false;
    missing.dormancy = NodeDormancy::default();
    assert!(dormancy_fact(&missing).is_none());
}

#[test]
fn a_parked_child_reads_what_it_waits_on() {
    let mut parked = node("c1", Some("root"), 1);
    parked.status = "running".to_owned();
    parked.dormancy.parked = true;
    parked.waiting_on = Some(vec!["p3".to_owned(), "c2".to_owned()]);
    assert_eq!(live_fact(&parked, 0), "waiting on p3, c2");
    parked.waiting_on = Some(Vec::new());
    assert_eq!(live_fact(&parked, 0), "parked");
    // The cue is the row's right-aligned status column.
    let clock = AgentRowClock {
        now_unix: 0,
        spinner: None,
        profile: None,
    };
    let row = agent_row(&parked, "", false, &clock, 100).body;
    assert!(row.ends_with(" parked"), "{row:?}");
}

/// Spec 02 §6.1: a template the session's profile carries reads as its
/// role; any other template in full; the description is not on the row
/// but heads the inspector.
#[test]
fn a_row_names_the_profile_role_and_leaves_the_description_to_the_inspector() {
    let role = |label: &str| crate::tui::ProfileRole {
        label: label.to_owned(),
        account: "claude".into(),
        model: "m".into(),
        effort: "default".into(),
        fast: false,
        depth: 0,
    };
    let profile = crate::tui::ProfileRow {
        id: "my-profile".into(),
        differences: Vec::new(),
        aliases: Vec::new(),
        root: role("root"),
        cards: vec![role("staff-worker")],
    };
    let mut staff = node("c23", Some("root"), 1);
    staff.template = Some("my-profile-staff-worker".into());
    staff.usage = Some(plakous_host_protocol::views::AttemptUsage {
        elapsed_ms: 0,
        turns: 3,
        tool_calls: 0,
        input_tokens: 12_000,
        output_tokens: 400,
        compaction: None,
    });
    let mut outside = node("c24", Some("root"), 1);
    outside.template = Some("my-profile-reviewer".into());
    let processes = BTreeMap::from([("c23".to_owned(), 2)]);
    let clock = AgentRowClock {
        now_unix: 100,
        spinner: None,
        profile: Some(&profile),
    };
    let row = agent_row(&staff, "", false, &clock, 120).body;
    assert!(
        row.contains("#c23  staff-worker") && row.trim_end().ends_with("running"),
        "{row}"
    );
    assert!(!row.contains("grok-4.6"), "{row}");
    assert!(!row.contains("12.4k"), "{row}");
    assert!(!row.contains("process"), "{row}");
    assert!(!row.contains("description"), "{row}");
    let row = agent_row(&outside, "", false, &clock, 120).body;
    assert!(row.contains("  my-profile-reviewer  "), "{row}");
    let without = AgentRowClock {
        profile: None,
        ..clock
    };
    let row = agent_row(&staff, "", false, &without, 120).body;
    assert!(row.contains("  my-profile-staff-worker  "), "{row}");

    let mut nav = TreeNavigator::awaiting();
    nav.apply_snapshot(snapshot(vec![node("root", None, 0), staff]));
    nav.focus_on("c23");
    let frame = nav.frame(120, 30, None, &processes, Some(&profile));
    let inspector = frame.inspector.join("\n");
    assert!(inspector.contains("· description ·"), "{inspector}");
    assert!(inspector.contains("grok-4.6"), "{inspector}");
    assert!(inspector.contains("1 turn"), "{inspector}");
    assert!(inspector.contains("12000 in"), "{inspector}");
    assert!(inspector.contains("2 processes"), "{inspector}");
}

fn snapshot(nodes: Vec<CanvasNode>) -> CanvasTree {
    CanvasTree {
        tree_id: "tree1".into(),
        revision: 3,
        state: "live".into(),
        solicited: false,
        nodes,
    }
}

#[test]
fn visible_row_navigation_stops_and_left_right_follow_ancestry() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
        node("b", Some("root"), 1),
        node("a1", Some("a"), 2),
    ]));
    nav.move_selection(1);
    assert_eq!(nav.selected_agent_id(), Some("a"));
    nav.move_selection(1);
    assert_eq!(nav.selected_agent_id(), Some("a1"));
    nav.select_parent();
    assert_eq!(nav.selected_agent_id(), Some("a"));
    nav.select_child();
    assert_eq!(nav.selected_agent_id(), Some("a1"));
    nav.focus_on("root");
    nav.move_selection(-1);
    assert_eq!(nav.selected_agent_id(), Some("root"));
    nav.focus_on("b");
    nav.select_child();
    assert_eq!(
        nav.selected_agent_id(),
        Some("b"),
        "a leaf cannot descend into a sibling's child"
    );
    nav.focus_on("root");
    nav.select_parent();
    assert_eq!(nav.selected_agent_id(), Some("root"));
}

#[test]
fn enter_shows_the_selected_transcript_and_esc_dismisses_tree() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("agent-c3", Some("root"), 1),
    ]));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Enter),
        TopologyNavigatorOutcome::ShowRoot
    );
    nav.focus_on("agent-c3");
    let TopologyNavigatorOutcome::ShowAgent { agent_id, label } =
        nav.handle_key(TopologyNavigatorKey::Enter)
    else {
        panic!("Enter on an agent row shows its transcript");
    };
    assert_eq!(agent_id, "agent-c3");
    assert!(label.starts_with("#c3 \u{b7} "), "{label}");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Escape),
        TopologyNavigatorOutcome::Dismiss
    );
}

#[test]
fn destructive_keys_ask_for_confirmation_and_never_act() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
    ]));
    nav.focus_on("a");
    for (key, action) in [
        (
            TopologyNavigatorKey::Interrupt,
            crate::tui::UserAction::InterruptTopologyNode {
                agent_id: "a".into(),
            },
        ),
        (
            TopologyNavigatorKey::Close,
            crate::tui::UserAction::CloseTopologyNode {
                agent_id: "a".into(),
            },
        ),
        (
            TopologyNavigatorKey::StopAll,
            crate::tui::UserAction::StopAllTopology,
        ),
    ] {
        assert_eq!(
            nav.handle_key(key),
            TopologyNavigatorOutcome::Confirm(action)
        );
        // The navigator holds no arm: a following Enter opens the
        // transcript, as it always does.
        assert!(matches!(
            nav.handle_key(TopologyNavigatorKey::Enter),
            TopologyNavigatorOutcome::ShowAgent { .. }
        ));
    }
}

#[test]
fn apply_snapshot_preserves_selection_by_id() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
    ]));
    nav.focus_on("a");
    let mut next = snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
        node("b", Some("root"), 1),
    ]);
    next.revision = 4;
    nav.apply_snapshot(next);
    assert_eq!(nav.selected_agent_id(), Some("a"));
    assert_eq!(nav.revision(), Some(4));
}

#[test]
fn a_non_advancing_snapshot_is_dropped() {
    // §5.10: a snapshot whose revision does not advance is dropped. The
    // host dedups too, but an explicit `R` re-pushes the same revision.
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
    ]));
    nav.focus_on("a");

    // Same revision: dropped whole.
    let same = snapshot(vec![node("root", None, 0)]);
    assert_eq!(same.revision, 3);
    nav.apply_snapshot(same);
    assert_eq!(nav.revision(), Some(3));
    assert_eq!(nav.selected_agent_id(), Some("a"), "selection is untouched");

    // An older revision is dropped too.
    let mut older = snapshot(vec![node("root", None, 0)]);
    older.revision = 2;
    nav.apply_snapshot(older);
    assert_eq!(nav.revision(), Some(3));

    // An advancing revision is applied.
    let mut newer = snapshot(vec![node("root", None, 0), node("a", Some("root"), 1)]);
    newer.revision = 4;
    nav.apply_snapshot(newer);
    assert_eq!(nav.revision(), Some(4));
}

#[test]
fn progress_only_snapshot_replaces_observations() {
    let mut row = node("root", None, 0);
    row.live_progress = Some(plakous_host_protocol::views::ChildProgress {
        turn_id: "turn-1".into(),
        ..Default::default()
    });
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![row.clone()]));
    row.live_progress.as_mut().unwrap().tool_calls = 2;
    nav.apply_snapshot(snapshot(vec![row]));
    assert_eq!(
        nav.selected_node()
            .unwrap()
            .live_progress
            .as_ref()
            .unwrap()
            .tool_calls,
        2
    );
}

#[test]
fn awaiting_esc_dismisses() {
    let mut nav = TreeNavigator::awaiting();
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Escape),
        TopologyNavigatorOutcome::Dismiss
    );
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Enter),
        TopologyNavigatorOutcome::Consumed
    );
}

fn terminal_node(id: &str, parent: Option<&str>, depth: u8, status: &str) -> CanvasNode {
    let mut node = node(id, parent, depth);
    node.status = status.to_owned();
    node.finished = true;
    // Ended at the navigator's clock, so it is listed as just finished.
    node.ended_at_unix = Some(0);
    node
}

/// root
/// ├── a ── a1, a2
/// └── b ── b1
fn family() -> CanvasTree {
    snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
        node("a1", Some("a"), 2),
        node("a2", Some("a"), 2),
        node("b", Some("root"), 1),
        node("b1", Some("b"), 2),
    ])
}

#[test]
fn arrows_follow_visible_rows_and_outline_relations() {
    let mut nav = TreeNavigator::from_snapshot(family());
    assert_eq!(nav.selected_agent_id(), Some("root"));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Down),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(nav.selected_agent_id(), Some("a"));
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    assert_eq!(nav.selected_agent_id(), Some("a1"));
    let _ = nav.handle_key(TopologyNavigatorKey::Right);
    assert_eq!(nav.selected_agent_id(), Some("a1"));
    let _ = nav.handle_key(TopologyNavigatorKey::Left);
    assert_eq!(nav.selected_agent_id(), Some("a"));
    let _ = nav.handle_key(TopologyNavigatorKey::Right);
    assert_eq!(nav.selected_agent_id(), Some("a1"));
}

#[test]
fn up_and_down_cross_subtrees_in_visible_preorder() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("a2");
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    assert_eq!(nav.selected_agent_id(), Some("b"));
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    assert_eq!(nav.selected_agent_id(), Some("b1"));
    let _ = nav.handle_key(TopologyNavigatorKey::Up);
    assert_eq!(nav.selected_agent_id(), Some("b"));
}

#[test]
fn right_prefers_the_remembered_child_and_stops_at_a_leaf() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("b");
    let _ = nav.handle_key(TopologyNavigatorKey::Right);
    assert_eq!(nav.selected_agent_id(), Some("b1"));
    nav.focus_on("a1");
    let _ = nav.handle_key(TopologyNavigatorKey::Right);
    assert_eq!(nav.selected_agent_id(), Some("a1"));
}

#[test]
fn left_climbs_by_parent_and_stops_at_a_root() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        node("a", Some("root"), 1),
        node("b", Some("root"), 1),
        node("a1", Some("a"), 2),
        // The parent id is not in the snapshot, so the layout severs the
        // edge and promotes this node to a root on depth 0.
        node("stray", Some("missing"), 0),
    ]));
    nav.focus_on("a1");
    let _ = nav.handle_key(TopologyNavigatorKey::Left);
    assert_eq!(nav.selected_agent_id(), Some("a"));
    let _ = nav.handle_key(TopologyNavigatorKey::Left);
    assert_eq!(nav.selected_agent_id(), Some("root"));
    nav.focus_on("stray");
    let _ = nav.handle_key(TopologyNavigatorKey::Left);
    assert_eq!(nav.selected_agent_id(), Some("stray"));
}

#[test]
fn home_returns_to_the_root() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("b1");
    let _ = nav.handle_key(TopologyNavigatorKey::Home);
    assert_eq!(nav.selected_agent_id(), Some("root"));
}

#[test]
fn selection_binds_to_the_agent_and_survives_it_leaving_the_list() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        terminal_node("done", Some("root"), 1, "completed"),
        node("live", Some("root"), 1),
    ]));
    assert_eq!(nav.scope, AgentScope::Live);
    nav.focus_on("done");
    assert_eq!(nav.selected_agent_id(), Some("done"));
    // Five minutes on, the finished agent leaves the live list: its
    // nearest listed ancestor stands in, never an unrelated row.
    nav.set_now(RECENT_PAST);
    assert_eq!(nav.selected_agent_id(), Some("root"));
    assert!(
        nav.snapshot
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .any(|node| node.agent_id == "done"),
        "hidden is a projection; the host snapshot still carries it"
    );
    let _ = nav.handle_key(TopologyNavigatorKey::Filter);
    assert_eq!(nav.scope, AgentScope::All);
    assert_eq!(nav.selected_agent_id(), Some("done"), "and it comes back");
    let _ = nav.handle_key(TopologyNavigatorKey::Filter);
    let mut next = nav.snapshot.clone().unwrap();
    next.revision = 9;
    next.nodes.push(node("new", Some("root"), 1));
    nav.apply_snapshot(next);
    nav.focus_on("live");
    let mut fewer = nav.snapshot.clone().unwrap();
    fewer.revision = 10;
    fewer.nodes.retain(|node| node.agent_id != "new");
    nav.apply_snapshot(fewer);
    assert_eq!(
        nav.selected_agent_id(),
        Some("live"),
        "rows leaving do not move it"
    );
}

const RECENT_PAST: u64 = agent_list::RECENT_SECS + 1;

#[test]
fn x_acknowledges_the_selected_attempt_and_waits_for_the_product() {
    let mut failed = terminal_node("tests", Some("root"), 1, "failed");
    failed.attempt_status = Some("failed".into());
    failed.attempt_id = Some("tests-a1".into());
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), failed]));
    nav.set_now(3_600);
    nav.focus_on("tests");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Acknowledge),
        TopologyNavigatorOutcome::Action(crate::tui::UserAction::Extension(
            SurfaceIntent::AcknowledgeAgent {
                tree_id: "tree1".into(),
                agent_id: "tests".into(),
                attempt_id: "tests-a1".into(),
                attention: "failed".into(),
                ended_at: Some(0),
            }
        ))
    );
    assert_eq!(
        nav.selected_agent_id(),
        Some("tests"),
        "no optimistic acknowledgement: the row stays until the product answers"
    );
    let covered = BTreeSet::from([AgentAcknowledgement {
        agent_id: "tests".into(),
        attempt_id: "tests-a1".into(),
        attention: "failed".into(),
        ended_at: Some(0),
    }]);
    nav.set_acknowledged("other-tree", &covered);
    assert_eq!(
        nav.projection().counts.attention,
        1,
        "another tree's set is ignored"
    );
    nav.set_acknowledged("tree1", &covered);
    assert_eq!(nav.projection().counts.attention, 0);
    assert_eq!(nav.selected_agent_id(), Some("root"));
    nav.focus_on("tests");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Acknowledge),
        TopologyNavigatorOutcome::Consumed,
        "an acknowledged attempt is not acknowledged twice"
    );
}

#[test]
fn the_inspector_offers_x_only_when_acknowledgement_can_be_recorded() {
    let mut root = node("root", None, 0);
    root.status = "failed".into();
    root.finished = true;
    let mut bare = node("bare", Some("root"), 1);
    bare.status = "failed".into();
    bare.finished = true;
    bare.ended_at_unix = Some(0);
    let mut failed = terminal_node("tests", Some("root"), 1, "failed");
    failed.attempt_status = Some("failed".into());
    failed.attempt_id = Some("tests-a1".into());
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![root, bare, failed]));
    nav.set_now(10);
    for id in ["root", "bare"] {
        nav.focus_on(id);
        let frame = nav.frame(80, 24, None, &BTreeMap::new(), None);
        assert!(
            frame
                .inspector
                .iter()
                .all(|line| !line.contains("x acknowledges")),
            "{id} cannot be acknowledged: {:?}",
            frame.inspector
        );
    }
    nav.focus_on("tests");
    let frame = nav.frame(80, 24, None, &BTreeMap::new(), None);
    assert!(
        frame
            .inspector
            .iter()
            .any(|line| line.contains("x acknowledges")),
        "{:?}",
        frame.inspector
    );
}

#[test]
fn refresh_emits_the_refresh_intent() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0)]));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Refresh),
        TopologyNavigatorOutcome::Action(crate::tui::UserAction::RefreshTopologyTree)
    );
}

#[test]
fn copy_yields_the_focused_id_on_the_canvas() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("b1");
    assert_eq!(nav.copy_text(), "b1");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Copy),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(nav.selected_agent_id(), Some("b1"));
}

#[test]
fn keys_paint_no_optimistic_transition() {
    let mut nav = TreeNavigator::from_snapshot(family());
    let before = nav.snapshot.clone();
    for key in [
        TopologyNavigatorKey::Interrupt,
        TopologyNavigatorKey::Close,
        TopologyNavigatorKey::StopAll,
        TopologyNavigatorKey::Refresh,
    ] {
        let _ = nav.handle_key(key);
    }
    assert_eq!(nav.snapshot, before);
}

#[test]
fn child_only_revision_reaches_the_canvas() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("b1");
    let mut next = family();
    next.revision = 4;
    for node in &mut next.nodes {
        if node.agent_id == "a1" {
            node.status = "completed".into();
            node.finished = true;
            node.ended_at_unix = Some(0);
        }
    }
    nav.apply_snapshot(next);
    assert_eq!(nav.revision(), Some(4));
    assert_eq!(nav.selected_agent_id(), Some("b1"));
    let frame = paint(&mut nav, 200, 44, 0);
    let painted = painted_text(&frame);
    let a1 = painted
        .lines()
        .find(|line| line.contains("#a1"))
        .expect("a1 is listed");
    assert!(a1.contains("✓      —  #a1  implement"), "{painted}");
    assert!(a1.trim_end().ends_with(" done"), "{painted}");
}

/// The root box follows the header's turn phase, not the tree's
/// permanent `running`.
#[test]
fn root_box_paints_idle_when_no_turn_is_in_flight() {
    let mut nav = TreeNavigator::from_snapshot(family());
    assert!(nav.project_root_turn(false));
    assert!(!nav.project_root_turn(false), "no change, no repaint");
    let frame = paint(&mut nav, 200, 44, 0);
    assert!(frame.inspector[0].contains("idle"), "{:?}", frame.inspector);
    let root = &nav.snapshot.as_ref().unwrap().nodes[0];
    assert!(root.parent_agent_id.is_none());
    assert_eq!(root.status, "idle");
    assert_eq!(status_glyph("idle"), '\u{25cc}');
    assert!(nav.project_root_turn(true));
    assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, "running");
}

/// A faulted child says why in the inspector instead of a brief
/// nobody is executing.
#[test]
fn inspector_shows_the_fault_reason_in_place_of_the_task() {
    let mut child = node("c1", Some("root"), 1);
    child.status = "recovery_required".into();
    child.finished = true;
    child.attempt_id = Some("c1-a1".into());
    child.fault = Some("child c1 could not start: no credential".into());
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    assert!(
        frame.inspector[1].starts_with("Fault   child c1 could not start"),
        "{:?}",
        frame.inspector
    );
}

#[test]
fn agent_rows_and_inspector_show_derived_compaction_counts() {
    let mut child = node("c1", Some("root"), 1);
    child.usage = Some(plakous_host_protocol::views::AttemptUsage {
        compaction: Some(plakous_host_protocol::views::AttemptCompaction {
            committed: 2,
            failed: 1,
            no_eligible: 3,
            tokens_before: Some(900),
            tokens_after: Some(300),
            complete: false,
        }),
        ..Default::default()
    });
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    assert_eq!(frame.inspector.len(), 6, "{:?}", frame.inspector);
    assert!(
        frame.inspector.iter().any(|line| line.starts_with("Usage")
            && line.contains(
                "Compact 2 committed · 1 failed · 3 no-eligible · 900 -> 300 input · retained only"
            )),
        "{:?}",
        frame.inspector
    );
    let painted = painted_text(&frame);
    assert!(
        painted.contains("running") && !painted.contains("compact"),
        "compaction counts stay in the inspector\n{painted}"
    );
}

#[test]
fn canvas_frame_draws_outline_rails_status_inspector_and_footer() {
    let mut nav = TreeNavigator::from_snapshot(family());
    nav.focus_on("a1");
    let frame = paint(&mut nav, 200, 44, 0);
    let painted = painted_text(&frame);
    assert!(painted.contains("├─"), "{painted}");
    assert!(painted.contains("└─"), "{painted}");
    assert!(painted.contains("│ ├─"), "{painted}");
    assert!(painted.contains('\u{25cf}'), "{painted}");
    assert_eq!(frame.inspector.len(), usize::from(INSPECTOR_ROWS));
    assert!(frame.inspector[0].contains("#a1"));
    assert!(frame.inspector[2].contains("grok-4.6"));
    assert!(frame.footer.contains("Enter transcript"));
    assert_eq!(frame.rows.len(), 6);
    assert!(frame.rows.iter().any(|row| row.id == "root"));
    assert!(frame.header.contains("5 active"), "{}", frame.header);
}

#[test]
fn short_overlays_drop_the_inspector() {
    let mut nav = TreeNavigator::from_snapshot(family());
    let frame = paint(&mut nav, 60, 8, 0);
    assert_eq!(frame.inspector, [] as [String; 0]);
    // The header takes a row and the long footer would need more than
    // its two rows at 60 columns, so the short one takes one (§5.5); the
    // list scrolls inside what is left.
    assert!(!frame.footer.contains("interrupt"), "{}", frame.footer);
    assert_eq!(frame.rows.len(), 6);
}

#[test]
fn row_and_inspector_labels_never_invent_state() {
    let mut unknown = node("root", None, 0);
    unknown.turns = None;
    unknown.started_at_unix = None;
    unknown.model_tuple = String::new();
    unknown.model_account = String::new();
    unknown.model_effort = None;
    unknown.prompt = String::new();
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![unknown]));
    let frame = paint(&mut nav, 120, 30, 100);
    assert!(
        painted_text(&frame).contains("\u{25cf}      \u{2014}  #root"),
        "the unknown elapsed reads as a dash: {frame:?}"
    );
    assert!(frame.inspector[1].contains('\u{2014}'));
    assert!(frame.inspector[2].contains('\u{2014}'));
    assert!(frame.inspector[3].contains("turns \u{2014}"));
}

#[test]
fn elapsed_and_turn_labels_format_known_values() {
    assert_eq!(elapsed_label(Some(100), 141), "41s");
    assert_eq!(elapsed_label(Some(100), 234), "2m14s");
    assert_eq!(elapsed_label(Some(100), 4000), "1h05m");
    assert_eq!(elapsed_label(None, 4000), "\u{2014}");
    assert_eq!(turns_label(Some(1)), "1 turn");
    assert_eq!(turns_label(Some(12)), "12 turns");
}

#[test]
fn status_glyphs_follow_the_spec_table() {
    assert_eq!(status_glyph("pending"), '\u{25cb}');
    assert_eq!(status_glyph("running"), '\u{25d0}');
    assert_eq!(status_glyph("completed"), '\u{25cf}');
    assert_eq!(status_glyph("interrupted"), '\u{25a0}');
    assert_eq!(status_glyph("closed"), '\u{25a0}');
    assert_eq!(status_glyph("failed"), '\u{d7}');
}

/// Regression: `status_glyph` must render every lifecycle label
/// and no test failed. Every label the host can project now has an arm, and
/// a new one that does not is a failure here rather than a dot on the
/// canvas nobody reports.
#[test]
fn every_projected_status_label_has_its_own_glyph() {
    for label in PROJECTED_STATUS_LABELS {
        assert_ne!(
            status_glyph(label),
            UNKNOWN_STATUS_GLYPH,
            "`{label}` falls through to the unknown glyph"
        );
    }
    assert_eq!(
        status_glyph("not_a_status"),
        UNKNOWN_STATUS_GLYPH,
        "an unprojected label must still be total, not a panic"
    );
}

/// sealing (a node) and adopting (a workspace row) are two
/// different things in flight and must not read as one another, nor as the
/// `·` both used to fall to.
#[test]
fn sealing_and_adopting_have_distinct_glyphs() {
    let sealing = status_glyph("sealing");
    let adopting = status_glyph("adopting");
    let adopted = status_glyph("adopted");
    let blocked = status_glyph("blocked");
    for (name, glyph) in [
        ("sealing", sealing),
        ("adopting", adopting),
        ("adopted", adopted),
        ("blocked", blocked),
    ] {
        assert_ne!(glyph, UNKNOWN_STATUS_GLYPH, "{name} is still the dot");
        assert_ne!(glyph, status_glyph("running"), "{name} reads as running");
        assert_ne!(
            glyph,
            status_glyph("completed"),
            "{name} reads as completed"
        );
    }
    assert_ne!(sealing, adopting);
    assert_ne!(adopting, adopted);
    assert_ne!(sealing, blocked);
}

/// Plan §F trap 2: the session's turn phase knew only `running`/`idle` and
/// painted them over a root whose durable status the phase cannot see —
/// erasing the lifecycle observation.
#[test]
fn a_sealing_or_adopting_root_is_not_overwritten_by_the_running_projection() {
    for durable in ["sealing", "adopting", "blocked", "paused", "failed"] {
        let mut root = node("root", None, 0);
        root.status = durable.to_owned();
        let mut nav = TreeNavigator::from_snapshot(snapshot(vec![root]));
        assert!(
            !nav.project_root_turn(false),
            "`{durable}` was repainted idle"
        );
        assert!(
            !nav.project_root_turn(true),
            "`{durable}` was repainted running"
        );
        assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, durable);
    }
    // The phase still owns the two labels that mean exactly what it knows.
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0)]));
    assert!(nav.project_root_turn(false));
    assert_eq!(nav.snapshot.as_ref().unwrap().nodes[0].status, "idle");
}

/// the attempt is the unit, and a `followup` re-task opens a
/// fresh one on the same node. The row shows the current attempt.
#[test]
fn a_retasked_node_shows_its_new_attempt() {
    let mut child = node("c1", Some("root"), 1);
    child.attempt_seq = Some(1);
    child.attempt_status = Some("completed".into());
    let first = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    assert_eq!(
        attempt_label(&first.snapshot.as_ref().unwrap().nodes[1]),
        "a1 · completed"
    );

    let mut retasked = node("c1", Some("root"), 1);
    retasked.attempt_seq = Some(2);
    retasked.attempts = vec![1, 2];
    retasked.attempt_status = Some("running".into());
    let mut nav = first;
    let mut next = snapshot(vec![node("root", None, 0), retasked]);
    next.revision = 4;
    nav.apply_snapshot(next);
    let row = &nav.snapshot.as_ref().unwrap().nodes[1];
    assert_eq!(attempt_label(row), "a2 · running");
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    assert!(
        frame.inspector.iter().any(|line| line.contains("a2")),
        "{:?}",
        frame.inspector
    );
    // Both attempts are listed and the current one is marked.
    let work = frame
        .inspector
        .iter()
        .find(|line| line.starts_with("Work"))
        .unwrap();
    assert!(work.contains("a1 \u{2039}a2\u{203a}"), "{work}");
}

/// One attempt is already named on the `Bind` line, so the strip stays out
/// of the way until there is a history to show.
#[test]
fn a_single_attempt_draws_no_strip() {
    let mut child = node("c1", Some("root"), 1);
    child.attempt_seq = Some(1);
    child.attempts = vec![1];
    assert_eq!(attempts_strip(&child), "");
    child.attempts = vec![1, 2, 3];
    child.attempt_seq = Some(3);
    assert_eq!(attempts_strip(&child), "a1 a2 \u{2039}a3\u{203a}");
}

/// A node with no attempt row says so rather than borrowing `a1`.
#[test]
fn a_node_without_an_attempt_says_so() {
    assert_eq!(attempt_label(&node("root", None, 0)), "no attempt");
}

/// a finished mutate/execute child carries the candidate it
/// sealed, and the box says which one instead of an elapsed clock.
#[test]
fn a_sealed_child_names_its_candidate_on_the_box_and_in_the_inspector() {
    let mut child = terminal_node("c1", Some("root"), 1, "completed");
    child.tier = Some("mutate".into());
    child.workspace_mode = "fork".into();
    child.workspace_status = Some("adopted".into());
    child.candidate_id = Some("cand-0f3a91b2".into());
    child.attempt_seq = Some(1);
    child.attempt_status = Some("adopted".into());
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    let painted = painted_text(&frame);
    assert!(painted.contains("adopted · #0f3a91b2"), "{painted}");
    let work = frame
        .inspector
        .iter()
        .find(|line| line.starts_with("Work"))
        .unwrap();
    assert!(work.contains("tier mutate"), "{work}");
    assert!(work.contains("copy adopted"), "{work}");
    assert!(work.contains("sealed · #0f3a91b2"), "{work}");
}

/// a quota refusal marks the *attempt* blocked and the node
/// `Failed`, so the reset time is only readable from the attempt. The row
/// shows how long the wait is, not just that there is one.
#[test]
fn a_blocked_attempt_shows_when_its_quota_resets() {
    let now = 1_000_000_u64;
    let mut child = terminal_node("c1", Some("root"), 1, "failed");
    child.attempt_seq = Some(2);
    child.attempt_id = Some("c1-a2".into());
    child.attempt_status = Some("blocked".into());
    child.blocked = Some(BlockedObservation {
        quota: "weekly".into(),
        account: "codex".into(),
        resets_at_unix: Some(now + 45 * 60),
    });
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, now);
    let painted = painted_text(&frame);
    assert!(painted.contains("blocked · resets in 45m"), "{painted}");
    let work = frame
        .inspector
        .iter()
        .find(|line| line.starts_with("Work"))
        .unwrap();
    assert!(work.contains("blocked weekly on codex"), "{work}");
    assert!(work.contains("resets in 45m"), "{work}");
}

/// Without a `resets_at` the row says `blocked` and stops: the provider
/// named no clock and the canvas does not invent one.
#[test]
fn a_blocked_attempt_without_a_reset_invents_no_clock() {
    let mut child = terminal_node("c1", Some("root"), 1, "failed");
    child.attempt_id = Some("c1-a1".into());
    child.blocked = Some(BlockedObservation {
        quota: "5h".into(),
        account: "grok".into(),
        resets_at_unix: None,
    });
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    let painted = painted_text(&frame);
    assert!(painted.contains("blocked"), "{painted}");
    assert!(!painted.contains("resets in"), "{painted}");
}

/// A reset that has already passed reads `now`, never a wrapped duration.
#[test]
fn a_reset_in_the_past_reads_now() {
    assert_eq!(until_label(10, 500), "now");
    assert_eq!(until_label(530, 500), "30s");
    assert_eq!(until_label(500 + 3600 + 120, 500), "1h02m");
    assert_eq!(until_label(500 + 86_400 * 2, 500), "2d00h");
}

/// the box says which template and which tier, never the
/// retired class or write effect.
#[test]
fn the_row_carries_the_template_and_the_tier() {
    let mut child = node("c1", Some("root"), 1);
    child.template = Some("review".into());
    child.tier = Some("read_only".into());
    child.workspace_mode = "snapshot".into();
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    nav.focus_on("c1");
    let frame = paint(&mut nav, 200, 44, 0);
    let painted = painted_text(&frame);
    assert!(painted.contains("review"), "{painted}");
    let bind = frame
        .inspector
        .iter()
        .find(|line| line.starts_with("Model"))
        .unwrap();
    assert!(bind.contains("snapshot"), "{bind}");
    let work = frame
        .inspector
        .iter()
        .find(|line| line.starts_with("Work"))
        .unwrap();
    assert!(work.contains("tier read_only"), "{work}");
}

/// A tier the host did not project stays unknown; the inspector never
/// defaults a node to `read_only`, which would understate what it may do.
#[test]
fn an_unknown_tier_stays_unknown() {
    let node = node("c1", Some("root"), 1);
    assert!(work_line(&node, 0).contains("tier —"));
}

fn paint(nav: &mut TreeNavigator, width: u16, height: u16, now: u64) -> AgentListFrame {
    nav.set_now(now);
    nav.frame(width, height, None, &BTreeMap::new(), None)
}

fn painted_text(frame: &AgentListFrame) -> String {
    frame
        .rows
        .iter()
        .map(|row| format!("{}{}{}", row.cells.rails, row.cells.body, row.cells.mark))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn orphaned_children_survive_a_hidden_parent() {
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        terminal_node("mid", Some("root"), 1, "closed"),
        node("leaf", Some("mid"), 2),
    ]));
    nav.set_now(10 * 60);
    let frame = paint(&mut nav, 120, 30, 10 * 60);
    assert_eq!(
        frame
            .rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        vec!["root", "mid", "leaf"],
        "a long-closed parent stays, dimmed, to explain its running child"
    );
    assert!(frame.rows[1].dim);
}

fn settled(id: &str, ended: u64, description: &str) -> CanvasNode {
    let mut node = terminal_node(id, Some("root"), 1, "completed");
    node.ended_at_unix = Some(ended);
    node.started_at_unix = Some(ended);
    node.description = description.to_owned();
    node.attempt_id = Some(format!("{id}-a1"));
    node.attempt_status = Some("completed".into());
    node
}

fn opened_history() -> (TreeNavigator, u64) {
    let now = 1_700_000_000_u64;
    let today = settled("c-today", now, "alpha notes");
    let mut earlier = settled("c-old", now.saturating_sub(3 * 86_400), "old notes");
    earlier.status = "failed".into();
    earlier.attempt_status = Some("failed".into());
    let mut unknown = settled("c-unknown", now, "no clock");
    unknown.started_at_unix = None;
    unknown.ended_at_unix = None;
    let running = node("c-run", Some("root"), 1);
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![
        node("root", None, 0),
        today,
        earlier,
        unknown,
        running,
    ]));
    nav.set_zone(&crate::tui::UTC_ZONE);
    nav.toggle_scope();
    assert!(matches!(nav.scope, AgentScope::All));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::History),
        TopologyNavigatorOutcome::Consumed
    );
    (nav, now)
}

#[test]
fn history_groups_by_local_day() {
    let (mut nav, now) = opened_history();
    let frame = paint(&mut nav, 100, 24, now);
    let ids: Vec<&str> = frame.rows.iter().map(|row| row.id.as_str()).collect();
    assert!(
        ids.windows(2)
            .any(|pair| pair == ["", "c-today"] || pair[0].is_empty()),
        "{ids:?}"
    );
    assert!(frame.header.starts_with("History · "));
    assert!(frame.footer.contains("t tree"));
    assert!(!ids.contains(&"c-run"));
    assert!(!ids.contains(&"root"));
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.cells.body.contains("Today"))
    );
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.cells.body.contains("Earlier"))
    );
    assert!(
        frame
            .rows
            .iter()
            .any(|row| row.cells.body.contains("Unknown time"))
    );
    assert!(
        frame
            .rows
            .iter()
            .filter(|row| row.id.is_empty())
            .all(|row| !row.selectable)
    );
    let midnight = nav.age_wake_at.expect("history regroups at local midnight");
    let regrouped = paint(&mut nav, 100, 24, midnight);
    assert!(
        regrouped
            .rows
            .iter()
            .all(|row| !row.cells.body.contains("Today")),
        "a local midnight moves today's rows into Earlier"
    );
}

#[test]
fn history_search_keeps_a_hidden_selection_and_restores_scope() {
    let (mut nav, now) = opened_history();
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    let selected = nav.selected_agent_id().unwrap().to_owned();
    let _ = nav.handle_key(TopologyNavigatorKey::Search);
    assert!(nav.is_searching());
    for ch in ['u', 'r', 't', '!', '/'] {
        nav.push_search(ch);
        assert_eq!(
            nav.handle_key(TopologyNavigatorKey::History),
            TopologyNavigatorOutcome::Consumed
        );
    }
    assert_eq!(nav.search, "urt!/");
    assert!(nav.is_searching());
    nav.paste_search(" notes\nextra");
    assert!(!nav.search.contains('\n'));
    assert!(nav.search.contains("notes"));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Enter),
        TopologyNavigatorOutcome::Consumed
    );
    assert!(!nav.is_searching());
    assert_eq!(nav.selected_agent_id(), Some(selected.as_str()));
    let hidden = paint(&mut nav, 100, 24, now);
    assert!(
        hidden
            .inspector
            .iter()
            .any(|line| line.contains("Hidden by the filter"))
    );
    assert!(
        hidden
            .rows
            .iter()
            .any(|row| row.cells.body.contains("No matches"))
    );
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Escape),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(nav.search, "");
    assert_eq!(nav.selected_agent_id(), Some(selected.as_str()));

    let _ = nav.handle_key(TopologyNavigatorKey::Failures);
    let failures = paint(&mut nav, 100, 24, now);
    let failure_ids: Vec<&str> = failures
        .rows
        .iter()
        .filter(|row| !row.id.is_empty())
        .map(|row| row.id.as_str())
        .collect();
    assert_eq!(failure_ids, ["c-old"]);
    let _ = nav.handle_key(TopologyNavigatorKey::History);
    assert!(!nav.history);
    assert!(matches!(nav.scope, AgentScope::All));
}

#[test]
fn u_recovers_only_a_child_that_still_needs_recovery() {
    let mut child = settled("c-stuck", 10, "stuck");
    child.status = "recovery_required".into();
    child.attempt_status = Some("recovery_required".into());
    let mut nav = TreeNavigator::from_snapshot(snapshot(vec![node("root", None, 0), child]));
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Recover),
        TopologyNavigatorOutcome::Consumed,
        "the root is not recovered from the list"
    );
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Recover),
        TopologyNavigatorOutcome::Confirm(crate::tui::UserAction::RecoverTopologyNode {
            tree_id: "tree1".into(),
            revision: 3,
            agent_id: "c-stuck".into(),
            expected_attempt_id: "c-stuck-a1".into(),
        })
    );
}

#[test]
fn history_esc_clears_a_filter_that_hides_the_selection() {
    let (mut nav, now) = opened_history();
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    let _ = nav.handle_key(TopologyNavigatorKey::Down);
    let selected = nav.selected_agent_id().unwrap().to_owned();
    assert_eq!(selected, "c-old");
    let _ = nav.handle_key(TopologyNavigatorKey::Search);
    nav.paste_search("alpha");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Enter),
        TopologyNavigatorOutcome::Consumed
    );
    let hidden = paint(&mut nav, 100, 24, now);
    assert!(hidden.rows.iter().any(|row| row.id == "c-today"));
    assert!(hidden.rows.iter().all(|row| row.id != selected));
    assert!(
        hidden
            .inspector
            .iter()
            .any(|line| line.contains("Hidden by the filter"))
    );
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Acknowledge),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Recover),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Enter),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(nav.copy_text(), "");
    assert_eq!(
        nav.handle_key(TopologyNavigatorKey::Escape),
        TopologyNavigatorOutcome::Consumed
    );
    assert_eq!(nav.search, "");
    assert_eq!(nav.selected_agent_id(), Some(selected.as_str()));
}

#[test]
fn an_eighty_column_footer_keeps_t_history_on_one_row() {
    let footer = agents_footer_hint(FooterView {
        scope: AgentScope::Live,
        hidden: 0,
        history: false,
        failures_only: false,
        query: "",
        offer_recover: false,
    });
    let rows = wrap_footer_hint(&footer, 80, 3);
    assert!(rows.iter().any(|row| row.contains("t history")), "{rows:?}");
}
