//! Topology navigator and Agents list tests.

use super::App;
use super::terminal::dispatch_action;
use super::test_support::{
    compose_transcript_app, key, key_ctrl, launch_basic, open_tree, serialize_frame, tree_node,
    tree_snapshot,
};
use crate::tui::TreeNavigator;
use crate::tui::tree_navigator::RESTORED_PAUSED_NAVIGATOR_HINT;
use crate::tui::{HostEvent, UserAction, user_action_channel};
use crossterm::event::KeyCode;
use plakous_host_protocol::topology::{
    AttemptStatus, CandidateId, NodeStatus, Tier, TreeAdmissionState, WorkspaceMode,
    WorkspaceStatus,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

/// After any `[interrupted]` episode the tree enters
/// `RestoredPaused`, every spawn is refused and the queue stops draining —
/// and the chrome said neither, so a queued follow-up sat unsent while the
/// session looked idle. The §5.4 row now names the state, the exit, and
/// the queue consequence until the host reports the tree resumed.
#[test]
fn a_restored_paused_tree_names_itself_in_the_turn_status_row() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.turn_status_label(), None, "an idle session has no row");

    let mut snapshot = tree_snapshot(vec![tree_node("root", None, 0)]);
    snapshot.state = TreeAdmissionState::RestoredPaused;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(snapshot.clone()));
    assert_eq!(
        app.turn_status_label().as_deref(),
        Some(super::TREE_PAUSED_STATUS),
        "the row states the condition, the exit and the queue consequence"
    );

    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(screen.contains("agents paused"), "{screen}");
    assert!(screen.contains("/tree resume"), "{screen}");
    // No wire value reaches the operator, here or on the queue.
    assert!(!screen.contains("restored_paused"), "{screen}");
    assert!(!screen.contains("ResumeTree"), "{screen}");

    // Resumed: the row goes away again.
    let mut resumed = snapshot;
    resumed.state = TreeAdmissionState::Live;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(resumed));
    assert_eq!(app.turn_status_label(), None);
}

/// After `/resume` of a session whose tree outlived a rebind,
/// the canvas read the binding the tree recorded when the root was minted
/// (`claude-opus-5 · low`) while the header read the session's own
/// (`codex · gpt-5.6-luna · low`). The host is the single authority here:
/// `apply_session_root_binding` repoints the root row before the snapshot
/// is sent (covered in `plakous_hosted::topology_navigator`). The canvas simply
/// paints the binding each projected row carries — root and child alike.
#[test]
fn the_canvas_bind_line_paints_the_projected_binding_per_row() {
    let mut app = App::new(launch_basic(), true);
    app.account = Some("codex".into());
    app.model = Some("gpt-5.6-luna".into());
    app.effort = Some("low".into());

    // The host already repointed the root at the attached session.
    let mut root = tree_node("root", None, 0);
    root.model_tuple = "gpt-5.6-luna".into();
    root.model_account = "codex".into();
    root.model_effort = Some("low".into());
    // The child keeps its frozen roster pin.
    let mut child = tree_node("kid", Some("root"), 1);
    child.model_tuple = "claude-opus-5".into();
    child.model_account = "claude".into();
    child.model_effort = Some("low".into());
    open_tree(&mut app, tree_snapshot(vec![root, child]));

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(screen.contains("gpt-5.6-luna · low"), "{screen}");
    assert!(
        !screen.contains("Bind   claude-opus-5"),
        "the root row paints the session's own binding: {screen}"
    );

    // The child keeps the tree's record: that is its real binding.
    let nav = app.tree_navigator.as_mut().expect("the canvas is open");
    let _ = nav.handle_key(crate::tui::tree_navigator::TopologyNavigatorKey::Down);
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(ratatui::buffer::Cell::symbol)
        .collect::<String>();
    assert!(screen.contains("claude-opus-5 · low"), "{screen}");
}

/// Chrome truncated mid-word instead of wrapping,
/// and the canvas printed the raw host state token as its title.
#[test]
fn canvas_chrome_wraps_its_footer_and_names_its_state_in_prose() {
    for width in [80u16, 120] {
        let mut app = App::new(launch_basic(), true);
        let mut snapshot = tree_snapshot(vec![tree_node("root", None, 0)]);
        snapshot.state = TreeAdmissionState::TreeTerminal;
        open_tree(&mut app, snapshot);

        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let painted: Vec<String> = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let screen = painted.join("\n");

        assert!(
            screen.contains(" Activity ") && screen.contains("tree terminal"),
            "the title and body state read as prose at {width}: {screen}"
        );
        assert!(
            !screen.contains("tree_terminal"),
            "no raw wire token is painted at {width}"
        );
        // The footer's last chord survives: it wrapped instead of being
        // cut off against the right border.
        assert!(
            screen.contains("Esc back"),
            "the whole footer is reachable at {width}: {screen}"
        );
        assert!(
            screen.contains("s stop all"),
            "no chord is cut mid-word at {width}: {screen}"
        );
    }
}

/// Spec 02 §5.5 mouse parity: a click selects an agent row; a double-click
/// does exactly what Enter does on that row.
#[test]
fn an_agent_row_selects_on_click_and_activates_like_enter_on_double_click() {
    let snapshot = || {
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child", Some("root"), 1),
        ])
    };
    let mut by_key = App::new(launch_basic(), true);
    open_tree(&mut by_key, snapshot());
    by_key.tree_navigator.as_mut().unwrap().focus_on("child");
    let entered = by_key.handle_key(key(KeyCode::Enter));

    let mut app = App::new(launch_basic(), true);
    open_tree(&mut app, snapshot());
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let rect = app
        .tree_hits
        .iter()
        .find(|(agent, _)| agent == "child")
        .map(|(_, rect)| *rect)
        .expect("the child row is clickable");
    assert_eq!(
        app.handle_mouse(super::test_support::left_click(rect.x + 2, rect.y)),
        None
    );
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child")
    );
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents),
        "a click only selects"
    );
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.handle_mouse(super::test_support::left_click(rect.x + 2, rect.y)),
        entered
    );
    assert_eq!(app.activity_tab, by_key.activity_tab);
    assert_eq!(
        app.tree_navigator.is_some(),
        by_key.tree_navigator.is_some()
    );
}

#[test]
fn tree_esc_on_tree_focus_dismisses_overlay() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    open_tree(&mut app, tree_snapshot(vec![tree_node("root", None, 0)]));
    assert!(app.tree_navigator.is_some());
    assert!(
        app.handle_key(key(KeyCode::Esc)).is_none(),
        "canvas Esc must not cancel the root turn"
    );
    assert!(app.tree_navigator.is_none());
    assert!(app.busy);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Interrupt)
    );
}

#[test]
fn tree_restored_paused_resume_emits_action() {
    let mut app = App::new(launch_basic(), true);
    let mut snap = tree_snapshot(vec![tree_node("root", None, 0)]);
    snap.state = TreeAdmissionState::RestoredPaused;
    open_tree(&mut app, snap);
    // `r` asks in the pop-up; its button resumes.
    assert_eq!(app.handle_key(key(KeyCode::Char('r'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::ResumeTopologyTree)
    );
}

#[test]
fn tree_close_confirm_emits_action() {
    let mut app = App::new(launch_basic(), true);
    open_tree(
        &mut app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child", Some("root"), 1),
        ]),
    );
    app.tree_navigator.as_mut().unwrap().focus_on("child");
    assert!(app.handle_key(key(KeyCode::Char('c'))).is_none());
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::CloseTopologyNode {
            agent_id: "child".into(),
        })
    );
}

#[test]
fn tree_ctrl_c_cancels_the_surface_and_never_confirms_close() {
    let mut app = App::new(launch_basic(), true);
    open_tree(&mut app, tree_snapshot(vec![tree_node("root", None, 0)]));
    for _ in 0..2 {
        let action = app.handle_key(key_ctrl(KeyCode::Char('c')));
        assert_ne!(
            action,
            Some(UserAction::CloseTopologyNode {
                agent_id: "root".into(),
            }),
            "Ctrl+C cancels the navigator; it never confirms a destructive close"
        );
        assert!(!app.should_quit);
    }
}

#[test]
fn tree_snapshot_ignored_when_overlay_closed_and_live() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
    ])));
    assert!(app.tree_navigator.is_none());
}

#[test]
fn tree_restored_paused_snapshot_stays_on_root_chat() {
    let mut app = App::new(launch_basic(), true);
    let mut snap = tree_snapshot(vec![tree_node("root", None, 0)]);
    snap.state = TreeAdmissionState::RestoredPaused;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(snap));
    assert!(app.tree_navigator.is_none());
    assert_eq!(
        app.status_detail.as_deref(),
        Some(RESTORED_PAUSED_NAVIGATOR_HINT)
    );
}

#[test]
fn attach_requests_open_topology_navigator() {
    let mut app = App::new(launch_basic(), true);
    let (tx, rx) = user_action_channel();
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.maybe_request_tree_snapshot_after_attach(Some(&tx));
    assert!(app.tree_navigator.is_none());
    assert_eq!(
        rx.try_recv().expect("attach should request the navigator"),
        UserAction::OpenTopologyNavigator
    );
    assert!(
        rx.try_recv().is_err(),
        "attach must send neither ResumeTopologyTree nor ResumeQueueDrain"
    );
}

#[test]
fn resume_session_load_requests_open_topology_navigator() {
    let mut app = App::new(launch_basic(), true);
    let (tx, rx) = user_action_channel();
    dispatch_action(
        &mut app,
        &UserAction::ResumeSession {
            id: Some("session-1".into()),
        },
        Some(&tx),
    );
    app.maybe_request_tree_snapshot_after_attach(Some(&tx));
    assert!(app.tree_navigator.is_none());
    assert_eq!(
        rx.try_recv().expect("resume should be queued first"),
        UserAction::ResumeSession {
            id: Some("session-1".into()),
        }
    );
    assert_eq!(
        rx.try_recv()
            .expect("resume attach should request the navigator"),
        UserAction::OpenTopologyNavigator
    );
    assert!(
        rx.try_recv().is_err(),
        "resume attach must not auto-resume the tree or the queue drain"
    );
}

/// Attach never auto-resumes a `RestoredPaused` tree (spec 02 §5.7). `r` on
/// the canvas is its only exit, and it still works.
#[test]
fn a_restored_paused_tree_resumes_only_through_the_navigator() {
    let mut app = App::new(launch_basic(), true);
    let (tx, rx) = user_action_channel();
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.maybe_request_tree_snapshot_after_attach(Some(&tx));
    assert_eq!(
        rx.try_recv().expect("attach requests the navigator"),
        UserAction::OpenTopologyNavigator
    );
    assert!(rx.try_recv().is_err(), "attach resumes nothing");

    let mut snap = tree_snapshot(vec![tree_node("root", None, 0)]);
    snap.state = TreeAdmissionState::RestoredPaused;
    open_tree(&mut app, snap);
    assert_eq!(app.handle_key(key(KeyCode::Char('r'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::ResumeTopologyTree),
        "the navigator's `r`, confirmed, is the explicit exit"
    );
}

/// Spec 02 §5.2: the registry and the input layer are one table. A canvas
/// chord cannot exist off the sheet, and the sheet cannot invent one.
#[test]
fn tree_bindings_match_the_input_layer() {
    let sheet: Vec<&str> = crate::tui::keymap::bindings_for(crate::tui::keymap::When::Tree)
        .iter()
        .filter(|binding| binding.chord != "Tab / Shift+Tab")
        .flat_map(|binding| binding.chord.split(" / "))
        .collect();
    for (token, code, _) in super::TREE_KEYS {
        assert!(
            sheet.contains(token),
            "the canvas consumes {code:?} but the sheet never names `{token}`"
        );
        assert!(
            super::tree_navigator_key(*code).is_some(),
            "{code:?} must dispatch"
        );
    }
    for chord in sheet {
        assert!(
            super::TREE_KEYS.iter().any(|(token, _, _)| *token == chord),
            "the sheet lists `{chord}`, which the canvas does not handle"
        );
    }
}

#[test]
fn tree_unavailable_keeps_activity_open_and_reports_the_reason_in_its_body() {
    let mut app = App::new(launch_basic(), true);
    app.activity_tab = Some(super::processes::ActivityTab::Agents);
    app.tree_navigator = Some(TreeNavigator::awaiting());
    app.apply_host_event(HostEvent::TopologyNavigatorUnavailable {
        reason: "no topology".into(),
    });
    assert!(app.tree_navigator.is_none());
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(app.agents_unavailable.as_deref(), Some("no topology"));
    assert_eq!(app.pane.blocks, [] as [crate::tui::blocks::ScrollBlock; 0]);
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
    ])));
    assert!(app.agents_unavailable.is_none());
    assert_eq!(app.pane.blocks, [] as [crate::tui::blocks::ScrollBlock; 0]);
}

#[test]
fn tree_slash_opens_awaiting_navigator() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/tree".into());
    assert_eq!(
        app.submit_line(false),
        Some(UserAction::OpenTopologyNavigator)
    );
    assert!(app.tree_navigator.is_some());
    assert!(app.tree_navigator.as_ref().unwrap().snapshot.is_none());
}

/// Spec 02 §5.4: at rest the Compose row is its header plus its child tree;
/// one activation adds the program rail, a second returns to rest.
#[test]
fn the_compose_row_rests_on_header_plus_tree_and_toggles_its_program_rail() {
    let mut app = compose_transcript_app();
    let rows = |app: &App| {
        crate::tui::blocks::layout_blocks_with(&app.pane.blocks, 116, app.render_prefs).plain
    };
    let rest = rows(&app);
    assert!(
        rest[0].contains("CodeMode · 3 calls (1 failed)"),
        "{rest:?}"
    );
    assert_eq!(rest.len(), 4);
    app.toggle_tool_expand_at(0);
    let open = rows(&app);
    assert!(open.iter().any(|row| row.contains("const out")), "{open:?}");
    assert!(
        open.iter().any(|row| row.contains("$ cargo test")),
        "{open:?}"
    );
    app.toggle_tool_expand_at(0);
    assert_eq!(rows(&app), rest);
}

fn agents_story(now: u64) -> plakous_host_protocol::views::TreeView {
    let mut root = tree_node("root", None, 0);
    root.description = "the session".into();
    root.started_at_unix = Some(now - 2 * 3600);
    let mut research = tree_node("agent-102", Some("root"), 1);
    research.template = Some("research".into());
    research.attempt_id = Some("agent-102-a1".into());
    research.attempt_status = Some(AttemptStatus::Running);
    research.started_at_unix = Some(now - 492);
    research.turns = Some(8);
    research.live_progress = Some(plakous_host_protocol::views::ChildProgress {
        turn_id: "t-102".into(),
        activity: Some("Searching".into()),
        ..Default::default()
    });
    let mut web = tree_node("agent-105", Some("agent-102"), 2);
    web.template = Some("web".into());
    web.status = NodeStatus::Completed;
    web.attempt_id = Some("agent-105-a1".into());
    web.attempt_status = Some(AttemptStatus::Completed);
    web.started_at_unix = Some(now - 120);
    web.ended_at_unix = Some(now - 54);
    let mut planning = tree_node("agent-98", Some("root"), 1);
    planning.template = Some("planning".into());
    planning.status = NodeStatus::Completed;
    planning.attempt_id = Some("agent-98-a1".into());
    planning.attempt_status = Some(AttemptStatus::Completed);
    planning.started_at_unix = Some(now - 400);
    planning.ended_at_unix = Some(now - 200);
    let mut tests = tree_node("agent-107", Some("root"), 1);
    tests.template = Some("tests".into());
    tests.status = NodeStatus::Failed;
    tests.attempt_id = Some("agent-107-a1".into());
    tests.attempt_status = Some(AttemptStatus::Failed);
    tests.started_at_unix = Some(now - 3600);
    tests.ended_at_unix = Some(now - 3416);
    tests.fault = Some("exit code 1".into());
    let mut stuck = tree_node("agent-110", Some("root"), 1);
    stuck.template = Some("fix".into());
    stuck.status = NodeStatus::RecoveryRequired;
    stuck.attempt_id = Some("agent-110-a1".into());
    stuck.attempt_status = Some(AttemptStatus::RecoveryRequired);
    stuck.started_at_unix = Some(now - 900);
    stuck.ended_at_unix = Some(now - 700);
    let mut old = tree_node("agent-90", Some("root"), 1);
    old.template = Some("survey".into());
    old.status = NodeStatus::Completed;
    old.attempt_id = Some("agent-90-a1".into());
    old.attempt_status = Some(AttemptStatus::Completed);
    old.started_at_unix = Some(now - 4000);
    old.ended_at_unix = Some(now - 3000);
    tree_snapshot(vec![root, research, web, planning, tests, stuck, old])
}

const AGENTS_NOW: u64 = 1_700_000_000;

/// Spec §5.7 Agents body: the live working set with header counts, rails,
/// one-line rows, the attention mark, the inspector and the hidden-agents
/// footer, at every width.
#[test]
fn the_agents_list_goldens_cover_every_width() {
    let (theme_name, theme) = ("dark", crate::tui::Theme::dark());
    for (width, height) in [(120u16, 40u16), (40, 40), (80, 24)] {
        let mut app = App::new(launch_basic(), true);
        app.theme = theme.clone();
        app.agents_clock = Some(AGENTS_NOW);
        app.processes = std::sync::Arc::new(vec![plakous_host_protocol::views::ProcessView {
            id: "p1".into(),
            command: "cargo test".into(),
            owner: "agent-102".into(),
            started_at: AGENTS_NOW - 1,
            elapsed_ms: 1_000,
            status: plakous_host_protocol::views::ProcessStatus::Running,
            output: String::new(),
        }]);
        open_tree(&mut app, agents_story(AGENTS_NOW));
        if let Some(nav) = app.tree_navigator.as_mut() {
            nav.focus_on("agent-107");
        }
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let actual = serialize_frame(terminal.backend().buffer());
        let counts: &[&str] = if width >= 80 {
            &["1·active", "3·completed", "2·attention"]
        } else {
            &["●·1", "✓·3", "⚠·2"]
        };
        for fragment in counts.iter().chain(&["#107"]) {
            assert!(
                actual.contains(fragment),
                "`{fragment}` is missing at {width}"
            );
        }
        assert!(
            !actual.contains("#90"),
            "an agent finished an hour ago is history"
        );
        let path = format!("tests/goldens/agents-live-{theme_name}-{width}x{height}.frame");
        if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
            std::fs::write(&path, &actual).expect("write reviewed frame golden");
        } else {
            let expected = std::fs::read_to_string(&path).expect("golden frame");
            assert_eq!(actual, expected, "{path}");
        }
    }
}

/// `x` asks the product, the product's answer hides the row, and a restart
/// (a fresh App) asks for the persisted set again before painting it.
#[test]
fn acknowledgement_is_the_products_answer_and_is_asked_for_on_every_new_tree() {
    let mut app = App::new(launch_basic(), true);
    app.agents_clock = Some(AGENTS_NOW);
    let (tx, rx) = user_action_channel();
    open_tree(&mut app, agents_story(AGENTS_NOW));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert_eq!(
        rx.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements {
            tree_id: "tree1".into()
        })
    );
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(rx.try_recv().is_err(), "asked once per tree");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    if let Some(nav) = app.tree_navigator.as_mut() {
        nav.focus_on("agent-107");
    }
    let action = app.handle_tree_navigator_key(KeyCode::Char('x'));
    assert_eq!(
        action,
        Some(UserAction::Extension(
            crate::tui::SurfaceIntent::AcknowledgeAgent {
                tree_id: "tree1".into(),
                agent_id: "agent-107".into(),
                attempt_id: "agent-107-a1".into(),
                attention: "failed".into(),
                ended_at: Some(AGENTS_NOW - 3416),
            }
        ))
    );
    let answer = HostEvent::Extension(crate::tui::SurfaceObservation::AgentAcknowledgements {
        tree_id: "tree1".into(),
        attempts: vec![crate::tui::AgentAcknowledgement {
            agent_id: "agent-107".into(),
            attempt_id: "agent-107-a1".into(),
            attention: "failed".into(),
            ended_at: Some(AGENTS_NOW - 3416),
        }],
    });
    app.apply_host_event(answer.clone());
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let nav = app.tree_navigator.as_ref().unwrap();
    assert_eq!(nav.projection().counts.attention, 1);
    assert_eq!(nav.projection().position("agent-107"), None);

    let mut restarted = App::new(launch_basic(), true);
    restarted.agents_clock = Some(AGENTS_NOW);
    open_tree(&mut restarted, agents_story(AGENTS_NOW));
    restarted.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(matches!(
        *rx.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements { .. })
    ));
    restarted.apply_host_event(answer);
    let nav = restarted.tree_navigator.as_ref().unwrap();
    assert_eq!(nav.projection().position("agent-107"), None);
}

/// Regression coverage: one tree carrying every lifecycle
/// §14 observation at once — a sealing node, an adopting copy, an adopted
/// one that names its candidate, and a blocked attempt with its reset —
/// so a glyph or a line silently falling back to `·` moves this frame.
#[test]
fn the_canvas_paints_sealing_adopting_and_blocked_rows() {
    let mut app = App::new(launch_basic(), true);

    let mut sealing = tree_node("sealing", Some("root"), 1);
    sealing.status = NodeStatus::Sealing;
    sealing.template = Some("implement".into());
    sealing.tier = Some(Tier::Mutate);
    sealing.workspace_mode = WorkspaceMode::Fork;
    sealing.workspace_status = Some(WorkspaceStatus::Active);
    sealing.attempt_seq = Some(1);
    sealing.attempt_status = Some(AttemptStatus::Submitted);

    let mut adopting = tree_node("adopting", Some("root"), 1);
    // A node whose candidate is being adopted is still sealing: the adopt is
    // work in flight on it. The fixture used to say `completed` and then set
    // `finished: false` beside it, which the typed view cannot express because
    // it derives `finished` from the status. The canvas paints
    // `adopting` either way — `row_status` prefers the workspace row.
    adopting.status = NodeStatus::Sealing;
    adopting.template = Some("implement".into());
    adopting.tier = Some(Tier::Mutate);
    adopting.workspace_mode = WorkspaceMode::Fork;
    adopting.workspace_status = Some(WorkspaceStatus::Adopting);
    adopting.attempt_seq = Some(2);
    adopting.attempt_status = Some(AttemptStatus::Completed);
    adopting.candidate_id = Some(CandidateId::from_persisted("cand-0f3a91b2".to_owned()));

    let mut blocked = tree_node("blocked", Some("root"), 1);
    blocked.status = NodeStatus::Failed;
    blocked.template = Some("review".into());
    blocked.tier = Some(Tier::ReadOnly);
    blocked.workspace_mode = WorkspaceMode::Inherit;
    blocked.attempt_seq = Some(1);
    blocked.attempt_id = Some("blocked-a1".into());
    blocked.attempt_status = Some(AttemptStatus::Blocked);
    blocked.blocked = Some(plakous_host_protocol::views::BlockedObservation {
        quota: "weekly".into(),
        account: "codex".into(),
        // No reset: the frame is painted against the real clock, so a
        // stamped reset would render a duration that changes by the
        // minute. `blocked · resets in 45m` is pinned in
        // `tree_navigator`'s own tests against a fixed `now`.
        resets_at_unix: None,
    });

    open_tree(
        &mut app,
        tree_snapshot(vec![tree_node("root", None, 0), sealing, adopting, blocked]),
    );
    app.agents_clock = Some(AGENTS_NOW);
    if let Some(nav) = app.tree_navigator.as_mut() {
        nav.focus_on("blocked");
    }

    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let actual = serialize_frame(terminal.backend().buffer());
    // The three observations are legible in the frame, not merely encoded.
    for fragment in [
        "sealing",
        "adopting",
        "0f3a91b2",
        "blocked",
        "blocked·weekly·on·codex",
    ] {
        assert!(actual.contains(fragment), "`{fragment}` is missing");
    }
    assert!(
        !actual.contains("no·attempt"),
        "every child here has an attempt"
    );
    let path = "tests/goldens/agents-sealing-120x40.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, &actual).expect("write reviewed frame golden");
    } else {
        let expected = std::fs::read_to_string(path).expect("golden frame");
        assert_eq!(actual, expected, "{path}");
    }
}

/// The canvas root box tracks the header phase.
#[test]
fn canvas_root_box_follows_the_header_phase() {
    let mut app = App::new(launch_basic(), true);
    open_tree(
        &mut app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("kid", Some("root"), 1),
        ]),
    );
    "idle".clone_into(&mut app.phase);
    app.sync_root_turn_projection();
    let nav = app.tree_navigator.as_mut().expect("the canvas is open");
    assert_eq!(nav.selected_node().unwrap().status, "idle");
    let _ = nav.handle_key(crate::tui::tree_navigator::TopologyNavigatorKey::Down);
    assert_eq!(
        nav.selected_node().unwrap().status,
        "running",
        "children keep the tree's status"
    );
    let _ = nav.handle_key(crate::tui::tree_navigator::TopologyNavigatorKey::Up);
    app.apply_host_event(HostEvent::Status {
        phase: "running".into(),
        queue_len: 0,
        detail: None,
    });
    let nav = app.tree_navigator.as_ref().unwrap();
    assert_eq!(nav.selected_node().unwrap().status, "running");
}

/// A failed acknowledgement read is not an empty set, and the next snapshot
/// asks again because `asked` was cleared.
#[test]
fn a_failed_acknowledgement_read_is_asked_again_on_the_next_snapshot() {
    let mut app = App::new(launch_basic(), true);
    app.agents_clock = Some(AGENTS_NOW);
    let (tx, rx) = user_action_channel();
    open_tree(&mut app, agents_story(AGENTS_NOW));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    let _ = rx.try_recv().unwrap();
    app.apply_host_event(HostEvent::Extension(
        crate::tui::SurfaceObservation::AgentAcknowledgementsUnavailable {
            tree_id: "tree1".into(),
        },
    ));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(
        rx.try_recv().is_err(),
        "clearing the ask does not spin; the next snapshot requests again"
    );
    let mut again = agents_story(AGENTS_NOW);
    again.revision = 4;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(again));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(matches!(
        *rx.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements { .. })
    ));
    app.apply_host_event(HostEvent::Extension(
        crate::tui::SurfaceObservation::AgentAcknowledgements {
            tree_id: "tree1".into(),
            attempts: Vec::new(),
        },
    ));
    app.apply_host_event(HostEvent::Extension(
        crate::tui::SurfaceObservation::AgentAcknowledgementsUnavailable {
            tree_id: "tree1".into(),
        },
    ));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(
        rx.try_recv().is_err(),
        "a failed re-read after a successful answer does not spin"
    );
    let mut later = agents_story(AGENTS_NOW);
    later.revision = 5;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(later));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(
        matches!(
            *rx.try_recv().unwrap(),
            UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements { .. })
        ),
        "a failed read after a successful answer is asked again on the next snapshot"
    );
}

/// An open list does not take the idle cadence. A recent row arms one wake
/// at the sooner of two and five minutes after it ended. A running row keeps
/// the spinner cadence.
#[test]
fn an_open_agents_list_arms_one_age_wake_and_running_rows_keep_the_spinner() {
    use crate::tui::redraw::{RedrawSchedule, RedrawState};

    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.apply_host_event(HostEvent::SetBusy(false));
    let now = AGENTS_NOW;
    app.agents_clock = Some(now);
    let mut done = tree_node("web", Some("root"), 1);
    done.status = NodeStatus::Completed;
    done.attempt_id = Some("web-a1".into());
    done.attempt_status = Some(AttemptStatus::Completed);
    done.started_at_unix = Some(now - 90);
    done.ended_at_unix = Some(now - 30);
    open_tree(
        &mut app,
        tree_snapshot(vec![tree_node("root", None, 0), done]),
    );
    assert!(
        app.redraw_demand().is_deep_idle(),
        "the list must not keep the idle cadence: {:?}",
        app.redraw_demand()
    );
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().age_wake_at,
        Some(now - 30 + 120),
        "dim at two minutes is sooner than leaving at five"
    );

    let mut running = tree_node("kid", Some("root"), 1);
    running.attempt_status = Some(AttemptStatus::Running);
    open_tree(
        &mut app,
        tree_snapshot(vec![tree_node("root", None, 0), running]),
    );
    assert_eq!(
        RedrawSchedule::state(app.redraw_demand()),
        RedrawState::Spinner,
        "a running row keeps the spinner cadence"
    );
}

/// `R` re-reads acknowledgements. A list that was already asked for is not
/// stuck on that answer.
#[test]
fn refresh_asks_for_acknowledgements_again() {
    let mut app = App::new(launch_basic(), true);
    app.agents_clock = Some(AGENTS_NOW);
    let (tx, rx) = user_action_channel();
    open_tree(&mut app, agents_story(AGENTS_NOW));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(matches!(
        *rx.try_recv().unwrap(),
        UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements { .. })
    ));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(rx.try_recv().is_err(), "asked once until refresh");
    let action = app.handle_tree_navigator_key(KeyCode::Char('R'));
    assert!(matches!(action, Some(UserAction::RefreshTopologyTree)));
    app.maybe_request_agent_acknowledgements(Some(&tx));
    assert!(
        matches!(
            *rx.try_recv().unwrap(),
            UserAction::Extension(crate::tui::SurfaceIntent::ListAgentAcknowledgements { .. })
        ),
        "R asks for acknowledgements again"
    );
}

#[test]
fn navigator_snapshot_clears_the_unavailable_reason() {
    let mut app = App::new(launch_basic(), true);
    app.agents_unavailable = Some("no topology".into());
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
    ])));
    assert!(app.agents_unavailable.is_none());
}
