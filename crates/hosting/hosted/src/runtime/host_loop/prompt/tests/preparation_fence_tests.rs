use super::*;
use crate::runtime::test_kit::BlockOn as _;
use crate::surface_protocol::UserAction;
use plakous_host_protocol::status::HostPhase;

#[test]
fn post_context_stop_does_not_admit_a_turn() {
    preparation_tests::stopped_preparation(preparation::Stage::Context, false, false);
}

#[test]
fn pre_dispatch_stop_does_not_admit_a_turn() {
    preparation_tests::stopped_preparation(preparation::Stage::Dispatch, false, false);
}

#[test]
fn every_stop_stage_has_identical_tui_and_jsonl_terminal_records() {
    let mut expected = None;
    for stage in [
        preparation::Stage::Refresh,
        preparation::Stage::Settle,
        preparation::Stage::Tree,
        preparation::Stage::Context,
        preparation::Stage::Dispatch,
    ] {
        let observed = preparation_tests::stopped_preparation(stage, false, false);
        let terminal: Vec<_> = observed
            .into_iter()
            .filter(|event| {
                matches!(event,
            HostEvent::Status { phase, .. } if !matches!(phase, HostPhase::Preparing))
                    || matches!(event, HostEvent::SetBusy(false))
            })
            .collect();
        assert_eq!(
            terminal.len(),
            2,
            "one resting status and one busy-false: {terminal:?}"
        );
        assert!(
            matches!(&terminal[0], HostEvent::Status {phase, detail: Some(detail), ..} if matches!(phase, HostPhase::Idle) && matches!(detail, HostStatusDetail::PromptPreparationStopped))
        );
        let jsonl: Vec<_> = terminal
            .iter()
            .filter_map(|event| {
                plakous_host_protocol::jsonl::project_host_event(
                    event,
                    plakous_host_protocol::envelope::JsonlCorrelation {
                        prompt_id: Some("p"),
                        omit_unknown_turn_id: false,
                        command_id: None,
                        pending_tree_snapshot_id: None,
                    },
                )
            })
            .collect();
        if let Some((tui, wire)) = &expected {
            assert_eq!(&terminal, tui);
            assert_eq!(&jsonl, wire);
        } else {
            expected = Some((terminal, jsonl));
        }
    }
}

fn wake_delivery(host: &mut HostLoop<'_, TestExtension>) -> SessionQueueItem {
    let item = host.config.session.enqueue_live("host wake").unwrap();
    host.config.session = crate::runtime::result_mail::prepare_instruction(
        &host.config.topology_rows(),
        host.config.store.as_ref(),
        &host.config.session,
        &item,
        "host wake",
        None,
        &["p1".into()],
    )
    .block()
    .unwrap();
    item
}

#[test]
fn stopped_host_wake_keeps_operator_items_behind_it_durably_paused() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, _events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    let item = wake_delivery(&mut host);
    let operator = host
        .config
        .session
        .enqueue_live("operator behind wake")
        .unwrap();
    host.config.store.save(&mut host.config.session).unwrap();
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::LiveAutoDrain
    );
    preparation::hooks::HOOK.with(|slot| {
        *slot.borrow_mut() = Some((
            preparation::Stage::Settle,
            Box::pin(async move {
                actions.send(UserAction::Interrupt).unwrap();
            }),
        ));
    });
    host.run_prompt(item.text.clone(), Some(item), true, None)
        .unwrap();
    assert_eq!(
        host.config.session.pending_queue,
        std::slice::from_ref(&operator)
    );
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    let stored = host.config.store.load(&host.config.session.id).unwrap();
    assert!(host.config.session.child_wake_suppressed);
    assert!(stored.child_wake_suppressed);
    assert_eq!(stored.pending_queue, [operator]);
    assert_eq!(
        stored.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    let intents = host
        .config
        .store
        .load_intents(&host.config.session.id)
        .unwrap();
    assert!(
        !intents.wake_suppressions.is_empty(),
        "the host-controls row suppresses wakes"
    );
    assert!(
        matches!(&intents.pause_causes.last().unwrap().value, sessions::projector::HostPauseCause::UnrecordedRefusal { reason } if reason.contains("preparation stopped"))
    );
    assert_eq!(host.config.session.root_turn_count, 0);
    host.release().unwrap();
}

#[test]
fn mail_selection_and_missing_wake_context_failures_leave_a_terminal_status() {
    for wake in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
        let item = if wake {
            wake_delivery(&mut host)
        } else {
            preparation::hooks::MAIL_ERROR
                .with(|slot| *slot.borrow_mut() = Some("injected mail read failure".into()));
            host.config.session.enqueue_live("operator").unwrap()
        };
        events.try_iter().for_each(drop);
        host.run_prompt(item.text.clone(), Some(item), true, None)
            .unwrap();
        let observed: Vec<_> = events.try_iter().collect();
        assert!(
            observed
                .iter()
                .any(|event| format!("{event:?}").contains(if wake {
                    "child wake preflight is unavailable"
                } else {
                    "injected mail read failure"
                })),
            "{observed:?}"
        );
        assert!(
            matches!(observed.iter().rev().find(|event| matches!(event, HostEvent::Status {..})), Some(HostEvent::Status {phase, ..}) if matches!(phase, HostPhase::Idle)),
            "{observed:?}"
        );
        assert_eq!(
            observed
                .iter()
                .filter(|event| matches!(event, HostEvent::SetBusy(false)))
                .count(),
            1
        );
        assert_eq!(host.config.session.root_turn_count, 0);
        host.release().unwrap();
    }
}

#[test]
fn an_admitted_prompt_emits_exactly_one_busy_false() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Arc::new(events);
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mailbox = Arc::downgrade(&events);
    let recorded = observed.clone();
    // Drain synchronously so replaceable observations cannot hide a duplicate.
    events.set_notify(move || {
        if let Some(mailbox) = mailbox.upgrade() {
            recorded.lock().unwrap().extend(mailbox.try_iter());
        }
    });
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    actions
        .send(UserAction::Prompt("one prompt".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt")
    };
    observed.lock().unwrap().clear();
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    let observed = std::mem::take(&mut *observed.lock().unwrap());
    assert_eq!(
        observed
            .iter()
            .filter(|event| matches!(event, HostEvent::SetBusy(false)))
            .count(),
        1
    );
    host.release().unwrap();
}
