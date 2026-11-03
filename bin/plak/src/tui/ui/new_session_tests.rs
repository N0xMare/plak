//! `/new` (spec 02 §5.1): a new session in the same TUI. While this session
//! has running agents or background processes it does not switch silently:
//! the confirmation names that work and offers to stop it first.

use super::App;
use super::terminal::dispatch_action;
use super::test_support::{
    blocks_contain, key, launch_basic, render_to_string, tree_node, tree_snapshot,
};
use crate::tui::{HostEvent, UserAction};
use crossterm::event::KeyCode;
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn process(status: ProcessStatus) -> Arc<Vec<ProcessView>> {
    Arc::new(vec![ProcessView {
        id: "root:p1".into(),
        command: "cargo build".into(),
        owner: "root".into(),
        started_at: 10,
        elapsed_ms: 12000,
        status,
        output: String::new(),
    }])
}

fn with_running_agent(app: &mut App) {
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ])));
}

fn type_new(app: &mut App) -> Option<UserAction> {
    app.set_input("/new".into());
    app.handle_key(key(KeyCode::Enter))
}

#[test]
fn new_with_nothing_live_starts_the_session_at_once() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(type_new(&mut app), Some(UserAction::ClearSession));
    assert!(app.destructive_confirm.is_none());
}

#[test]
fn the_retired_clear_command_is_unknown() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("/clear".into());
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    assert!(blocks_contain(&app, "unknown command /clear"));
}

#[test]
fn new_over_a_running_agent_names_it_and_starts_only_when_confirmed() {
    let mut app = App::new(launch_basic(), true);
    with_running_agent(&mut app);
    assert_eq!(type_new(&mut app), None, "live work is never left silently");
    let confirm = app.destructive_confirm.as_ref().expect("confirmation");
    assert_eq!(confirm.kind(), "new-session");
    let screen = render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("1 running agent: interrupted, resumable."),
        "{screen}"
    );
    assert!(screen.contains("stays resumable with /resume"), "{screen}");
    assert!(screen.contains("[ Stop work + new ]"), "{screen}");
    assert_eq!(app.input, "");

    // No process runs, so the switch's release interrupts the agent.
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ClearSession)
    );
    assert!(app.new_after_stop.is_none());
}

#[test]
fn esc_keeps_the_session_and_its_work() {
    let mut app = App::new(launch_basic(), true);
    with_running_agent(&mut app);
    assert_eq!(type_new(&mut app), None);
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(app.destructive_confirm.is_none());
    assert!(app.new_after_stop.is_none());
}

#[test]
fn new_over_a_running_process_stops_it_then_starts_once_none_runs() {
    let mut app = App::new(launch_basic(), true);
    app.apply_process_snapshot(process(ProcessStatus::Running));
    assert_eq!(type_new(&mut app), None);
    let screen = render_to_string(&mut app, 120, 40);
    assert!(
        screen.contains("1 background process: stopped; work in flight is lost."),
        "{screen}"
    );

    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopProcesses {
            id: None,
            interrupt: false,
        })
    );
    let now = Instant::now();
    assert!(app.new_after_stop.is_some());
    assert!(
        app.new_after_stop_wake(now)
            .is_some_and(|at| at <= now + Duration::from_millis(100))
    );

    app.apply_process_snapshot(process(ProcessStatus::Stopping));
    assert_eq!(app.new_session_after_stop(now), None, "still stopping");
    app.apply_process_snapshot(process(ProcessStatus::Stopped));
    assert_eq!(
        app.new_session_after_stop(now),
        Some(UserAction::ClearSession)
    );
    assert!(app.new_after_stop.is_none());
    assert_eq!(app.new_session_after_stop(now), None, "it fires once");
}

#[test]
fn a_process_that_outlives_the_deadline_drops_the_switch_with_a_note() {
    let mut app = App::new(launch_basic(), true);
    app.apply_process_snapshot(process(ProcessStatus::Running));
    assert_eq!(type_new(&mut app), None);
    assert!(app.handle_key(key(KeyCode::Enter)).is_some());
    let late = Instant::now() + Duration::from_secs(11);
    assert_eq!(app.new_session_after_stop(late), None);
    assert!(app.new_after_stop.is_none());
    assert!(blocks_contain(
        &app,
        "new session not started: 1 background process is still running"
    ));
}

#[test]
fn a_prompt_sent_while_the_switch_waits_drops_it() {
    let mut app = App::new(launch_basic(), true);
    app.apply_process_snapshot(process(ProcessStatus::Running));
    assert_eq!(type_new(&mut app), None);
    let stop = app.handle_key(key(KeyCode::Enter)).expect("stop");
    dispatch_action(&mut app, &stop, None);
    assert!(
        app.new_after_stop.is_some(),
        "the stop it sent keeps it armed"
    );

    dispatch_action(&mut app, &UserAction::Prompt("next".into()), None);
    assert!(app.new_after_stop.is_none());
    assert!(blocks_contain(&app, "the confirmed /new was dropped"));
    app.apply_process_snapshot(process(ProcessStatus::Stopped));
    assert_eq!(app.new_session_after_stop(Instant::now()), None);
}

#[test]
fn a_busy_root_leaves_the_refusal_to_the_host() {
    let mut app = App::new(launch_basic(), true);
    with_running_agent(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_eq!(type_new(&mut app), Some(UserAction::ClearSession));
    assert!(app.destructive_confirm.is_none());
}
