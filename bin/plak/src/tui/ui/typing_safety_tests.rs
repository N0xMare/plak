//! Keys in a view with no prompt (spec 02 §5.7). A key the view maps acts at
//! once; a key it does not map does nothing. The pop-up that a mapped key
//! opens confirms on `Enter` at once.

use super::App;
use super::test_support::{
    FIXED_NOW, confirm_action, key, launch_basic, open_tree, pass_time, tree_node, tree_snapshot,
};
use crate::tui::UserAction;
use crossterm::event::KeyCode;
use plakous_host_protocol::views::{ProcessStatus, ProcessView};
use std::sync::Arc;

/// Letters none of these views map. `Enter` after them is the view's own
/// `Enter`, not a confirmation.
const UNMAPPED: &[&str] = &["qq", "zzz", "bump"];

fn destructive(action: &UserAction) -> bool {
    matches!(
        action,
        UserAction::InterruptTopologyNode { .. }
            | UserAction::CloseTopologyNode { .. }
            | UserAction::StopAllTopology
            | UserAction::ResumeTopologyTree
            | UserAction::StopProcesses { .. }
    )
}

/// Types `draft` then Enter, one key event per character, and returns every
/// intent the keys produced.
fn type_draft(app: &mut App, draft: &str) -> Vec<UserAction> {
    draft
        .chars()
        .map(KeyCode::Char)
        .chain([KeyCode::Enter])
        .filter_map(|code| app.handle_key(key(code)))
        .collect()
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
    app.apply_host_event(crate::tui::HostEvent::TopologyNavigatorSnapshot(
        tree_snapshot(vec![
            tree_node("root", None, 0),
            tree_node("child-a", Some("root"), 1),
        ]),
    ));
    app.open_agent_view("child-a".into(), "#child-a · description".into());
    app
}

#[test]
fn unmapped_letters_in_activity_agents_send_no_topology_intent() {
    for draft in UNMAPPED {
        let mut app = agents_app();
        let sent = type_draft(&mut app, draft);
        assert!(
            !sent.iter().any(destructive),
            "{draft:?} + Enter in Activity > Agents sent {sent:?}"
        );
        assert!(app.destructive_confirm.is_none());
    }
}

#[test]
fn unmapped_letters_in_activity_processes_stop_nothing() {
    for draft in UNMAPPED.iter().copied().chain(["qqq"]) {
        let mut app = processes_app();
        let sent = type_draft(&mut app, draft);
        assert!(
            !sent.iter().any(destructive),
            "{draft:?} + Enter in Activity > Processes sent {sent:?}"
        );
    }
}

#[test]
fn unmapped_letters_in_the_agent_view_send_no_topology_intent() {
    for draft in UNMAPPED {
        let mut app = agent_view_app();
        let sent = type_draft(&mut app, draft);
        assert!(
            !sent.iter().any(destructive),
            "{draft:?} + Enter in the agent view sent {sent:?}"
        );
        assert!(app.destructive_confirm.is_none());
    }
}

fn paint(app: &mut App, width: u16, height: u16) -> String {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every surface that opens the pop-up, with the intent it confirms.
fn surfaces() -> Vec<(&'static str, App, UserAction)> {
    let mut agents_interrupt = agents_app();
    agents_interrupt.handle_key(key(KeyCode::Char('i')));
    let mut agents_close = agents_app();
    agents_close.handle_key(key(KeyCode::Char('c')));
    let mut agents_stop_all = agents_app();
    agents_stop_all.handle_key(key(KeyCode::Char('s')));
    let mut view_interrupt = agent_view_app();
    view_interrupt.handle_key(key(KeyCode::Char('i')));
    let mut view_close = agent_view_app();
    view_close.handle_key(key(KeyCode::Char('c')));
    let mut slash_stop_all = agent_view_app();
    // Closing the transcript leaves Activity open; `/stop-all` is the root's.
    slash_stop_all.close_activity();
    assert_eq!(
        slash_stop_all.apply_slash_effect(crate::tui::slash::SlashEffect::Action(
            UserAction::StopAllTopology
        )),
        None,
        "/stop-all opens the pop-up instead of sending"
    );
    let mut process_stop = processes_app();
    process_stop.handle_key(key(KeyCode::Char('s')));
    let mut process_stop_all = processes_app();
    process_stop_all.handle_key(key(KeyCode::Char('S')));
    let child = || "child-a".to_owned();
    vec![
        (
            "agents i",
            agents_interrupt,
            UserAction::InterruptTopologyNode { agent_id: child() },
        ),
        (
            "agents c",
            agents_close,
            UserAction::CloseTopologyNode { agent_id: child() },
        ),
        ("agents s", agents_stop_all, UserAction::StopAllTopology),
        (
            "agent view i",
            view_interrupt,
            UserAction::InterruptTopologyNode { agent_id: child() },
        ),
        (
            "agent view c",
            view_close,
            UserAction::CloseTopologyNode { agent_id: child() },
        ),
        ("/stop-all", slash_stop_all, UserAction::StopAllTopology),
        (
            "processes s",
            process_stop,
            UserAction::StopProcesses {
                id: Some("root:p1".into()),
                interrupt: false,
            },
        ),
        (
            "processes S",
            process_stop_all,
            UserAction::StopProcesses {
                id: None,
                interrupt: false,
            },
        ),
    ]
}

#[test]
fn enter_on_a_freshly_opened_pop_up_confirms() {
    for (surface, mut app, intent) in surfaces() {
        assert!(
            app.destructive_confirm.is_some(),
            "{surface} opens the pop-up"
        );
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(intent),
            "{surface}"
        );
        assert!(
            app.destructive_confirm.is_none(),
            "{surface}: Enter confirms"
        );
    }
}

/// A letter in a full-page pop-up does nothing: it neither confirms nor
/// cancels, and it is not typed into the draft. `Enter` still confirms.
/// Over the composer (`/stop-all` from the root) the letter closes the
/// pop-up into the draft, and `Enter` does not stop.
#[test]
fn a_letter_in_the_pop_up_does_nothing_and_enter_still_confirms() {
    for (surface, mut app, intent) in surfaces() {
        let over_composer = !app.has_no_prompt() && app.control_tab.is_none();
        assert!(app.handle_key(key(KeyCode::Char('y'))).is_none());
        if over_composer {
            assert!(
                app.destructive_confirm.is_none(),
                "{surface}: a letter left the pop-up open"
            );
            assert_eq!(app.input, "y", "{surface}");
            assert_ne!(
                app.handle_key(key(KeyCode::Enter)),
                Some(intent),
                "{surface}"
            );
        } else {
            assert!(
                app.destructive_confirm.is_some(),
                "{surface}: a letter left the pop-up"
            );
            assert!(app.input.is_empty(), "{surface}");
            assert_eq!(
                app.handle_key(key(KeyCode::Enter)),
                Some(intent),
                "{surface}"
            );
        }
    }
}

fn surface(name: &str) -> App {
    surfaces()
        .into_iter()
        .find(|(surface, _, _)| *surface == name)
        .map(|(_, app, _)| app)
        .unwrap()
}

#[test]
fn right_and_tab_move_focus_at_once_and_enter_presses_it() {
    for (surface, mut app, intent) in surfaces() {
        assert_eq!(confirm_action(&mut app), Some(intent), "{surface}");
        assert!(app.destructive_confirm.is_none());
    }
    // Focus starts on the action and moves at once: Left goes back to Cancel,
    // Right does not wrap past the action, and BackTab from there cancels.
    for (name, _, intent) in surfaces() {
        let mut app = surface(name);
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Left));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), None, "{name}");
        let mut app = surface(name);
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(intent), "{name}");
        let mut app = surface(name);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::BackTab));
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            None,
            "{name}: BackTab did not return to Cancel"
        );
    }
}

/// Right after the key that opened it, focus already sits on the action, so
/// `→` then `Enter` confirms at once. There is no settle and no hint.
#[test]
fn right_then_enter_confirms_the_moment_the_pop_up_opens() {
    for (name, _, intent) in surfaces() {
        if name == "/stop-all" {
            continue;
        }
        let mut app = surface(name);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.handle_key(key(KeyCode::Enter)), Some(intent), "{name}");
        assert!(app.destructive_confirm.is_none(), "{name}");
        assert!(!paint(&mut app, 100, 30).contains("pause"), "{name}");
    }
}

#[test]
fn esc_and_ctrl_keys_cancel_the_pop_up() {
    for (surface, mut app, _) in surfaces() {
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.handle_key(key(KeyCode::Esc)), None, "{surface}");
        assert!(app.destructive_confirm.is_none(), "{surface}: Esc cancels");
    }
    for (surface, mut app, _) in surfaces() {
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(
            app.handle_key(super::test_support::key_ctrl(KeyCode::Char('c'))),
            None,
            "{surface}"
        );
        assert!(
            app.destructive_confirm.is_none(),
            "{surface}: Ctrl+C cancels"
        );
        assert!(!app.should_quit);
    }
}

#[test]
fn the_pop_up_is_usable_by_mouse_alone() {
    for (surface, mut app, intent) in surfaces() {
        let screen = paint(&mut app, 100, 30);
        assert!(screen.contains("[ Cancel ]"), "{surface}: {screen}");
        let confirm = app.destructive_confirm.as_ref().unwrap();
        let action = confirm.action_hit().expect("action button");
        let cancel = confirm.cancel_hit().expect("cancel button");
        // A click on Cancel cancels.
        assert_eq!(
            app.handle_mouse(super::test_support::left_click(cancel.x, cancel.y)),
            None
        );
        assert!(app.destructive_confirm.is_none(), "{surface}");
        // A click outside cancels too, and does nothing else.
        let (_, mut app, _) = surfaces()
            .into_iter()
            .find(|(name, _, _)| *name == surface)
            .unwrap();
        paint(&mut app, 100, 30);
        assert_eq!(
            app.handle_mouse(super::test_support::left_click(0, 0)),
            None
        );
        assert!(app.destructive_confirm.is_none(), "{surface}");
        // A click on the action button confirms.
        let (_, mut app, _) = surfaces()
            .into_iter()
            .find(|(name, _, _)| *name == surface)
            .unwrap();
        paint(&mut app, 100, 30);
        assert_eq!(
            app.handle_mouse(super::test_support::left_click(action.x, action.y)),
            Some(intent),
            "{surface}"
        );
        assert!(app.destructive_confirm.is_none());
    }
}

/// The destructive controls are reachable by mouse alone: a click on the
/// painted key label opens the pop-up, as the key does.
#[test]
fn a_click_on_a_painted_control_opens_the_pop_up() {
    let cases: Vec<(&str, App, &str)> = vec![
        ("agents", agents_app(), "s stop all"),
        ("agent view", agent_view_app(), "c close"),
        ("processes", processes_app(), "S stop all"),
    ];
    for (surface, mut app, label) in cases {
        paint(&mut app, 120, 40);
        let (hit, _) = *app
            .key_label_hits
            .iter()
            .find(|(_, code)| *code == KeyCode::Char(label.chars().next().unwrap()))
            .unwrap_or_else(|| panic!("{surface}: {label} is clickable"));
        assert_eq!(
            app.handle_mouse(super::test_support::left_click(hit.x, hit.y)),
            None
        );
        assert!(app.destructive_confirm.is_some(), "{surface}: {label}");
    }
}

#[test]
fn the_pop_up_says_what_will_happen() {
    let screens: Vec<(String, String)> = surfaces()
        .into_iter()
        .map(|(surface, mut app, _)| (surface.to_owned(), paint(&mut app, 100, 30)))
        .collect();
    for (surface, screen) in &screens {
        assert!(
            screen.contains("work in flight is lost"),
            "{surface}: {screen}"
        );
    }
    let find = |name: &str| &screens.iter().find(|(s, _)| s == name).unwrap().1;
    assert!(
        find("agents i").contains("Interrupt #a?"),
        "{}",
        find("agents i")
    );
    assert!(find("agents i").contains("[ Interrupt ]"));
    assert!(find("agents c").contains("Close #a?"));
    assert!(find("agents c").contains("[ Close ]"));
    assert!(find("agents s").contains("Stop all agents?"));
    assert!(
        find("agents s").contains("2 open agents"),
        "{}",
        find("agents s")
    );
    assert!(find("agents s").contains("[ Stop all ]"));
    assert!(
        find("/stop-all").contains("1 open agent:"),
        "{}",
        find("/stop-all")
    );
    assert!(find("processes s").contains("cargo build"));
    assert!(find("processes S").contains("1 running background process"));
}

/// An unmapped letter shows no hint and moves nothing. A mapped key acts at
/// once, with no pause.
#[test]
fn unmapped_letters_show_no_hint_and_mapped_keys_act_at_once() {
    let mut app = agents_app();
    for ch in "bump".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    let screen = paint(&mut app, 100, 30);
    assert!(
        !screen.contains("no prompt") && !screen.contains("pause to"),
        "{screen}"
    );
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child-a")
    );
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child-b")
    );
    app.handle_key(key(KeyCode::Char('k')));
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child-a")
    );

    let mut view = agent_view_app();
    view.handle_key(key(KeyCode::Char('w')));
    let screen = paint(&mut view, 100, 30);
    assert!(
        !screen.contains("no prompt") && !screen.contains("pause to"),
        "{screen}"
    );

    let mut processes = processes_app();
    processes.handle_key(key(KeyCode::Char('w')));
    let screen = paint(&mut processes, 100, 30);
    assert!(
        !screen.contains("no prompt") && !screen.contains("pause to"),
        "{screen}"
    );
}

/// No false composer: nothing boxed stands where the composer was in the
/// agent view, and the view does not say it has no prompt.
#[test]
fn the_agent_view_draws_no_composer_box() {
    let mut app = agent_view_app();
    for (width, height) in [(40, 20), (80, 24), (120, 40)] {
        let screen = paint(&mut app, width, height);
        let rows: Vec<&str> = screen.lines().collect();
        let bottom = &rows[rows.len().saturating_sub(4)..];
        // The transcript is the Activity frame's body, so the frame's own
        // border closes the foot. A composer would put its prompt there.
        assert!(
            bottom
                .iter()
                .all(|row| !row.contains('❯') && !row.contains("no prompt")),
            "{width}x{height}: the frame's foot is not a composer: {screen}"
        );
        assert!(screen.contains("Activity"), "{width}x{height}: {screen}");
    }
}

// Probes: each names a typing, paste or editing path that once acted.

/// A fixture that opens one prompt-less view.
type View = fn() -> App;

/// A mapped letter opens the pop-up at once. `Left` then `Enter` declines;
/// `Enter` alone confirms. A letter in between does neither.
#[test]
fn left_then_enter_declines_and_enter_alone_confirms() {
    let cases: Vec<(&str, View, char)> = vec![
        ("agents", agents_app, 's'),
        ("agent view", agent_view_app, 'c'),
        ("processes", processes_app, 's'),
    ];
    for (surface, make, letter) in cases {
        let mut declined = make();
        assert!(declined.handle_key(key(KeyCode::Char(letter))).is_none());
        declined.handle_key(key(KeyCode::Left));
        assert!(
            declined.handle_key(key(KeyCode::Enter)).is_none(),
            "{surface}"
        );
        assert!(declined.destructive_confirm.is_none());

        let mut confirmed = make();
        assert!(confirmed.handle_key(key(KeyCode::Char(letter))).is_none());
        assert!(confirmed.handle_key(key(KeyCode::Char('z'))).is_none());
        let sent = confirmed.handle_key(key(KeyCode::Enter));
        assert!(
            sent.as_ref().is_some_and(destructive),
            "{surface}: {letter:?} then a letter then Enter sent {sent:?}"
        );
    }
}

/// The root's busy Esc pop-up (spec 02 §5.3) confirms on `Enter` at once.
/// `Esc` cancels. A letter over the composer closes it into the draft
/// (`a_busy_esc_then_a_typed_draft_and_enter_stops_nothing`).
#[test]
fn enter_after_a_busy_esc_stops_the_turn_at_once() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(crate::tui::HostEvent::SetBusy(true));
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Interrupt)
    );

    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(crate::tui::HostEvent::SetBusy(true));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.handle_key(key(KeyCode::Esc)).is_none());
    assert!(app.destructive_confirm.is_none());
}

/// `q` is not a key in Activity. An unmapped draft is not sent, and it does
/// not leave the view.
#[test]
fn a_draft_starting_with_q_in_activity_sends_no_prompt() {
    let cases: Vec<(&str, View)> = vec![("agents", agents_app), ("processes", processes_app)];
    for (surface, make) in cases {
        let mut app = make();
        let sent = type_draft(&mut app, "quick fix for the tests");
        assert!(
            !sent.iter().any(|action| matches!(
                action,
                UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_)
            )),
            "{surface}: a typed draft was sent as {sent:?}"
        );
    }
}

/// On a restored-paused tree `r` opens the resume pop-up at once, and
/// `Enter` resumes. A letter in between does not.
#[test]
fn r_then_enter_resumes_a_restored_paused_tree_at_once() {
    let mut app = App::new(launch_basic(), true);
    app.render_prefs.reduced_motion = true;
    app.agents_clock = Some(FIXED_NOW);
    let mut snapshot = tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
    ]);
    snapshot.state = plakous_host_protocol::topology::TreeAdmissionState::RestoredPaused;
    open_tree(&mut app, snapshot);
    assert!(app.handle_key(key(KeyCode::Char('r'))).is_none());
    assert!(app.destructive_confirm.is_some());
    assert!(app.handle_key(key(KeyCode::Char('u'))).is_none());
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::ResumeTopologyTree)
    );
}

/// Paste routing (spec 02 §4): paste never reaches the composer while a
/// modal owns input, and the agent view's hidden draft is never typed into.
#[test]
fn a_paste_never_reaches_a_hidden_or_covered_composer() {
    let mut view = agent_view_app();
    assert_eq!(view.handle_paste("pass the tests"), None);
    assert!(
        view.input.is_empty(),
        "agent view: the paste landed in the hidden root draft: {:?}",
        view.input
    );
    let mut root = App::new(launch_basic(), true);
    assert_eq!(
        root.apply_slash_effect(crate::tui::slash::SlashEffect::Action(
            UserAction::StopAllTopology
        )),
        None
    );
    assert!(root.destructive_confirm.is_some());
    assert_eq!(root.handle_paste("pass the tests"), None);
    assert!(
        root.input.is_empty(),
        "pop-up open: the paste landed in the composer under it: {:?}",
        root.input
    );
}

/// The pop-up must say what will happen at the 40-column golden width; it
/// clips its consequence line instead of wrapping it.
#[test]
fn the_pop_up_keeps_its_consequence_at_40_columns() {
    for (surface, mut app, _) in surfaces() {
        let screen = paint(&mut app, 40, 24);
        assert!(
            screen.contains("is lost."),
            "{surface}: the consequence is cut at 40 columns:\n{screen}"
        );
    }
}

/// With queued follow-ups, `Tab` focuses the queue; typed letters other
/// than `d`/`e`/`r` still land in the composer, but `d` (and Backspace)
/// drop queued prompts and Enter sends one now.
#[test]
fn typing_with_the_queue_focused_discards_nothing() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(crate::tui::HostEvent::SetBusy(true));
    app.apply_host_event(crate::tui::HostEvent::QueueSnapshot {
        items: vec![
            super::test_support::queue_item("q1", 1, "first"),
            super::test_support::queue_item("q2", 1, "second"),
        ],
        drain: crate::tui::QueueDrainState::LiveAutoDrain,
    });
    let mut sent: Vec<UserAction> = app.handle_key(key(KeyCode::Tab)).into_iter().collect();
    sent.extend(type_draft(&mut app, "add docs"));
    assert!(
        !sent.iter().any(|action| matches!(
            action,
            UserAction::DeleteQueued { .. } | UserAction::SendQueuedNow { .. }
        )),
        "Tab then \"add docs\" + Enter sent {sent:?}"
    );
}

/// A focused approval row owns bare `a`/`d` even with an empty draft, so a
/// reply typed after clicking the row to read it resolves the approval.
#[test]
fn a_reply_typed_after_focusing_an_approval_does_not_resolve_it() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(crate::tui::HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: Some("call-1".into()),
        tool_name: "exec_command".into(),
        subject: Some("rm -rf build".into()),
        prompt: Some("this deletes files".into()),
        expires_at_ms: 0,
    });
    app.pane.focused_block = Some(0);
    let sent = type_draft(&mut app, "actually wait");
    assert!(
        !sent
            .iter()
            .any(|action| matches!(action, UserAction::ResolveApproval { .. })),
        "a typed reply resolved the approval: {sent:?}"
    );
}

/// Spec 02 §5.7: in the agent view "a hidden root draft is never typed into
/// or sent", and Activity leaves the hidden draft alone. `Ctrl+S` and
/// `Ctrl+Enter` reach `submit_line` there and send it unseen.
#[test]
fn ctrl_s_where_there_is_no_prompt_does_not_send_the_hidden_draft() {
    let cases: Vec<(&str, View)> = vec![("agent view", agent_view_app), ("agents", agents_app)];
    for (surface, make) in cases {
        for chord in [KeyCode::Char('s'), KeyCode::Enter] {
            let mut app = make();
            app.set_input("ship it".into());
            let sent = app.handle_key(super::test_support::key_ctrl(chord));
            assert!(
                !matches!(
                    sent,
                    Some(UserAction::Prompt(_) | UserAction::FollowUp(_) | UserAction::Steer(_))
                ),
                "{surface}: Ctrl+{chord:?} sent the hidden draft as {sent:?}"
            );
        }
    }
}

// Typo fixes and slow typists (spec 02 §5.2, §5.7).

/// Arrows move at once, including straight after an unmapped letter. They
/// are not absorbed, and they do not open the pop-up.
#[test]
fn arrows_move_at_once_after_an_unmapped_letter() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Char('q')));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.tree_navigator.as_ref().unwrap().selected_agent_id(),
        Some("child-b")
    );
    assert!(app.destructive_confirm.is_none());
    let screen = paint(&mut app, 100, 30);
    assert!(
        !screen.contains("pause to") && !screen.contains("no prompt"),
        "{screen}"
    );
}

/// A slow typist in the composer: `Tab` (which focuses a non-empty queue)
/// then, after a pause past the 300 ms quiet period, a word starting with
/// `d` (or `Delete`) leaves the queue for the draft and drops nothing.
#[test]
fn a_slow_letter_after_tab_never_drops_a_queued_prompt() {
    for first in [KeyCode::Char('d'), KeyCode::Delete] {
        let mut app = App::new(launch_basic(), true);
        app.apply_host_event(crate::tui::HostEvent::SetBusy(true));
        app.apply_host_event(crate::tui::HostEvent::QueueSnapshot {
            items: vec![
                super::test_support::queue_item("q1", 1, "first"),
                super::test_support::queue_item("q2", 1, "second"),
            ],
            drain: crate::tui::QueueDrainState::LiveAutoDrain,
        });
        app.set_input("fix the ".into());
        pass_time(&mut app, 0);
        let mut sent: Vec<UserAction> = app.handle_key(key(KeyCode::Tab)).into_iter().collect();
        pass_time(&mut app, 400);
        sent.extend(app.handle_key(key(first)));
        for ch in "ocs".chars() {
            pass_time(&mut app, 400);
            sent.extend(app.handle_key(key(KeyCode::Char(ch))));
        }
        assert!(
            !sent
                .iter()
                .any(|action| matches!(action, UserAction::DeleteQueued { .. })),
            "Tab, pause, {first:?} then slow typing dropped a queued prompt: {sent:?}"
        );
    }
}
