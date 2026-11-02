use super::*;
use crate::surface_protocol::UserAction;
use plakous_host_protocol::status::HostPhase;

fn next_non_idle_step(host: &mut HostLoop<'_, TestExtension>) -> HostStep {
    loop {
        let step = host.next_prompt(None).unwrap();
        if !matches!(step, HostStep::Continue) {
            return step;
        }
    }
}

#[test]
fn interrupt_during_native_refresh_preserves_queue_without_admission_or_dispatch() {
    stopped_preparation(preparation::Stage::Refresh, false, false);
}

#[test]
fn quit_during_wait_settlement_preserves_queue_and_exits_without_a_turn() {
    stopped_preparation(preparation::Stage::Settle, true, false);
}

#[test]
fn interrupt_during_tree_projection_preserves_queue_without_a_turn() {
    stopped_preparation(preparation::Stage::Tree, false, false);
}

#[test]
fn one_shot_preparation_stop_reports_interruption_without_a_turn_terminal() {
    stopped_preparation(preparation::Stage::Refresh, false, true);
}

pub(super) fn stopped_preparation(
    stage: preparation::Stage,
    quit: bool,
    one_shot: bool,
) -> Vec<HostEvent<TestExtension>> {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.attached_one_shot = one_shot;
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    actions
        .send(UserAction::Prompt("keep exact input".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt");
    };
    let before = host.config.session.pending_queue.clone();
    drop(events.try_iter().collect::<Vec<_>>());
    let locator = host.opened.locator();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let wake = started.clone();
    let ready = release.clone();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    // The stop is posted on the host thread inside the step. A second runtime
    // only releases the step, so the checkpoint does not race the send.
    let control = std::thread::spawn(move || {
        executor.block_on(wake.notified()).unwrap();
        ready.notify_one();
    });
    let stop = if quit {
        UserAction::Quit
    } else {
        UserAction::Interrupt
    };
    // The hook drops its sender when the step returns. Disconnected is quit,
    // so another sender has to outlive the retry or quit branch.
    let actions_held = actions.clone();
    preparation::hooks::HOOK.with(|slot| {
        *slot.borrow_mut() = Some((
            stage,
            Box::pin(async move {
                actions.send(stop).unwrap();
                if stage == preparation::Stage::Refresh {
                    use crate::runtime::startup::{
                        native_refresh_tests::AwaitingNativeModel, refresh_native_authority,
                    };
                    let changed = refresh_native_authority(
                        Arc::new(AwaitingNativeModel),
                        "Claude",
                        "claude",
                        async {
                            started.notify_one();
                            release.notified().await;
                            Ok(false)
                        },
                        async { panic!("credential wait must resolve") },
                    )
                    .await
                    .unwrap();
                    assert!(!changed);
                } else {
                    started.notify_one();
                    release.notified().await;
                }
            }),
        ));
    });
    let mut invocation = crate::runtime::oneshot::OneShotState {
        attempted: true,
        resume_tree: false,
        result: None,
        run: None,
    };
    preparation::hooks::CALLED.with(|calls| calls.borrow_mut().clear());
    host.run_prompt(
        text.clone(),
        queued_delivery.clone(),
        user_echoed,
        one_shot.then_some(&mut invocation),
    )
    .unwrap();
    control.join().unwrap();
    let skipped = match stage {
        preparation::Stage::Refresh => Some(preparation::Stage::Settle),
        preparation::Stage::Settle => Some(preparation::Stage::Tree),
        preparation::Stage::Tree => Some(preparation::Stage::Query),
        _ => None,
    };
    if let Some(skipped) = skipped {
        preparation::hooks::CALLED.with(|calls| {
            assert!(
                !calls.borrow().contains(&skipped),
                "stop at {stage:?} must skip {skipped:?}"
            );
            assert!(
                !calls.borrow().contains(&preparation::Stage::Compact),
                "stop at {stage:?} must skip scheduled compaction"
            );
        });
    }
    assert_eq!(host.config.session.pending_queue, before);
    assert_eq!(
        host.config.session.queue_drain_state,
        SessionQueueDrainState::RestoredPaused
    );
    assert_eq!(
        host.config
            .store
            .load(&host.config.session.id)
            .unwrap()
            .pending_queue,
        before
    );
    assert!(!host.config.session.queue_commands.has_pending());
    assert_eq!(host.config.session.root_turn_count, 0);
    assert_eq!(host.opened.locator(), locator);
    assert!(host.opened.agent().is_some());
    assert!(!host.recovery_blocked && !host.quit_abandoned_turn);
    let observed: Vec<_> = events.try_iter().collect();
    assert!(!observed.iter().any(|event| matches!(
        event,
        HostEvent::TurnLifecycle(_) | HostEvent::PromptAdmitted(_)
    )));
    assert!(
        observed
            .iter()
            .rev()
            .find_map(|event| match event {
                HostEvent::Status { phase, .. } => Some(phase),
                _ => None,
            })
            .is_some_and(|phase| matches!(phase, HostPhase::Idle))
    );
    assert_eq!(
        observed.iter().rev().find_map(|event| match event {
            HostEvent::SetBusy(busy) => Some(*busy),
            _ => None,
        }),
        Some(false)
    );
    if one_shot {
        assert_eq!(
            invocation.result.unwrap().unwrap(),
            crate::runtime::oneshot::OneShotOutcome::Interrupted {
                assistant: String::new()
            }
        );
    } else if quit {
        assert!(matches!(host.next_prompt(None).unwrap(), HostStep::Quit));
    } else {
        // The next explicit retry uses the same delivery and a clean watch.
        host.run_prompt(text, queued_delivery, true, None).unwrap();
        assert_eq!(host.config.session.root_turn_count, 1);
        assert_eq!(
            host.config.session.pending_queue,
            [] as [SessionQueueItem; 0]
        );
        assert!(!host.recovery_blocked);
    }
    drop(actions_held);
    host.release().unwrap();
    observed
}

#[test]
fn fork_clock_excludes_source_prompts_and_keeps_its_own_prompt_across_attach() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    actions
        .send(UserAction::Prompt("source input".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt")
    };
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    assert!(host.opened.last_operator_input_unix_ms().is_some());
    let source = host.config.session.id.clone();
    drop(events.try_iter().collect::<Vec<_>>());
    actions
        .send(UserAction::ForkSession {
            account: None,
            compact: false,
        })
        .unwrap();
    actions.send(UserAction::Quit).unwrap();
    assert!(matches!(next_non_idle_step(&mut host), HostStep::Quit));
    assert_ne!(host.config.session.id, source);
    assert_eq!(host.config.session.prompt_history, [] as [String; 0]);
    assert!(
        events
            .try_iter()
            .any(|event| matches!(event, HostEvent::LastUserPromptAt(None)))
    );
    // A fork still carries the source's canonical input; the clock must not.
    assert!(host.opened.last_operator_input_unix_ms().is_some());
    let fork = host.config.session.id.clone();
    let store = host.config.store.clone();
    host.release().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session = store.load(&fork).unwrap();
    config.store = store.clone();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    assert!(
        events
            .try_iter()
            .any(|event| matches!(event, HostEvent::LastUserPromptAt(None)))
    );
    actions
        .send(UserAction::Prompt("fork input".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host.next_prompt(None).unwrap()
    else {
        panic!("prompt")
    };
    host.run_prompt(text, queued_delivery, user_echoed, None)
        .unwrap();
    let committed_at = host.opened.last_operator_input_unix_ms().unwrap();
    drop(events.try_iter().collect::<Vec<_>>());
    crate::runtime::info::send_last_user_prompt_at(&host.events, &host.config, &host.opened);
    assert!(
        events.try_iter().any(
            |event| matches!(event, HostEvent::LastUserPromptAt(Some(at)) if at == committed_at)
        )
    );
    let stored = host.config.store.load(&host.config.session.id).unwrap();
    assert_eq!(stored.prompt_history, ["fork input"]);
    host.release().unwrap();
}

#[test]
fn an_idle_queue_edit_has_exactly_one_receipt() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    queue::enqueue(&mut config.session.pending_queue, "before").unwrap();
    config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
    config.store.save(&mut config.session).unwrap();
    let item = config.session.pending_queue[0].clone();
    let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (wire, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host =
        HostLoop::open(config, &receiver, crate::runtime::Sender::over(wire), None).unwrap();
    actions
        .send(UserAction::EditQueued {
            id: item.id.clone(),
            revision: item.revision,
            text: "after".into(),
        })
        .unwrap();
    actions.send(UserAction::Quit).unwrap();
    assert!(matches!(next_non_idle_step(&mut host), HostStep::Quit));
    assert_eq!(
        events
            .try_iter()
            .filter(|event| matches!(event, HostEvent::QueueEditApplied { id } if id == &item.id))
            .count(),
        1
    );
    host.release().unwrap();
}
