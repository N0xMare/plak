//! Regression tests from the independent review of the agent view: view
//! lifecycle, root isolation, root notices, agent call detail and cadence.

use super::App;
use super::test_support::{key, launch_basic, left_mouse, restore_snapshot, say, tool_item};
use crate::tui::blocks::{DisplayMode, ScrollBlock};
use crate::tui::presentation::ActivityKind;
use crate::tui::{HostEvent, UserAction, user_action_channel};
use crossterm::event::{KeyCode, MouseButton, MouseEventKind};
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptRow};
use plakous_host_protocol::views::{
    AgentTranscript, CallDetail, CallDetailOutcome, CallDetailSource,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const AGENT: &str = "agent-c3";
const OTHER: &str = "agent-d4";

type ChildOp = plakous_host_protocol::SessionObservation<plakous_host_protocol::Core>;

fn paint(app: &mut App) {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
}

fn root_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    for line in 0..30 {
        say(&mut app, &format!("root line {line:02}"));
    }
    app
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

fn transcript(agent: &str, live: Option<&str>) -> HostEvent {
    HostEvent::AgentTranscript(AgentTranscript {
        agent_id: agent.into(),
        rows: agent_rows(),
        live_turn_id: live.map(str::to_owned),
        note: None,
    })
}

fn sent(app: &mut App) -> Vec<UserAction> {
    let (tx, rx) = user_action_channel();
    app.sync_agent_transcript(Some(&tx));
    std::iter::from_fn(|| rx.try_recv().ok())
        .map(|delivery| delivery.into_parts().0)
        .collect()
}

/// Open `agent` as the loop would: the intent goes out on the next pass and
/// the host answers with its rows.
fn open_served(app: &mut App, agent: &str, live: Option<&str>) {
    app.open_agent_view(agent.into(), format!("{agent} label"));
    assert_eq!(
        sent(app),
        vec![UserAction::OpenAgentTranscript {
            agent_id: agent.into()
        }]
    );
    app.apply_host_event(transcript(agent, live));
    assert_ne!(app.pane.blocks, [] as [ScrollBlock; 0]);
}

fn child_op(agent: &str, turn: &str, op: ChildOp) -> HostEvent {
    HostEvent::ChildLive {
        agent_id: agent.into(),
        turn_id: turn.into(),
        op: Box::new(op),
    }
}

fn tool_op(call_id: &str, state: &str) -> ChildOp {
    ChildOp::ToolLine {
        call_id: Some(call_id.into()),
        name: "exec_command".into(),
        state: state.into(),
        subject: Some("cargo clippy".into()),
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

/// The host re-sends a same-session `PresentationSnapshot` after a root
/// refusal (publication.rs, turn.rs) and after observation-queue saturation
/// (`resnapshot_transcript_if_required`). Neither is a session switch; spec
/// 02 §6.3 closes the view only on `/clear`, open, resume or fork.
#[test]
fn a_same_session_resnapshot_keeps_the_agent_view() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    app.apply_host_event(restore_snapshot(
        7,
        vec![TranscriptRow::Assistant {
            text: "root re-snapshot".into(),
        }],
    ));
    assert_eq!(
        app.viewed_agent_id(),
        Some(AGENT),
        "a same-session re-snapshot must not close the agent view"
    );
}

/// Esc and Enter on the same agent before the loop syncs: the view is a
/// fresh, empty pane, but the sync sees the host still serving that agent
/// and sends nothing, so no rows ever arrive.
#[test]
fn reopening_the_served_agent_in_one_pass_refetches_its_rows() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    app.handle_key(key(KeyCode::Esc));
    app.open_agent_view(AGENT.into(), "agent-c3 label".into());
    let asked = sent(&mut app);
    assert!(
        !app.pane.blocks.is_empty()
            || asked.contains(&UserAction::OpenAgentTranscript {
                agent_id: AGENT.into()
            }),
        "an empty agent pane with no request outstanding: {asked:?}"
    );
}

/// A -> B -> A in one pass (`[`/`]` with key repeat makes this
/// routine): same outcome.
#[test]
fn switching_back_to_the_served_agent_in_one_pass_refetches_its_rows() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    app.open_agent_view(OTHER.into(), "agent-d4 label".into());
    app.open_agent_view(AGENT.into(), "agent-c3 label".into());
    let asked = sent(&mut app);
    assert!(
        !app.pane.blocks.is_empty()
            || asked.contains(&UserAction::OpenAgentTranscript {
                agent_id: AGENT.into()
            }),
        "an empty agent pane with no request outstanding: {asked:?}"
    );
}

/// The disconnect notice is the root's: it must survive leaving the view.
#[test]
fn the_host_disconnect_notice_lands_in_the_root_transcript() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    app.note_host_disconnected();
    assert!(
        !app.pane
            .blocks
            .iter()
            .any(|block| block.body().contains("product host disconnected")),
        "the root's notice stays out of the agent pane"
    );
    app.handle_key(key(KeyCode::Esc));
    assert!(
        app.pane
            .blocks
            .iter()
            .any(|block| block.body().contains("product host disconnected")),
        "the disconnect notice is in the root transcript"
    );
}

/// A send failure notice (here for Ctrl+B's Background) is the root's too.
#[test]
fn a_send_failure_notice_lands_in_the_root_transcript() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    let (tx, rx) = user_action_channel();
    drop(rx);
    assert!(!super::terminal::send_user_action(
        &mut app,
        Some(&tx),
        UserAction::Background
    ));
    assert!(
        !app.pane
            .blocks
            .iter()
            .any(|block| block.body().contains("action not sent")),
        "the root's notice stays out of the agent pane"
    );
    app.handle_key(key(KeyCode::Esc));
    assert!(
        app.pane
            .blocks
            .iter()
            .any(|block| block.body().contains("action not sent")),
        "the send failure is in the root transcript"
    );
}

/// An agent item opened while it ran asks once it ends; a `preview` answer
/// within the commit race is asked again for the root (note_ended) but not
/// for an agent, so the agent row keeps the preview.
#[test]
fn an_agent_preview_answer_in_the_commit_race_is_asked_again() {
    let mut app = root_app();
    open_served(&mut app, AGENT, Some("t1"));
    app.apply_host_event(child_op(AGENT, "t1", tool_op("call-a2", "running")));
    let idx = app
        .pane
        .blocks
        .iter()
        .rposition(|block| matches!(block, ScrollBlock::Tool { .. }))
        .unwrap();
    app.toggle_tool_expand_at(idx);
    app.apply_host_event(child_op(AGENT, "t1", tool_op("call-a2", "done")));
    let (tx, rx) = user_action_channel();
    app.maybe_request_call_details(Some(&tx));
    assert!(rx.try_recv().is_ok(), "the ended item asks");
    app.apply_host_event(HostEvent::CallDetail(CallDetail {
        call_id: "call-a2".into(),
        agent_id: Some(AGENT.into()),
        session_id: "s1".into(),
        outcome: CallDetailOutcome::Detail {
            input: "{}".into(),
            output: "partial".into(),
            input_truncated: false,
            output_truncated: false,
            source: CallDetailSource::Preview,
            sources: Vec::new(),
        },
    }));
    assert_eq!(
        app.call_details.waiting(),
        1,
        "a commit-race preview is asked again, as for the root"
    );
}

/// The re-emit at an agent turn end replaces the rows wholesale: an item
/// the operator opened collapses under them.
#[test]
fn a_re_emitted_transcript_keeps_an_open_item_open() {
    let mut app = root_app();
    open_served(&mut app, AGENT, Some("t1"));
    let idx = app
        .pane
        .blocks
        .iter()
        .position(|block| matches!(block, ScrollBlock::Tool { .. }))
        .unwrap();
    app.toggle_tool_expand_at(idx);
    app.apply_host_event(transcript(AGENT, Some("t2")));
    assert!(
        matches!(
            app.pane.blocks.get(idx),
            Some(ScrollBlock::Tool {
                mode: DisplayMode::Expanded,
                ..
            })
        ),
        "the open item stays open across the turn-end re-emit"
    );
}

/// A root event mid-drag in the agent view: the gesture, the selection and
/// the on-screen pane are untouched, and the copy reads the agent's rows.
#[test]
fn a_root_event_mid_drag_leaves_the_agent_gesture_alone() {
    let mut app = root_app();
    open_served(&mut app, AGENT, None);
    paint(&mut app);
    let body = app.transcript_body_area.unwrap();
    app.handle_mouse(left_mouse(
        MouseEventKind::Down(MouseButton::Left),
        body.x,
        body.y,
    ));
    app.handle_mouse(left_mouse(
        MouseEventKind::Drag(MouseButton::Left),
        body.x + 30,
        body.bottom() - 1,
    ));
    let agent_blocks = app.pane.blocks.clone();
    for line in 0..5 {
        say(&mut app, &format!("root while dragging {line}"));
    }
    // An early-returning root op (unknown reasoning key) swaps back too.
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "nope".into(),
    });
    assert_eq!(app.viewed_agent_id(), Some(AGENT));
    assert_eq!(app.pane.blocks, agent_blocks);
    assert!(app.transcript_drag_pointer.is_some());
    paint(&mut app);
    app.handle_mouse(left_mouse(
        MouseEventKind::Up(MouseButton::Left),
        body.x + 30,
        body.bottom() - 1,
    ));
    let copied = app.take_transcript_selection_text().unwrap_or_default();
    assert!(copied.contains("review the parser"), "{copied}");
    assert!(!copied.contains("root while dragging"), "{copied}");
    app.handle_key(key(KeyCode::Esc));
    assert!(
        app.pane
            .blocks
            .iter()
            .any(|block| block.body().contains("root while dragging 4"))
    );
}

/// Agent ops never feed the root's busy, HUD clocks or transcript.
#[test]
fn agent_ops_never_touch_root_bookkeeping() {
    let mut app = root_app();
    app.busy = true;
    open_served(&mut app, AGENT, Some("t1"));
    let before = (
        app.busy,
        app.model_request_started,
        app.phase_started,
        app.root_pane().blocks.clone(),
        app.root_pane().tool_started.len(),
    );
    for op in [
        tool_op("call-a2", "running"),
        tool_op("call-a2", "done"),
        ChildOp::SetBusy(false),
        ChildOp::AssistantDelta("x".into()),
        ChildOp::AssistantDone,
    ] {
        app.apply_host_event(child_op(AGENT, "t1", op));
    }
    let after = (
        app.busy,
        app.model_request_started,
        app.phase_started,
        app.root_pane().blocks.clone(),
        app.root_pane().tool_started.len(),
    );
    assert_eq!(before, after);
}

/// The open agent's live stream paints at the 80 ms spinner cadence: the
/// streaming frame cap reads only the root pane's stream cursor.
#[test]
fn the_open_agents_stream_gets_the_streaming_cadence() {
    let mut app = root_app();
    app.busy = true;
    open_served(&mut app, AGENT, Some("t1"));
    app.apply_host_event(child_op(
        AGENT,
        "t1",
        ChildOp::AssistantDelta("streaming".into()),
    ));
    assert!(app.pane.assistant_stream_active);
    assert!(
        app.redraw_demand().streaming,
        "the agent stream on screen asks for the streaming cadence"
    );
}
