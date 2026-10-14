//! The agent transcript inside Activity (spec 02 §5.3, §5.7): switching,
//! Esc and composer ownership, root isolation and agent call detail.

use super::App;
use super::pilot::PilotAction;
use super::test_support::{
    FIXED_NOW, key, launch_basic, left_click, left_mouse, open_tree, say, serialize_frame,
    tool_item, tree_node, tree_snapshot,
};
use crate::tui::blocks::ScrollBlock;
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, UserAction, user_action_channel};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptRow};
use plakous_host_protocol::views::{
    AgentTranscript, CallDetail, CallDetailOutcome, CallDetailSource, TreeView,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const AGENT: &str = "agent-c3";

fn paint(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    serialize_frame(terminal.backend().buffer())
}

fn plain(app: &App) -> String {
    app.pane.transcript_cache.plain().join("\n")
}

fn agent_rows() -> Vec<TranscriptRow> {
    vec![
        TranscriptRow::user("review the parser"),
        TranscriptRow::Tool(ToolItem {
            subject: Some("cargo test -p parser".into()),
            ..tool_item("call-a1", "exec_command")
        }),
        TranscriptRow::Assistant {
            text: "The parser rejects trailing commas.".into(),
        },
    ]
}

fn root_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.agents_clock = Some(FIXED_NOW);
    for line in 0..30 {
        say(&mut app, &format!("root line {line:02}"));
    }
    app
}

fn transcript(agent: &str, rows: Vec<TranscriptRow>, live: Option<&str>) -> HostEvent {
    HostEvent::AgentTranscript(AgentTranscript {
        agent_id: agent.into(),
        rows,
        live_turn_id: live.map(str::to_owned),
        note: None,
    })
}

/// The agent view of a finished agent: its committed rows, no live turn.
fn open_agent(app: &mut App) {
    open_live_agent(app, None);
}

fn open_live_agent(app: &mut App, live: Option<&str>) {
    app.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    app.apply_host_event(transcript(AGENT, agent_rows(), live));
}

fn sent(app: &mut App) -> Vec<UserAction> {
    let (tx, rx) = user_action_channel();
    app.sync_agent_transcript(Some(&tx));
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|delivery| delivery.into_parts().0)
        .collect()
}

fn family() -> TreeView {
    tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node(AGENT, Some("root"), 1),
    ])
}

/// Root and three agents, the middle one shown.
fn siblings(app: &mut App) {
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("agent-a1", Some("root"), 1),
        tree_node(AGENT, Some("root"), 1),
        tree_node("agent-e5", Some("root"), 1),
    ])));
}

#[test]
fn enter_on_an_agent_row_shows_its_transcript_and_enter_on_root_returns() {
    let mut app = root_app();
    open_tree(&mut app, family());
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert!(app.tree_navigator.is_some(), "Esc can return to this row");
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert!(
        app.pane.blocks.is_empty(),
        "no root row is in the agent pane"
    );
    let screen = paint(&mut app, 120, 40);
    assert!(
        screen.contains("Activity")
            && screen.contains("Agents")
            && screen.contains("root·›")
            && screen.contains("#c3")
            && screen.contains("Esc·back"),
        "{screen}"
    );
    assert!(app.composer_inner.is_none());

    // Esc returns to the list; Enter on the root row closes Activity.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), None);
    app.handle_key(key(KeyCode::Home));
    super::test_support::pass_time(&mut app, 1_000);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.viewed_agent_id(), None);
    assert!(app.activity_tab.is_none());
    assert_eq!(app.pane.blocks.len(), 30);
    assert!(!paint(&mut app, 120, 40).contains("Esc·back"));
}

#[test]
fn enter_asks_the_host_for_the_agent_and_going_back_closes_it() {
    let mut app = root_app();
    open_tree(&mut app, family());
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        sent(&mut app),
        vec![UserAction::OpenAgentTranscript {
            agent_id: AGENT.into()
        }]
    );
    assert!(sent(&mut app).is_empty(), "asked once");
    app.apply_host_event(transcript(AGENT, agent_rows(), None));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(sent(&mut app), vec![UserAction::CloseAgentTranscript]);
}

#[test]
fn a_finished_agents_transcript_renders_through_the_root_renderer() {
    let mut app = root_app();
    open_agent(&mut app);
    paint(&mut app, 120, 40);
    let text = plain(&app);
    for fragment in [
        "review the parser",
        "cargo test -p parser",
        "trailing commas",
    ] {
        assert!(text.contains(fragment), "{fragment}: {text}");
    }
    assert!(!text.contains("root line"), "{text}");

    // A transcript for an agent not on screen is dropped.
    app.apply_host_event(transcript("agent-other", Vec::new(), None));
    assert!(plain(&app).contains("trailing commas"));
}

#[test]
fn cells_have_identical_outlines_in_root_and_agent_transcripts() {
    let rows = vec![
        TranscriptRow::Tool(ToolItem {
            subject: Some("2 tools · 1 failed".into()),
            detail: Some("print('hello')".into()),
            ..tool_item("cell", "exec_py")
        }),
        TranscriptRow::Tool(ToolItem {
            parent_call_id: Some("cell".into()),
            cell_id: Some("cell".into()),
            subject: Some("pwd".into()),
            ..tool_item("child1", "exec_command")
        }),
        TranscriptRow::Tool(ToolItem {
            parent_call_id: Some("cell".into()),
            cell_id: Some("cell".into()),
            subject: Some("missing.rs".into()),
            state: "failed:exit 2".into(),
            ..tool_item("child2", "read_file")
        }),
    ];
    let mut root = root_app();
    root.apply_host_event(super::test_support::restore_snapshot(7, rows.clone()));
    let mut agent = root_app();
    agent.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    agent.apply_host_event(transcript(AGENT, rows, None));
    // Activity adds four cells of panel chrome; compare equal inner widths.
    for width in [120, 40] {
        paint(&mut root, width, 40);
        paint(&mut agent, width + 4, 40);
        assert_eq!(plain(&root), plain(&agent));
        assert_eq!(
            root.pane.transcript_cache.row_plans()[0]
                .activity_action
                .as_deref(),
            Some("CodeMode · 2 calls (1 failed)")
        );
        assert!(plain(&root).contains("$ pwd") && plain(&root).contains("Read missing.rs"));
        root.toggle_tool_expand_at(0);
        agent.toggle_tool_expand_at(0);
        paint(&mut root, width, 40);
        paint(&mut agent, width + 4, 40);
        assert_eq!(plain(&root), plain(&agent));
        assert!(plain(&root).contains("output ▸"));
        root.toggle_tool_expand_at(0);
        agent.toggle_tool_expand_at(0);
    }
}

#[test]
fn a_fail_closed_note_renders_as_a_notice_row() {
    let mut app = root_app();
    app.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    app.apply_host_event(HostEvent::AgentTranscript(AgentTranscript {
        agent_id: AGENT.into(),
        rows: Vec::new(),
        live_turn_id: None,
        note: Some("this agent has no session yet".into()),
    }));
    assert_eq!(
        app.pane.blocks,
        vec![ScrollBlock::System("this agent has no session yet".into())]
    );
}

#[test]
fn a_re_emitted_transcript_binds_the_next_turn() {
    let mut app = root_app();
    open_live_agent(&mut app, Some("t1"));
    app.apply_host_event(child_op(
        AGENT,
        "t1",
        ChildOp::AssistantDelta("first".into()),
    ));
    let mut converged = agent_rows();
    converged.push(TranscriptRow::Assistant {
        text: "first".into(),
    });
    app.apply_host_event(transcript(AGENT, converged.clone(), Some("t2")));
    app.apply_host_event(child_op(
        AGENT,
        "t1",
        ChildOp::AssistantDelta("stale".into()),
    ));
    app.apply_host_event(child_op(
        AGENT,
        "t2",
        ChildOp::AssistantDelta("second".into()),
    ));
    assert_eq!(app.pane.blocks.len(), converged.len() + 1);
    assert_eq!(
        app.pane.blocks.last(),
        Some(&ScrollBlock::Assistant("second".into()))
    );
}

#[test]
fn root_events_land_only_in_the_root_pane_and_show_on_return() {
    let mut app = root_app();
    open_agent(&mut app);
    let agent_blocks = app.pane.blocks.clone();
    say(&mut app, "root reply while away");
    assert_eq!(app.pane.blocks, agent_blocks);
    paint(&mut app, 120, 40);
    assert!(!plain(&app).contains("root reply while away"));
    assert!(plain(&app).contains("trailing commas"), "{}", plain(&app));

    // Esc returns to the Agents list; the root transcript stays behind it.
    app.handle_key(key(KeyCode::Esc));
    paint(&mut app, 120, 40);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert!(!plain(&app).contains("root reply while away"));
    assert!(!plain(&app).contains("trailing commas"));
    // A second Esc closes Activity and shows the root transcript.
    app.handle_key(key(KeyCode::Esc));
    paint(&mut app, 120, 40);
    assert!(app.activity_tab.is_none());
    assert!(plain(&app).contains("root reply while away"));
    assert!(!plain(&app).contains("trailing commas"));
}

#[test]
fn esc_in_the_agent_view_goes_back_and_never_opens_the_stop_confirmation() {
    let mut app = root_app();
    app.busy = true;
    open_agent(&mut app);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert!(app.destructive_confirm.is_none(), "Esc back is not a stop");
    assert!(app.busy);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    // The next Esc leaves Activity for the root, still without a stop.
    app.handle_key(key(KeyCode::Esc));
    assert!(app.activity_tab.is_none());
    assert!(
        app.destructive_confirm.is_none(),
        "closing Activity is not a stop"
    );
    assert!(app.busy);
    // Back on the root, Esc is the root's again.
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(super::destructive_confirm::DestructiveConfirm::kind),
        Some("turn-stop")
    );
}

#[test]
fn back_restores_the_root_scroll_anchor() {
    let mut app = root_app();
    paint(&mut app, 80, 24);
    app.handle_key(key(KeyCode::PageUp));
    let before = paint(&mut app, 80, 24);
    assert!(app.pane.scroll_from_bottom > 0);

    open_agent(&mut app);
    paint(&mut app, 80, 24);
    // Esc returns to the Agents list; the next Esc closes Activity.
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        paint(&mut app, 80, 24),
        before,
        "an idle round trip is exact"
    );

    open_agent(&mut app);
    paint(&mut app, 80, 24);
    say(&mut app, "root reply while away");
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    let after = paint(&mut app, 80, 24);
    // The first painted row carries the position label, which counts the
    // new output; the rows below it are the operator's place.
    let rows = |frame: &str| {
        frame
            .lines()
            .filter(|line| line.contains(" symbols "))
            .skip(1)
            .take(9)
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        rows(&after),
        rows(&before),
        "root output does not move the anchor"
    );
}

#[test]
fn back_follows_the_root_tail_when_it_was_at_the_bottom() {
    let mut app = root_app();
    paint(&mut app, 80, 24);
    assert_eq!(app.pane.scroll_from_bottom, 0);
    open_agent(&mut app);
    paint(&mut app, 80, 24);
    say(&mut app, "root reply while away");
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Esc));
    let screen = paint(&mut app, 80, 24);
    assert_eq!(app.pane.scroll_from_bottom, 0);
    assert!(screen.contains("root·reply·while·away"), "{screen}");
}

type ChildOp = plakous_host_protocol::SessionObservation<plakous_host_protocol::Core>;

fn child_op(agent: &str, turn: &str, op: ChildOp) -> HostEvent {
    HostEvent::ChildLive {
        agent_id: agent.into(),
        turn_id: turn.into(),
        op: Box::new(op),
    }
}

fn running_tool(call_id: &str, subject: &str) -> ChildOp {
    ChildOp::ToolLine {
        call_id: Some(call_id.into()),
        name: "exec_command".into(),
        state: "running".into(),
        subject: Some(subject.into()),
        detail: None,
        parent_call_id: None,
        cell_id: None,
        kind: ActivityKind::Process,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    }
}

#[test]
fn the_open_agents_live_turn_streams_into_its_pane_and_never_the_roots() {
    let mut app = root_app();
    open_live_agent(&mut app, Some("t1"));
    let root_blocks = app.root_pane().blocks.clone();
    let clocks = (app.model_request_started, app.phase_started);
    for op in [
        running_tool("call-a2", "cargo clippy"),
        ChildOp::AssistantDelta("Checking ".into()),
        ChildOp::AssistantDelta("lints.".into()),
        ChildOp::SetBusy(false),
    ] {
        app.apply_host_event(child_op(AGENT, "t1", op));
    }
    assert_eq!(
        app.pane.blocks.last(),
        Some(&ScrollBlock::Assistant("Checking lints.".into()))
    );
    assert!(
        app.pane.blocks.iter().any(|block| matches!(
            block,
            ScrollBlock::Tool { subject: Some(subject), .. } if subject == "cargo clippy"
        )),
        "the running call is an agent row"
    );
    assert_eq!(
        app.root_pane().blocks,
        root_blocks,
        "the root pane is untouched"
    );
    assert_eq!((app.model_request_started, app.phase_started), clocks);
    paint(&mut app, 120, 40);
    assert!(plain(&app).contains("Checking lints."), "{}", plain(&app));
}

#[test]
fn ops_for_another_agent_another_turn_or_a_closed_view_are_dropped() {
    let mut app = root_app();
    open_live_agent(&mut app, Some("t1"));
    let blocks = app.pane.blocks.clone();
    app.apply_host_event(child_op(
        "agent-other",
        "t1",
        ChildOp::AssistantDelta("x".into()),
    ));
    app.apply_host_event(child_op(AGENT, "t0", ChildOp::AssistantDelta("y".into())));
    assert_eq!(app.pane.blocks, blocks);

    app.handle_key(key(KeyCode::Esc));
    let root_blocks = app.pane.blocks.clone();
    app.apply_host_event(child_op(AGENT, "t1", ChildOp::AssistantDelta("z".into())));
    assert_eq!(app.pane.blocks, root_blocks, "a closed view takes nothing");
}

#[test]
fn a_session_switch_closes_the_agent_view() {
    for event in [
        HostEvent::ClearView,
        HostEvent::SessionOpening,
        HostEvent::Binding {
            session_id: Some("s2".into()),
            account: Some("codex".into()),
            model: Some("gpt-5.6-luna".into()),
            effort: Some("low".into()),
            extra: None,
            composition: crate::tui::CompositionSelection::FollowCatalog,
            protocol: crate::tui::ProtocolSelection::Auto,
            effective_presentation: "direct".to_owned(),
        },
    ] {
        let mut app = root_app();
        open_agent(&mut app);
        app.apply_host_event(event);
        assert_eq!(app.viewed_agent_id(), None);
        assert!(app.agent_view().is_none());
    }
}

#[test]
fn tree_snapshots_mid_view_change_nothing() {
    let mut app = root_app();
    open_agent(&mut app);
    let blocks = app.pane.blocks.clone();
    for revision in 4..7 {
        let mut snapshot = family();
        snapshot.revision = revision;
        app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(snapshot));
    }
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert_eq!(app.pane.blocks, blocks);
}

#[test]
fn item_detail_in_the_agent_view_asks_with_its_agent_id_and_takes_the_answer() {
    let mut app = root_app();
    open_agent(&mut app);
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, ScrollBlock::Tool { .. }))
        .expect("the agent's tool row");
    app.toggle_tool_expand_at(idx);
    let (tx, rx) = user_action_channel();
    app.maybe_request_call_details(Some(&tx));
    let asked: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
        .map(|delivery| delivery.into_parts().0)
        .collect();
    assert_eq!(
        asked,
        vec![UserAction::RequestCallDetail {
            call_id: "call-a1".into(),
            agent_id: Some(AGENT.into()),
            session_id: "s1".into(),
        }]
    );
    let answer = |agent: &str, output: &str| {
        HostEvent::CallDetail(CallDetail {
            call_id: "call-a1".into(),
            agent_id: Some(agent.into()),
            session_id: "s1".into(),
            outcome: CallDetailOutcome::Detail {
                input: r#"{"cmd":"cargo test -p parser"}"#.into(),
                output: output.into(),
                input_truncated: false,
                output_truncated: false,
                source: CallDetailSource::Canonical,
            },
        })
    };
    app.apply_host_event(answer("agent-other", "another agent's output"));
    app.apply_host_event(answer(AGENT, "3 passed"));
    paint(&mut app, 120, 40);
    assert!(plain(&app).contains("3 passed"), "{}", plain(&app));
    assert!(!plain(&app).contains("another agent"));
    assert_eq!(app.call_details.waiting(), 0);

    // Closing and reopening the item reads the agent's cached answer.
    app.toggle_tool_expand_at(idx);
    app.toggle_tool_expand_at(idx);
    app.maybe_request_call_details(Some(&tx));
    assert!(rx.try_recv().is_err(), "the cache answers");
}

/// The agent view at both widths and in both themes: breadcrumb on the
/// transcript's top row, the agent's rows through the root's renderer.
#[test]
fn the_agent_view_goldens_cover_both_themes_and_widths() {
    for (theme_name, theme) in [
        ("dark", crate::tui::Theme::dark()),
        ("light", crate::tui::Theme::light()),
    ] {
        for (width, height) in [(80u16, 24u16), (120, 40), (40, 40)] {
            let mut app = root_app();
            app.theme = theme.clone();
            app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(family()));
            open_agent(&mut app);
            let actual = paint(&mut app, width, height);
            let path = format!("tests/goldens/agent-view-{theme_name}-{width}x{height}.frame");
            if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
                std::fs::write(&path, &actual).expect("write reviewed frame golden");
            } else {
                let expected = std::fs::read_to_string(&path).expect("golden frame");
                assert_eq!(actual, expected, "{path}");
            }
        }
    }
}

/// Pilot story: open an agent from the Agents list, watch its live turn,
/// and go back to the root, driven by the semantic pilot's legal actions.
#[test]
fn pilot_story_open_an_agent_watch_it_live_and_go_back() {
    let invoke = |app: &mut App, id: &str| {
        assert!(
            app.legal_pilot_actions()
                .iter()
                .any(|action| action.id == id),
            "{id} is offered"
        );
        app.dispatch_pilot_action(PilotAction::Invoke(id.into()))
            .unwrap();
    };
    let mut app = root_app();
    open_tree(&mut app, family());
    invoke(&mut app, "move-down");
    invoke(&mut app, "accept");
    assert_eq!(
        app.semantic_snapshot().transcript.agent.as_deref(),
        Some(AGENT)
    );
    assert_eq!(
        sent(&mut app),
        vec![UserAction::OpenAgentTranscript {
            agent_id: AGENT.into()
        }]
    );
    app.apply_host_event(transcript(AGENT, agent_rows(), Some("t1")));
    app.apply_host_event(child_op(
        AGENT,
        "t1",
        ChildOp::AssistantDelta("Checking lints.".into()),
    ));
    let watching = app.semantic_snapshot().transcript;
    assert!(watching.streaming);
    assert!(
        watching
            .tail
            .iter()
            .any(|line| line.contains("Checking lints.")),
        "{:?}",
        watching.tail
    );

    invoke(&mut app, "agent-transcript-back");
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(agents_selection(&app), Some(AGENT));
    assert_eq!(sent(&mut app), vec![UserAction::CloseAgentTranscript]);
    app.handle_key(key(KeyCode::Esc));
    assert!(app.activity_tab.is_none());
    let back = app.semantic_snapshot().transcript;
    assert_eq!(back.agent, None);
    assert!(back.tail.iter().any(|line| line.contains("root line 29")));
}

#[test]
fn the_agent_view_has_no_composer_only_its_bar_and_key_guide() {
    let mut app = root_app();
    app.set_input("draft".to_owned());
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(family()));
    open_agent(&mut app);
    let screen = paint(&mut app, 120, 40);
    assert!(!screen.contains("/·commands"), "no composer hint: {screen}");
    assert!(!screen.contains("draft"), "no composer box: {screen}");
    assert!(app.composer_inner.is_none());
    assert!(screen.contains("Esc·back·"), "{screen}");
    assert!(screen.contains("[·]·prev/next·agent"), "{screen}");
    assert!(
        screen.contains("implement"),
        "the Agents row names the agent: {screen}"
    );

    // Printable keys do nothing: there is no composer to type into.
    let blocks = app.pane.blocks.clone();
    for ch in "hey".chars() {
        assert_eq!(app.handle_key(key(KeyCode::Char(ch))), None);
    }
    assert_eq!(app.input, "draft");
    assert_eq!(app.pane.blocks, blocks);
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    // Enter opens a row, never submits the hidden draft.
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    assert_eq!(app.input, "draft");
}

#[test]
fn brackets_show_the_previous_and_next_agent_in_list_order() {
    let mut app = root_app();
    siblings(&mut app);
    open_agent(&mut app);
    let _ = sent(&mut app);
    app.handle_key(key(KeyCode::Char(']')));
    assert_eq!(app.viewed_agent_id(), Some("agent-e5"));
    assert_eq!(
        sent(&mut app),
        vec![UserAction::OpenAgentTranscript {
            agent_id: "agent-e5".into()
        }]
    );
    app.handle_key(key(KeyCode::Char(']')));
    assert_eq!(
        app.viewed_agent_id(),
        Some("agent-e5"),
        "no wrap past the end"
    );
    app.handle_key(key(KeyCode::Char('[')));
    app.handle_key(key(KeyCode::Char('[')));
    assert_eq!(app.viewed_agent_id(), Some("agent-a1"));
    app.handle_key(key(KeyCode::Char('[')));
    assert_eq!(
        app.viewed_agent_id(),
        Some("agent-a1"),
        "the root is not an agent"
    );
    let screen = paint(&mut app, 120, 40);
    assert!(screen.contains("#a1"), "{screen}");
}

#[test]
fn a_click_on_the_agent_bar_opens_agents_at_this_agent() {
    let mut app = root_app();
    siblings(&mut app);
    open_agent(&mut app);
    paint(&mut app, 120, 40);
    let bar = app.agent_bar_hit.expect("the bar is painted");
    let click = |app: &mut App| {
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(MouseButton::Left),
            bar.x + 2,
            bar.y,
        ))
    };
    // Activity is already open: the bar returns to the list at this agent.
    assert_eq!(click(&mut app), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    let nav = app.tree_navigator.as_ref().expect("Agents is open");
    assert_eq!(nav.selected_agent_id(), Some(AGENT));
}

/// The root waits on an approval only its own view answers: the Activity
/// footer says so, in the error colour, ahead of the agent transcript's keys.
#[test]
fn the_key_guide_says_when_the_root_needs_approval() {
    let mut app = root_app();
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(family()));
    open_agent(&mut app);
    assert!(!paint(&mut app, 120, 40).contains("root·needs"));
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: None,
        tool_name: "shell".into(),
        subject: None,
        prompt: None,
        expires_at_ms: 0,
    });
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    let actual = paint(&mut app, 120, 40);
    assert!(actual.contains("root·needs·approval"), "{actual}");
    assert!(actual.contains("Esc·back"), "{actual}");
    let path = "tests/goldens/agent-view-root-approval-dark-120x40.frame";
    if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
        std::fs::write(path, &actual).expect("write reviewed frame golden");
    } else {
        let expected = std::fs::read_to_string(path).expect("golden frame");
        assert_eq!(actual, expected, "{path}");
    }
}

fn thoughtful_rows() -> Vec<TranscriptRow> {
    vec![
        TranscriptRow::Tool(ToolItem {
            subject: Some("a.rs".into()),
            ..tool_item("call-r1", "read_file")
        }),
        TranscriptRow::Thinking(plakous_host_protocol::views::transcript::ThinkingRow {
            location_key: "think-1".into(),
            text: "check the other file".into(),
            elapsed_ms: Some(1_200),
        }),
        TranscriptRow::Tool(ToolItem {
            subject: Some("b.rs".into()),
            ..tool_item("call-r2", "read_file")
        }),
        TranscriptRow::Assistant {
            text: "Both files agree.".into(),
        },
    ]
}

/// Spec 02 §5.4: every family groups sealed thoughts in both panes.
#[test]
fn every_family_groups_thoughts_in_root_and_agent_views() {
    let grouped = "Read 2 files, 1 thought";
    let mut app = root_app();
    app.apply_host_event(super::test_support::restore_snapshot(7, thoughtful_rows()));
    paint(&mut app, 120, 40);
    assert!(plain(&app).contains(grouped), "{}", plain(&app));
    assert!(!plain(&app).contains("◆"), "{}", plain(&app));

    for model in [
        "codex/gpt-5.6-sol",
        "openai/gpt-5.6",
        "claude-opus-5",
        "grok-4.6",
        "openrouter/deepseek",
    ] {
        app.model = Some(model.into());
        paint(&mut app, 120, 40);
        assert!(plain(&app).contains(grouped), "{model}: {}", plain(&app));
        assert!(!plain(&app).contains("◆"), "{model}: {}", plain(&app));
    }

    // Root and agent views share the same grouping rule.
    app.model = Some("gpt-5.6-sol".into());
    let mut agent = tree_node(AGENT, Some("root"), 1);
    agent.model_tuple = "grok-4.6".into();
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        agent.clone(),
    ])));
    app.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    app.apply_host_event(transcript(AGENT, thoughtful_rows(), None));
    paint(&mut app, 120, 40);
    assert!(plain(&app).contains(grouped), "{}", plain(&app));

    agent.model_tuple = "gpt-5.6-sol".into();
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        agent,
    ])));
    paint(&mut app, 120, 40);
    assert!(plain(&app).contains(grouped), "{}", plain(&app));
}

fn open_from_agents(app: &mut App) {
    open_tree(
        app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("agent-a1", Some("root"), 1),
            tree_node(AGENT, Some("root"), 1),
            tree_node("agent-e5", Some("root"), 1),
        ]),
    );
    if let Some(nav) = app.tree_navigator.as_mut() {
        nav.focus_on(AGENT);
    }
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert!(app.tree_navigator.is_some());
}

fn agents_selection(app: &App) -> Option<&str> {
    app.tree_navigator
        .as_ref()
        .and_then(|nav| nav.selected_agent_id())
}

/// Spec 02 §5.3: an agent view opened from Activity > Agents goes back there
/// with the agent selected; Esc there closes Activity to the root.
#[test]
fn esc_from_an_agent_opened_in_agents_goes_back_to_agents_at_that_agent() {
    let mut app = root_app();
    siblings(&mut app);
    open_from_agents(&mut app);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(agents_selection(&app), Some(AGENT));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.activity_tab.is_none() && app.tree_navigator.is_none());
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(app.pane.blocks.len(), 30, "the root transcript is back");
}

#[test]
fn bracket_switching_keeps_the_agents_back_target() {
    let mut app = root_app();
    siblings(&mut app);
    open_from_agents(&mut app);
    app.handle_key(key(KeyCode::Char(']')));
    assert_eq!(app.viewed_agent_id(), Some("agent-e5"));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(agents_selection(&app), Some("agent-e5"));
}

#[test]
fn an_agent_view_opened_directly_still_returns_to_the_agents_list() {
    let mut app = root_app();
    siblings(&mut app);
    open_agent(&mut app);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(agents_selection(&app), Some(AGENT));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.activity_tab.is_none());
    assert_eq!(app.pane.blocks.len(), 30);
}

/// The close glyph and Ctrl+T leave Activity for the root, from the agent
/// transcript as well as from the list.
#[test]
fn close_glyph_and_ctrl_t_close_activity_to_the_root() {
    let mut app = root_app();
    siblings(&mut app);
    open_from_agents(&mut app);
    paint(&mut app, 80, 24);
    let close = app.activity_close_hit.expect("close glyph");
    app.handle_mouse(left_click(close.x, close.y));
    assert!(app.activity_tab.is_none());
    assert!(app.tree_navigator.is_none());
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(app.pane.blocks.len(), 30);
    let screen = paint(&mut app, 80, 24);
    assert!(app.composer_inner.is_some(), "{screen}");
    assert!(!screen.contains("Esc·back"), "{screen}");

    open_from_agents(&mut app);
    assert_eq!(
        app.handle_key(super::test_support::key_ctrl(KeyCode::Char('t'))),
        None
    );
    assert!(app.activity_tab.is_none());
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(app.pane.blocks.len(), 30);
    let screen = paint(&mut app, 80, 24);
    assert!(!screen.contains("Esc·back"), "{screen}");
    assert!(app.composer_inner.is_some());
}

/// Spec 02 §5.7: the agent view offers the Agents tab's per-agent controls
/// for the agent on screen, through the same confirmation pop-up.
#[test]
fn the_agent_view_interrupts_and_closes_its_agent_through_the_pop_up() {
    let mut app = root_app();
    siblings(&mut app);
    open_agent(&mut app);
    // Pinned time: the guards below depend on gaps, not on how fast this
    // host paints.
    super::test_support::pass_time(&mut app, 0);
    assert_eq!(app.handle_key(key(KeyCode::Char('i'))), None);
    let screen = paint(&mut app, 120, 40);
    assert!(screen.contains("Interrupt·#c3?"), "{screen}");
    assert!(screen.contains("[·Cancel·]"), "{screen}");
    // Esc cancels the pop-up; it does not leave the view.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert!(!paint(&mut app, 120, 40).contains("Interrupt·#c3?"));

    // `i` opens again at once, and Enter confirms.
    app.handle_key(key(KeyCode::Char('i')));
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::InterruptTopologyNode {
            agent_id: AGENT.into()
        })
    );
    app.handle_key(key(KeyCode::Char('c')));
    // A letter does not confirm and does not cancel.
    assert_eq!(app.handle_key(key(KeyCode::Char('c'))), None);
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::CloseTopologyNode {
            agent_id: AGENT.into()
        })
    );
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
}

#[test]
fn the_agent_view_acknowledges_its_agents_attention() {
    use plakous_host_protocol::topology::{AttemptStatus, NodeStatus};
    let mut app = root_app();
    let mut failed = tree_node(AGENT, Some("root"), 1);
    failed.status = NodeStatus::Failed;
    failed.attempt_id = Some("agent-c3-a1".into());
    failed.attempt_status = Some(AttemptStatus::Failed);
    failed.ended_at_unix = Some(FIXED_NOW - 60);
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        failed,
    ])));
    open_agent(&mut app);
    let screen = paint(&mut app, 120, 40);
    assert!(screen.contains("x·acknowledge"), "{screen}");
    assert_eq!(
        app.handle_key(key(KeyCode::Char('x'))),
        Some(UserAction::Extension(
            crate::tui::SurfaceIntent::AcknowledgeAgent {
                tree_id: "tree1".into(),
                agent_id: AGENT.into(),
                attempt_id: "agent-c3-a1".into(),
                attention: "failed".into(),
                ended_at: Some(FIXED_NOW - 60),
            }
        ))
    );
}

/// After an agent view, a click on the chat block's
/// bottom lines neither moved the caret nor reached the composer, because
/// the agent bar's last hit rectangle outlived the view.
#[test]
fn composer_clicks_reach_every_draft_line_after_an_agent_view() {
    let drafts = [
        ("two lines", "alpha\nbravo".to_owned()),
        ("five lines", "l1\nl2\nl3\nl4\nl5".to_owned()),
        ("wrapped", "word ".repeat(60)),
        (
            "scrolled",
            (0..30)
                .map(|index| format!("r{index:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
    ];
    for (name, draft) in drafts {
        let mut app = root_app();
        app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(family()));
        open_agent(&mut app);
        paint(&mut app, 80, 24);
        // Back to the Agents list, then out of Activity to the root composer.
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Esc));
        app.set_input(draft.clone());
        app.input_cursor = if name == "scrolled" {
            draft.chars().count()
        } else {
            0
        };
        paint(&mut app, 80, 24);
        let inner = app.composer_inner.expect("composer");
        for offset in 0..inner.height {
            let row = inner.y + offset;
            app.input_cursor = 0;
            let column = inner.x + 3;
            assert_eq!(app.handle_mouse(left_click(column, row)), None);
            app.handle_mouse(left_mouse(
                MouseEventKind::Up(MouseButton::Left),
                column,
                row,
            ));
            let expected = app.composer_index_at(column, row).expect("in the composer");
            assert_eq!(
                app.input_cursor, expected,
                "{name}: a click on composer row {offset} places the caret"
            );
            assert!(
                offset == 0 || expected > 0,
                "{name}: row {offset} maps past the first line"
            );
            assert!(app.tree_navigator.is_none(), "{name}: no Agents popup");
            paint(&mut app, 80, 24);
        }
    }
}

fn raw_rows(app: &mut App, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    let area = buffer.area;
    (area.y..area.bottom())
        .map(|y| {
            (area.x..area.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect()
        })
        .collect()
}

fn agent_with_lines(lines: usize) -> App {
    let mut app = root_app();
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(family()));
    app.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    app.apply_host_event(transcript(
        AGENT,
        (0..lines)
            .map(|i| TranscriptRow::Assistant {
                text: format!("agent line {i:03}"),
            })
            .collect(),
        None,
    ));
    app
}

fn shows_agents_list(app: &mut App) {
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(agents_selection(app), Some(AGENT));
    let screen = paint(app, 120, 40);
    assert!(
        screen.contains("Enter·transcript") && !screen.contains("prev/next"),
        "{screen}"
    );
}

/// The agent body's painted track pages and the thumb drags. Activity owning
/// input must not swallow it, and Menu or a confirmation must.
#[test]
fn agent_body_scrollbar_track_click_pages_and_thumb_drags() {
    for (width, height) in [(80u16, 24u16), (120, 40)] {
        let mut app = agent_with_lines(200);
        raw_rows(&mut app, width, height);
        let bar = app
            .transcript_scrollbar
            .expect("the agent body paints a track");
        let before = app.pane.scroll_from_bottom;
        app.handle_mouse(left_click(bar.area.x, bar.area.y));
        raw_rows(&mut app, width, height);
        assert!(
            app.pane.scroll_from_bottom > before,
            "{width}x{height}: a track click above the thumb did not page ({before} -> {})",
            app.pane.scroll_from_bottom
        );
        assert!(!app.selection.has_range(), "{width}x{height}: it selected");
        assert_eq!(app.viewed_agent_id(), Some(AGENT));

        // A fresh tail: the thumb sits on the last row, so the press grabs it.
        let mut app = agent_with_lines(200);
        raw_rows(&mut app, width, height);
        let bar = app.transcript_scrollbar.expect("track");
        let x = bar.area.x;
        app.handle_mouse(left_click(x, bar.area.bottom() - 1));
        app.handle_mouse(left_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            x,
            bar.area.y,
        ));
        raw_rows(&mut app, width, height);
        assert!(
            app.pane.scroll_from_bottom > 40,
            "{width}x{height}: dragging the thumb up did not move ({})",
            app.pane.scroll_from_bottom
        );
        assert!(!app.selection.has_range());
        let bottom = app
            .transcript_scrollbar
            .expect("track")
            .area
            .bottom()
            .saturating_sub(1);
        app.handle_mouse(left_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            x,
            bottom,
        ));
        raw_rows(&mut app, width, height);
        assert_eq!(
            app.pane.scroll_from_bottom, 0,
            "{width}x{height}: dragging the thumb down did not reach the tail"
        );
    }

    let mut app = agent_with_lines(200);
    raw_rows(&mut app, 80, 24);
    let bar = app.transcript_scrollbar.expect("track");
    let before = app.pane.scroll_from_bottom;
    app.open_control_tab(super::ControlTab::Settings);
    app.handle_mouse(left_click(bar.area.x, bar.area.y));
    assert_eq!(
        app.pane.scroll_from_bottom, before,
        "Menu owns the pointer; the hidden track must not page"
    );
    assert!(app.control_tab.is_some());

    let mut app = agent_with_lines(200);
    raw_rows(&mut app, 80, 24);
    let bar = app.transcript_scrollbar.expect("track");
    app.handle_key(key(KeyCode::Char('i')));
    assert!(app.destructive_confirm.is_some());
    let before = app.pane.scroll_from_bottom;
    app.handle_mouse(left_click(bar.area.x, bar.area.y));
    assert_eq!(
        app.pane.scroll_from_bottom, before,
        "a confirmation owns the pointer; the track must not page"
    );
}

/// The wheel reaches the agent body, and the painted `i interrupt` / `c close`
/// labels open their pop-ups where they are drawn.
#[test]
fn agent_body_wheel_scrolls_and_footer_labels_open_popups() {
    for (width, height) in [(80u16, 24u16), (120, 40), (200, 50)] {
        for approval in [false, true] {
            let mut app = agent_with_lines(200);
            if approval {
                app.apply_host_event(HostEvent::ApprovalRequired {
                    approval_id: "approval-1".into(),
                    call_id: Some("call-1".into()),
                    tool_name: "exec_command".into(),
                    subject: Some("ls".into()),
                    prompt: None,
                    expires_at_ms: 0,
                });
            }
            raw_rows(&mut app, width, height);
            let body = app.transcript_body_area.expect("agent body");
            app.handle_mouse(left_mouse(MouseEventKind::ScrollUp, body.x + 2, body.y + 2));
            assert!(
                app.mouse_scroll_queue != 0,
                "{width}x{height}: wheel did not reach the agent body"
            );
            app.mouse_scroll_queue = 0;
            for (label, interrupt) in [("i interrupt", true), ("c close", false)] {
                let rows = raw_rows(&mut app, width, height);
                let (y, x) = rows
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(y, row)| {
                        row.find(label).map(|byte| (y, row[..byte].chars().count()))
                    })
                    .unwrap_or_else(|| panic!("{width}x{height}: {label} not painted"));
                let x = u16::try_from(x).unwrap();
                let y = u16::try_from(y).unwrap();
                assert_eq!(app.handle_mouse(left_click(x + 1, y)), None);
                let action = app
                    .destructive_confirm
                    .as_ref()
                    .and_then(|confirm| confirm.first_action().cloned());
                assert_eq!(
                    matches!(action, Some(UserAction::InterruptTopologyNode { .. })),
                    interrupt,
                    "{width}x{height} approval={approval}: {label} at ({x},{y}) opened {action:?}"
                );
                assert!(
                    action.is_some(),
                    "{width}x{height} approval={approval}: {label}"
                );
                app.handle_key(key(KeyCode::Esc));
            }
        }
    }
}

/// Tab, Shift+Tab and a tab click leave the transcript. Coming back to Agents
/// shows the list at that agent.
#[test]
fn leaving_agents_closes_the_transcript_and_returning_shows_the_list() {
    let mut shift_tab = key(KeyCode::Tab);
    shift_tab.modifiers = KeyModifiers::SHIFT;
    for leave in [key(KeyCode::Tab), key(KeyCode::BackTab), shift_tab] {
        let mut app = root_app();
        siblings(&mut app);
        open_from_agents(&mut app);
        let _ = sent(&mut app);
        app.handle_key(leave);
        assert_eq!(
            app.activity_tab,
            Some(super::processes::ActivityTab::Processes)
        );
        assert_eq!(app.viewed_agent_id(), None);
        assert_eq!(agents_selection(&app), Some(AGENT));
        assert_eq!(sent(&mut app), vec![UserAction::CloseAgentTranscript]);
        app.handle_key(key(KeyCode::Tab));
        shows_agents_list(&mut app);
        assert!(sent(&mut app).is_empty(), "returning does not reopen it");
    }

    let mut app = root_app();
    siblings(&mut app);
    open_from_agents(&mut app);
    let _ = sent(&mut app);
    raw_rows(&mut app, 120, 40);
    let agents = app.activity_agents_hit.expect("Agents tab");
    assert_eq!(app.handle_mouse(left_click(agents.x + 1, agents.y)), None);
    shows_agents_list(&mut app);
    assert_eq!(sent(&mut app), vec![UserAction::CloseAgentTranscript]);

    open_from_agents(&mut app);
    let _ = sent(&mut app);
    raw_rows(&mut app, 120, 40);
    let processes = app.activity_processes_hit.expect("Processes tab");
    assert_eq!(
        app.handle_mouse(left_click(processes.x + 1, processes.y)),
        None
    );
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Processes)
    );
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(agents_selection(&app), Some(AGENT));
    raw_rows(&mut app, 120, 40);
    let agents = app.activity_agents_hit.expect("Agents tab");
    app.handle_mouse(left_click(agents.x + 1, agents.y));
    shows_agents_list(&mut app);
}

/// Below the modal floor the previous transcript track and wheel rect are gone.
#[test]
fn a_too_small_activity_frame_drops_the_agent_transcript_hits() {
    let mut app = agent_with_lines(200);
    raw_rows(&mut app, 80, 24);
    let body = app.transcript_body_area.expect("agent body");
    let bar = app.transcript_scrollbar.expect("track");
    raw_rows(&mut app, 40, 5);
    assert!(app.transcript_body_area.is_none());
    assert!(app.transcript_scrollbar.is_none());
    assert!(app.activity_agents_hit.is_none());
    assert!(app.activity_close_hit.is_none());
    assert!(app.tree_popup.is_none());
    let before = app.pane.scroll_from_bottom;
    app.handle_mouse(left_mouse(
        MouseEventKind::ScrollUp,
        body.x + 2,
        body.y.saturating_add(1),
    ));
    app.handle_mouse(left_click(bar.area.x, bar.area.y));
    assert_eq!(app.mouse_scroll_queue, 0);
    assert_eq!(app.pane.scroll_from_bottom, before);
}
