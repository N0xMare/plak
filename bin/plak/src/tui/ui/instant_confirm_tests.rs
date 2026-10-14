//! The confirmation pop-up is instant (spec 02 §5.7). It opens focused on the
//! primary action; `Enter` presses that button at once, `Esc` cancels, and
//! arrows and `Tab` move focus with no settle. Over the composer a printable
//! key closes it into the draft. A view with no prompt maps its keys at once
//! and ignores the rest.

use super::App;
use super::test_support::{
    FIXED_NOW, key, key_alt, key_ctrl, launch_basic, left_click, models_app, open_tree,
    render_to_string, tree_node, tree_snapshot,
};
use crate::tui::{HostEvent, SurfaceIntent, UserAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;

fn agents_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.agents_clock = Some(FIXED_NOW);
    open_tree(
        &mut app,
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child-a", Some("root"), 1),
            tree_node("child-b", Some("root"), 1),
        ]),
    );
    app.tree_navigator.as_mut().unwrap().focus_on("child-a");
    app
}

fn processes_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.apply_process_snapshot(Arc::new(vec![ProcessView {
        id: "root:p1".into(),
        command: "cargo build".into(),
        owner: "root".into(),
        started_at: 10,
        elapsed_ms: 12000,
        status: ProcessStatus::Running,
        output: String::new(),
    }]));
    app.open_processes();
    app
}

fn agent_view_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.agents_clock = Some(FIXED_NOW);
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ])));
    app.open_agent_view("child-a".into(), "#child-a · description".into());
    app
}

fn open_stop_all(app: &mut App) {
    assert!(app.handle_key(key(KeyCode::Char('s'))).is_none());
    assert!(app.destructive_confirm.is_some());
}

fn no_typing_hint(screen: &str) {
    assert!(
        !screen.contains("pause to")
            && !screen.contains("you were typing")
            && !screen.contains("Esc to type"),
        "{screen}"
    );
}

#[test]
fn enter_confirms_the_pop_up_the_moment_it_opens() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopAllTopology)
    );
    assert!(app.destructive_confirm.is_none());
}

#[test]
fn esc_cancels_and_a_letter_does_nothing() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
    assert!(
        app.destructive_confirm.is_some(),
        "a letter neither confirms nor cancels"
    );
    assert!(
        app.input.is_empty(),
        "the letter is not typed into the draft"
    );
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_none());
}

/// The agent transcript is Activity's body, so it is a promptless view: a
/// printable key or `Backspace` leaves the pop-up alone and out of the draft.
/// The same key over the composer closes the pop-up into the draft.
#[test]
fn the_agent_transcript_inside_activity_ignores_typing_over_the_pop_up() {
    let mut app = agent_view_app();
    app.set_input("draft".into());
    assert!(app.has_no_prompt());
    assert_eq!(
        app.activity_tab,
        Some(super::processes::ActivityTab::Agents)
    );
    assert!(app.viewed_agent_id().is_some());
    assert!(app.handle_key(key(KeyCode::Char('i'))).is_none());
    assert!(app.destructive_confirm.is_some());
    for code in [KeyCode::Char('y'), KeyCode::Backspace] {
        assert!(app.handle_key(key(code)).is_none());
        assert!(
            app.destructive_confirm.is_some(),
            "{code:?} leaves the pop-up open"
        );
        assert_eq!(app.input, "draft", "{code:?} is not typed");
    }
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::InterruptTopologyNode {
            agent_id: "child-a".into(),
        })
    );

    let mut root = App::new(launch_basic(), true);
    root.apply_host_event(HostEvent::SetBusy(true));
    assert!(!root.has_no_prompt());
    assert!(root.handle_key(key(KeyCode::Esc)).is_none());
    assert!(root.destructive_confirm.is_some());
    assert!(root.handle_key(key(KeyCode::Char('y'))).is_none());
    assert!(root.destructive_confirm.is_none());
    assert_eq!(root.input, "y");
}

#[test]
fn arrows_and_tab_move_focus_at_once() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    // Focus starts on the action. Left returns to Cancel; Enter declines.
    app.handle_key(key(KeyCode::Left));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert!(app.destructive_confirm.is_none());

    open_stop_all(&mut app);
    // Right does not wrap past the action, so Enter still confirms.
    app.handle_key(key(KeyCode::Right));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopAllTopology)
    );

    open_stop_all(&mut app);
    // Tab moves right; BackTab moves left onto Cancel.
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::BackTab));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert!(app.destructive_confirm.is_none());
}

#[test]
fn alt_enter_cancels() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    assert!(app.handle_key(key_alt(KeyCode::Enter)).is_none());
    assert!(app.destructive_confirm.is_none());
    assert!(!app.should_quit);
}

#[test]
fn a_click_confirms_or_cancels() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    let screen = render_to_string(&mut app, 100, 30);
    assert!(screen.contains("[ Cancel ]"), "{screen}");
    assert!(screen.contains("Enter confirms · Esc cancels"), "{screen}");
    let cancel = app
        .destructive_confirm
        .as_ref()
        .unwrap()
        .cancel_hit()
        .expect("cancel");
    assert!(app.handle_mouse(left_click(cancel.x, cancel.y)).is_none());
    assert!(app.destructive_confirm.is_none());

    open_stop_all(&mut app);
    render_to_string(&mut app, 100, 30);
    let action = app
        .destructive_confirm
        .as_ref()
        .unwrap()
        .action_hit()
        .expect("action");
    assert_eq!(
        app.handle_mouse(left_click(action.x, action.y)),
        Some(UserAction::StopAllTopology)
    );
}

#[test]
fn activity_s_then_enter_stops_at_once() {
    let mut app = agents_app();
    assert!(app.handle_key(key(KeyCode::Char('s'))).is_none());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopAllTopology)
    );
}

#[test]
fn the_agent_view_opens_interrupt_and_close_at_once() {
    for (letter, closes) in [('i', false), ('c', true)] {
        let mut app = agent_view_app();
        assert!(app.handle_key(key(KeyCode::Char(letter))).is_none());
        let action = app
            .destructive_confirm
            .as_ref()
            .expect("pop-up")
            .first_action()
            .cloned()
            .expect("action");
        assert_eq!(
            matches!(action, UserAction::CloseTopologyNode { .. }),
            closes,
            "{letter}"
        );
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(action));
    }
}

#[test]
fn the_resume_picker_enter_resumes_at_once() {
    let mut app = App::new(launch_basic(), true);
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(vec![
        crate::tui::ResumeSessionEntry {
            id: "018f2537-5ed2-7b11-a4b1-f009be85f101".into(),
            first_prompt: Some("first".into()),
            in_place: true,
            ..crate::tui::ResumeSessionEntry::default()
        },
    ]));
    assert!(app.resume_menu.is_some());
    no_typing_hint(&render_to_string(&mut app, 80, 24));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeSession {
            id: Some("018f2537-5ed2-7b11-a4b1-f009be85f101".into())
        })
    );
}

#[test]
fn unmapped_keys_in_promptless_views_do_nothing_and_show_no_hint() {
    let mut agents = agents_app();
    let before = agents
        .tree_navigator
        .as_ref()
        .unwrap()
        .selected_agent_id()
        .unwrap()
        .to_owned();
    for ch in ['q', 'w', 'z'] {
        assert!(agents.handle_key(key(KeyCode::Char(ch))).is_none());
    }
    agents.handle_paste("pasted words");
    assert_eq!(
        agents
            .tree_navigator
            .as_ref()
            .unwrap()
            .selected_agent_id()
            .unwrap(),
        before
    );
    assert_eq!(agents.input, "");
    assert!(agents.destructive_confirm.is_none());
    no_typing_hint(&render_to_string(&mut agents, 100, 30));

    let mut view = agent_view_app();
    assert!(view.handle_key(key(KeyCode::Char('q'))).is_none());
    view.handle_paste("pasted");
    assert_eq!(view.input, "");
    assert_eq!(view.viewed_agent_id(), Some("child-a"));
    no_typing_hint(&render_to_string(&mut view, 100, 30));

    let mut processes = processes_app();
    processes.handle_key(key(KeyCode::Enter));
    assert!(processes.process_output);
    assert!(processes.handle_key(key(KeyCode::Char('s'))).is_none());
    assert!(processes.destructive_confirm.is_none());
    processes.handle_paste("pasted");
    assert_eq!(processes.input, "");
    no_typing_hint(&render_to_string(&mut processes, 100, 30));
}

#[test]
fn a_ctrl_chord_cancels_the_pop_up() {
    let mut app = agents_app();
    open_stop_all(&mut app);
    assert!(app.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
    assert!(app.destructive_confirm.is_none());
}

fn busy_drafting_with_approval() -> App {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: Some("call-1".into()),
        tool_name: "exec_command".into(),
        subject: Some("rm -rf build".into()),
        prompt: Some("this deletes files".into()),
        expires_at_ms: 0,
    });
    app.set_input("hmm ".into());
    app
}

fn composer_pop_up() -> App {
    let mut app = busy_drafting_with_approval();
    app.set_input(String::new());
    app.background_processes = 1;
    app
}

fn pop_up_action(action: &UserAction) -> bool {
    matches!(
        action,
        UserAction::Interrupt
            | UserAction::StopProcesses { .. }
            | UserAction::Quit
            | UserAction::ResolveApproval { .. }
    )
}

/// A legacy terminal delivers a typed `Esc` then `a` as one `Alt+a`. The
/// chord opens the approval pop-up; the letters that follow close it into
/// the draft, and that draft's `Enter` does not allow or deny the tool.
/// Readline's `Alt+d` denies the same way.
#[test]
fn a_coalesced_esc_letter_then_a_typed_draft_and_enter_never_resolves_the_approval() {
    for (code, rest) in [('a', "lso fix the docs"), ('d', "on't run that")] {
        let mut app = busy_drafting_with_approval();
        assert!(app.handle_key(key_alt(KeyCode::Char(code))).is_none());
        assert!(app.destructive_confirm.is_some(), "Alt+{code} asks");
        for ch in rest.chars() {
            assert!(app.handle_key(key(KeyCode::Char(ch))).is_none());
        }
        assert!(
            app.destructive_confirm.is_none(),
            "Alt+{code}: the draft closed the pop-up"
        );
        assert_eq!(app.input, format!("hmm {rest}"));
        let sent = app.handle_key(key(KeyCode::Enter));
        assert!(
            sent.as_ref().is_none_or(|action| !pop_up_action(action)),
            "Alt+{code}, {rest:?}, Enter sent {sent:?}"
        );
    }
}

/// Busy `Esc`, then a typed draft, then `Enter`: the draft closes the pop-up
/// and `Enter` sends it. Nothing stops.
#[test]
fn a_busy_esc_then_a_typed_draft_and_enter_stops_nothing() {
    let mut app = busy_drafting_with_approval();
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_some());
    for ch in "also".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch))).is_none());
    }
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.input, "hmm also");
    let sent = app.handle_key(key(KeyCode::Enter));
    assert!(
        sent.as_ref().is_none_or(|action| !pop_up_action(action)),
        "busy Esc, typed draft, Enter sent {sent:?}"
    );

    let mut app = busy_drafting_with_approval();
    app.handle_key(key(KeyCode::Esc));
    assert!(app.handle_key(key(KeyCode::Backspace)).is_none());
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.input, "hmm");
}

/// Every keyboard-opened pop-up over the composer starts on its primary
/// action. `Alt` and `Ctrl` cancel without typing, a printable key closes
/// into the draft, and `Enter` on a pop-up that is still open presses the
/// focused button. The busy `Esc` pop-up's arrows and `Tab`/`Shift+Tab` move
/// at once between its two actions.
#[test]
fn every_keyboard_pop_up_over_the_composer_follows_the_instant_model() {
    type Open = fn(&mut App);
    let openers: Vec<(&str, Open)> = vec![
        ("busy esc", |app| {
            app.handle_key(key(KeyCode::Esc));
        }),
        ("alt+a", |app| {
            app.handle_key(key_alt(KeyCode::Char('a')));
        }),
        ("alt+d", |app| {
            app.handle_key(key_alt(KeyCode::Char('d')));
        }),
        ("ctrl+q", |app| {
            app.handle_key(key_ctrl(KeyCode::Char('q')));
        }),
    ];
    for (name, open) in &openers {
        for cancel in [
            key_alt(KeyCode::Enter),
            key_alt(KeyCode::Char('y')),
            key_ctrl(KeyCode::Char('m')),
            key(KeyCode::Esc),
        ] {
            let mut app = composer_pop_up();
            open(&mut app);
            assert!(app.destructive_confirm.is_some(), "{name} opens");
            assert_eq!(app.handle_key(cancel), None, "{name}: {cancel:?}");
            assert!(app.destructive_confirm.is_none(), "{name}: {cancel:?}");
            assert!(app.input.is_empty(), "{name}: {cancel:?} typed");
        }
        let mut app = composer_pop_up();
        open(&mut app);
        for ch in "yes".chars() {
            assert!(app.handle_key(key(KeyCode::Char(ch))).is_none(), "{name}");
        }
        assert!(app.destructive_confirm.is_none(), "{name}");
        assert_eq!(app.input, "yes", "{name}");
        let sent = app.handle_key(key(KeyCode::Enter));
        assert!(
            sent.as_ref().is_none_or(|action| !pop_up_action(action)),
            "{name}: {sent:?}"
        );

        let mut app = composer_pop_up();
        open(&mut app);
        let first = app
            .destructive_confirm
            .as_ref()
            .unwrap()
            .first_action()
            .cloned();
        assert!(first.is_some(), "{name}");
        assert_eq!(app.handle_key(key(KeyCode::Enter)), first, "{name}");
    }

    let mut app = composer_pop_up();
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::StopProcesses {
            id: None,
            interrupt: true
        })
    );
}

/// Dropping `[ Stop turn + processes ]` clamps a focus that sat on it onto
/// `[ Stop turn ]`, not onto `Cancel` and not past the last button.
#[test]
fn dropping_stop_processes_clamps_focus_onto_stop_turn() {
    let mut app = App::new(launch_basic(), true);
    app.apply_process_snapshot(Arc::new(vec![ProcessView {
        id: "root:p1".into(),
        command: "cargo build".into(),
        owner: "root".into(),
        started_at: 10,
        elapsed_ms: 12000,
        status: ProcessStatus::Running,
        output: String::new(),
    }]));
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.destructive_confirm.as_ref().unwrap().focus, 2);
    app.apply_process_snapshot(Arc::new(Vec::new()));
    let confirm = app.destructive_confirm.as_ref().unwrap();
    assert_eq!(confirm.buttons(), 2, "the processes button is gone");
    assert_eq!(confirm.focus, 1, "focus stays on Stop turn");
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Interrupt)
    );
}

/// `Tab` moves right on the profile pop-up and `Shift+Tab` moves left, including
/// when the terminal reports it as `Tab` with `Shift` rather than `BackTab`.
#[test]
fn tab_and_shift_tab_move_the_profile_pop_up() {
    let open = || {
        let mut app = models_app();
        assert!(app.handle_key(key(KeyCode::Enter)).is_none());
        assert!(app.destructive_confirm.is_some());
        app
    };
    let mut app = open();
    app.handle_key(key(KeyCode::Tab));
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
            fork_now: false,
            ..
        }))
    ));

    let mut app = open();
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::BackTab));
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
            fork_now: true,
            ..
        }))
    ));

    let mut app = open();
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
            fork_now: true,
            ..
        }))
    ));

    let mut app = open();
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT));
    assert!(app.handle_key(key(KeyCode::Enter)).is_none());
    assert!(
        app.destructive_confirm.is_none(),
        "Shift+Tab landed on Cancel"
    );
}
