//! Regressions: agent view confirmation
//! safety, Activity's root cues and full-screen floor, stale chrome hits,
//! long tool durations and the agent view's back target.

use super::App;
use super::test_support::{
    FIXED_NOW, key, key_ctrl, launch_basic, left_mouse, open_tree, pin_clock, render_to_string,
    restore_snapshot, say, serialize_frame, tool_item, tool_line, tree_node, tree_snapshot,
};
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, UserAction};
use crossterm::event::{KeyCode, MouseEventKind};
use plakous_host_protocol::views::transcript::{ThinkingRow, ToolItem, TranscriptRow};
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

fn root_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.agents_clock = Some(FIXED_NOW);
    for line in 0..30 {
        say(&mut app, &format!("root line {line:02}"));
    }
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node(AGENT, Some("root"), 1),
    ])));
    app
}

fn open_agent(app: &mut App) {
    app.open_agent_view(AGENT.into(), "#c3 · reviewer".into());
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
}

fn type_text(app: &mut App, text: &str) -> Vec<UserAction> {
    text.chars()
        .filter_map(|ch| app.handle_key(key(KeyCode::Char(ch))))
        .collect()
}

/// The agent view has no composer, so an operator who starts typing there
/// (the root view one Esc away has one) must not close or interrupt the agent
/// on screen. Today `c` arms close, the letters in between are ignored, and
/// the next `c` confirms: typing "check" closes the agent.
#[test]
fn typing_a_word_in_the_agent_view_does_not_close_or_interrupt_the_agent() {
    for word in ["check", "i think so"] {
        let mut app = root_app();
        open_agent(&mut app);
        let sent = type_text(&mut app, word);
        assert!(
            sent.iter().all(|action| !matches!(
                action,
                UserAction::CloseTopologyNode { .. } | UserAction::InterruptTopologyNode { .. }
            )),
            "typing {word:?} sent {sent:?}"
        );
    }
}

/// A `Ctrl` key over the confirmation pop-up cancels it and does nothing
/// else: Ctrl+T does not open Activity under it, and the later Enter (the
/// view's own "expand" key) does not interrupt.
#[test]
fn a_ctrl_key_cancels_the_pop_up_and_does_nothing_else() {
    let mut app = root_app();
    open_agent(&mut app);
    app.handle_key(key(KeyCode::Char('i')));
    assert!(app.destructive_confirm.is_some(), "i opens the pop-up");
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('t'))), None);
    assert!(app.destructive_confirm.is_none(), "Ctrl+T cancels it");
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents),
        "the pop-up ate Ctrl+T; the agent transcript's Activity frame stays"
    );
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        None,
        "Enter after a cancelled pop-up must not interrupt"
    );
}

/// Same class as the agent-bar root cause: the HUD's profile chip rectangle
/// is set by the root view's composer and never cleared by the agent view,
/// so hovering where the chip was paints the profile popup over the agent
/// transcript.
#[test]
fn the_profile_chip_hover_does_not_outlive_the_root_view() {
    let mut app = root_app();
    paint(&mut app, 120, 24);
    let hit = app.composer_chrome.profile_hit.expect("profile chip");
    open_agent(&mut app);
    paint(&mut app, 120, 24);
    app.handle_mouse(left_mouse(MouseEventKind::Moved, hit.x, hit.y));
    let screen = paint(&mut app, 120, 24);
    assert!(
        !screen.contains("Menu·→·Agents"),
        "a profile popup over the agent view: {screen}"
    );
}

/// Full-screen Activity hides the HUD; the agent view (which also hides the
/// composer) says `root needs approval`. Activity says nothing, so a root
/// approval that arrives while the operator reads Agents or Processes goes
/// unnoticed.
#[test]
fn full_screen_activity_says_when_the_root_needs_approval() {
    let mut app = root_app();
    open_tree(
        &mut app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node(AGENT, Some("root"), 1),
        ]),
    );
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: None,
        tool_name: "shell".into(),
        subject: None,
        prompt: None,
        expires_at_ms: 0,
    });
    for (width, height) in [(120, 40), (80, 24)] {
        let screen = paint(&mut app, width, height);
        assert!(
            screen.contains("approval"),
            "{width}x{height}: nothing on screen says the root waits: {screen}"
        );
    }
}

fn thoughtful_rows() -> Vec<TranscriptRow> {
    vec![
        TranscriptRow::Tool(ToolItem {
            subject: Some("a.rs".into()),
            ..tool_item("call-r1", "read_file")
        }),
        TranscriptRow::Thinking(ThinkingRow {
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

fn read(app: &mut App, call_id: &str, state: &str, subject: &str) {
    app.apply_host_event(tool_line(
        call_id,
        None,
        None,
        ActivityKind::Tool,
        "read_file",
        state,
        Some(subject),
    ));
}

/// Every family groups thoughts, with live and restored render parity.
#[test]
fn every_family_and_live_restore_parity() {
    let grouped = "Read 2 files, 1 thought";
    for model in [
        "grok-4.6",
        "grok-code-fast-1",
        "x-ai/grok-4",
        "openrouter/x-ai/grok-4-fast",
        "Grok-4.7",
        "gpt-5.6-sol",
        "anthropic/claude-opus-5",
    ] {
        let mut restored = App::new(launch_basic(), true);
        restored.model = Some(model.into());
        restored.apply_host_event(restore_snapshot(7, thoughtful_rows()));
        paint(&mut restored, 120, 40);
        assert!(plain(&restored).contains(grouped), "{model}");

        let mut live = App::new(launch_basic(), true);
        live.model = Some(model.into());
        read(&mut live, "call-r1", "running", "a.rs");
        read(&mut live, "call-r1", "done", "a.rs");
        live.apply_host_event(HostEvent::ReasoningDelta {
            location_key: "think-1".into(),
            text: "check the other file".into(),
        });
        paint(&mut live, 120, 40);
        live.apply_host_event(HostEvent::ReasoningSealed {
            location_key: "think-1".into(),
        });
        paint(&mut live, 120, 40);
        read(&mut live, "call-r2", "running", "b.rs");
        paint(&mut live, 120, 40);
        read(&mut live, "call-r2", "done", "b.rs");
        paint(&mut live, 120, 40);
        say(&mut live, "Both files agree.");
        paint(&mut live, 120, 40);
        assert!(plain(&live).contains(grouped), "{model}: {}", plain(&live));
        assert!(!plain(&live).contains("◆"), "{model}: {}", plain(&live));
    }
}

fn open_agents_at_agent(app: &mut App) {
    open_tree(
        app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node(AGENT, Some("root"), 1),
            tree_node("agent-e5", Some("root"), 1),
        ]),
    );
    if let Some(nav) = app.tree_navigator.as_mut() {
        nav.focus_on(AGENT);
    }
}

#[test]
fn typing_in_the_agents_tab_sends_nothing_and_keys_inside_the_pop_up_do_nothing() {
    for word in ["check", "i think so"] {
        let mut app = root_app();
        open_agents_at_agent(&mut app);
        let sent = type_text(&mut app, word);
        assert!(
            sent.iter().all(|action| !matches!(
                action,
                UserAction::CloseTopologyNode { .. } | UserAction::InterruptTopologyNode { .. }
            )),
            "typing {word:?} sent {sent:?}"
        );
    }
    let mut app = root_app();
    open_agents_at_agent(&mut app);
    for (open, other) in [('c', 'i'), ('i', 'c'), ('s', 'y')] {
        app.handle_key(key(KeyCode::Char(open)));
        assert!(app.destructive_confirm.is_some(), "{open} opens the pop-up");
        // A letter does nothing. Esc cancels; it is not absorbed into a draft.
        assert_eq!(app.handle_key(key(KeyCode::Char(other))), None);
        assert!(app.destructive_confirm.is_some(), "{other} left the pop-up");
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None, "Esc cancels");
        assert!(app.destructive_confirm.is_none());
    }
    assert_eq!(
        app.tree_navigator
            .as_ref()
            .and_then(|nav| nav.selected_agent_id()),
        Some(AGENT),
        "keys inside the pop-up do not also move the selection"
    );
}

/// Inside the agent view's pop-up a letter or an arrow does nothing. `Enter`
/// presses the focused action.
#[test]
fn keys_inside_the_agent_views_pop_up_do_not_type() {
    let mut app = root_app();
    open_agent(&mut app);
    app.handle_key(key(KeyCode::Char('c')));
    assert_eq!(app.handle_key(key(KeyCode::Up)), None);
    assert!(app.destructive_confirm.is_some(), "Up does nothing");
    for other in [KeyCode::Char('x'), KeyCode::Char('h')] {
        assert_eq!(app.handle_key(key(other)), None, "{other:?}");
        assert!(
            app.destructive_confirm.is_some(),
            "{other:?} left the pop-up"
        );
    }
    assert!(
        matches!(
            app.handle_key(key(KeyCode::Enter)),
            Some(UserAction::CloseTopologyNode { .. })
        ),
        "Enter closes"
    );
    assert!(app.destructive_confirm.is_none());
}

/// A click outside the pop-up, on the agent bar included, cancels it and
/// does nothing else.
#[test]
fn a_click_outside_the_pop_up_cancels_it_and_does_nothing_else() {
    let mut app = root_app();
    open_agent(&mut app);
    paint(&mut app, 120, 40);
    let bar = app.agent_bar_hit.expect("agent bar");
    app.handle_key(key(KeyCode::Char('i')));
    paint(&mut app, 120, 40);
    assert_eq!(
        app.handle_mouse(left_mouse(
            MouseEventKind::Down(crossterm::event::MouseButton::Left),
            bar.x + 2,
            bar.y,
        )),
        None
    );
    assert!(app.destructive_confirm.is_none(), "the click cancels");
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents),
        "cancelling the pop-up does not leave the agent transcript"
    );
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
}

/// Spec 02 §5.7: Activity is full-screen at every width, narrow included,
/// and a terminal too small for even that says so on one line.
#[test]
fn activity_fills_a_narrow_terminal_and_says_when_it_cannot() {
    for (width, height) in [(60, 20), (50, 20), (30, 12)] {
        let mut app = root_app();
        open_agents_at_agent(&mut app);
        let screen = render_to_string(&mut app, width, height);
        let top = screen.lines().next().unwrap_or_default();
        assert!(
            top.starts_with('╭') && top.ends_with('╮'),
            "{width}x{height}: Activity is not full width:\n{screen}"
        );
    }
    let mut app = root_app();
    open_agents_at_agent(&mut app);
    let screen = render_to_string(&mut app, 16, 4);
    assert!(screen.contains("too small"), "{screen}");
}

/// Full-screen Activity carries the root turn's timer, or its idle clock,
/// on its top border when it fits.
#[test]
fn full_screen_activity_shows_the_root_turn_state() {
    let mut app = root_app();
    pin_clock(&mut app);
    open_agents_at_agent(&mut app);
    app.busy = true;
    app.turn_started = app.now().checked_sub(std::time::Duration::from_secs(65));
    let screen = render_to_string(&mut app, 120, 40);
    let top = screen.lines().next().unwrap_or_default();
    assert!(top.contains("root · 1m05s"), "{top}");

    app.busy = false;
    app.last_prompt_at = app
        .now()
        .checked_sub(std::time::Duration::from_secs(25 * 60));
    let screen = render_to_string(&mut app, 120, 40);
    let top = screen.lines().next().unwrap_or_default();
    assert!(top.contains("root · last user prompt · 25m ago"), "{top}");
    let screen = render_to_string(&mut app, 40, 20);
    let top = screen.lines().next().unwrap_or_default();
    assert!(!top.contains("root"), "no room at 40 columns: {top}");
}

/// Tool row metas past a minute read `21m40s`, not
/// `1300.0s`, live and restored alike.
#[test]
fn tool_row_metas_use_the_long_format_past_a_minute() {
    for (ms, long, raw) in [
        (1_300_000, "21m40s", "1300.0s"),
        (3_900_000, "1h05m", "3900.0s"),
    ] {
        let mut restored = App::new(launch_basic(), true);
        restored.apply_host_event(restore_snapshot(
            7,
            vec![TranscriptRow::Tool(ToolItem {
                subject: Some("a.rs".into()),
                started_at_ms: Some(1_000),
                finished_at_ms: Some(1_000 + ms),
                ..tool_item("call-r1", "read_file")
            })],
        ));
        paint(&mut restored, 120, 40);
        let text = plain(&restored);
        assert!(text.contains(long) && !text.contains(raw), "{text}");

        let mut live = App::new(launch_basic(), true);
        read(&mut live, "call-r1", "running", "a.rs");
        let HostEvent::ToolLine {
            call_id,
            parent_call_id,
            cell_id,
            kind,
            name,
            subject,
            ..
        } = tool_line(
            "call-r1",
            None,
            None,
            ActivityKind::Tool,
            "read_file",
            "done",
            Some("a.rs"),
        )
        else {
            unreachable!()
        };
        live.apply_host_event(HostEvent::ToolLine {
            call_id,
            parent_call_id,
            cell_id,
            kind,
            name,
            state: "done".into(),
            subject,
            detail: None,
            duration_ms: Some(ms),
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
        paint(&mut live, 120, 40);
        let text = plain(&live);
        assert!(text.contains(long) && !text.contains(raw), "{text}");
    }
    // Under a minute the row keeps its tenths.
    let mut restored = App::new(launch_basic(), true);
    restored.apply_host_event(restore_snapshot(
        7,
        vec![TranscriptRow::Tool(ToolItem {
            subject: Some("a.rs".into()),
            started_at_ms: Some(1_000),
            finished_at_ms: Some(13_300),
            ..tool_item("call-r1", "read_file")
        })],
    ));
    paint(&mut restored, 120, 40);
    assert!(plain(&restored).contains("12.3s"), "{}", plain(&restored));
}

/// Spec 02 §5.3: Esc from an agent transcript returns to the Agents list at
/// the agent on screen, including when another agent replaced the view.
#[test]
fn opening_another_agent_returns_to_the_agents_list_at_that_agent() {
    let mut app = root_app();
    open_agents_at_agent(&mut app);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    app.open_agent_view("agent-e5".into(), "#e5 · other".into());
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert_eq!(
        app.tree_navigator
            .as_ref()
            .and_then(|nav| nav.selected_agent_id()),
        Some("agent-e5")
    );
}

/// The agent bar names the remedy of a recovery-required agent, as the
/// Agents list's detail does.
#[test]
fn a_recovery_required_agent_shows_its_remedy_on_the_agent_bar() {
    use plakous_host_protocol::topology::NodeStatus;
    let mut app = root_app();
    let mut stuck = tree_node(AGENT, Some("root"), 1);
    stuck.status = NodeStatus::RecoveryRequired;
    stuck.attempt_id = Some("agent-c3-a1".into());
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        stuck,
    ])));
    open_agent(&mut app);
    let screen = render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("remedy: ask the root to run recover_agent"),
        "{screen}"
    );
}
