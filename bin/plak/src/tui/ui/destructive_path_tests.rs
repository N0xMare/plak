//! Destructive paths and usability gaps in typing, interrupt and Activity
//! (spec 02 §5.2, §5.3, §5.7), each pinned.

use super::App;
use super::destructive_confirm::DestructiveConfirm;
use super::test_support::{
    FIXED_NOW, key, key_alt, key_ctrl, launch_basic, left_click, open_tree, pass_time, queue_item,
    render_to_string, settings_fixture, tree_node, tree_snapshot,
};
use crate::tui::{ApprovalChoice, HostEvent, QueueDrainState, SurfaceIntent, UserAction};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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

fn busy_with_queue(drain: QueueDrainState) -> App {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![
            queue_item("q1", 1, "first"),
            queue_item("q2", 1, "second"),
            queue_item("q3", 1, "third"),
        ],
        drain,
    });
    pass_time(&mut app, 0);
    app
}

fn with_approval(app: &mut App, subject: &str) {
    app.apply_host_event(HostEvent::ApprovalRequired {
        approval_id: "approval-1".into(),
        call_id: Some("call-1".into()),
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
            tree_node("child-c", Some("root"), 1),
        ]),
    );
    app.tree_navigator.as_mut().unwrap().focus_on("child-a");
    pass_time(&mut app, 0);
    app
}

fn selected(app: &App) -> String {
    app.tree_navigator
        .as_ref()
        .and_then(|nav| nav.selected_agent_id())
        .unwrap_or_default()
        .to_owned()
}

// ---- Typing reaching an irreversible action -------------------------------

/// Legacy-encoding terminals (Terminal.app, tmux with its default 500 ms
/// escape-time, or any read that holds ESC and the next byte together)
/// deliver `Esc` then `a` as one `Alt+a`: crossterm 0.29 parse.rs:78-88. A
/// typed `Esc`, "also ..." then allows the open approval; readline's
/// kill-word-forward (`Alt+d`, beside the composer's `Alt+B`/`Alt+F`) denies.
#[test]
fn esc_then_a_letter_or_readline_alt_d_resolves_the_approval() {
    let mut hits = Vec::new();
    for (code, word) in [('a', "lso fix the docs"), ('d', "on't run that")] {
        let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
        with_approval(&mut app, "rm -rf build");
        app.set_input("hmm ".into());
        pass_time(&mut app, 120);
        let mut sent: Vec<UserAction> = app
            .handle_key(key_alt(KeyCode::Char(code)))
            .into_iter()
            .collect();
        sent.extend(run(&mut app, &typed_at(word, 90)));
        if sent
            .iter()
            .any(|action| matches!(action, UserAction::ResolveApproval { .. }))
        {
            hits.push(format!("Esc+{code} (one Alt+{code}) sent {sent:?}"));
        }
    }
    assert!(hits.is_empty(), "{hits:#?}");
}

/// `Ctrl+D` is forward-delete in readline and every macOS text field, and the
/// composer already speaks that dialect (`Ctrl+A/E/K/U/W`). With the queue
/// not focused it drops the last queued prompt, unasked and for good.
#[test]
fn ctrl_d_as_forward_delete_mid_draft_drops_a_queued_prompt() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    run(&mut app, &typed_at("fix teh bug", 90));
    let mut steps = vec![(90, KeyCode::Left); 6];
    steps.push((90, KeyCode::Left));
    let mut sent = run(&mut app, &steps);
    pass_time(&mut app, 150);
    sent.extend(app.handle_key(key_ctrl(KeyCode::Char('d'))));
    assert!(
        !sent
            .iter()
            .any(|action| matches!(action, UserAction::DeleteQueued { .. })),
        "Ctrl+D while editing a draft sent {sent:?}"
    );
}

/// `Ctrl+Q` sits beside `Ctrl+W` (kill word). Mid-turn it quits at once when
/// no background process runs: the root turn's work in flight is lost with
/// no pop-up, while a deliberate `Esc` for the same loss asks.
#[test]
fn ctrl_q_mid_turn_quits_without_the_pop_up() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    pass_time(&mut app, 0);
    app.set_input("fix the bu".into());
    let sent = app.handle_key(key_ctrl(KeyCode::Char('q')));
    assert_ne!(sent, Some(UserAction::Quit), "Ctrl+Q mid-turn quit unasked");
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("quit")
    );
}

/// A doubled `Enter` a beat apart (the "did it take?" press): the first
/// queues the draft, the second, 400 ms later, sends the oldest queued
/// prompt into the running turn now.
#[test]
fn enter_again_after_a_beat_sends_a_queued_prompt_now() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let mut steps = typed_at("then update the docs", 90);
    steps.push((90, KeyCode::Enter));
    steps.push((400, KeyCode::Enter));
    let sent = run(&mut app, &steps);
    assert!(
        !sent
            .iter()
            .any(|action| matches!(action, UserAction::SendQueuedNow { .. })),
        "Enter, 400 ms, Enter sent {sent:?}"
    );
}

/// `Tab`, `↓` onto the second row, `Enter` sends that row now and returns
/// focus to the composer. The draft is untouched.
#[test]
fn enter_on_the_focused_queue_sends_the_selected_and_returns_focus() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    app.set_input("keep this draft".into());
    let sent = run(
        &mut app,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Down),
            (500, KeyCode::Enter),
        ],
    );
    assert_eq!(
        sent,
        vec![UserAction::SendQueuedNow {
            id: "q2".into(),
            revision: 1
        }]
    );
    assert!(!app.queue_focused, "focus returned to the composer");
    assert_eq!(app.input, "keep this draft");
}

/// A doubled `Enter` on the focused queue sends exactly one row: the second
/// press is the composer's empty-Enter rule, inside the send-now gap.
#[test]
fn doubled_enter_on_the_focused_queue_sends_exactly_one() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let sent = run(
        &mut app,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Down),
            (500, KeyCode::Enter),
            (0, KeyCode::Enter),
        ],
    );
    let sends: Vec<_> = sent
        .iter()
        .filter(|action| matches!(action, UserAction::SendQueuedNow { .. }))
        .collect();
    assert_eq!(sends.len(), 1, "{sent:?}");
    assert!(matches!(
        sends[0],
        UserAction::SendQueuedNow { id, .. } if id == "q2"
    ));
    assert!(!app.queue_focused);
}

/// A restored-paused drain still sends: `Enter` is `Ctrl+S`. Focus returns
/// to the composer and the draft is untouched. Offline, `Enter` only leaves.
#[test]
fn enter_on_a_paused_queue_sends_and_offline_only_leaves() {
    let mut paused = busy_with_queue(QueueDrainState::RestoredPaused);
    paused.set_input("draft".into());
    let sent = run(
        &mut paused,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Down),
            (500, KeyCode::Enter),
        ],
    );
    assert_eq!(
        sent,
        vec![UserAction::SendQueuedNow {
            id: "q2".into(),
            revision: 1
        }]
    );
    assert!(!paused.queue_focused);
    assert_eq!(paused.input, "draft");

    let mut offline = busy_with_queue(QueueDrainState::LiveAutoDrain);
    offline.agent_enabled = false;
    offline.set_input("draft".into());
    let sent = run(&mut offline, &[(500, KeyCode::Tab), (500, KeyCode::Enter)]);
    assert!(sent.is_empty() && !offline.queue_focused, "{sent:?}");
    assert_eq!(offline.input, "draft");
}

/// Idle, with a live drain: `Enter` still sends the selected row.
#[test]
fn enter_on_an_idle_queue_sends_the_selected() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    app.apply_host_event(HostEvent::SetBusy(false));
    let sent = run(
        &mut app,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Down),
            (500, KeyCode::Enter),
        ],
    );
    assert_eq!(
        sent,
        vec![UserAction::SendQueuedNow {
            id: "q2".into(),
            revision: 1
        }]
    );
    assert!(!app.queue_focused);
}

/// A doubled `Enter` with a draft sends the row, then the draft. The second
/// press is the composer's `Enter`, not another queued row.
#[test]
fn doubled_enter_with_a_draft_sends_the_row_then_the_draft() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    app.set_input("half-typed draft".into());
    let sent = run(
        &mut app,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Enter),
            (0, KeyCode::Enter),
        ],
    );
    let queued = sent
        .iter()
        .filter(|action| matches!(action, UserAction::SendQueuedNow { .. }))
        .count();
    assert_eq!(queued, 1, "{sent:?}");
    assert!(
        sent.iter().any(
            |action| matches!(action, UserAction::FollowUp(text) if text == "half-typed draft")
        ),
        "{sent:?}"
    );
}

/// A held `Enter` is the same path: one queued send, then the draft.
#[test]
fn held_enter_with_a_draft_sends_the_row_then_the_draft() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    app.set_input("draft".into());
    let mut sent = run(&mut app, &[(500, KeyCode::Tab), (500, KeyCode::Enter)]);
    for _ in 0..3 {
        pass_time(&mut app, 30);
        let mut repeat = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        sent.extend(app.handle_key(repeat));
    }
    let queued = sent
        .iter()
        .filter(|action| matches!(action, UserAction::SendQueuedNow { .. }))
        .count();
    assert_eq!(queued, 1, "{sent:?}");
    assert!(
        sent.iter()
            .any(|action| matches!(action, UserAction::FollowUp(_))),
        "{sent:?}"
    );
}

/// A held `Enter` on an empty draft sends exactly one queued prompt.
#[test]
fn held_enter_on_an_empty_draft_sends_exactly_one() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let mut sent = run(&mut app, &[(500, KeyCode::Tab), (500, KeyCode::Enter)]);
    for _ in 0..20 {
        pass_time(&mut app, 30);
        let mut repeat = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        repeat.kind = KeyEventKind::Repeat;
        sent.extend(app.handle_key(repeat));
    }
    assert_eq!(sent.len(), 1, "{sent:?}");
}

/// After the 2 s empty-Enter gap, a later `Enter` is the composer's and
/// sends the oldest queued follow-up, not a second press of the same row.
#[test]
fn second_enter_after_the_empty_enter_gap_sends_the_oldest() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let first = run(
        &mut app,
        &[
            (500, KeyCode::Tab),
            (500, KeyCode::Down),
            (500, KeyCode::Enter),
        ],
    );
    pass_time(&mut app, 2_100);
    let second = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(first.as_slice(), [UserAction::SendQueuedNow { id, .. }] if id == "q2")
            && matches!(&second, Some(UserAction::SendQueuedNow { id, .. }) if id == "q1"),
        "first={first:?} second={second:?}"
    );
}

/// The unfocused title names `Ctrl+S`, not `Enter`. `Enter` there sends the
/// draft.
#[test]
fn unfocused_queue_title_names_ctrl_s() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let screen = render_to_string(&mut app, 120, 30);
    assert!(
        screen.contains("queue · Ctrl+S sends now · Tab focus"),
        "{screen}"
    );
    assert!(!screen.contains("Enter send now"), "{screen}");
    app.set_input("draft".into());
    pass_time(&mut app, 2_500);
    let sent = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(&sent, Some(UserAction::FollowUp(text)) if text == "draft"),
        "{sent:?}"
    );
}

/// Settings reset opens the shared pop-up at once. `y` does not confirm it;
/// `Enter` does, with no settle.
#[test]
fn settings_d_then_enter_resets_a_row_at_once() {
    let mut app = settings_fixture();
    let sent = run(
        &mut app,
        &[(0, KeyCode::Char('d')), (350, KeyCode::Char('y'))],
    );
    assert!(sent.is_empty(), "y confirmed a reset: {sent:?}");
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("settings-reset")
    );
    let sent = app.handle_key(key(KeyCode::Enter));
    assert!(
        matches!(
            sent,
            Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
        ),
        "Enter confirmed: {sent:?}"
    );
    assert!(app.destructive_confirm.is_none());
}

/// A mapped letter opens the pop-up at once. `→` does not leave the action
/// on a one-action pop-up, so `Enter` confirms with no think.
#[test]
fn one_letter_then_right_enter_confirms_at_once() {
    for letter in ['s', 'c', 'i'] {
        let mut app = agents_app();
        let sent = run(
            &mut app,
            &[
                (0, KeyCode::Char(letter)),
                (0, KeyCode::Right),
                (0, KeyCode::Enter),
            ],
        );
        assert!(!sent.is_empty(), "{letter:?}, →, Enter confirmed nothing");
        assert!(app.destructive_confirm.is_none());
    }
}

/// There is no burst to absorb a typo fix. `s` opens the pop-up at once and
/// the `Enter` after it confirms, arrows in between included.
#[test]
fn s_then_arrows_then_enter_stops_all() {
    let mut app = agents_app();
    let sent = run(
        &mut app,
        &[
            (0, KeyCode::Char('s')),
            (0, KeyCode::Right),
            (0, KeyCode::Left),
            (0, KeyCode::Right),
            (0, KeyCode::Enter),
        ],
    );
    assert_eq!(sent, vec![UserAction::StopAllTopology]);
}

/// A two-letter palette word and `Enter` runs the highlighted destructive
/// command: `/co` compacts (paid, irreversible), `/d` drops the last queued
/// prompt, `/f` forks.
#[test]
fn two_letters_and_enter_in_the_palette_run_a_destructive_command() {
    let mut hits = Vec::new();
    for draft in ["/co", "/d", "/f", "/cl", "/c", "/re"] {
        let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
        app.apply_host_event(HostEvent::SetBusy(false));
        let mut steps = typed_at(draft, 90);
        steps.push((90, KeyCode::Enter));
        let sent = run(&mut app, &steps);
        if sent.iter().any(|action| {
            matches!(
                action,
                UserAction::Compact
                    | UserAction::DropQueued
                    | UserAction::DeleteQueued { .. }
                    | UserAction::ForkSession { .. }
                    | UserAction::ClearSession
                    | UserAction::ResumeSession { .. }
                    | UserAction::Recover
            )
        }) {
            hits.push(format!("{draft} + Enter sent {sent:?}"));
        }
    }
    assert!(hits.is_empty(), "{hits:#?}");
}

/// The approval chips are found with `str::find` on the whole row, so a
/// subject that contains `[allow]` or `[deny]` moves the hit target onto
/// the model-written subject and off the painted chip.
#[test]
fn approval_chip_hit_follows_text_in_the_subject() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    with_approval(&mut app, "printf '[allow]\\n' > policy");
    let screen = render_to_string(&mut app, 140, 30);
    let hit = app
        .approval_chip_hits
        .iter()
        .find(|hit| hit.choice == ApprovalChoice::Allow)
        .expect("an allow hit")
        .rect;
    let line = screen.lines().nth(usize::from(hit.y)).unwrap();
    let tail: String = line.chars().skip(usize::from(hit.x)).collect();
    assert!(
        tail.starts_with("[allow] Alt+A"),
        "the [allow] hit is on the subject: {line:?}"
    );
}

// ---- Usability -------------------------------------------------------------

/// Arrows navigate at once, including right after unmapped letters.
#[test]
fn arrows_pressed_while_reading_the_hint_never_navigate() {
    let mut app = agents_app();
    run(&mut app, &typed_at("hello", 80));
    let before = selected(&app);
    run(
        &mut app,
        &[
            (1_500, KeyCode::Down),
            (1_500, KeyCode::Down),
            (1_500, KeyCode::Down),
        ],
    );
    assert_ne!(selected(&app), before, "three ↓ over 4.5 s moved nothing");
}

/// `s`, `→`, `Enter` inside a few hundred milliseconds confirms. Nothing
/// says to pause.
#[test]
fn a_fast_deliberate_confirm_stops_all() {
    let mut app = agents_app();
    let sent = run(
        &mut app,
        &[
            (0, KeyCode::Char('s')),
            (250, KeyCode::Right),
            (150, KeyCode::Enter),
        ],
    );
    assert_eq!(sent, vec![UserAction::StopAllTopology]);
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        !screen.contains("cancelled ·") && !screen.contains("pause"),
        "{screen}"
    );
}

/// Control: a click on the painted action presses it at once.
#[test]
fn control_click_on_the_action_confirms_at_once() {
    let mut app = agents_app();
    app.handle_key(key(KeyCode::Char('s')));
    render_to_string(&mut app, 100, 30);
    let hit = app
        .destructive_confirm
        .as_ref()
        .and_then(DestructiveConfirm::action_hit)
        .expect("painted action");
    assert_eq!(
        app.handle_mouse(left_click(hit.x, hit.y)),
        Some(UserAction::StopAllTopology)
    );
}

/// `/resume` and a doubled `Enter`: the host's list lands between the two
/// presses and the second resumes the top row at once. The picker has no
/// settle.
#[test]
fn a_doubled_enter_after_slash_resume_resumes_the_top_row() {
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    let mut steps = typed_at("/resume", 80);
    steps.push((80, KeyCode::Enter));
    let first = run(&mut app, &steps);
    assert_eq!(first, vec![UserAction::ResumeSession { id: None }]);
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(
        super::test_support::resume_fixture_entries(),
    ));
    assert!(app.resume_menu.is_some(), "the picker opened");
    let sent = run(&mut app, &[(120, KeyCode::Enter)]);
    assert!(
        sent.iter().any(|action| matches!(
            action,
            UserAction::ResumeSession { id: Some(_) } | UserAction::ResumeSessionHandoff { .. }
        )),
        "Enter resumes the open picker at once: {sent:?}"
    );
}

// ---- The deliberate paths stay one gesture ---------------------------------

/// `Alt+A` asks in the pop-up, which names the tool and the subject; `Enter`
/// allows. `Alt+D` asks to deny.
#[test]
fn alt_a_and_alt_d_ask_in_the_pop_up_and_confirm_resolves() {
    for (code, choice, kind, title) in [
        ('a', ApprovalChoice::Allow, "approval-allow", "Allow"),
        ('d', ApprovalChoice::Deny, "approval-deny", "Deny"),
    ] {
        let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
        with_approval(&mut app, "rm -rf build");
        assert_eq!(app.handle_key(key_alt(KeyCode::Char(code))), None);
        assert_eq!(
            app.destructive_confirm
                .as_ref()
                .map(DestructiveConfirm::kind),
            Some(kind)
        );
        let screen = render_to_string(&mut app, 120, 30);
        assert!(
            screen.contains(title) && screen.contains("rm -rf build"),
            "{screen}"
        );
        assert_eq!(
            super::test_support::confirm_action(&mut app),
            Some(UserAction::ResolveApproval {
                approval_id: "approval-1".into(),
                choice,
            })
        );
    }
}

/// A click on the painted `[allow]` chip stays one step, once the chip has
/// held its place for 300 ms; a click sooner does nothing.
#[test]
fn an_approval_chip_click_resolves_once_it_has_held_its_place() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    with_approval(&mut app, "rm -rf build");
    render_to_string(&mut app, 140, 30);
    let hit = app
        .approval_chip_hits
        .iter()
        .find(|hit| hit.choice == ApprovalChoice::Allow)
        .expect("an allow hit")
        .rect;
    assert_eq!(app.handle_mouse(left_click(hit.x, hit.y)), None);
    pass_time(&mut app, 300);
    render_to_string(&mut app, 140, 30);
    assert_eq!(
        app.handle_mouse(left_click(hit.x, hit.y)),
        Some(UserAction::ResolveApproval {
            approval_id: "approval-1".into(),
            choice: ApprovalChoice::Allow,
        })
    );
    assert!(app.destructive_confirm.is_none());
}

/// A queue chip that just moved under the pointer does not fire; the same
/// chip 300 ms later does.
#[test]
fn a_queue_chip_fires_only_after_it_held_its_place() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    render_to_string(&mut app, 100, 30);
    let drop = app.queue_button_hits[0].drop;
    assert_eq!(app.handle_mouse(left_click(drop.x, drop.y)), None);
    pass_time(&mut app, 300);
    render_to_string(&mut app, 100, 30);
    assert!(matches!(
        app.handle_mouse(left_click(drop.x, drop.y)),
        Some(UserAction::DeleteQueued { .. })
    ));
}

/// With no approval open, `Alt+D` is readline's kill-word-forward.
#[test]
fn alt_d_without_an_approval_deletes_the_word_after_the_caret() {
    let mut app = App::new(launch_basic(), true);
    app.set_input("fix the bug".into());
    app.input_cursor = 3;
    assert_eq!(app.handle_key(key_alt(KeyCode::Char('d'))), None);
    assert_eq!(app.input, "fix bug");
    assert!(app.destructive_confirm.is_none());
}

/// Unfocused, `Ctrl+D` deletes the character after the caret; on the
/// focused queue it drops the selected row.
#[test]
fn ctrl_d_deletes_forward_and_drops_only_on_the_focused_queue() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    app.set_input("abc".into());
    app.input_cursor = 1;
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('d'))), None);
    assert_eq!(app.input, "ac");
    app.set_input(String::new());
    run(&mut app, &[(500, KeyCode::Tab), (500, KeyCode::Down)]);
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('d'))),
        Some(UserAction::DeleteQueued {
            id: "q2".into(),
            revision: 1
        })
    );
}

/// Idle with nothing running, `Ctrl+Q` quits at once; with a live agent or
/// a background process it asks, naming what stops.
#[test]
fn ctrl_q_asks_only_while_something_runs() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('q'))),
        Some(UserAction::Quit)
    );
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.handle_key(key_ctrl(KeyCode::Char('q')));
    let screen = render_to_string(&mut app, 120, 30);
    assert!(
        screen.contains("Quit plak?") && screen.contains("root turn is cancelled"),
        "{screen}"
    );
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Quit)
    );
    let mut app = App::new(launch_basic(), true);
    app.tree_live_descendants = 2;
    app.set_input("/quit".into());
    assert_eq!(app.handle_key(key(KeyCode::Enter)), None);
    let screen = render_to_string(&mut app, 120, 30);
    assert!(
        screen.contains("2 running agents are interrupted"),
        "{screen}"
    );
}

/// On the focused queue `Ctrl+S` still sends the selected prompt now.
/// `Enter` does the same and returns focus, so it is not a second path.
#[test]
fn ctrl_s_on_the_focused_queue_sends_the_selected() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    run(&mut app, &[(500, KeyCode::Tab), (500, KeyCode::Down)]);
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('s'))),
        Some(UserAction::SendQueuedNow {
            id: "q2".into(),
            revision: 1
        })
    );
    assert!(app.queue_focused, "Ctrl+S leaves focus where it was");
}

/// After a pause of two seconds with no typing, an empty `Enter` sends the
/// oldest queued prompt now.
#[test]
fn an_empty_enter_after_a_two_second_pause_sends_the_oldest_now() {
    let mut app = busy_with_queue(QueueDrainState::LiveAutoDrain);
    let mut steps = typed_at("then update the docs", 90);
    steps.push((90, KeyCode::Enter));
    steps.push((2_000, KeyCode::Enter));
    let sent = run(&mut app, &steps);
    assert!(
        sent.iter().any(|action| matches!(
            action,
            UserAction::SendQueuedNow { id, .. } if id == "q1"
        )),
        "{sent:?}"
    );
}

/// The palette: `Enter` on a partial irreversible word completes it and runs
/// nothing, a doubled `Enter` runs nothing, and `Enter` on the full name
/// after a look runs it. A partial word for an ordinary command still runs.
#[test]
fn irreversible_palette_commands_run_only_from_their_full_name() {
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    let mut steps = typed_at("/co", 90);
    steps.push((90, KeyCode::Enter));
    steps.push((120, KeyCode::Enter));
    assert!(run(&mut app, &steps).is_empty());
    assert_eq!(app.input, "/compact");
    assert_eq!(
        run(&mut app, &[(700, KeyCode::Enter)]),
        vec![UserAction::Compact]
    );
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    let mut steps = typed_at("/clear", 90);
    steps.push((90, KeyCode::Enter));
    assert_eq!(run(&mut app, &steps), vec![UserAction::ClearSession]);
    let mut app = App::new(launch_basic(), true);
    pass_time(&mut app, 0);
    let mut steps = typed_at("/sett", 90);
    steps.push((90, KeyCode::Enter));
    run(&mut app, &steps);
    assert!(app.control_tab.is_some(), "a partial ordinary command runs");
}

/// The resume picker maps no letter: a draft leaves the selection where it
/// was, and `Enter` resumes at once.
#[test]
fn the_resume_picker_ignores_a_draft_and_enter_resumes_at_once() {
    for draft in ["quick fix", "just do it", "keep going"] {
        let mut app = App::new(launch_basic(), true);
        app.pending_open_resume = true;
        app.apply_host_event(HostEvent::ResumeSessions(
            super::test_support::resume_fixture_entries(),
        ));
        let before = app.resume_menu.as_ref().unwrap().selected;
        let sent = run(&mut app, &typed_at(draft, 0));
        assert!(sent.is_empty(), "{draft:?} sent {sent:?}");
        let menu = app.resume_menu.as_ref().expect("the picker stays open");
        assert_eq!(menu.selected, before, "{draft:?} moved the selection");
        let screen = render_to_string(&mut app, 100, 30);
        assert!(
            !screen.contains("pause to") && !screen.contains("you were typing"),
            "{screen}"
        );
    }
    let mut app = App::new(launch_basic(), true);
    app.pending_open_resume = true;
    app.apply_host_event(HostEvent::ResumeSessions(
        super::test_support::resume_fixture_entries(),
    ));
    let sent = run(&mut app, &[(0, KeyCode::Enter)]);
    assert!(
        matches!(
            sent.as_slice(),
            [UserAction::ResumeSession { id: Some(_) } | UserAction::ResumeSessionHandoff { .. }]
        ),
        "{sent:?}"
    );
}

/// `s` opens the pop-up the moment it is pressed, even straight after other
/// letters. There is no grace window.
#[test]
fn s_opens_the_pop_up_straight_after_other_letters() {
    let mut app = agents_app();
    run(&mut app, &typed_at("he", 0));
    assert_eq!(app.handle_key(key(KeyCode::Char('s'))), None);
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(DestructiveConfirm::kind),
        Some("stop-all")
    );
}

/// Agents' `y` copies at once, including right after another letter. It does
/// not open the pop-up.
#[test]
fn agents_y_copies_straight_after_a_letter() {
    let mut app = agents_app();
    run(
        &mut app,
        &[(0, KeyCode::Char('e')), (0, KeyCode::Char('y'))],
    );
    assert!(app.destructive_confirm.is_none());
    assert!(app.activity_tab.is_some());
}

/// Busy `Esc`, then `→` and `Enter` with no pause, stops the turn. The view
/// does not say to pause.
#[test]
fn esc_then_right_enter_stops_the_turn_at_once() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    let sent = run(
        &mut app,
        &[(0, KeyCode::Esc), (0, KeyCode::Right), (0, KeyCode::Enter)],
    );
    assert_eq!(sent, vec![UserAction::Interrupt]);
    let screen = render_to_string(&mut app, 100, 30);
    assert!(!screen.contains("pause"), "{screen}");
}

/// Unmapped letters in Activity show no typing hint.
#[test]
fn unmapped_letters_show_no_typing_hint() {
    let mut app = agents_app();
    run(&mut app, &typed_at("bump", 0));
    let screen = render_to_string(&mut app, 120, 30);
    assert!(
        !screen.contains("no prompt")
            && !screen.contains("pause to")
            && !screen.contains("you were typing"),
        "{screen}"
    );
}

/// Settings' reset confirms with `Enter` at once. `Esc` cancels and writes
/// nothing. No settle separates `d` from either key.
#[test]
fn settings_reset_enter_confirms_and_esc_cancels() {
    let mut app = settings_fixture();
    let before = app.settings_menu.as_ref().unwrap().draft.clone();
    assert!(run(&mut app, &[(0, KeyCode::Char('d'))]).is_empty());
    let screen = render_to_string(&mut app, 100, 30);
    assert!(
        screen.contains("Reset Scroll lines?") && screen.contains("Scroll lines returns to 3."),
        "{screen}"
    );
    assert_eq!(
        app.destructive_confirm
            .as_ref()
            .map(|confirm| confirm.focus),
        Some(1),
        "focus starts on Reset"
    );
    assert!(run(&mut app, &[(700, KeyCode::Esc)]).is_empty());
    assert!(app.destructive_confirm.is_none());
    assert_eq!(app.settings_menu.as_ref().unwrap().draft, before);

    app.handle_key(key(KeyCode::Char('d')));
    render_to_string(&mut app, 100, 30);
    let hit = app
        .destructive_confirm
        .as_ref()
        .and_then(DestructiveConfirm::action_hit)
        .expect("Reset is painted");
    let clicked = app.handle_mouse(left_click(hit.x, hit.y));
    assert!(
        matches!(
            clicked,
            Some(UserAction::Extension(SurfaceIntent::SaveSettings(_)))
        ),
        "a click on Reset sent {clicked:?}"
    );
    assert!(app.destructive_confirm.is_none());

    let sent = run(&mut app, &[(0, KeyCode::Char('d')), (0, KeyCode::Enter)]);
    assert!(
        sent.iter().any(|action| matches!(
            action,
            UserAction::Extension(SurfaceIntent::SaveSettings(_))
        )),
        "d, Enter sent {sent:?}"
    );
}
