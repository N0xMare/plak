//! Provisional `sending…` rows: drawn from the drafts the mailbox took,
//! replaced by host rows or steer chrome, removed with a refusal.

use super::App;
use super::terminal::dispatch_action;
use super::test_support::{launch_basic, pin_clock, queue_item, serialize_frame};
use crate::config::user_action_channel;
use crate::tui::{HostEvent, QueueDrainState, UserAction};
use plakous_host_protocol::views::transcript::{
    Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptRow, TranscriptScope,
};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

fn rows(app: &mut App) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal.draw(|frame| app.draw(frame)).unwrap();
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}

fn showing(app: &mut App, needle: &str) -> Vec<String> {
    rows(app)
        .into_iter()
        .filter(|row| row.contains(needle))
        .collect()
}

#[test]
fn preventive_storage_refusal_renders_the_operator_health_command_live() {
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::StorageAdmissionRefused(
        plakous_host_protocol::status::DiagnosticText::new("opaque storage facts"),
    ));
    let notice = showing(&mut app, "plak doctor");
    assert_eq!(notice.len(), 1, "{notice:?}");
    assert!(notice[0].contains("opaque storage facts"));
}

/// Enter on an idle prompt draws it at once, before the host has answered
/// anything; the host's acknowledgement alone keeps it there, and the
/// host's user row replaces it in place, once.
#[test]
fn an_idle_prompt_shows_sending_until_the_host_row_replaces_it() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.set_input("ship the fix".into());
    let action = app.submit_line(false).expect("prompt");
    assert!(matches!(action, UserAction::Prompt(_)));
    dispatch_action(&mut app, &action, Some(&sender));
    let sending = showing(&mut app, "ship the fix");
    assert_eq!(sending.len(), 1, "{sending:?}");
    assert!(sending[0].contains("sending…"), "{sending:?}");

    // A batch boundary between the acknowledgement and the row.
    app.apply_host_event(HostEvent::DraftAccepted(action));
    let accepted = showing(&mut app, "ship the fix");
    assert_eq!(accepted.len(), 1, "{accepted:?}");
    assert!(accepted[0].contains("sending…"), "{accepted:?}");

    app.apply_host_event(HostEvent::UserMessage {
        text: "ship the fix".into(),
        sent_at_ms: None,
    });
    let landed = showing(&mut app, "ship the fix");
    assert_eq!(landed.len(), 1, "{landed:?}");
    assert!(!landed[0].contains("sending…"), "{landed:?}");
}

/// A refused prompt leaves no provisional row and is back in the composer.
#[test]
fn a_rejected_prompt_drops_its_row_and_restores_the_draft() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.set_input("refuse me".into());
    let action = app.submit_line(false).expect("prompt");
    dispatch_action(&mut app, &action, Some(&sender));
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::DraftRejected(action));
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    assert_eq!(app.input, "refuse me");
}

#[test]
fn a_refused_busy_draft_merges_with_new_typing_without_changing_the_committed_queue() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    let committed = queue_item("committed", 1, "committed follow-up");
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![committed],
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.set_input("refused follow-up".into());
    let action = app.submit_line(false).expect("follow-up");
    dispatch_action(&mut app, &action, Some(&sender));
    app.set_input("new typing".into());
    app.apply_host_event(HostEvent::DraftRejected(action));
    assert_eq!(app.input, "refused follow-up\n\nnew typing");
    assert_eq!(app.local_queue.len(), 1);
    assert_eq!(app.local_queue[0].id, "committed");
    assert_eq!(app.local_queue[0].text, "committed follow-up");
}

#[test]
fn storage_warning_is_idle_chrome_and_clears_with_a_healthy_projection() {
    let mut app = App::new(launch_basic(), true);
    let mut info = plakous_host_protocol::views::presentation::SessionInfoView {
        storage_level: plakous_host_protocol::views::presentation::StorageLevel::Warning,
        ..Default::default()
    };
    app.apply_host_event(HostEvent::SessionInfo(info.clone()));
    assert_eq!(
        app.turn_status_label().as_deref(),
        Some("storage warning · plak doctor")
    );
    app.apply_host_event(HostEvent::SetBusy(true));
    assert_ne!(
        app.turn_status_label().as_deref(),
        Some("storage warning · plak doctor")
    );
    app.apply_host_event(HostEvent::SetBusy(false));
    info.storage_level = plakous_host_protocol::views::presentation::StorageLevel::Refused;
    app.apply_host_event(HostEvent::SessionInfo(info.clone()));
    assert_eq!(
        app.turn_status_label().as_deref(),
        Some("storage low · new turns refused")
    );
    info.storage_level = plakous_host_protocol::views::presentation::StorageLevel::Available;
    app.apply_host_event(HostEvent::SessionInfo(info.clone()));
    assert_eq!(app.turn_status_label(), None);
    // From 70% the session store's fill and the reclaim command show while idle.
    info.session_store_percent = Some(87);
    info.storage_level = plakous_host_protocol::views::presentation::StorageLevel::Warning;
    app.apply_host_event(HostEvent::SessionInfo(info.clone()));
    assert_eq!(
        app.turn_status_label().as_deref(),
        Some("session store 87% full · plak session gc --all")
    );
    info.session_store_percent = None;
    info.storage_level = plakous_host_protocol::views::presentation::StorageLevel::Available;
    app.apply_host_event(HostEvent::SessionInfo(info));
    assert_eq!(app.turn_status_label(), None);
}

/// A follow-up typed during a turn is a numberless queue row until the
/// host's queue snapshot numbers it; the queue panel's `#N` rows are the
/// host's alone.
#[test]
fn a_busy_follow_up_is_a_numberless_queue_row_until_the_snapshot() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("after this turn".into());
    let action = app.submit_line(false).expect("follow-up");
    assert!(matches!(action, UserAction::FollowUp(_)));
    dispatch_action(&mut app, &action, Some(&sender));
    let pending = showing(&mut app, "after this turn");
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert!(
        pending[0].contains("· after this turn · sending…") && !pending[0].contains("#1"),
        "{pending:?}"
    );

    let item = queue_item("q1", 1, "after this turn");
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![item.clone()],
        drain: QueueDrainState::LiveAutoDrain,
    });
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::PromptAdmitted(item));
    let queued = showing(&mut app, "after this turn");
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert!(
        queued[0].contains("#1 after this turn") && !queued[0].contains("sending…"),
        "{queued:?}"
    );
}

/// The mailbox keeps only the newest queue snapshot, at the back, so the
/// snapshot that numbers a follow-up can land after its admission. The row
/// stays provisional until then, and is never drawn beside its own `#1`.
#[test]
fn a_follow_up_admitted_before_its_snapshot_retires_on_the_snapshot() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("late snapshot".into());
    let action = app.submit_line(false).expect("follow-up");
    dispatch_action(&mut app, &action, Some(&sender));
    let item = queue_item("q1", 1, "late snapshot");
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::PromptAdmitted(item.clone()));
    let admitted = showing(&mut app, "late snapshot");
    assert!(
        admitted.len() == 1 && admitted[0].contains("sending…"),
        "{admitted:?}"
    );
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![item],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let numbered = showing(&mut app, "late snapshot");
    assert!(
        numbered.len() == 1
            && numbered[0].contains("#1 late snapshot")
            && !numbered[0].contains("sending…"),
        "{numbered:?}"
    );
}

/// Two follow-ups whose first snapshot the mailbox superseded: each
/// admission names its own item, and the one snapshot left numbers both.
#[test]
fn two_follow_ups_retire_by_identity_on_a_coalesced_snapshot() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    let mut items = Vec::new();
    for (id, text) in [("q1", "first follow-up"), ("q2", "second follow-up")] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("follow-up");
        dispatch_action(&mut app, &action, Some(&sender));
        let item = queue_item(id, 1, text);
        app.apply_host_event(HostEvent::DraftAccepted(action));
        app.apply_host_event(HostEvent::PromptAdmitted(item.clone()));
        items.push(item);
    }
    assert_eq!(showing(&mut app, "sending…").len(), 2);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items,
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    assert_eq!(showing(&mut app, "#1 first follow-up").len(), 1);
    assert_eq!(showing(&mut app, "#2 second follow-up").len(), 1);
}

/// The item can leave the queue (dropped, deleted, sent now) before the TUI
/// drains, so the one snapshot it sees after the admission no longer holds
/// it. That snapshot is still at least as new as the admission: the row
/// retires on it rather than lingering for the rest of the turn.
#[test]
fn a_follow_up_dropped_before_its_snapshot_retires_on_the_next_snapshot() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("gone again".into());
    let action = app.submit_line(false).expect("follow-up");
    dispatch_action(&mut app, &action, Some(&sender));
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::PromptAdmitted(queue_item("q1", 1, "gone again")));
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: Vec::new(),
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(showing(&mut app, "gone again"), Vec::<String>::new());
}

/// A saturated mailbox can lose the `PromptAdmitted` that would name the
/// item and report a correlation reset instead. With no identity to wait
/// on, the accepted row retires there, so the snapshot's `#1` is alone.
#[test]
fn a_correlation_reset_retires_an_unnamed_follow_up() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("receipt lost".into());
    let action = app.submit_line(false).expect("follow-up");
    dispatch_action(&mut app, &action, Some(&sender));
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::PromptCorrelationReset);
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "receipt lost")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    let rows = showing(&mut app, "receipt lost");
    assert!(
        rows.len() == 1 && rows[0].contains("#1 receipt lost") && !rows[0].contains("sending…"),
        "{rows:?}"
    );
}

/// An idle prompt's row retires on the user row of the item `PromptStarted`
/// named, and a snapshot between the two does not take it early.
#[test]
fn an_idle_prompt_retires_on_its_own_user_row() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.set_input("started now".into());
    let action = app.submit_line(false).expect("prompt");
    dispatch_action(&mut app, &action, Some(&sender));
    let item = queue_item("p1", 1, "started now");
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::PromptAdmitted(item.clone()));
    app.apply_host_event(HostEvent::PromptStarted(Some(item)));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: Vec::new(),
        drain: QueueDrainState::LiveAutoDrain,
    });
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::UserMessage {
        text: "started now".into(),
        sent_at_ms: None,
    });
    let landed = showing(&mut app, "started now");
    assert!(
        landed.len() == 1 && !landed[0].contains("sending…"),
        "{landed:?}"
    );
}

/// A refused follow-up leaves the queue panel as the host has it.
#[test]
fn a_rejected_follow_up_leaves_no_queue_row() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("not queued".into());
    let action = app.submit_line(false).expect("follow-up");
    dispatch_action(&mut app, &action, Some(&sender));
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::DraftRejected(action));
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    assert!(app.queue_area.is_none(), "an empty queue draws no panel");
}

/// A queued acknowledgement replaces the sending shelf with host chrome;
/// only the applied canonical patch contributes a transcript row.
#[test]
fn a_steer_is_provisional_until_its_steer_row_lands() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(super::test_support::restore_snapshot(1, Vec::new()));
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("look at the logs".into());
    let action = app.submit_line(true).expect("steer");
    assert!(matches!(action, UserAction::Steer(_)));
    dispatch_action(&mut app, &action, Some(&sender));
    let pending = showing(&mut app, "look at the logs");
    assert!(
        pending.len() == 1 && pending[0].contains("steer · look at the logs · sending…"),
        "{pending:?}"
    );
    app.apply_host_event(HostEvent::DraftAccepted(action));
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    let sent = showing(&mut app, "look at the logs");
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].contains(" sent · look at the logs"),
        "the sent row leads with the steer text: {sent:?}"
    );
    assert!(
        showing(&mut app, "sent · waiting for the current reply").is_empty(),
        "the accepted draft's text replaces the waiting sentence"
    );
    app.apply_host_event(HostEvent::SteerAck {
        state: "applied".into(),
        position: None,
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    assert_eq!(
        showing(&mut app, "sent · waiting for the current reply"),
        Vec::<String>::new()
    );
    app.apply_host_event(HostEvent::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: 1,
        to_revision: 2,
        upserts: vec![TranscriptItem {
            key: RowKey::new(2, Lane::Runtime, 0),
            row: TranscriptRow::steer("look at the logs"),
        }],
        deletes: Vec::new(),
    }));
    let landed = showing(&mut app, "look at the logs");
    assert!(
        landed.len() == 1 && !landed[0].contains("sending…"),
        "{landed:?}"
    );
}

#[test]
fn a_sent_steer_preview_keeps_only_a_few_hundred_characters() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("s".repeat(500));
    let action = app.submit_line(true).expect("steer");
    dispatch_action(&mut app, &action, Some(&sender));
    app.apply_host_event(HostEvent::DraftAccepted(action));
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    let stored = app
        .composer_chrome
        .sent_steers
        .back()
        .cloned()
        .flatten()
        .expect("captured steer text");
    assert_eq!(stored.chars().count(), 300);
    assert!(stored.chars().all(|ch| ch == 's'));
    assert!(!stored.contains(&"s".repeat(301)));
}

#[test]
fn an_applied_steer_ack_keeps_the_next_steer_provisional_until_its_queued_ack() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("steer A".into());
    let first = app.submit_line(true).expect("first steer");
    dispatch_action(&mut app, &first, Some(&sender));
    app.apply_host_event(HostEvent::DraftAccepted(first));
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    app.set_input("steer B".into());
    let second = app.submit_line(true).expect("second steer");
    dispatch_action(&mut app, &second, Some(&sender));
    app.apply_host_event(HostEvent::DraftAccepted(second));
    app.apply_host_event(HostEvent::SteerAck {
        state: "applied".into(),
        position: None,
    });
    let provisional = showing(&mut app, "steer B");
    assert!(
        provisional.len() == 1 && provisional[0].contains("sending…"),
        "the first steer's application cannot acknowledge the second: {provisional:?}"
    );
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
    app.apply_host_event(HostEvent::SteerAck {
        state: "applied".into(),
        position: None,
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
}

#[test]
fn an_applied_steer_ack_retires_a_draft_accepted_after_its_queued_ack() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.set_input("acknowledgements raced".into());
    let action = app.submit_line(true).expect("steer");
    dispatch_action(&mut app, &action, Some(&sender));
    app.apply_host_event(HostEvent::SteerAck {
        state: "queued".into(),
        position: Some(1),
    });
    app.apply_host_event(HostEvent::DraftAccepted(action));
    assert_eq!(showing(&mut app, "sending…").len(), 1);
    app.apply_host_event(HostEvent::SteerAck {
        state: "applied".into(),
        position: None,
    });
    assert_eq!(showing(&mut app, "sending…"), Vec::<String>::new());
}

/// The reviewed look of every provisional kind: an idle prompt at the
/// transcript tail, and a follow-up and a steer under a host queue row.
#[test]
fn provisional_rows_match_reviewable_goldens() {
    let (sender, _receiver) = user_action_channel();
    let mut idle = App::new(launch_basic(), true);
    pin_clock(&mut idle);
    idle.set_input("an idle prompt on its way".into());
    let action = idle.submit_line(false).expect("prompt");
    dispatch_action(&mut idle, &action, Some(&sender));

    let mut busy = App::new(launch_basic(), true);
    pin_clock(&mut busy);
    busy.apply_host_event(HostEvent::SetBusy(true));
    busy.apply_host_event(HostEvent::QueueSnapshot {
        items: vec![queue_item("q1", 1, "a queued follow-up")],
        drain: QueueDrainState::LiveAutoDrain,
    });
    for (text, steer) in [
        ("a follow-up on its way", false),
        ("a steer on its way", true),
    ] {
        busy.set_input(text.into());
        let action = busy.submit_line(steer).expect("draft");
        dispatch_action(&mut busy, &action, Some(&sender));
    }

    for (app, path, expected) in [
        (
            &mut idle,
            "tests/goldens/provisional-idle-dark-80x24.frame",
            include_str!("../../../tests/goldens/provisional-idle-dark-80x24.frame"),
        ),
        (
            &mut busy,
            "tests/goldens/provisional-busy-dark-80x24.frame",
            include_str!("../../../tests/goldens/provisional-busy-dark-80x24.frame"),
        ),
    ] {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let actual = serialize_frame(terminal.backend().buffer());
        if std::env::var_os("PLAK_UPDATE_GOLDENS").is_some() {
            std::fs::write(path, &actual).expect("write reviewed frame golden");
        } else {
            assert_eq!(actual, expected, "{path}");
        }
    }
}

/// Four idle prompts share a three-row shelf: the last row names the two
/// that do not fit, instead of clipping them.
#[test]
fn provisional_prompts_past_the_shelf_end_in_an_overflow_line() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    for text in ["one", "two", "three", "four"] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("prompt");
        dispatch_action(&mut app, &action, Some(&sender));
    }
    let shelf = showing(&mut app, "sending");
    assert!(
        shelf.iter().any(|row| row.contains("+2 sending")),
        "{shelf:?}"
    );
    assert!(shelf.iter().any(|row| row.contains("one")));
    assert!(shelf.iter().any(|row| row.contains("two")));
    assert!(!shelf.iter().any(|row| row.contains("three")));
    assert!(!shelf.iter().any(|row| row.contains("four")));
}

/// Follow-ups beyond the queue panel's four content rows end on `+N sending`.
#[test]
fn queued_follow_ups_past_the_panel_end_in_an_overflow_line() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    for text in ["one", "two", "three", "four", "five"] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("follow-up");
        dispatch_action(&mut app, &action, Some(&sender));
    }
    let panel = showing(&mut app, "sending");
    assert!(
        panel.iter().any(|row| row.contains("+2 sending")),
        "{panel:?}"
    );
    assert!(panel.iter().any(|row| row.contains("one")));
    assert!(panel.iter().any(|row| row.contains("three")));
    assert!(!panel.iter().any(|row| row.contains("five")));
}

/// Three idle prompts fill the shelf. Nothing is hidden, so there is no
/// overflow line.
#[test]
fn three_idle_prompts_fill_the_shelf_without_an_overflow_line() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    for text in ["one", "two", "three"] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("prompt");
        dispatch_action(&mut app, &action, Some(&sender));
    }
    let shelf = showing(&mut app, "sending");
    assert!(!shelf.iter().any(|row| row.contains('+')), "{shelf:?}");
    assert!(shelf.iter().any(|row| row.contains("one")));
    assert!(shelf.iter().any(|row| row.contains("two")));
    assert!(shelf.iter().any(|row| row.contains("three")));
}

#[test]
fn sending_overflow_names_only_the_rows_past_the_last_slot() {
    let cases = [
        (0, 0, 0, None),
        (1, 0, 0, Some(1)),
        (0, 1, 0, None),
        (1, 1, 1, None),
        (2, 1, 0, Some(2)),
        (2, 3, 2, None),
        (3, 3, 3, None),
        (4, 3, 2, Some(2)),
        (5, 4, 3, Some(2)),
    ];
    for (total, slots, shown, hidden) in cases {
        assert_eq!(
            super::provisional::sending_overflow(total, slots),
            (shown, hidden),
            "total {total} slots {slots}"
        );
    }
}

/// `+N sending` counts unacknowledged rows only. Sent steers that do not fit
/// are omitted, not folded into N.
#[test]
fn sent_steers_that_do_not_fit_are_omitted_from_the_overflow_count() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    for text in ["one", "two"] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("follow-up");
        dispatch_action(&mut app, &action, Some(&sender));
    }
    app.composer_chrome.sent_steers.extend([None, None, None]);
    let mut terminal = Terminal::new(TestBackend::new(80, 6)).unwrap();
    terminal
        .draw(|frame| app.draw_queue_panel(frame, frame.area()))
        .unwrap();
    let rows = buffer_rows(&terminal);
    assert_eq!(
        rows.iter().filter(|row| row.contains("sending…")).count(),
        2,
        "{rows:?}"
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row.contains("sent · waiting"))
            .count(),
        2,
        "{rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains('+')), "{rows:?}");
}

/// A one-row panel that cannot show the host row and the overflow line keeps
/// the overflow line and drops the host row's hit target with it.
#[test]
fn a_squeezed_queue_panel_has_no_hit_target_on_a_hidden_row() {
    let (sender, _receiver) = user_action_channel();
    let mut app = App::new(launch_basic(), true);
    app.apply_host_event(HostEvent::SetBusy(true));
    app.apply_host_event(HostEvent::QueueSnapshot {
        items: (0..3)
            .map(|index| queue_item(&format!("q{index}"), index + 1, &format!("host {index}")))
            .collect(),
        drain: QueueDrainState::LiveAutoDrain,
    });
    for text in ["one", "two"] {
        app.set_input(text.into());
        let action = app.submit_line(false).expect("follow-up");
        dispatch_action(&mut app, &action, Some(&sender));
    }

    let mut squeezed = Terminal::new(TestBackend::new(80, 3)).unwrap();
    squeezed
        .draw(|frame| app.draw_queue_panel(frame, frame.area()))
        .unwrap();
    let rows = buffer_rows(&squeezed);
    assert!(
        rows.iter().any(|row| row.contains("+2 sending")),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|row| row.contains("host ")), "{rows:?}");
    assert!(
        app.queue_button_hits.is_empty(),
        "hidden host rows kept hits: {:?}",
        app.queue_button_hits
    );

    let mut taller = Terminal::new(TestBackend::new(80, 4)).unwrap();
    taller
        .draw(|frame| app.draw_queue_panel(frame, frame.area()))
        .unwrap();
    assert_eq!(app.queue_button_hits.len(), 1);
    assert_eq!(app.queue_button_hits[0].abs_idx, 0);
    let inner_y = taller.backend().buffer().area.y.saturating_add(1);
    assert_eq!(app.queue_button_hits[0].now.y, inner_y);
    assert_eq!(app.queue_button_hits[0].drop.y, inner_y);
    let painted = buffer_rows(&taller);
    assert!(painted.iter().any(|row| row.contains("#1")));
    assert!(painted.iter().any(|row| row.contains("+2 sending")));
    assert!(!painted.iter().any(|row| row.contains("host 1")));
}

fn buffer_rows(terminal: &Terminal<TestBackend>) -> Vec<String> {
    let buffer = terminal.backend().buffer();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect()
}
