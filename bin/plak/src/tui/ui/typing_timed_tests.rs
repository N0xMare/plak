//! Timed typing sequences (spec 02 §5.2, §5.5). The composer still waits out
//! [`QUIET`] before `?` opens Keybinds. Views with no prompt and the
//! confirmation pop-up do not: a mapped key acts at once and `Enter` confirms.

use super::App;
use super::destructive_confirm::DestructiveConfirm;
use super::test_support::{
    FIXED_NOW, confirm_action, key, key_ctrl, launch_basic, left_click, open_tree, pass_time,
    queue_item, render_to_string, resume_fixture, settings_fixture, tree_node, tree_snapshot,
};
use super::typing_guard::QUIET;
use crate::tui::{ApprovalChoice, HostEvent, QueueDrainState, SurfaceIntent, UserAction};
use crossterm::event::KeyCode;
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;

/// Any intent that destroys work, spends, or cannot be taken back.
fn irreversible(action: &UserAction) -> bool {
    matches!(
        action,
        UserAction::Interrupt
            | UserAction::StopProcesses { .. }
            | UserAction::InterruptTopologyNode { .. }
            | UserAction::CloseTopologyNode { .. }
            | UserAction::StopAllTopology
            | UserAction::ResumeTopologyTree
            | UserAction::DeleteQueued { .. }
            | UserAction::DropQueued { .. }
            | UserAction::SendQueuedNow { .. }
            | UserAction::ResumeQueueDrain
            | UserAction::ResolveApproval { .. }
            | UserAction::Quit
            | UserAction::ClearSession
            | UserAction::Compact
            | UserAction::ForkSession { .. }
            | UserAction::ResumeSession { .. }
            | UserAction::ResumeSessionHandoff { .. }
            | UserAction::Extension(SurfaceIntent::ApplyProfile { .. })
    )
}

const QUIET_MS: u64 = 300;

#[test]
fn the_quiet_period_matches_the_guard() {
    use std::time::Duration;
    assert_eq!(Duration::from_millis(QUIET_MS), QUIET);
}

/// One step of a timed sequence: wait `gap` ms, then press `code`.
type Step = (u64, KeyCode);

/// `text` typed with `gap` ms before each key.
fn typed_at(text: &str, gap: u64) -> Vec<Step> {
    text.chars().map(|ch| (gap, KeyCode::Char(ch))).collect()
}

fn run(app: &mut App, steps: &[Step]) -> Vec<UserAction> {
    let mut sent = Vec::new();
    for (gap, code) in steps {
        pass_time(app, *gap);
        sent.extend(app.handle_key(key(*code)));
    }
    sent
}

fn assert_safe(view: &str, sequence: &str, sent: &[UserAction]) {
    assert!(
        !sent.iter().any(irreversible),
        "{view}: {sequence} sent {sent:?}"
    );
}

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
    pass_time(&mut app, 0);
    app
}

fn running_process(app: &mut App) {
    app.apply_process_snapshot(Arc::new(vec![ProcessView {
        id: "root:p1".into(),
        command: "cargo build".into(),
        owner: "root".into(),
        started_at: 10,
        elapsed_ms: 12000,
        status: ProcessStatus::Running,
        output: "line\n".into(),
    }]));
}

fn processes_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    running_process(&mut app);
    app.open_processes();
    pass_time(&mut app, 0);
    app
}

fn agent_view_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ])));
    app.open_agent_view("child-a".into(), "#child-a · description".into());
    pass_time(&mut app, 0);
    app
}

/// The root mid-turn with a queue, a running process and an open approval:
/// every root surface a typed key could reach.
fn busy_root_app() -> App {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "first"), queue_item("q2", 1, "second")],
        drain: QueueDrainState::RestoredPaused,
    });
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: Some("call-1".into()),
        tool_name: "exec_command".into(),
        subject: Some("rm -rf build".into()),
        prompt: Some("this deletes files".into()),
        expires_at_ms: 0,
    });
    pass_time(&mut app, 0);
    app
}

type View = fn() -> App;

/// Each prompt-less view with the letter there that opens a pop-up.
fn promptless_views() -> Vec<(&'static str, View, char)> {
    vec![
        ("agents", agents_app, 's'),
        ("agents", agents_app, 'i'),
        ("agents", agents_app, 'c'),
        ("processes", processes_app, 's'),
        ("processes", processes_app, 'S'),
        ("agent view", agent_view_app, 'c'),
        ("agent view", agent_view_app, 'i'),
    ]
}

/// The mapped key opens the pop-up at once and `Enter` confirms, with no pause.
#[test]
fn a_mapped_letter_opens_the_pop_up_and_enter_confirms() {
    for (view, make, letter) in promptless_views() {
        let mut app = make();
        app.handle_key(key(KeyCode::Char(letter)));
        assert!(
            confirm_action(&mut app).is_some_and(|action| irreversible(&action)),
            "{view}"
        );
    }
}

/// Unmapped letters delivered with no gap, as an unbracketed paste would,
/// send nothing and show no hint. A mapped letter still opens the pop-up.
#[test]
fn unmapped_letters_at_paste_speed_do_nothing() {
    let views: Vec<(&str, View)> = vec![
        ("agents", agents_app),
        ("processes", processes_app),
        ("agent view", agent_view_app),
    ];
    for (view, make) in views {
        let mut app = make();
        let sent = run(&mut app, &typed_at("bump", 0));
        assert_safe(view, "bump", &sent);
        assert!(app.destructive_confirm.is_none(), "{view}");
        let screen = render_to_string(&mut app, 100, 30);
        assert!(
            !screen.contains("no prompt") && !screen.contains("pause to"),
            "{view}: {screen}"
        );
    }
}

/// A click on a row selects it. A click on a painted control opens the
/// pop-up at once, and a letter typed into that pop-up does not cancel it.
#[test]
fn a_click_on_a_control_opens_the_pop_up_and_a_letter_does_not_cancel_it() {
    let mut app = agents_app();
    render_to_string(&mut app, 100, 30);
    let (_, row) = *app
        .tree_hits
        .iter()
        .find(|(agent, _)| agent == "child-b")
        .expect("child-b is painted");
    app.handle_mouse(left_click(row.x + 1, row.y));
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child-b")
    );
    assert!(app.destructive_confirm.is_none());

    let mut app = agents_app();
    render_to_string(&mut app, 100, 30);
    let (hit, _) = *app
        .key_label_hits
        .iter()
        .find(|(_, code)| *code == KeyCode::Char('s'))
        .expect("s stop all is clickable");
    app.handle_mouse(left_click(hit.x, hit.y));
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("stop-all"),
        "a click on the control asks"
    );
    let sent = run(&mut app, &typed_at(" world", 80));
    assert!(
        app.destructive_confirm.is_some(),
        "a letter left the pop-up"
    );
    assert_safe("agents", "click on s, then a letter", &sent);
}

/// Busy `Esc` opens the turn-stop pop-up at once. `Enter` stops the turn
/// and leaves the draft as it was. A letter closes the pop-up into the draft.
#[test]
fn a_busy_esc_then_enter_stops_the_turn_at_once() {
    let mut app = busy_root_app();
    app.set_input("hello wrld".into());
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Interrupt)
    );
    assert_eq!(app.input, "hello wrld");
}

/// `Ctrl+C` twice while busy clears the draft, then asks in the busy pop-up
/// as `Esc` does; a third press cancels the pop-up.
#[test]
fn ctrl_c_twice_while_busy_asks_before_stopping() {
    let mut app = busy_root_app();
    app.set_input("half a draft".into());
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.input.is_empty());
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("turn-stop")
    );
    assert!(!app.cancel_pending);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.destructive_confirm.is_none());
    assert!(!app.should_quit);
    app.handle_key(key_ctrl(KeyCode::Char('c')));
    assert_eq!(confirm_action(&mut app), Some(UserAction::Interrupt));
}

/// Composer typing with a queue and an open approval: `Tab`, arrows,
/// `Delete`, `Backspace`, `Enter` and letters at any speed or after a pause
/// drop, send, resume and resolve nothing.
#[test]
fn composer_editing_keys_at_any_speed_touch_no_queued_prompt_or_approval() {
    let sequences: Vec<Vec<KeyCode>> = vec![
        vec![KeyCode::Tab, KeyCode::Char('d'), KeyCode::Char('o')],
        vec![KeyCode::Tab, KeyCode::Delete, KeyCode::Enter],
        vec![
            KeyCode::Tab,
            KeyCode::Down,
            KeyCode::Char('e'),
            KeyCode::Enter,
        ],
        vec![
            KeyCode::Tab,
            KeyCode::Char('r'),
            KeyCode::Backspace,
            KeyCode::Enter,
        ],
        vec![
            KeyCode::Tab,
            KeyCode::Left,
            KeyCode::Char('a'),
            KeyCode::Enter,
        ],
        vec![KeyCode::Char('a'), KeyCode::Char('d'), KeyCode::Enter],
    ];
    for gap in [0, 120, QUIET_MS, 1_000, 2_000] {
        for sequence in &sequences {
            let mut app = busy_root_app();
            app.set_input("fix the ".into());
            let steps: Vec<Step> = sequence.iter().map(|code| (gap, *code)).collect();
            let sent = run(&mut app, &steps);
            assert_safe("composer", &format!("{sequence:?} every {gap} ms"), &sent);
        }
    }
}

/// The open approval's chips are the mouse path beside `Alt+A`/`Alt+D`.
#[test]
fn the_approval_row_chips_allow_and_deny() {
    for (label, choice) in [
        ("[allow]", ApprovalChoice::Allow),
        ("[deny]", ApprovalChoice::Deny),
    ] {
        let mut app = busy_root_app();
        let screen = render_to_string(&mut app, 120, 30);
        assert!(screen.contains("[allow] Alt+A · [deny] Alt+D"), "{screen}");
        let hit = app
            .approval_chip_hits
            .iter()
            .find(|hit| hit.choice == choice)
            .unwrap_or_else(|| panic!("{label} is clickable"))
            .rect;
        let painted: String = screen
            .lines()
            .nth(usize::from(hit.y))
            .unwrap()
            .chars()
            .skip(usize::from(hit.x))
            .take(usize::from(hit.width))
            .collect();
        assert_eq!(painted, label);
        // A chip fires once it has held its place for CHIP_SETTLE.
        pass_time(&mut app, 300);
        assert_eq!(
            app.handle_mouse(left_click(hit.x, hit.y)),
            Some(UserAction::ResolveApproval {
                approval_id: "approval-1".into(),
                choice,
            })
        );
    }
    // Resolved, the row has no chips.
    let mut app = busy_root_app();
    app.apply_host_event(HostEvent::ApprovalResolved {
        approval_id: "approval-1".into(),
        resolution: "allowed".into(),
    });
    render_to_string(&mut app, 120, 30);
    assert!(app.approval_chip_hits.is_empty());
}

/// `Tab` in the palette completes only the prefix match `Enter` would run.
#[test]
fn tab_completes_only_a_prefix_match() {
    for draft in ["/fc", "/tmp", "/opt"] {
        let mut app = App::new(launch_basic(), true);
        run(&mut app, &typed_at(draft, 60));
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.input, draft, "Tab rewrote a fuzzy match");
    }
    let mut app = App::new(launch_basic(), true);
    run(&mut app, &typed_at("/he", 60));
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.input, "/help");
}

/// `?` opens Keybinds only after a pause: typed into a draft just emptied it
/// stays text.
#[test]
fn question_mark_opens_keybinds_only_after_a_pause() {
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    run(
        &mut app,
        &[
            (80, KeyCode::Char('a')),
            (80, KeyCode::Backspace),
            (80, KeyCode::Char('?')),
        ],
    );
    assert!(app.control_tab.is_none());
    assert_eq!(app.input, "?");
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    run(
        &mut app,
        &[(80, KeyCode::Backspace), (QUIET_MS, KeyCode::Char('?'))],
    );
    assert_eq!(app.control_tab, Some(super::ControlTab::Keybinds));
}

// Dialogs and pickers.

fn resume_picker_app() -> App {
    let mut app = resume_fixture();
    pass_time(&mut app, 0);
    app
}

/// The resume picker maps no letter. `Enter` resumes the selected row at
/// once, and the footer never talks about a prompt.
#[test]
fn the_resume_picker_ignores_letters_and_enter_resumes_at_once() {
    let mut app = resume_picker_app();
    let sent = run(&mut app, &typed_at("fix it", 0));
    assert_safe("resume picker", "fix it", &sent);
    assert!(app.resume_menu.is_some());
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        !screen.contains("pause to") && !screen.contains("you were typing"),
        "{screen}"
    );
    let sent = run(&mut app, &[(0, KeyCode::Down), (0, KeyCode::Enter)]);
    assert!(
        sent.iter().any(|action| matches!(
            action,
            UserAction::ResumeSession { .. } | UserAction::ResumeSessionHandoff { .. }
        )),
        "↓, Enter resumes: {sent:?}"
    );
}

/// The help overlay: a draft typed over it closes it at most.
#[test]
fn a_draft_typed_over_the_help_overlay_sends_nothing_irreversible() {
    for draft in ["pass the tests", "quit", "yes"] {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(HostEvent::SetBusy(true));
        run(&mut app, &typed_at("/help", 60));
        app.handle_key(key(KeyCode::Enter));
        assert!(app.info_overlay.is_some());
        let mut steps = typed_at(draft, 80);
        steps.push((80, KeyCode::Enter));
        let sent = run(&mut app, &steps);
        assert_safe("help overlay", &format!("{draft:?} + Enter"), &sent);
    }
}

/// Settings reset has no timer: `d` opens the pop-up, and `Enter` confirms
/// at once. `Esc` leaves the value alone.
#[test]
fn settings_reset_confirms_with_enter_at_once() {
    let mut app = settings_fixture();
    let before = app.settings_menu.as_ref().unwrap().draft.clone();
    assert!(app.handle_key(key(KeyCode::Char('d'))).is_none());
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("settings-reset"),
        "d asks"
    );
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.settings_menu.as_ref().unwrap().draft, before);

    assert!(app.handle_key(key(KeyCode::Char('d'))).is_none());
    let sent = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            sent,
            Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
        ),
        "Enter confirms at once: {sent:?}"
    );
    assert!(app.destructive_confirm.is_none());
    assert!(
        matches!(
            app.settings_menu.as_ref().map(|menu| &menu.pane),
            Some(crate::tui::SettingsPane::Browse)
        ),
        "Enter left the confirmation"
    );
}

fn profiles_app() -> App {
    let mut app = super::test_support::models_app();
    pass_time(&mut app, 0);
    app
}

/// The profile dialog is the same pop-up: focus starts on Fork now, `Enter`
/// forks at once, a letter does nothing, and `→` then `Enter` is Next session.
#[test]
fn the_profile_dialog_confirms_on_enter_at_once() {
    let mut app = profiles_app();
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("profile")
    );
    let screen = render_to_string(&mut app, 100, 30);
    assert!(screen.contains("[ Cancel ]"), "{screen}");
    assert!(screen.contains("Enter confirms · Esc cancels"), "{screen}");
    assert!(!screen.contains("→ then Enter"), "{screen}");
    assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
            fork_now: true,
            ..
        }))
    ));

    let mut app = profiles_app();
    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Right));
    assert!(matches!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Extension(SurfaceIntent::ApplyProfile {
            fork_now: false,
            ..
        }))
    ));
}
