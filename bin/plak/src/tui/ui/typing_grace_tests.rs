//! Typing-grace and Activity paths (spec 02 §5.2, §5.7), each pinned.

use super::App;
use super::test_support::{
    FIXED_NOW, key, key_alt, key_ctrl, launch_basic, left_click, open_tree, pass_time, queue_item,
    render_to_string, resume_fixture, tree_node, tree_snapshot,
};
use crate::tui::{ApprovalChoice, HostEvent, QueueDrainState, UserAction};
use crossterm::event::KeyCode;
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;

type Step = (u64, KeyCode);

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

fn busy_with_queue() -> App {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![
            queue_item("q1", 1, "first"),
            queue_item("q2", 1, "second"),
            queue_item("q3", 1, "third"),
        ],
        drain: QueueDrainState::LiveAutoDrain,
    });
    pass_time(&mut app, 0);
    app
}

fn approval(app: &mut App, id: &str, subject: &str) {
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: id.into(),
        call_id: Some(format!("call-{id}")),
        tool_name: "exec_command".into(),
        subject: Some(subject.into()),
        prompt: Some("this deletes files".into()),
        expires_at_ms: 0,
    });
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

fn stops(sent: &[UserAction]) -> bool {
    sent.iter().any(|action| {
        matches!(
            action,
            UserAction::StopAllTopology
                | UserAction::CloseTopologyNode { .. }
                | UserAction::InterruptTopologyNode { .. }
        )
    })
}

// ---- Reaching an irreversible action --------------------------------------

/// The operator moved onto the action, then backs out with `Esc` and goes
/// on. On a legacy terminal `Esc` and the next key arrive as one `Alt` chord.
/// `Alt+Enter` and `Alt+n` are that `Esc`: each cancels the pop-up, so the
/// following `Enter` does not stop anything.
#[test]
fn esc_coalesced_with_the_next_key_confirms_instead_of_cancelling() {
    let mut hits = Vec::new();
    for tail in ["alt-enter", "alt-n-then-enter"] {
        let mut app = agents_app();
        let mut sent = run(
            &mut app,
            &[(0, KeyCode::Char('s')), (1_200, KeyCode::Right)],
        );
        assert!(app.destructive_confirm.is_some());
        if tail == "alt-enter" {
            sent.extend(app.handle_key(key_alt(KeyCode::Enter)));
        } else {
            sent.extend(app.handle_key(key_alt(KeyCode::Char('n'))));
            sent.extend(run(&mut app, &[(200, KeyCode::Enter)]));
        }
        if stops(&sent) {
            hits.push(format!("→ then Esc {tail}: {sent:?}"));
        }
    }
    assert!(hits.is_empty(), "{hits:#?}");
}

/// The resume picker routes `key.code` only: `Esc Enter` coalesced into
/// `Alt+Enter` (the operator declining, then sending a draft) resumes the
/// highlighted session.
#[test]
fn esc_enter_coalesced_in_the_resume_picker_resumes() {
    let mut app = resume_fixture();
    let sent = app.handle_key(key_alt(KeyCode::Enter));
    assert!(
        !matches!(
            sent,
            Some(UserAction::ResumeSession { .. } | UserAction::ResumeSessionHandoff { .. })
        ),
        "Alt+Enter in the picker sent {sent:?}"
    );
}

/// Ctrl+S on the focused queue sends the selected row. With a half-typed
/// draft (Tab focuses the queue from a non-empty draft) Ctrl+S steers the draft.
#[test]
fn ctrl_s_on_the_focused_queue_steers_a_half_draft() {
    let mut app = busy_with_queue();
    run(&mut app, &typed_at("half a tho", 90));
    run(&mut app, &[(500, KeyCode::Tab), (500, KeyCode::Down)]);
    assert!(app.queue_focused);
    pass_time(&mut app, 500);
    let sent = app.handle_key(key_ctrl(KeyCode::Char('s')));
    assert!(
        matches!(&sent, Some(UserAction::SendQueuedNow { id, .. }) if id == "q2"),
        "Ctrl+S on the focused queue with q2 selected sent {sent:?}"
    );
}

/// There is no ignored-arrow flash. `s`, `→`, `Enter` confirms on the first
/// `Enter`; the pop-up does not stay open for a later try.
#[test]
fn the_first_enter_confirms_and_nothing_stays_open() {
    let mut app = agents_app();
    let sent = run(
        &mut app,
        &[
            (0, KeyCode::Char('s')),
            (0, KeyCode::Right),
            (0, KeyCode::Enter),
        ],
    );
    assert!(stops(&sent), "s → Enter sent {sent:?}");
    assert!(app.destructive_confirm.is_none());
}

/// The approval chip's settle is carried by rect and choice only. When one
/// approval resolves and the next lands in the same frame at the same cells,
/// a click aimed at the old chip resolves the new approval at once.
#[test]
fn chip_settle_carries_over_to_the_next_approval() {
    let mut app = busy_with_queue();
    let filler: String = (0..80).map(|i| format!("line {i}\n\n")).collect();
    super::test_support::say(&mut app, &filler);
    approval(&mut app, "approval-1", "rm -rf build");
    render_to_string(&mut app, 120, 30);
    pass_time(&mut app, 1_000);
    render_to_string(&mut app, 120, 30);
    let old = app
        .approval_chip_hits
        .iter()
        .find(|hit| hit.choice == ApprovalChoice::Allow)
        .map(|hit| hit.rect)
        .expect("the first approval's allow chip");
    app.apply_host_event(HostEvent::ApprovalResolved {
        approval_id: "approval-1".into(),
        resolution: "allowed".into(),
    });
    approval(&mut app, "approval-2", "rm -rf dist!");
    render_to_string(&mut app, 120, 30);
    let hit = app
        .approval_chip_hits
        .iter()
        .find(|hit| hit.choice == ApprovalChoice::Allow)
        .map(|hit| hit.rect)
        .expect("the new approval's allow chip");
    assert_eq!(hit, old, "the new chip is painted where the old one was");
    let sent = app.handle_mouse(left_click(hit.x, hit.y));
    assert_eq!(sent, None, "a click on a just-painted chip sent {sent:?}");
}

/// Control: `/fork` + `Enter` runs fork, not a completion to
/// fork-then-compact that a second `Enter` would run.
#[test]
fn control_slash_fork_enter_runs_fork() {
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    let mut steps = typed_at("/fork", 90);
    steps.push((90, KeyCode::Enter));
    let sent = run(&mut app, &steps);
    assert_eq!(
        sent,
        vec![UserAction::ForkSession {
            account: None,
            compact: false
        }],
        "draft now {:?}",
        app.input
    );
}

// ---- Deliberate-use feel ---------------------------------------------------

/// `↓` and `Enter` or `c` act at once, including right after other letters.
/// The view shows no typing hint.
#[test]
fn down_then_enter_or_c_acts_at_once() {
    for code in [KeyCode::Enter, KeyCode::Char('c')] {
        let mut app = agents_app();
        run(&mut app, &typed_at("bump", 0));
        run(&mut app, &[(0, KeyCode::Down), (0, code)]);
        assert!(
            app.viewed_agent_id().is_some() || app.destructive_confirm.is_some(),
            "{code:?} did nothing"
        );
        let screen = render_to_string(&mut app, 120, 30);
        assert!(
            !screen.contains("you were typing") && !screen.contains("pause to"),
            "{screen}"
        );
    }
}

/// `c` asks the moment it is pressed, including a few seconds after other
/// letters. Nothing about the earlier letters has to idle out first.
#[test]
fn c_asks_seconds_after_other_letters() {
    let mut app = agents_app();
    run(&mut app, &typed_at("hello", 80));
    // The draft's mapped first letter `h` moved the selection to the root
    // row, where `c` closes nothing; the probe is about the wait.
    app.tree_navigator.as_mut().unwrap().focus_on("child-a");
    run(
        &mut app,
        &[(3_000, KeyCode::Char('c')), (3_000, KeyCode::Char('c'))],
    );
    assert!(
        app.destructive_confirm.is_some(),
        "c pressed 3 s and 6 s after stray typing never asked"
    );
}

/// A deliberate operator's `s`, a 0.8 s read, `→`, `Enter` confirms at once.
/// There is no settle: the arrow moves and the first `Enter` stops.
#[test]
fn deliberate_confirm_after_a_short_read() {
    let mut app = agents_app();
    let sent = run(
        &mut app,
        &[
            (0, KeyCode::Char('s')),
            (800, KeyCode::Right),
            (200, KeyCode::Enter),
        ],
    );
    assert!(
        sent.contains(&UserAction::StopAllTopology),
        "s, 0.8 s, →, Enter: {sent:?}; open {}",
        app.destructive_confirm.is_some()
    );
}

// ---- The decisions' other halves -------------------------------------------

/// An `Alt` chord where a modal confirms on `Enter` is its `Esc`: the
/// pop-up cancels, the picker declines, Menu closes; nothing is sent.
#[test]
fn an_alt_chord_is_esc_wherever_a_modal_confirms_on_enter() {
    let mut app = agents_app();
    run(
        &mut app,
        &[(0, KeyCode::Char('s')), (1_200, KeyCode::Right)],
    );
    assert_eq!(app.handle_key(key_alt(KeyCode::Char('n'))), None);
    assert!(app.destructive_confirm.is_none());
    let mut app = resume_fixture();
    assert_eq!(app.handle_key(key_alt(KeyCode::Enter)), None);
    assert!(app.resume_menu.is_none(), "Alt+Enter declines the picker");
    let mut app = App::new(launch_basic(), true);
    let _ = app.activate_control_tab(super::ControlTab::Settings);
    assert_eq!(app.handle_key(key_alt(KeyCode::Enter)), None);
    assert!(app.control_tab.is_none(), "Alt+Enter closes Menu");
}

/// `s` asks the moment it is pressed. The view does not say the operator
/// was typing.
#[test]
fn s_asks_at_once_and_shows_no_typing_hint() {
    let mut app = agents_app();
    run(&mut app, &typed_at("bump", 0));
    assert_eq!(app.handle_key(key(KeyCode::Char('s'))), None);
    assert!(app.destructive_confirm.is_some(), "s asked");
    let screen = render_to_string(&mut app, 120, 30);
    assert!(
        !screen.contains("you were typing") && !screen.contains("pause to"),
        "{screen}"
    );
}

/// A fast `s`, `→`, `Enter` confirms. It does not cancel for a later try.
#[test]
fn a_fast_s_right_enter_confirms() {
    let mut app = agents_app();
    let sent = run(
        &mut app,
        &[
            (0, KeyCode::Char('s')),
            (0, KeyCode::Right),
            (0, KeyCode::Enter),
        ],
    );
    assert_eq!(sent, vec![UserAction::StopAllTopology]);
    assert!(app.destructive_confirm.is_none());
}

// ---- The grace holds through a typo fix with pauses -----------------------

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
        output: "line\n".into(),
    }]));
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

fn destructive(sent: &[UserAction]) -> bool {
    sent.iter().any(|action| {
        matches!(
            action,
            UserAction::StopAllTopology
                | UserAction::CloseTopologyNode { .. }
                | UserAction::InterruptTopologyNode { .. }
                | UserAction::StopProcesses { .. }
        )
    })
}

/// A mapped letter followed by `Enter` confirms at once in every no-prompt
/// view. Arrows in between move focus; they do not swallow the letter.
#[test]
fn a_mapped_letter_then_enter_confirms_in_every_promptless_view() {
    type View = fn() -> App;
    let cases: Vec<(&str, View, char)> = vec![
        ("agents", agents_app, 's'),
        ("processes", processes_app, 's'),
        ("agent view", agent_view_app, 'c'),
    ];
    for (view, make, letter) in cases {
        let mut app = make();
        let sent = run(
            &mut app,
            &[
                (0, KeyCode::Char(letter)),
                (0, KeyCode::Right),
                (0, KeyCode::Enter),
            ],
        );
        assert!(
            destructive(&sent),
            "{view} {letter:?} then Enter sent {sent:?}"
        );
    }
}
