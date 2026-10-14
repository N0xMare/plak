//! Shell lifecycle, host event, and queue tests.

use super::terminal::{
    TerminalRestoration, combine_terminal_cleanup, dispatch_action, request_terminal_handoff,
    send_user_action,
};
use super::test_support::{
    blocks_contain, key, key_ctrl, launch_basic, left_click, left_mouse, open_tree, providers_app,
    providers_text, queue_item, screen_text, serialize_frame, status_row, tool_line, tree_node,
    tree_snapshot, typed, visual_fixture,
};
use super::{App, QUITTING_STATUS, format_status_elapsed, user_phase, user_status_detail};
use crate::tui::blocks::ScrollBlock;
use crate::tui::control::{is_drop_command, is_quit_command, local_echo_line, route_submit};
use crate::tui::presentation::ActivityKind;
use crate::tui::quit_guard::{CtrlCOutcome, classify_ctrl_c};
use crate::tui::redraw::{FrameWork, RedrawReason, RedrawSchedule, RedrawState, RedrawWait};
use crate::tui::{
    HostEvent, QueueDrainState, TuiError, TuiHandoff, TuiLaunch, UserAction, user_action_channel,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::{Duration, Instant};

/// `render_scrim` was on every overlay and the
/// transcript still framed each one at readable contrast. `Modifier::DIM`
/// is a no-op on a great many terminals, and a quarter-strength blend left
/// the backdrop legible, so the scrim is asserted on the *cells*: every
/// painted backdrop cell must move measurably toward the app ground, in
/// both fg and bg, against the same frame drawn undimmed.
#[test]
fn the_scrim_darkens_the_backdrop_cells_themselves_not_only_the_modifier() {
    fn channels(color: Color) -> (u16, u16, u16) {
        match color {
            Color::Rgb(r, g, b) => (r.into(), g.into(), b.into()),
            other => panic!("the scrim must resolve every colour to RGB, got {other:?}"),
        }
    }

    let mut app = App::new(launch_basic(), true);
    for index in 0..30 {
        app.push_block(ScrollBlock::Assistant(format!(
            "line {index} of transcript text that runs the whole width of the pane"
        )));
    }
    app.mark_transcript_structural();
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let before = terminal.backend().buffer().clone();

    app.resume_menu = Some(crate::tui::ResumeMenu::open(
        super::test_support::resume_fixture_entries(),
        super::test_support::FIXED_NOW,
        jiff::tz::TimeZone::UTC,
    ));
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let after = terminal.backend().buffer().clone();
    let popup = app.resume_popup.expect("the resume picker painted");
    let ground = channels(app.theme.app_bg);

    let mut compared = 0usize;
    for y in 0..40u16 {
        for x in 0..120u16 {
            if popup.contains((x, y).into()) {
                continue;
            }
            let plain = &before[(x, y)];
            let dimmed = &after[(x, y)];
            if plain.symbol().trim().is_empty() {
                continue;
            }
            compared += 1;
            let fg = channels(dimmed.style().fg.expect("the scrim paints an explicit fg"));
            let bg = channels(dimmed.style().bg.expect("the scrim paints an explicit bg"));
            // Every scrimmed cell sits within a sixth of the app ground on
            // every channel: the backdrop stops competing with the modal.
            for (dimmed, ground) in [(fg.0, ground.0), (fg.1, ground.1), (fg.2, ground.2)] {
                assert!(
                    dimmed.abs_diff(ground) <= 42,
                    "a backdrop fg at ({x},{y}) is still at contrast: {dimmed} vs {ground}"
                );
            }
            assert_eq!(bg, ground, "a backdrop bg at ({x},{y}) is not the ground");
            // And it is a change: the undimmed cell was not already there.
            let plain_fg = plain.style().fg.map_or(ground, channels);
            assert_ne!(
                (fg, bg),
                (
                    plain_fg,
                    channels(plain.style().bg.unwrap_or(app.theme.app_bg))
                ),
                "the scrim left the cell at ({x},{y}) untouched"
            );
        }
    }
    assert!(
        compared > 100,
        "the transcript framed the modal: {compared}"
    );

    // The modal body itself is untouched.
    let inner = &after[(popup.x + popup.width / 2, popup.y + popup.height / 2)];
    assert!(
        !inner.modifier.contains(ratatui::style::Modifier::DIM),
        "the modal body stays opaque"
    );
}

#[test]
fn user_status_mapping_hides_runtime_jargon() {
    assert_eq!(user_phase("idle"), "ready");
    assert_eq!(user_phase("preparing"), "starting…");
    assert_eq!(user_phase("not_ready"), "setup needed");
    assert_eq!(user_phase("interrupted"), "interrupted");
    assert_eq!(user_phase("error"), "needs attention");
    let detail = user_status_detail("canonical Plakous locator recovery-required");
    assert!(!detail.contains("canonical"));
    assert!(!detail.contains("Plakous"));
    assert!(!detail.contains("locator"));
    assert!(detail.contains("needs recovery"));
}

#[test]
fn cleanup_errors_are_combined_with_primary_failure() {
    let result = combine_terminal_cleanup(
        Err(TuiError::Io("event read failed".into())),
        &[
            "keyboard flags: denied".into(),
            "mouse capture: denied".into(),
        ],
    );
    let Err(TuiError::Io(message)) = result else {
        panic!("expected combined I/O error");
    };
    assert!(message.contains("event read failed"));
    assert!(message.contains("keyboard flags: denied"));
    assert!(message.contains("mouse capture: denied"));
}

#[test]
fn idle_composer_has_no_state_label_or_stop_button() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(false));
    assert!(app.turn_status_label().is_none());
    assert!(!status_row(&mut app, 80).contains("[stop]"));
}

#[test]
fn clicking_and_ctrl_o_cycle_the_same_row_through_its_modes() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(tool_line(
        "call",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("cargo test"),
    ));
    app.apply_host_event(tool_line(
        "call-2",
        None,
        None,
        ActivityKind::Tool,
        "exec_command",
        "done",
        Some("cargo build"),
    ));
    app.apply_host_event(HostEvent::AssistantDelta("Finished.".into()));
    app.apply_host_event(HostEvent::AssistantDone);
    let mut terminal = Terminal::new(TestBackend::new(80, 14)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let &(index, row) = app.tool_hit_rows.first().unwrap();
    app.handle_mouse(left_click(4, row));
    app.handle_mouse(left_mouse(MouseEventKind::Up(MouseButton::Left), 4, row));
    assert!(!crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[index].group_folded);
    app.handle_key(key_ctrl(KeyCode::Char('o')));
    assert!(crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[index].group_folded);
    app.handle_key(key(KeyCode::Enter));
    assert!(!crate::tui::blocks::plan_rows(&app.pane.blocks, app.render_prefs)[index].group_folded);
}

#[test]
fn a_steer_ack_does_not_add_a_system_row() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::steer("status update?"),
    ));
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(2),
    });
    assert_eq!(
        app.pane.blocks,
        vec![ScrollBlock::steer("status update?".into())]
    );
}

#[test]
fn quit_commands_recognized() {
    assert!(is_quit_command("/quit"));
    assert!(is_quit_command("/exit"));
    for bare in ["quit", "exit", ":q", "hello"] {
        assert!(!is_quit_command(bare), "{bare}");
    }
    assert!(is_drop_command("/drop"));
    assert!(!is_drop_command("drop"));
}

/// A bare quit word is a prompt, idle or mid-turn; only the slash spelling
/// quits.
#[test]
fn bare_quit_words_are_ordinary_prompts() {
    for busy in [false, true] {
        for word in ["quit", "exit", ":q"] {
            let mut app = App::new(launch_basic(), true);
            app.busy = busy;
            app.set_input(word.to_owned());
            let expected = if busy {
                UserAction::FollowUp(word.to_owned())
            } else {
                UserAction::Prompt(word.to_owned())
            };
            assert_eq!(app.submit_line(false), Some(expected), "{word} busy={busy}");
            assert!(!app.should_quit, "{word} busy={busy} must not quit");
        }
        for slash in ["/quit", "/exit"] {
            let mut app = App::new(launch_basic(), true);
            app.busy = busy;
            app.set_input(slash.to_owned());
            if busy {
                // Mid-turn it asks first (spec 02 §5.3).
                assert_eq!(app.submit_line(false), None, "{slash}");
                assert!(!app.should_quit && app.destructive_confirm.is_some());
            } else {
                assert_eq!(app.submit_line(false), Some(UserAction::Quit), "{slash}");
                assert!(app.should_quit, "{slash} busy={busy} quits");
            }
        }
    }
}

#[test]
fn route_submit_idle_vs_busy() {
    assert_eq!(
        route_submit(false, false, "hi".into()),
        UserAction::Prompt("hi".into())
    );
    assert_eq!(
        route_submit(true, false, "hi".into()),
        UserAction::FollowUp("hi".into())
    );
    assert_eq!(
        route_submit(true, true, "now".into()),
        UserAction::Steer("now".into())
    );
    assert_eq!(
        route_submit(false, true, "now".into()),
        UserAction::Prompt("now".into())
    );
}

#[test]
fn route_ctrl_c_matrix_clear_cancel_escalate() {
    let now = Instant::now();
    assert_eq!(
        classify_ctrl_c(false, true, false, None, now),
        CtrlCOutcome::ClearDraft
    );
    assert_eq!(
        classify_ctrl_c(true, true, false, None, now),
        CtrlCOutcome::Interrupt
    );
    // Idle with an empty draft arms quit; a second press inside the window quits.
    assert_eq!(
        classify_ctrl_c(true, false, false, None, now),
        CtrlCOutcome::ArmQuit
    );
    // A stuck cancel also escalates.
    assert_eq!(
        classify_ctrl_c(true, true, true, None, now),
        CtrlCOutcome::ArmQuit
    );
}

#[test]
fn local_echo_labels() {
    assert_eq!(
        local_echo_line(&UserAction::Prompt("a".into()), false).as_deref(),
        Some("a")
    );
    assert_eq!(
        local_echo_line(&UserAction::FollowUp("b".into()), false).as_deref(),
        Some("[queue] b")
    );
    // Hosted path: host echoes user — no local Prompt echo.
    assert_eq!(local_echo_line(&UserAction::Prompt("a".into()), true), None);
    assert_eq!(local_echo_line(&UserAction::Steer("c".into()), true), None);
    assert_eq!(local_echo_line(&UserAction::Interrupt, false), None);
    assert_eq!(local_echo_line(&UserAction::DropQueued, false), None);
    assert_eq!(local_echo_line(&UserAction::ClearSession, false), None);
}

#[test]
fn ctrl_b_backgrounds_only_a_running_foreground_process() {
    let mut app = App::new(launch_basic(), true);
    let background = key_ctrl(KeyCode::Char('b'));
    assert_eq!(app.handle_key(background), None);
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_eq!(app.handle_key(background), None);
    app.apply_host_event(tool_line(
        "process",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "running",
        Some("read line"),
    ));
    assert_eq!(app.handle_key(background), Some(UserAction::Background));
    app.apply_host_event(tool_line(
        "process",
        None,
        None,
        ActivityKind::Process,
        "exec_command",
        "done",
        Some("read line"),
    ));
    assert_eq!(app.handle_key(background), None);
}

#[test]
fn submit_refused_when_hosted_not_ready() {
    let mut launch = launch_basic();
    launch.agent_enabled = false;
    let mut app = App::new(launch, true);
    assert!(app.pane.blocks.is_empty(), "no static application header");
    app.input = "hello".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, None);
    assert!(blocks_contain(&app, "queued until agent is ready"));
    assert_eq!(app.input, "hello");
    assert_eq!(app.pending_prompt.as_deref(), Some("hello"));
}

#[test]
fn terminal_handoffs_remain_typed_and_quit_the_shell() {
    for handoff in [
        TuiHandoff::EditConfig,
        TuiHandoff::ResumeSession { id: "s-1".into() },
    ] {
        let mut app = App::new(launch_basic(), true);
        request_terminal_handoff(&mut app, None, handoff.clone());
        assert_eq!(app.terminal_handoff, Some(handoff));
        assert!(app.should_quit);
    }
}

#[test]
fn submit_busy_is_follow_up() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.input = "later".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::FollowUp("later".into())));
    // The host owns queue truth; the UI waits for its authoritative snapshot.
    assert!(app.local_queue.is_empty());
}

#[test]
fn hosted_busy_follow_up_queue_chip_is_real_after_snapshot() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.input = "later".into();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::FollowUp("later".into()))
    );
    assert_eq!(app.queue_len, 0);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-later", 1, "later")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(app.queue_len, 1);
    assert_eq!(app.local_queue.len(), 1);
}

/// Click targets for `[now]` / `[drop]` must overlap the painted chips, not
/// sit on the panel's right edge while the chips follow the preview text.
#[test]
fn queue_now_and_drop_chips_are_clickable() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-click", 3, "hello")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = screen_text(&terminal);
    assert!(screen.contains("[now]"), "{screen}");
    assert!(screen.contains("[drop]"), "{screen}");

    let hit = app
        .queue_button_hits
        .first()
        .expect("queue chips publish hit rects");
    let (idx, now_r, drop_r) = (hit.abs_idx, hit.now, hit.drop);
    assert_eq!(idx, 0);
    assert!(now_r.width >= 5, "now chip: {now_r:?}");
    assert!(drop_r.width >= 6, "drop chip: {drop_r:?}");

    let painted_now = (0..now_r.width)
        .map(|dx| {
            terminal.backend().buffer()[(now_r.x.saturating_add(dx), now_r.y)]
                .symbol()
                .to_owned()
        })
        .collect::<String>();
    assert!(
        painted_now.contains("[now]"),
        "hit rect must cover the painted chip, got {painted_now:?} at {now_r:?}"
    );
    // A chip fires once it has held its place for CHIP_SETTLE.
    super::test_support::pass_time(&mut app, 300);

    assert_eq!(
        app.handle_mouse(left_click(now_r.x, now_r.y)),
        Some(UserAction::SendQueuedNow {
            id: "q-click".into(),
            revision: 3,
        })
    );

    terminal.draw(|frame| app.draw(frame)).unwrap();
    let drop_r = app.queue_button_hits[0].drop;
    let painted_drop = (0..drop_r.width)
        .map(|dx| {
            terminal.backend().buffer()[(drop_r.x.saturating_add(dx), drop_r.y)]
                .symbol()
                .to_owned()
        })
        .collect::<String>();
    assert!(
        painted_drop.contains("[drop]"),
        "hit rect must cover the painted chip, got {painted_drop:?} at {drop_r:?}"
    );
    assert_eq!(
        app.handle_mouse(left_click(drop_r.x, drop_r.y)),
        Some(UserAction::DeleteQueued {
            id: "q-click".into(),
            revision: 3,
        })
    );
}

#[test]
fn refused_queue_edit_adopts_snapshot_revision_and_applied_edit_clears_exact_text() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.queue_editing = Some(super::QueueEdit {
        id: "q-1".into(),
        revision: 1,
        replaced_draft: String::new(),
    });
    app.set_input("edited".into());
    let action = UserAction::EditQueued {
        id: "q-1".into(),
        revision: 1,
        text: "edited".into(),
    };
    assert!(send_user_action(&mut app, Some(&sender), action));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-1", 2, "old")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.apply_host_event(HostEvent::QueueEditRefused { id: "q-1".into() });
    assert_eq!(app.input, "edited");
    assert_eq!(
        app.queue_editing.as_ref().map(super::QueueEdit::target),
        Some(("q-1", 2))
    );

    let retry = UserAction::EditQueued {
        id: "q-1".into(),
        revision: 2,
        text: "edited".into(),
    };
    assert!(send_user_action(&mut app, Some(&sender), retry));
    app.apply_host_event(HostEvent::QueueEditApplied { id: "q-1".into() });
    assert!(app.input.is_empty());
    assert!(app.queue_editing.is_none());
}

#[test]
fn submit_busy_ctrl_enter_is_steer() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.input = "now".into();
    let action = app.handle_key(key_ctrl(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Steer("now".into())));
    // Echo waits until host-mailbox admission.
    assert!(!blocks_contain(&app, "now"));
}

#[test]
fn busy_composer_steer_survives_continuous_queue_snapshot_refreshes() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.input = "wake the long wait and use this instruction".into();
    for revision in 1..=24 {
        app.apply_host_event(HostEvent::QueueSnapshot {
            items: vec![queue_item("q-busy", revision, "queued follow-up")],
            drain: QueueDrainState::LiveAutoDrain,
        });
    }
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Enter)),
        Some(UserAction::Steer(
            "wake the long wait and use this instruction".into()
        ))
    );
}

#[test]
fn submit_busy_ctrl_s_is_steer_fallback() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    app.input = "now".into();
    let action = app.handle_key(key_ctrl(KeyCode::Char('s')));
    assert_eq!(action, Some(UserAction::Steer("now".into())));
}

#[test]
fn set_busy_host_event_affects_next_submit() {
    let mut app = App::new(launch_basic(), true);
    app.input = "first".into();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Prompt("first".into()))
    );

    app.apply_host_event(HostEvent::SetBusy(true));
    assert!(app.busy);

    app.input = "queued".into();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::FollowUp("queued".into()))
    );

    app.apply_host_event(HostEvent::SetBusy(false));
    app.input = "again".into();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::Prompt("again".into()))
    );
}

#[test]
fn ctrl_c_idle_double_tap_quits_and_busy_cancels() {
    let mut app = App::new(TuiLaunch::default(), false);
    assert!(app.handle_key(key_ctrl(KeyCode::Char('c'))).is_none());
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('c'))),
        Some(UserAction::Quit)
    );
    assert!(app.should_quit);

    let mut busy_app = App::new(launch_basic(), true);
    busy_app.busy = true;
    busy_app.input.clear();
    assert_eq!(busy_app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut busy_app),
        Some(UserAction::Interrupt)
    );
    assert!(!busy_app.should_quit);
}

#[test]
fn esc_cancels_the_turn_and_never_quits() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    // Mid-turn Esc opens the pop-up; Enter on its Stop turn button emits
    // the interrupt.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(app.destructive_confirm.is_some());
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Interrupt)
    );
    assert!(!app.should_quit);
    assert!(app.cancel_pending);
    // A second Esc while the cancel is in flight is a no-op, not an escalation.
    assert_eq!(app.handle_key(key(KeyCode::Esc)), None);
    assert!(!app.should_quit);
}

#[test]
fn ctrl_d_and_drop_command() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('d'))), None);

    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-last", 7, "last")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let expected = Some(UserAction::DeleteQueued {
        id: "q-last".into(),
        revision: 7,
    });
    // Unfocused, `Ctrl+D` is the composer's forward delete.
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('d'))), None);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('d'))), expected);
    app.handle_key(key(KeyCode::Esc));

    app.input = "/drop".into();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::DeleteQueued {
            id: "q-last".into(),
            revision: 7,
        })
    );
    assert!(app.input.is_empty());
}

#[test]
fn queue_edit_and_reorder_target_snapshot_identity() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-one", 2, "one"), queue_item("q-two", 4, "two")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.queue_focused = true;
    app.queue_selected = 1;

    let mut shifted_up = key(KeyCode::Up);
    shifted_up.modifiers = KeyModifiers::SHIFT;
    assert_eq!(
        app.handle_key(shifted_up),
        Some(UserAction::MoveQueued {
            id: "q-two".into(),
            revision: 4,
            new_index: 0,
        })
    );

    assert!(app.handle_key(key_ctrl(KeyCode::Char('e'))).is_none());
    assert_eq!(app.input, "two");
    app.input = "two edited".into();
    app.input_cursor = app.input.chars().count();
    assert_eq!(
        app.handle_key(key(KeyCode::Enter)),
        Some(UserAction::EditQueued {
            id: "q-two".into(),
            revision: 4,
            text: "two edited".into(),
        })
    );
    assert_eq!(app.input, "two edited");
    assert_eq!(
        app.queue_editing.as_ref().map(super::QueueEdit::target),
        Some(("q-two", 4))
    );
    app.apply_host_event(HostEvent::Row(
        plakous_host_protocol::views::transcript::TranscriptRow::error(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            "queue edit refused",
        ),
    ));
    assert_eq!(app.input, "two edited");
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![
            queue_item("q-one", 2, "one"),
            queue_item("q-two", 5, "two edited"),
        ],
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(app.input, "two edited");
    assert_eq!(
        app.queue_editing.as_ref().map(super::QueueEdit::target),
        Some(("q-two", 5))
    );
    app.apply_host_event(HostEvent::QueueEditRefused { id: "q-two".into() });
    assert_eq!(app.input, "two edited");
    assert_eq!(
        app.queue_editing.as_ref().map(super::QueueEdit::target),
        Some(("q-two", 5))
    );
}

#[test]
fn submit_quit_sets_flag() {
    let mut app = App::new(TuiLaunch::default(), false);
    app.input = "/quit".into();
    let action = app.handle_key(key(KeyCode::Enter));
    assert_eq!(action, Some(UserAction::Quit));
    assert!(app.should_quit);
}

#[test]
fn protected_capacity_recovery_refuses_compaction() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::ContextCapacityRecovery(Some(
        crate::tui::ContextCapacityRecovery::Protected,
    )));
    let backend = TestBackend::new(90, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();

    let rendered = serialize_frame(terminal.backend().buffer());
    assert!(rendered.contains("Protected·context·cannot·be·compacted"));
    assert!(rendered.contains("larger·model,·fork,·or·/clear"));
    assert!(app.capacity_recovery_action.is_none());
}

#[test]
fn turn_status_spinner_advances_and_reduced_motion_is_static() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::Status {
        phase: "running".into(),
        queue_len: 0,
        detail: Some("working".into()),
    });
    app.apply_host_event(HostEvent::SetBusy(true));
    let backend = TestBackend::new(80, 12);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let first = app.status_spinner();
    assert!(app.advance_status_spinner(app.status_spinner_at + Duration::from_millis(101)));
    assert_ne!(app.status_spinner(), first);

    // §5.5 reduced motion: one static glyph, and no frame at the spinner
    // cadence. The row still repaints once a second so the §5.4 phase and
    // turn timers are not frozen between host events.
    app.render_prefs.reduced_motion = true;
    assert_eq!(app.status_spinner(), "⠿");
    assert!(!app.advance_status_spinner(app.status_spinner_at + Duration::from_millis(101)));
    assert!(app.advance_status_spinner(app.status_spinner_at + Duration::from_secs(1)));
    assert_eq!(app.status_spinner(), "⠿", "the glyph never moves");
}

/// An overlay that only cleared its own popup left the
/// transcript framing it at full contrast on both sides. §5.5 makes the
/// scrim part of every overlay, not just the Menu.
#[test]
fn activity_covers_the_surface_behind_it() {
    let mut app = App::new(launch_basic(), true);
    for index in 0..40 {
        app.push_block(ScrollBlock::user(format!(
            "line {index} of transcript text that fills the width"
        )));
    }
    app.mark_transcript_structural();

    let snapshot = tree_snapshot(vec![tree_node("root", None, 0)]);
    open_tree(&mut app, snapshot);

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let popup = app.tree_popup.expect("the canvas painted a popup");
    assert_eq!(popup, ratatui::layout::Rect::new(0, 0, 100, 30));
    let screen = serialize_frame(terminal.backend().buffer());
    assert!(
        !screen.contains("transcript·text"),
        "nothing of the transcript shows beside Activity: {screen}"
    );
    let buffer = terminal.backend().buffer();
    assert!(
        (popup.y + 1..popup.bottom() - 1).any(|y| {
            !buffer[(popup.x + 1, y)]
                .style()
                .add_modifier
                .contains(ratatui::style::Modifier::DIM)
        }),
        "the overlay body itself stays opaque"
    );
}

#[test]
fn restored_paused_queue_snapshot_after_attach_stays_paused() {
    let mut app = App::new(launch_basic(), true);
    let (tx, rx) = user_action_channel();
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.maybe_request_tree_snapshot_after_attach(Some(&tx));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-restored", 1, "later")],
        drain: QueueDrainState::RestoredPaused,
    });
    assert_eq!(app.queue_drain, QueueDrainState::RestoredPaused);
    assert_eq!(
        rx.try_recv().expect("attach should request the navigator"),
        UserAction::OpenTopologyNavigator
    );
    assert!(
        rx.try_recv().is_err(),
        "paused queue drain must not auto-resume"
    );
}

/// Spec 02 §5.4 segment 5. The count is the host's non-terminal descendant
/// count from the last topology snapshot; clicking it opens the canvas.
#[test]
fn the_turn_status_row_names_the_live_agent_count() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::TopologyNavigatorSnapshot(tree_snapshot(vec![
        tree_node("root", None, 0),
        tree_node("child-a", Some("root"), 1),
        tree_node("child-b", Some("root"), 1),
    ])));
    assert_eq!(app.tree_live_descendants, 2, "the root is not a descendant");
    let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    assert!(serialize_frame(terminal.backend().buffer()).contains("2·agents"));
    let hit = app.status_agents_hit.expect("the segment is clickable");
    assert_eq!(
        app.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        }),
        Some(UserAction::OpenTopologyNavigator)
    );
}

#[test]
fn complete_normal_and_narrow_frames_match_reviewable_goldens() {
    for (width, height, narrow, path, expected) in [
        (
            80,
            30,
            false,
            "tests/goldens/chrome-normal.frame",
            include_str!("../../../tests/goldens/chrome-normal.frame"),
        ),
        (
            48,
            22,
            true,
            "tests/goldens/chrome-narrow.frame",
            include_str!("../../../tests/goldens/chrome-narrow.frame"),
        ),
    ] {
        let mut app = visual_fixture(narrow);
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let actual = serialize_frame(terminal.backend().buffer());
        if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
            std::fs::write(path, &actual).expect("write reviewed frame golden");
        } else {
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn terminal_restoration_guard_runs_once_during_unwind() {
    let restores = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&restores);
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = TerminalRestoration::with_test_hook(move || {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        panic!("fault after terminal initialization");
    }));
    assert!(unwound.is_err());
    assert_eq!(restores.load(Ordering::SeqCst), 1);
}

#[test]
fn ctrl_c_ladder_clears_then_cancels_then_escalates() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    typed(&mut app, "draft");

    // 1. A non-empty draft: clear it. That is the only effect.
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.input.is_empty());
    assert!(!app.cancel_pending);
    assert!(!app.should_quit);

    // 2. Empty draft with a turn running: ask, then cancel this turn.
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(!app.cancel_pending);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Interrupt)
    );
    assert!(app.cancel_pending);

    // 3. Cancel in flight and unsettled: arm, then confirm, quit.
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.quit_arm.is_some());
    assert!(!app.should_quit);
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('c'))),
        Some(UserAction::Quit)
    );
    assert!(app.should_quit);
}

/// §6: the frame painted before the loop exits names the quitting state
/// on the HUD, so a slow release reads as a state, not a freeze.
#[test]
fn quit_paints_the_quitting_status_on_its_last_frame() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.turn_status_label(), None);
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('q'))),
        Some(UserAction::Quit)
    );
    assert!(app.should_quit && app.quitting && app.dirty);
    assert_eq!(app.turn_status_label().as_deref(), Some(QUITTING_STATUS));
    assert!(app.turn_status_label().is_some());

    let mut slash = App::new(launch_basic(), true);
    slash.input = "/quit".to_owned();
    slash.input_cursor = slash.input.len();
    assert_eq!(
        slash.handle_key(KeyEvent::from(KeyCode::Enter)),
        Some(UserAction::Quit)
    );
    assert_eq!(slash.turn_status_label().as_deref(), Some(QUITTING_STATUS));
}

#[test]
fn voluntary_quit_does_not_ask_the_host_to_shut_down() {
    let (sender, receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    dispatch_action(&mut app, &UserAction::Quit, Some(&sender));
    assert!(app.should_quit && app.quitting);
    assert!(
        receiver.try_recv().is_err(),
        "quit must not start host shutdown"
    );
}

#[test]
fn ctrl_c_idle_empty_double_tap_quits() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.quit_arm.is_some());
    assert_eq!(
        app.handle_key(key_ctrl(KeyCode::Char('c'))),
        Some(UserAction::Quit)
    );
    assert!(app.should_quit);
}

#[test]
fn a_settled_turn_disarms_the_ctrl_c_escalation() {
    let mut app = App::new(launch_basic(), true);
    app.busy = true;
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert_eq!(
        super::test_support::confirm_action(&mut app),
        Some(UserAction::Interrupt)
    );
    app.apply_host_event(HostEvent::SetBusy(false));
    assert!(!app.cancel_pending);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('c'))), None);
    assert!(app.quit_arm.is_some());
    assert!(!app.should_quit);
}

#[test]
fn ctrl_dot_is_gone_from_the_keymap_and_from_every_hint() {
    let mut app = App::new(launch_basic(), true);
    assert_eq!(app.handle_key(key_ctrl(KeyCode::Char('.'))), None);
    assert_eq!(app.control_tab, None);
    let sheet = crate::tui::keymap::sheet_text();
    assert!(
        !sheet.contains("Ctrl+."),
        "keybind sheet still names Ctrl+."
    );
    assert!(sheet.contains("Ctrl+X"));
    assert!(sheet.contains("Ctrl+Q"));
}

#[test]
fn status_elapsed_follows_the_spec_duration_ladder() {
    assert_eq!(format_status_elapsed(Duration::from_millis(500)), "0.5s");
    assert_eq!(format_status_elapsed(Duration::from_millis(5_200)), "5.2s");
    assert_eq!(format_status_elapsed(Duration::from_millis(10_400)), "10s");
    assert_eq!(format_status_elapsed(Duration::from_secs(65)), "1m05s");
    assert_eq!(format_status_elapsed(Duration::from_secs(80)), "1m20s");
    assert_eq!(format_status_elapsed(Duration::from_secs(3_720)), "1h02m");
}

/// `cancelled` and `error` are terminal states like any other, so a
/// row that measured an elapsed keeps it.
#[test]
fn a_cancelled_or_errored_terminal_keeps_the_recorded_elapsed() {
    for terminal in ["cancelled", "error"] {
        let mut app = App::new(launch_basic(), true);
        let row = |state: &str| HostEvent::ToolLine {
            call_id: Some("call-1".into()),
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            name: "shell".into(),
            state: state.into(),
            subject: Some("sleep 30".into()),
            detail: None,
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        };
        app.apply_host_event(row("running"));
        app.apply_host_event(row("done"));
        let measured = match app.pane.blocks.last() {
            Some(ScrollBlock::Tool { state, .. }) => state.clone(),
            other => panic!("expected a tool row, got {other:?}"),
        };
        let elapsed = super::blocks::recorded_tool_duration(&measured)
            .expect("the row measured an elapsed")
            .to_owned();
        app.apply_host_event(row(terminal));
        match app.pane.blocks.last() {
            Some(ScrollBlock::Tool { state, .. }) => {
                assert!(state.starts_with(terminal), "{state}");
                assert_eq!(
                    super::blocks::recorded_tool_duration(state),
                    Some(elapsed.as_str()),
                    "{terminal} must keep the elapsed: {state}"
                );
            }
            other => panic!("expected a tool row, got {other:?}"),
        }
    }
}

#[test]
fn a_bounded_host_drain_leaves_the_rest_for_the_next_frame() {
    let mut app = App::new(launch_basic(), true);
    let (sender, receiver) = crate::tui::host_event_channel();
    for index in 0..10 {
        sender
            .send(HostEvent::Row(
                plakous_host_protocol::views::transcript::TranscriptRow::info(
                    plakous_host_protocol::views::transcript::NoticeScope::Host,
                    format!("note {index}"),
                ),
            ))
            .unwrap();
    }
    let mut connected = true;
    let drain = super::terminal::drain_host_events(&mut app, &receiver, &mut connected, 4);
    assert!(drain.batched);
    assert!(
        drain.pending,
        "a saturated budget must report leftover work"
    );
    assert_eq!(app.pane.blocks.len(), 4);

    let rest = super::terminal::drain_host_events(&mut app, &receiver, &mut connected, 32);
    assert!(!rest.pending);
    assert_eq!(app.pane.blocks.len(), 10);
    assert!(connected);
}

#[test]
fn an_idle_app_schedules_no_ticks() {
    let mut app = App::new(launch_basic(), true);
    let schedule = RedrawSchedule::default();
    assert!(
        !app.redraw_demand().is_deep_idle(),
        "a hosted app keeps a cadence until the host settles"
    );
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.apply_host_event(HostEvent::SetBusy(false));
    let demand = app.redraw_demand();
    assert!(demand.is_deep_idle(), "{demand:?} must be deep idle");
    assert_eq!(schedule.wait(demand), RedrawWait::Park);
    assert!(!RedrawSchedule::periodic_redraw_required(demand));
    assert_eq!(
        RedrawSchedule::reason(FrameWork::default()),
        RedrawReason::Nothing,
        "an idle tick must paint nothing"
    );
}

#[test]
fn a_live_turn_moves_the_cadence_off_idle_and_back() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::AgentAttached(true));
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_eq!(
        RedrawSchedule::state(app.redraw_demand()),
        RedrawState::Spinner,
        "a working turn shows a spinner"
    );
    app.apply_host_event(HostEvent::AssistantDelta("hello".into()));
    assert_eq!(
        RedrawSchedule::state(app.redraw_demand()),
        RedrawState::Streaming,
        "stream deltas run at the frame cap"
    );
    app.apply_host_event(HostEvent::AssistantDone);
    app.apply_host_event(HostEvent::SetBusy(false));
    assert_eq!(
        RedrawSchedule::state(app.redraw_demand()),
        RedrawState::DeepIdle
    );
}

/// The four unsupported commands below open nothing. `/providers` is a
/// read-only status tab. The unsupported surfaces have no
/// entry point, so each one is an unknown command and opens nothing.
#[test]
fn the_removed_commands_open_nothing() {
    for line in ["/login", "/connect", "/load-api-key openai"] {
        let mut app = App::new(launch_basic(), true);
        app.set_input(line.to_owned());
        let emitted = app.submit_line(false);
        assert_eq!(emitted, None, "{line}");
        assert_eq!(app.control_tab, None, "{line}");
        assert!(
            blocks_contain(&app, "unknown command"),
            "{line} must say so"
        );
    }
}

/// Spec 02 §5.5: the status colour follows the status word — green ready,
/// yellow login required, red blocked.
#[test]
fn each_status_is_painted_in_its_own_colour_token() {
    let app = providers_app();
    let styled: Vec<(String, ratatui::style::Style)> = app
        .provider_lines(0)
        .into_iter()
        .flat_map(|line| line.spans)
        .map(|span| (span.content.to_string(), span.style))
        .collect();
    let style_of = |needle: &str| {
        styled
            .iter()
            .find(|(content, _)| content == needle)
            .unwrap_or_else(|| panic!("{needle} is painted: {styled:?}"))
            .1
    };
    assert_eq!(style_of("ready"), app.theme.success);
    assert_eq!(style_of("login required"), app.theme.warning);
    assert_eq!(style_of("refresh outcome unknown"), app.theme.error);
}

/// Spec 02 §5.5: the status word deliberately omits the expiry nuance, so
/// the description panel is where a quarantine is explained.
#[test]
fn the_description_panel_explains_a_quarantined_credential() {
    let mut app = providers_app();
    providers_text(&mut app);
    app.handle_key(key(KeyCode::Down));
    let panel = app
        .provider_description()
        .expect("the focused vertical is described");
    assert_eq!(panel.title, "Grok (SuperGrok)");
    assert!(panel.body.contains("unknown"), "{}", panel.body);
    assert!(panel.body.contains("retry automatically"), "{}", panel.body);
    assert!(
        !panel.body.contains("plak login grok"),
        "grok retries without re-login: {}",
        panel.body
    );
    assert!(
        !panel.body.contains("expired"),
        "quarantine is not painted as expiry: {}",
        panel.body
    );

    let painted = providers_text(&mut app);
    assert!(painted.contains("refresh·outcome·unknown"), "{painted}");
    assert!(
        !painted.contains("re-login·required·(expired)"),
        "{painted}"
    );
}

/// A replay announcement names its wait, budget and cause on the HUD
///, marks the streamed text it discarded, and clears once the replay
/// streams.
#[test]
fn a_model_retry_shows_its_wait_and_marks_the_discarded_text() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::ReasoningDelta {
        location_key: "0:0".into(),
        text: "early thought".into(),
    });
    app.apply_host_event(HostEvent::ReasoningSealed {
        location_key: "0:0".into(),
    });
    app.apply_host_event(HostEvent::AssistantDelta("partial answer".into()));
    app.apply_host_event(HostEvent::ModelRetry(
        plakous_host_protocol::ModelRetryNotice {
            next_attempt: 2,
            max_attempts: 4,
            delay_ms: 3_000,
            cause_kind: "stream_interrupted".into(),
            cause: "response stream interrupted (connection reset)".into(),
            output_discarded: true,
        },
    ));
    let label = app.turn_status_label().unwrap_or_default();
    assert!(
        label.starts_with("retrying ") && label.ends_with("s (2/4)"),
        "{label}"
    );
    assert!(
        app.pane.blocks
            .iter()
            .any(|block| matches!(block, ScrollBlock::Discarded { text, thinking: false } if text == "partial answer"))
    );
    assert!(
        app.pane.blocks
            .iter()
            .any(|block| matches!(block, ScrollBlock::Discarded { text, thinking: true } if text == "early thought"))
    );
    assert!(!app.pane.blocks.iter().any(|block| matches!(
        block,
        ScrollBlock::Assistant(_) | ScrollBlock::Thinking { .. }
    )));
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let screen = screen_text(&terminal);
    assert!(
        screen.contains(crate::tui::blocks::DISCARDED_LABEL),
        "{screen}"
    );
    app.apply_host_event(HostEvent::AssistantDelta("replayed answer".into()));
    assert_eq!(app.turn_status_label().as_deref(), Some("responding"));
    assert!(
        app.pane.blocks.iter().any(
            |block| matches!(block, ScrollBlock::Assistant(text) if text == "replayed answer")
        )
    );
}

#[test]
fn escape_from_a_queue_edit_restores_the_replaced_draft_and_never_interrupts() {
    for busy in [false, true] {
        let mut app = App::new(launch_basic(), true);
        app.busy = busy;
        app.apply_host_event(HostEvent::QueueSnapshot {
            items: vec![queue_item("q-one", 2, "queued text")],
            drain: QueueDrainState::LiveAutoDrain,
        });
        typed(&mut app, "my unsent draft");
        app.handle_key(key(KeyCode::Tab));
        assert!(app.queue_focused);
        assert!(app.handle_key(key_ctrl(KeyCode::Char('e'))).is_none());
        assert_eq!(app.input, "queued text");
        let screen = super::test_support::render_to_string(&mut app, 120, 24);
        assert!(
            screen.contains("editing queued follow-up · Enter saves · Esc cancels"),
            "{screen}"
        );

        assert_eq!(app.handle_key(key(KeyCode::Esc)), None, "busy={busy}");
        assert!(app.queue_editing.is_none());
        assert_eq!(app.input, "my unsent draft");
        assert!(
            !app.cancel_pending,
            "cancelling an edit never stops the turn"
        );
        app.handle_key(key_ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "queued text", "the restore is one undoable step");
        app.handle_key(key_ctrl(KeyCode::Char('z')));
        assert_eq!(app.input, "my unsent draft");
    }
}

#[test]
fn an_applied_queue_edit_restores_the_replaced_draft() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-one", 2, "queued")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    typed(&mut app, "keep me");
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key_ctrl(KeyCode::Char('e')));
    typed(&mut app, " edited");
    let action = app.handle_key(key(KeyCode::Enter)).expect("edit action");
    assert_eq!(
        action,
        UserAction::EditQueued {
            id: "q-one".into(),
            revision: 2,
            text: "queued edited".into(),
        }
    );
    assert!(send_user_action(&mut app, Some(&sender), action));
    app.apply_host_event(HostEvent::QueueEditApplied { id: "q-one".into() });
    assert!(app.queue_editing.is_none());
    assert_eq!(app.input, "keep me");
    app.handle_key(key_ctrl(KeyCode::Char('z')));
    assert_eq!(
        app.input, "queued edited",
        "the restore is one undoable step"
    );
}

#[test]
fn a_queue_edit_whose_item_left_the_queue_restores_the_replaced_draft() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q-one", 2, "queued")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    typed(&mut app, "keep me");
    app.handle_key(key(KeyCode::Tab));
    app.handle_key(key_ctrl(KeyCode::Char('e')));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: Vec::new(),
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert!(app.queue_editing.is_none());
    assert!(app.input.contains("keep me"), "{}", app.input);
    assert!(app.input.contains("queued"), "{}", app.input);
}
