//! The typing-safety sweep (spec 02 §5.2, §5.3, §5.7): one test per path a
//! review found from typing, editing keys, paste or a click to a destructive
//! or irreversible intent, in Activity, the agent view, the root's busy `Esc`,
//! the queue, slash commands, quit over processes and the process output
//! view. Timed sequences per view are in `typing_timed_tests`.

use super::App;
use super::destructive_confirm::DestructiveConfirm;
use super::test_support::{
    FIXED_NOW, confirm_action, key, key_alt, key_ctrl, launch_basic, left_click, open_tree,
    pass_time, queue_item, render_to_string, tree_node, tree_snapshot,
};
use crate::tui::{HostEvent, QueueDrainState, UserAction};
use crossterm::event::KeyCode;
use plakous_host_protocol::topology::NodeStatus;
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
    )
}

/// A fixture that opens one view.
type View = fn() -> App;

fn press(app: &mut App, codes: &[KeyCode]) -> Vec<UserAction> {
    codes
        .iter()
        .filter_map(|code| app.handle_key(key(*code)))
        .collect()
}

fn typed(text: &str) -> Vec<KeyCode> {
    text.chars().map(KeyCode::Char).collect()
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
        ]),
    );
    app.tree_navigator.as_mut().unwrap().focus_on("child-a");
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

fn agent_view_app() -> App {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ])));
    app.open_agent_view("child-a".into(), "#child-a · description".into());
    app
}

fn busy_with_queue() -> App {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "first"), queue_item("q2", 1, "second")],
        drain: QueueDrainState::RestoredPaused,
    });
    app
}

// Activity > Agents.

/// `q` is not a key in Activity: it does nothing, and only `Esc` leaves.
#[test]
fn q_in_activity_does_nothing_and_only_esc_leaves() {
    let mut app = agents_app();
    assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
    assert!(app.activity_tab.is_some(), "q left Activity");
    assert!(app.destructive_confirm.is_none());
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        !screen.contains("pause to") && !screen.contains("no prompt"),
        "{screen}"
    );
    app.handle_key(key(KeyCode::Esc));
    assert!(app.activity_tab.is_none());
}

/// The root row is never a target of interrupt or close.
#[test]
fn i_and_c_on_the_root_row_open_nothing() {
    let mut app = agents_app();
    app.tree_navigator.as_mut().unwrap().focus_on("root");
    for code in [KeyCode::Char('i'), KeyCode::Char('c')] {
        assert_eq!(app.handle_key(key(code)), None);
        assert!(
            app.destructive_confirm.is_none(),
            "{code:?} on the root row"
        );
    }
}

/// `r` on a paused tree asks first; the pop-up's button resumes.
#[test]
fn r_on_a_paused_tree_opens_the_pop_up() {
    let mut app = App::new(launch_basic(), true);
    let mut snapshot = tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ]);
    snapshot.state = plakous_host_protocol::topology::TreeAdmissionState::RestoredPaused;
    open_tree(&mut app, snapshot);
    assert_eq!(app.handle_key(key(KeyCode::Char('r'))), None);
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("resume")
    );
    assert_eq!(
        confirm_action(&mut app),
        Some(UserAction::ResumeTopologyTree)
    );
}

/// An Alt chord is not `s`: where there is no prompt it does nothing.
#[test]
fn alt_letters_do_nothing_where_there_is_no_prompt() {
    for make in [agents_app as View, agent_view_app] {
        let mut app = make();
        for code in [KeyCode::Char('s'), KeyCode::Char('i'), KeyCode::Char('c')] {
            assert_eq!(app.handle_key(key_alt(code)), None);
            assert!(app.destructive_confirm.is_none(), "Alt+{code:?}");
        }
    }
}

/// A mapped key acts the moment it is pressed, even right after an unmapped
/// letter. There is no burst to wait out.
#[test]
fn a_mapped_key_acts_straight_after_an_unmapped_letter() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Char('w')));
    app.handle_key(key(KeyCode::Char('f')));
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        screen.contains("f live") || screen.contains("all agents"),
        "f toggled the filter at once: {screen}"
    );
}

/// The pop-up closes when its agent stops being active.
#[test]
fn the_pop_up_closes_when_its_target_settles() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Char('i')));
    assert!(app.destructive_confirm.is_some());
    let mut done = tree_node("child-a", Some("root"), 1);
    done.status = NodeStatus::Completed;
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        done,
    ])));
    assert!(app.destructive_confirm.is_none());
}

/// A narrow Agents footer keeps `i`, `c` and `s` short and clickable.
#[test]
fn a_narrow_agents_footer_keeps_clickable_short_keys() {
    let mut app = agents_app();
    render_to_string(&mut app, 40, 20);
    let (hit, _) = *app
        .key_label_hits
        .iter()
        .find(|(_, code)| *code == KeyCode::Char('s'))
        .expect("s is clickable at 40 columns");
    assert_eq!(app.handle_mouse(left_click(hit.x, hit.y)), None);
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("stop-all")
    );
}

// Prompt-less Ctrl chords and paste.

/// Ctrl+D, Ctrl+S, Ctrl+Enter and the draft ladder belong to the composer:
/// where there is no prompt they drop, send and edit nothing.
#[test]
fn composer_chords_do_nothing_where_there_is_no_prompt() {
    let views: [(&str, View); 2] = [("agents", agents_app), ("agent view", agent_view_app)];
    for (name, make) in views {
        let mut app = make();
        app.apply_host_event(HostEvent::QueueSnapshot {
            items: vec![queue_item("q1", 1, "first")],
            drain: QueueDrainState::LiveAutoDrain,
        });
        app.set_input("hidden draft".into());
        for chord in [
            KeyCode::Char('d'),
            KeyCode::Char('s'),
            KeyCode::Enter,
            KeyCode::Char('u'),
            KeyCode::Char('w'),
            KeyCode::Char('k'),
        ] {
            assert_eq!(
                app.handle_key(key_ctrl(chord)),
                None,
                "{name}: Ctrl+{chord:?}"
            );
        }
        assert_eq!(app.input, "hidden draft", "{name}");
    }
    let mut processes = App::new(launch_basic(), true);
    running_process(&mut processes);
    processes.open_processes();
    for chord in [KeyCode::Char('s'), KeyCode::Char('d')] {
        assert_eq!(processes.handle_key(key_ctrl(chord)), None);
        assert!(processes.destructive_confirm.is_none(), "Ctrl+{chord:?}");
    }
}

/// Ctrl+C in the agent view goes back; it never interrupts the root turn.
#[test]
fn ctrl_c_in_the_agent_view_goes_back() {
    let mut app = agent_view_app();
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert_eq!(app.viewed_agent_id(), None);
    assert!(!app.cancel_pending);
}

/// A paste under the turn stop pop-up reaches neither the draft nor a button.
#[test]
fn a_paste_under_the_turn_stop_pop_up_does_nothing() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(app.handle_paste("y\n"), None);
    assert_eq!(app.input, "");
    assert!(app.destructive_confirm.is_some());
    // Focus already sits on Stop turn, so Enter confirms at once.
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Interrupt)
    );
    assert!(app.cancel_pending);
}

// The root's busy Esc.

/// Busy Esc opens the pop-up with both stops, focused on Stop turn. Over the
/// composer a letter closes it into the draft and stops nothing; `Enter` on
/// the pop-up that is still open stops the turn.
#[test]
fn the_busy_esc_pop_up_offers_both_stops_and_enter_stops_the_turn() {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    let screen = render_to_string(&mut app, 100, 30);
    for label in ["[ Cancel ]", "[ Stop turn ]", "[ Stop turn + processes ]"] {
        assert!(screen.contains(label), "{label}: {screen}");
    }
    let sent = press(&mut app, &typed("pass"));
    assert!(sent.iter().all(|action| !irreversible(action)), "{sent:?}");
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.input, "pass");

    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Interrupt)
    );
    assert!(app.cancel_pending);
}

/// Each of the pop-up's buttons is clickable.
#[test]
fn the_busy_esc_pop_up_buttons_are_clickable() {
    for (button, expected) in [
        (1, UserAction::Interrupt),
        (
            2,
            UserAction::StopProcesses {
                id: None,
                interrupt: true,
            },
        ),
    ] {
        let mut app = App::new(launch_basic(), true);
        running_process(&mut app);
        app.apply_host_event(HostEvent::SetBusy(true));
        app.handle_key(key(KeyCode::Esc));
        render_to_string(&mut app, 100, 30);
        let hit = app.destructive_confirm.as_ref().unwrap().action_hits()[button - 1].unwrap();
        assert_eq!(app.handle_mouse(left_click(hit.x, hit.y)), Some(expected));
        assert!(app.cancel_pending);
    }
}

/// The pop-up closes if the turn settles first, and drops `+ processes`
/// once none runs.
#[test]
fn the_busy_esc_pop_up_follows_its_targets() {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    app.apply_process_snapshot(Arc::new(Vec::new()));
    assert_eq!(app.destructive_confirm.as_ref().unwrap().buttons(), 2);
    app.apply_host_event(HostEvent::SetBusy(false));
    assert!(app.destructive_confirm.is_none());
}

// Queue.

/// Backspace never drops; it leaves the queue and edits the draft.
#[test]
fn backspace_with_the_queue_focused_drops_nothing() {
    let mut app = busy_with_queue();
    app.set_input("ab".into());
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.handle_key(key(KeyCode::Backspace)), None);
    assert!(!app.queue_focused);
    assert_eq!(app.input, "a");
}

/// `d`, `e`, `r` and Delete right after Tab (typing speed) are typing.
/// Enter sends the selected row even inside that run, and leaves the draft.
#[test]
fn queue_keys_inside_a_typed_run_type() {
    for draft in ["d", "e", "r", "dr"] {
        let mut app = busy_with_queue();
        let mut sent: Vec<UserAction> = app.handle_key(key(KeyCode::Tab)).into_iter().collect();
        sent.extend(press(&mut app, &typed(draft)));
        assert!(
            !sent.iter().any(irreversible),
            "Tab then {draft:?} sent {sent:?}"
        );
        assert_eq!(app.input, draft);
    }
    let mut app = busy_with_queue();
    app.set_input("fix".into());
    let mut sent: Vec<UserAction> = app.handle_key(key(KeyCode::Tab)).into_iter().collect();
    sent.extend(app.handle_key(key(KeyCode::Delete)));
    assert!(
        !sent.iter().any(irreversible),
        "Tab then Delete sent {sent:?}"
    );
    let mut app = busy_with_queue();
    app.set_input("fix".into());
    let sent = app.handle_key(key(KeyCode::Tab));
    assert!(sent.is_none());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::SendQueuedNow {
            id: "q1".into(),
            revision: 1
        })
    );
    assert_eq!(app.input, "fix");
    assert!(!app.queue_focused);
}

/// After a pause the queue still takes no letter or `Delete`: they leave
/// it for the draft. Enter sends the selected row. Its other commands are
/// chords and chips.
#[test]
fn queue_letters_never_act_and_its_chords_do() {
    for code in [
        KeyCode::Char('d'),
        KeyCode::Char('e'),
        KeyCode::Char('r'),
        KeyCode::Delete,
    ] {
        let mut app = busy_with_queue();
        app.set_input("fix".into());
        app.handle_key(key(KeyCode::Tab));
        pass_time(&mut app, 1_000);
        let sent = app.handle_key(key(code));
        assert!(
            !sent.iter().any(irreversible),
            "Tab, pause, {code:?} sent {sent:?}"
        );
        assert!(!app.queue_focused, "{code:?} left the queue");
    }
    let mut app = busy_with_queue();
    app.set_input("fix".into());
    app.handle_key(key(KeyCode::Tab));
    pass_time(&mut app, 1_000);
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::SendQueuedNow {
            id: "q1".into(),
            revision: 1
        })
    );
    assert_eq!(app.input, "fix");
    assert!(!app.queue_focused);
    let mut app = busy_with_queue();
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('d'))),
        Some(UserAction::DeleteQueued {
            id: "q2".into(),
            revision: 1
        }),
        "Ctrl+D drops the selected one"
    );
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('r'))),
        Some(UserAction::ResumeQueueDrain)
    );
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('e'))), None);
    assert_eq!(app.input, "second", "Ctrl+E loads the selected one");
    assert!(!app.queue_focused);
    app.input_cursor = 0;
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('d'))),
        None,
        "unfocused, Ctrl+D drops nothing"
    );
    assert_eq!(app.input, "econd", "unfocused, Ctrl+D deletes forward");
}

// Slash commands and quit.

/// Enter runs a palette command only for a prefix of its name: `/tmp`
/// fuzzy-matches `/fork-then-compact` but is submitted as typed.
#[test]
fn a_fuzzy_palette_match_never_runs_on_enter() {
    for draft in ["/tmp", "/opt", "/fc"] {
        let mut app = App::new(launch_basic(), true);
        let mut codes = typed(draft);
        codes.push(KeyCode::Enter);
        let sent = press(&mut app, &codes);
        assert!(
            !sent.iter().any(irreversible),
            "{draft:?} + Enter sent {sent:?}"
        );
    }
    let mut app = App::new(launch_basic(), true);
    let mut codes = typed("/he");
    codes.push(KeyCode::Enter);
    press(&mut app, &codes);
    assert!(app.info_overlay.is_some(), "a prefix still runs");
}

/// With processes running, `/quit` asks in the pop-up. `Enter` quits at once.
/// A letter closes the pop-up into the draft and does not quit.
#[test]
fn quit_over_processes_confirms_on_enter_and_a_letter_closes_into_the_draft() {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    let mut codes = typed("/quit");
    codes.push(KeyCode::Enter);
    let sent = press(&mut app, &codes);
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("quit")
    );
    assert!(!sent.contains(&UserAction::Quit));
    assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(UserAction::Quit));
    assert!(app.should_quit);

    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    let mut codes = typed("/quit");
    codes.push(KeyCode::Enter);
    press(&mut app, &codes);
    assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.input, "y");
    assert!(!app.should_quit);
}

// Processes.

/// The output view maps no stop key: `s` there does nothing and shows no hint.
#[test]
fn s_in_the_process_output_view_stops_nothing() {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.open_processes();
    app.handle_key(key(KeyCode::Enter));
    assert!(app.process_output);
    let sent = press(&mut app, &typed("sS"));
    assert_eq!(
        sent,
        [] as [plakous_host_protocol::SessionIntent<crate::surface_protocol::PlakExtension>; 0]
    );
    assert!(app.destructive_confirm.is_none());
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        !screen.contains("no prompt") && !screen.contains("pause to"),
        "{screen}"
    );
}

/// A narrow Processes footer keeps `s` and `S` short and clickable.
#[test]
fn a_narrow_processes_footer_keeps_clickable_short_keys() {
    let mut app = App::new(launch_basic(), true);
    running_process(&mut app);
    app.open_processes();
    render_to_string(&mut app, 30, 20);
    for code in [KeyCode::Char('s'), KeyCode::Char('S')] {
        assert!(
            app.key_label_hits.iter().any(|(_, hit)| *hit == code),
            "{code:?} is clickable at 30 columns"
        );
    }
}
