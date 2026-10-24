#[test]
fn child_live_deltas_coalesce_and_flood_leaves_room_for_root() {
    let (sender, receiver) = observation_channel::<Core>();
    for _ in 0..(128 * crate::limits::COALESCED_DELTA_BYTES / 4096 + 256) {
        let _ = sender.send(SessionObservation::ChildLive {
            agent_id: "c1".into(),
            turn_id: "t1".into(),
            op: Box::new(SessionObservation::AssistantDelta("x".repeat(4096))),
        });
    }
    sender
        .send(SessionObservation::Row(TranscriptRow::assistant("root")))
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.len() <= 130);
    assert_eq!(events.iter().filter(|event| matches!(event, SessionObservation::ChildLive {op, ..} if op.as_ref() == &super::child_gap_notice())).count(), 1);
    let SessionObservation::Row(TranscriptRow::Notice(gap)) = super::child_gap_notice::<Core>()
    else {
        panic!("the gap marker is a notice row");
    };
    assert_eq!(
        gap.placement(),
        crate::views::transcript::NoticePlacement::NoticeRow,
        "the operator must see that child rows were dropped"
    );
    let (sender, receiver) = observation_channel::<Core>();
    for text in ["a", "b"] {
        sender
            .send(SessionObservation::ChildLive {
                agent_id: "c1".into(),
                turn_id: "t1".into(),
                op: Box::new(SessionObservation::AssistantDelta(text.into())),
            })
            .unwrap();
    }
    assert!(
        matches!(receiver.try_recv().unwrap(), SessionObservation::ChildLive {op, ..} if matches!(*op, SessionObservation::AssistantDelta(ref text) if text == "ab"))
    );
    assert!(receiver.try_recv().is_err());
}

#[test]
fn job_lifecycle_is_not_evicted_at_saturation() {
    let (sender, receiver) = observation_channel::<Core>();
    sender
        .send(SessionObservation::JobTerminal {
            job_id: "p1".into(),
            owner: "root".into(),
            status: "exited".into(),
            exit_code: Some(0),
            stop_reason: None,
            elapsed_ms: 1,
            output: String::new(),
        })
        .unwrap();
    for _ in 1..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::assistant("fill")))
            .unwrap();
    }
    sender.send(SessionObservation::AssistantDone).unwrap();
    assert!(matches!(
        receiver.try_recv().unwrap(),
        SessionObservation::JobTerminal { .. }
    ));
}

use super::{IntentSendError, intent_channel, observation_channel};
use crate::intent::{Core, SessionIntent};
use crate::limits::{INTENT_QUEUE_CAPACITY, OBSERVATION_QUEUE_CAPACITY};
use crate::observation::{
    ContextCapacityRecovery, QueueDrainState, QueueItem, SessionObservation, TurnLifecycle,
    TurnPhase,
};
use crate::views::transcript::{NoticeScope, TranscriptRow};
use std::sync::mpsc;

#[test]
fn running_process_count_is_shared_without_enqueuing_an_intent() {
    let (sender, receiver) = intent_channel::<Core>();
    assert_eq!(sender.background_processes(), 0);
    receiver.set_background_processes(2);
    assert_eq!(sender.clone().background_processes(), 2);
    assert!(receiver.try_recv().is_err());
    receiver.set_background_processes(0);
    assert_eq!(sender.background_processes(), 0);
}

#[test]
fn context_capacity_snapshots_supersede_their_own_older_value() {
    let (sender, receiver) = observation_channel::<Core>();
    sender
        .send(SessionObservation::ContextCapacityRecovery(Some(
            ContextCapacityRecovery::Compactable,
        )))
        .unwrap();
    sender
        .send(SessionObservation::ContextCapacityRecovery(None))
        .unwrap();
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        vec![SessionObservation::ContextCapacityRecovery(None)]
    );
}

#[test]
fn promptable_is_a_local_signal_and_abandonment_outranks_queued_work() {
    let (sender, receiver) = intent_channel::<Core>();
    assert!(!receiver.is_promptable());
    sender.mark_promptable();
    assert!(receiver.is_promptable());
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    sender.send(SessionIntent::Interrupt).unwrap();
    sender.request_abandon_and_release();
    assert!(receiver.abandon_and_release_requested());
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Quit);
}

#[test]
fn correlation_receipt_loss_resets_before_remaining_observations() {
    for receipt in [
        SessionObservation::<Core>::PromptAdmitted(QueueItem {
            rejected: false,
            id: "q".into(),
            revision: 1,
            text: "same".into(),
            images: Vec::new(),
        }),
        SessionObservation::PromptStarted(None),
        SessionObservation::ToolInvocationOrigin {
            call_id: "call".into(),
            command_id: "iv".into(),
        },
        SessionObservation::ClearView,
        SessionObservation::DraftRejected(SessionIntent::Prompt("same".into())),
    ] {
        // Refusal evicts a non-state receipt at saturation.
        let (tx, rx) = observation_channel();
        tx.send(receipt.clone()).unwrap();
        for index in 1..OBSERVATION_QUEUE_CAPACITY {
            tx.send(SessionObservation::Row(TranscriptRow::assistant(format!(
                "line {index}"
            ))))
            .unwrap();
        }
        tx.send(SessionObservation::ModelRefusal {
            detail: Some("refused".into()),
        })
        .unwrap();
        assert!(matches!(
            rx.try_recv().unwrap(),
            SessionObservation::PromptCorrelationReset
        ));
        assert!(!matches!(
            rx.try_recv().unwrap(),
            SessionObservation::PromptCorrelationReset
        ));

        // A mailbox of admission receipts refuses another receipt without
        // growing; the independent reset marker still wakes the receiver.
        let (tx, rx) = observation_channel();
        for _ in 0..OBSERVATION_QUEUE_CAPACITY {
            tx.send(SessionObservation::DraftAccepted(SessionIntent::Prompt(
                "fill".into(),
            )))
            .unwrap();
        }
        assert!(tx.send(receipt).is_err());
        assert!(matches!(
            rx.recv_timeout(std::time::Duration::ZERO).unwrap(),
            SessionObservation::PromptCorrelationReset
        ));
        assert_eq!(rx.try_iter().count(), OBSERVATION_QUEUE_CAPACITY);
    }
}

#[test]
fn saturated_action_queue_preserves_interrupt_liveness() {
    let (sender, receiver) = intent_channel::<Core>();
    for index in 0..INTENT_QUEUE_CAPACITY {
        sender
            .send(SessionIntent::Prompt(index.to_string().into()))
            .unwrap();
    }
    sender.send(SessionIntent::Interrupt).unwrap();
    sender.send(SessionIntent::Interrupt).unwrap();
    let actions = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(actions.len(), INTENT_QUEUE_CAPACITY + 1);
    assert_eq!(
        actions
            .iter()
            .filter(|action| **action == SessionIntent::Interrupt)
            .count(),
        1
    );
}

#[test]
fn saturated_action_queue_typed_refusals_preserve_user_actions() {
    let (sender, _receiver) = intent_channel::<Core>();
    for index in 0..INTENT_QUEUE_CAPACITY {
        sender
            .send(SessionIntent::Prompt(index.to_string().into()))
            .unwrap();
    }
    for action in [
        SessionIntent::Prompt("draft".into()),
        SessionIntent::EditQueued {
            id: "q-1".into(),
            revision: 7,
            text: "edited".into(),
        },
        SessionIntent::ResumeSession {
            id: Some("session-1".into()),
        },
    ] {
        assert_eq!(
            sender.send(action.clone()),
            Err(IntentSendError::Saturated(action))
        );
    }
}

#[test]
fn nonrepeatable_session_mutations_are_refused_until_host_acknowledgement() {
    for action in [
        SessionIntent::ClearSession,
        SessionIntent::Compact,
        SessionIntent::ForkSession {
            account: None,
            compact: false,
        },
    ] {
        let (sender, receiver) = intent_channel::<Core>();
        sender.send(action.clone()).unwrap();
        assert_eq!(
            sender.send(action.clone()),
            Err(IntentSendError::Duplicate(action.clone()))
        );

        let delivery = receiver.try_recv().unwrap();
        let (delivered_action, acknowledgement) = delivery.into_parts();
        assert_eq!(delivered_action, action);
        assert_eq!(
            sender.send(action.clone()),
            Err(IntentSendError::Duplicate(action.clone()))
        );

        assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
        assert_eq!(
            sender.send(action.clone()),
            Err(IntentSendError::Duplicate(action.clone()))
        );
        drop(acknowledgement);
        sender.send(action.clone()).unwrap();
        assert_eq!(*receiver.try_recv().unwrap(), action);
    }
}

#[test]
fn timeout_and_interrupt_do_not_acknowledge_active_compaction() {
    use std::time::Duration;

    let (sender, receiver) = intent_channel::<Core>();
    sender.send(SessionIntent::Compact).unwrap();
    let (action, acknowledgement) = receiver.try_recv().unwrap().into_parts();
    assert_eq!(action, SessionIntent::Compact);

    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
    assert_eq!(
        receiver.recv_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    sender.send(SessionIntent::Interrupt).unwrap();
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Interrupt);
    assert_eq!(
        sender.send(SessionIntent::Compact),
        Err(IntentSendError::Duplicate(SessionIntent::Compact))
    );

    drop(acknowledgement);
    sender.send(SessionIntent::Compact).unwrap();
}

#[test]
#[allow(
    clippy::result_large_err,
    reason = "the refusal hands the rejected action back verbatim"
)]
fn concurrent_senders_admit_one_nonrepeatable_generation() {
    use std::sync::{Arc, Barrier};

    fn send(sender: &super::IntentSender<Core>) -> Result<(), IntentSendError> {
        sender.send(SessionIntent::Compact)
    }
    let (sender, _receiver) = intent_channel::<Core>();
    let other = sender.clone();
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first = std::thread::spawn(move || {
        first_barrier.wait();
        send(&sender)
    });
    let second_barrier = Arc::clone(&barrier);
    let second = std::thread::spawn(move || {
        second_barrier.wait();
        send(&other)
    });
    barrier.wait();

    let results = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(IntentSendError::Duplicate(_))))
            .count(),
        1
    );
}

#[test]
fn stale_acknowledgement_cannot_release_later_generation() {
    let (sender, receiver) = intent_channel::<Core>();
    sender.send(SessionIntent::Compact).unwrap();
    let (_, first_acknowledgement) = receiver.try_recv().unwrap().into_parts();
    let stale = first_acknowledgement.admission.unwrap();
    drop(first_acknowledgement);

    sender.send(SessionIntent::Compact).unwrap();
    let (_, second_acknowledgement) = receiver.try_recv().unwrap().into_parts();
    receiver.0.release_nonrepeatable(stale);
    assert_eq!(
        sender.send(SessionIntent::Compact),
        Err(IntentSendError::Duplicate(SessionIntent::Compact))
    );

    drop(second_acknowledgement);
    sender.send(SessionIntent::Compact).unwrap();
}

#[test]
fn interrupt_preempts_and_quit_follows_the_accepted_ordinary_queue() {
    let (sender, receiver) = intent_channel::<Core>();
    sender.send(SessionIntent::Prompt("draft".into())).unwrap();
    sender.send(SessionIntent::Quit).unwrap();
    sender.send(SessionIntent::Quit).unwrap();
    sender.send(SessionIntent::Interrupt).unwrap();
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Interrupt);
    assert_eq!(
        *receiver.try_recv().unwrap(),
        SessionIntent::Prompt("draft".into())
    );
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Quit);
    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Empty));
}

#[test]
fn quit_is_ordered_after_already_accepted_ordinary_actions() {
    let (sender, receiver) = intent_channel::<Core>();
    for index in 0..INTENT_QUEUE_CAPACITY - 1 {
        sender
            .send(SessionIntent::Prompt(index.to_string().into()))
            .unwrap();
    }
    sender.send(SessionIntent::Quit).unwrap();
    let actions = std::iter::from_fn(|| receiver.try_recv().ok()).collect::<Vec<_>>();
    assert_eq!(actions.len(), INTENT_QUEUE_CAPACITY);
    assert_eq!(
        actions.last().map(std::ops::Deref::deref),
        Some(&SessionIntent::Quit)
    );
}

#[test]
fn accepted_ordinary_action_is_drained_before_sender_disconnect() {
    let (sender, receiver) = intent_channel::<Core>();
    sender
        .send(SessionIntent::ResumeSession {
            id: Some("session-1".into()),
        })
        .unwrap();
    drop(sender);
    assert_eq!(
        *receiver.recv().unwrap(),
        SessionIntent::ResumeSession {
            id: Some("session-1".into())
        }
    );
    assert_eq!(receiver.recv(), Err(mpsc::RecvError));
}

#[test]
fn timed_action_receive_preserves_urgent_timeout_and_disconnect_semantics() {
    use std::time::Duration;

    let (sender, receiver) = intent_channel::<Core>();
    assert_eq!(
        receiver.recv_timeout(Duration::ZERO),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    sender.send(SessionIntent::Prompt("queued".into())).unwrap();
    sender.send(SessionIntent::Interrupt).unwrap();
    assert_eq!(
        *receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        SessionIntent::Interrupt
    );
    assert_eq!(
        *receiver.recv_timeout(Duration::from_secs(1)).unwrap(),
        SessionIntent::Prompt("queued".into())
    );
    drop(sender);
    assert_eq!(
        receiver.recv_timeout(Duration::from_secs(1)),
        Err(mpsc::RecvTimeoutError::Disconnected)
    );
}

#[test]
fn action_sender_refuses_immediately_after_host_receiver_exits() {
    let (sender, receiver) = intent_channel::<Core>();
    drop(receiver);
    let action = SessionIntent::Prompt("preserve me".into());
    assert_eq!(
        sender.send(action.clone()),
        Err(IntentSendError::Disconnected(action))
    );
}

#[test]
fn observation_mailbox_coalesces_deltas_and_preserves_final_state() {
    let (sender, receiver) = observation_channel::<Core>();
    for _ in 0..1_000 {
        let _ = sender.send(SessionObservation::AssistantDelta("x".into()));
    }
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    sender
        .send(SessionObservation::Status {
            phase: "idle".into(),
            queue_len: 3,
            detail: Some("done".into()),
        })
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.len() < 10);
    assert!(events.contains(&SessionObservation::SetBusy(false)));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::Status { phase, queue_len: 3, .. } if phase == "idle"
    )));
}

#[test]
fn observation_receiver_reports_host_disconnect_after_draining() {
    let (sender, receiver) = observation_channel::<Core>();
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    drop(sender);
    assert_eq!(receiver.try_recv(), Ok(SessionObservation::SetBusy(false)));
    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}

#[test]
fn transcript_replacement_clears_saturation_gap() {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::error(
                NoticeScope::Host,
                format!("diagnostic-{index}"),
            )))
            .unwrap();
    }
    assert_eq!(
        sender.send(SessionObservation::AssistantDelta("dropped".into())),
        Err(SessionObservation::AssistantDelta("dropped".into()))
    );
    assert!(sender.transcript_resnapshot_required());

    let mut authoritative = crate::views::PresentationSnapshot::empty(1);
    authoritative.rows = vec![
        TranscriptRow::user("prompt"),
        TranscriptRow::assistant("complete"),
    ];
    sender
        .send(SessionObservation::PresentationSnapshot(
            authoritative.clone(),
        ))
        .unwrap();
    assert!(!sender.transcript_resnapshot_required());
    assert!(
        receiver
            .try_iter()
            .any(|event| event == SessionObservation::PresentationSnapshot(authoritative.clone()))
    );
}

#[test]
fn typed_presentation_snapshot_clears_saturation_gap() {
    let (sender, _receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        assert!(
            sender
                .send(SessionObservation::Row(TranscriptRow::error(
                    NoticeScope::Host,
                    format!("diagnostic-{index}")
                )))
                .is_ok()
        );
    }
    assert!(
        sender
            .send(SessionObservation::AssistantDelta("dropped".into()))
            .is_err()
    );
    assert!(sender.transcript_resnapshot_required());
    assert!(
        sender
            .send(SessionObservation::PresentationSnapshot(
                crate::views::PresentationSnapshot::empty(1),
            ))
            .is_ok()
    );
    assert!(!sender.transcript_resnapshot_required());
}

#[test]
fn final_state_displaces_saturated_diagnostics() {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::error(
                NoticeScope::Host,
                format!("diagnostic-{index}"),
            )))
            .unwrap();
    }
    sender
        .send(SessionObservation::QueueSnapshot {
            items: vec![QueueItem {
                rejected: false,
                id: "q-final".into(),
                revision: 1,
                text: "pending".into(),
                images: Vec::new(),
            }],
            drain: QueueDrainState::RestoredPaused,
        })
        .unwrap();
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    sender
        .send(SessionObservation::Status {
            phase: "idle".into(),
            queue_len: 1,
            detail: Some("final".into()),
        })
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.contains(&SessionObservation::SetBusy(false)));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::QueueSnapshot { items, drain: QueueDrainState::RestoredPaused }
            if items.first().is_some_and(|item| item.id == "q-final")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::Status { phase, queue_len: 1, detail: Some(detail) }
            if phase == "idle" && detail == "final"
    )));
}

#[test]
fn terminal_error_survives_saturation_and_final_recovery_state() {
    let (sender, receiver) = observation_channel::<Core>();
    for _ in 0..OBSERVATION_QUEUE_CAPACITY {
        sender.send(SessionObservation::AssistantDone).unwrap();
    }
    let terminal = SessionObservation::Row(TranscriptRow::error(
        NoticeScope::Host,
        "provider terminal failure",
    ));
    sender.send(terminal.clone()).unwrap();
    sender
        .send(SessionObservation::QueueSnapshot {
            items: vec![QueueItem {
                rejected: false,
                id: "q-recovery".into(),
                revision: 1,
                text: "pending".into(),
                images: Vec::new(),
            }],
            drain: QueueDrainState::RestoredPaused,
        })
        .unwrap();
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    sender
        .send(SessionObservation::Status {
            phase: "error".into(),
            queue_len: 1,
            detail: Some("recovery required".into()),
        })
        .unwrap();

    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.contains(&terminal));
    assert!(events.contains(&SessionObservation::SetBusy(false)));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::QueueSnapshot {
            drain: QueueDrainState::RestoredPaused,
            ..
        }
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::Status { phase, detail: Some(detail), .. }
            if phase == "error" && detail == "recovery required"
    )));
}

#[test]
fn model_refusal_survives_saturation_and_final_idle_state() {
    let (sender, receiver) = observation_channel::<Core>();
    for _ in 0..OBSERVATION_QUEUE_CAPACITY {
        sender.send(SessionObservation::AssistantDone).unwrap();
    }
    sender
        .send(SessionObservation::ModelRefusal { detail: None })
        .unwrap();
    assert!(
        sender
            .send(SessionObservation::Row(TranscriptRow::info(
                NoticeScope::Host,
                "later note"
            )))
            .is_err()
    );
    sender
        .send(SessionObservation::Row(TranscriptRow::error(
            NoticeScope::Host,
            "later diagnostic",
        )))
        .unwrap();
    let _ = sender.send(SessionObservation::AssistantDone);
    sender
        .send(SessionObservation::DraftRejected(SessionIntent::Prompt(
            "draft retained".into(),
        )))
        .unwrap();
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    sender
        .send(SessionObservation::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.contains(&SessionObservation::ModelRefusal { detail: None }));
    let lines = events
        .iter()
        .filter_map(|event| {
            crate::jsonl::project_host_event(event, crate::envelope::JsonlCorrelation::default())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("\"kind\":\"model_refusal\""))
            .count(),
        1
    );

    assert!(events.contains(&SessionObservation::SetBusy(false)));
}

#[test]
fn turn_lifecycle_survives_replaceable_saturation() {
    let (sender, receiver) = observation_channel::<Core>();
    for _ in 0..OBSERVATION_QUEUE_CAPACITY {
        sender.send(SessionObservation::AssistantDone).unwrap();
    }
    let lifecycle = SessionObservation::TurnLifecycle(TurnLifecycle {
        turn_id: "00000000-0000-4000-8000-0000000000cc".into(),
        phase: TurnPhase::Completed,
        model_error_kind: None,
        http_status: None,
    });
    sender.send(lifecycle.clone()).unwrap();
    // Replaceable transcript/delta/progress traffic cannot displace the
    // retained terminal marker, and neither can a later final state.
    assert!(sender.send(SessionObservation::AssistantDone).is_err());
    assert!(
        sender
            .send(SessionObservation::AssistantDelta("more".into()))
            .is_err()
    );
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    sender
        .send(SessionObservation::Status {
            phase: "idle".into(),
            queue_len: 0,
            detail: None,
        })
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.contains(&lifecycle));
    let lines = events
        .iter()
        .filter_map(|event| {
            crate::jsonl::project_host_event(event, crate::envelope::JsonlCorrelation::default())
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("\"kind\":\"turn_lifecycle\""))
            .count(),
        1
    );
}

#[test]
fn terminal_usage_observation_remains_bounded_at_saturation() {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::info(
                NoticeScope::Host,
                format!("diagnostic-{index}"),
            )))
            .unwrap();
    }
    let usage = SessionObservation::TurnUsage {
        input_tokens: 10,
        output_tokens: 4,
        reasoning_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 0,
        model_operations: Some(2),
        reporting_model_operations: Some(1),
        peak_prepared_input_tokens: None,
        terminal: true,
    };
    sender.send(usage.clone()).unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(events.len(), OBSERVATION_QUEUE_CAPACITY);
    assert!(events.contains(&usage));
}

fn counting_notifier(
    receiver: &super::ObservationReceiver<Core>,
) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
    let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = std::sync::Arc::clone(&count);
    receiver.set_notify(move || {
        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    count
}

fn fired(count: &std::sync::atomic::AtomicUsize) -> usize {
    count.load(std::sync::atomic::Ordering::SeqCst)
}

/// A waiter outside the mailbox is woken once per empty to non-empty edge,
/// never again while the queue still holds what it has not read.
#[test]
fn the_notifier_fires_once_per_empty_to_non_empty_edge() {
    let (sender, receiver) = observation_channel::<Core>();
    let count = counting_notifier(&receiver);
    sender.send(SessionObservation::AssistantDone).unwrap();
    sender
        .send(SessionObservation::Row(TranscriptRow::assistant("second")))
        .unwrap();
    sender
        .send(SessionObservation::AssistantDelta("coalesced".into()))
        .unwrap();
    assert_eq!(fired(&count), 1, "one edge, one wake");
    assert!(receiver.try_recv().is_ok());
    sender.send(SessionObservation::SetBusy(true)).unwrap();
    assert_eq!(fired(&count), 1, "the queue never became empty");
    assert_eq!(receiver.try_iter().count(), 3);
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    assert_eq!(fired(&count), 2, "draining to empty re-arms the edge");
}

/// The receiver learns of a departed host only by looking, so the last
/// sender's drop wakes it; a sender clone leaving does not.
#[test]
fn the_last_sender_leaving_fires_the_notifier() {
    let (sender, receiver) = observation_channel::<Core>();
    let count = counting_notifier(&receiver);
    let clone = sender.clone();
    drop(clone);
    assert_eq!(fired(&count), 0);
    drop(sender);
    assert_eq!(fired(&count), 1);
    assert_eq!(receiver.try_recv(), Err(mpsc::TryRecvError::Disconnected));
}

/// A dropped receiver takes its notifier with it: a host still publishing
/// after the terminal left never calls into the departed consumer.
#[test]
fn a_dropped_receiver_never_calls_its_notifier() {
    let (sender, receiver) = observation_channel::<Core>();
    let count = counting_notifier(&receiver);
    drop(receiver);
    sender.send(SessionObservation::AssistantDone).unwrap();
    drop(sender);
    assert_eq!(fired(&count), 0);
}
