#[test]
fn patch_overflow_and_replacements_are_isolated_by_scope() {
    use crate::views::transcript::{TranscriptPatch, TranscriptScope};
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::error(
                NoticeScope::Host,
                format!("error {index}"),
            )))
            .unwrap();
    }
    let root = TranscriptScope::Root;
    let child = TranscriptScope::Agent("c1".into());
    for scope in [&root, &child] {
        assert!(
            sender
                .send(SessionObservation::TranscriptPatch(TranscriptPatch {
                    scope: scope.clone(),
                    from_revision: 1,
                    to_revision: 2,
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                }))
                .is_err()
        );
        assert!(sender.transcript_resnapshot_required_for(scope));
    }
    let _ = receiver.try_iter().count();
    sender
        .send(SessionObservation::PresentationSnapshot(
            crate::views::PresentationSnapshot::empty(3),
        ))
        .unwrap();
    assert!(!sender.transcript_resnapshot_required_for(&root));
    assert!(sender.transcript_resnapshot_required_for(&child));
    let mut snapshot = crate::views::PresentationSnapshot::empty(4);
    snapshot.scope = child.clone();
    sender
        .send(SessionObservation::PresentationSnapshot(snapshot))
        .unwrap();
    assert!(!sender.transcript_resnapshot_required_for(&child));
    assert_eq!(
        receiver.try_iter().count(),
        2,
        "a child snapshot never supersedes the root snapshot"
    );
}

#[test]
fn presentation_revisions_are_allocated_by_the_channel_not_a_sender_wrapper() {
    let (sender, _receiver) = observation_channel::<Core>();
    let clone = sender.clone();
    assert_eq!(sender.next_presentation_revision(), Some(1));
    assert_eq!(clone.next_presentation_revision(), Some(2));
    assert_eq!(sender.next_presentation_revision(), Some(3));
}

#[test]
fn root_interrupt_is_not_process_shutdown_and_quit_remains_latched_after_dequeue() {
    let (sender, receiver) = intent_channel::<Core>();
    let interrupt = receiver.storage_interrupt_probe();
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&shutdowns);
    receiver.on_shutdown(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    sender.send(SessionIntent::Interrupt).unwrap();
    assert!(interrupt());
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Interrupt);
    assert!(
        !interrupt(),
        "a later root turn does not inherit an earlier interrupt"
    );
    sender.send(SessionIntent::Quit).unwrap();
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(*receiver.try_recv().unwrap(), SessionIntent::Quit);
    drop(sender);
    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        1,
        "the shutdown hook runs once"
    );
    let late = Arc::clone(&shutdowns);
    receiver.on_shutdown(move || {
        late.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        2,
        "a hook registered after shutdown runs at once"
    );
}

#[test]
fn the_last_sender_dropping_runs_the_shutdown_hooks() {
    let (sender, receiver) = intent_channel::<Core>();
    let second = sender.clone();
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&shutdowns);
    receiver.on_shutdown(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    });
    drop(sender);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
    drop(second);
    assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

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
        SessionIntent::NewSession,
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
    use std::sync::Barrier;

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
            phase: crate::status::HostPhase::Idle,
            queue_len: 3,
            detail: Some(crate::status::HostStatusDetail::PlanDiagnostic {
                diagnostic: crate::status::DiagnosticText::new("done"),
            }),
        })
        .unwrap();
    let events = receiver.try_iter().collect::<Vec<_>>();
    assert!(events.len() < 10);
    assert!(events.contains(&SessionObservation::SetBusy(false)));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionObservation::Status { phase, queue_len: 3, .. } if matches!(phase, crate::status::HostPhase::Idle)
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
    authoritative.items = vec![
        TranscriptRow::user("prompt"),
        TranscriptRow::assistant("complete"),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, row)| crate::views::transcript::TranscriptItem {
        key: crate::views::transcript::RowKey::new(
            index as u64 + 1,
            crate::views::transcript::Lane::Runtime,
            0,
        ),
        row,
    })
    .collect();
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
            phase: crate::status::HostPhase::Idle,
            queue_len: 1,
            detail: Some(crate::status::HostStatusDetail::PlanDiagnostic {
                diagnostic: crate::status::DiagnosticText::new("final"),
            }),
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
            if matches!(phase, crate::status::HostPhase::Idle) && matches!(detail, crate::status::HostStatusDetail::PlanDiagnostic { diagnostic } if diagnostic.as_str() == "final")
    )));
}

#[test]
fn admission_saturation_preserves_final_latest_state() {
    let (tx, rx) = observation_channel::<Core>();
    tx.send(SessionObservation::SetBusy(false)).unwrap();
    let status = SessionObservation::Status {
        phase: crate::status::HostPhase::Idle,
        queue_len: 0,
        detail: Some(crate::status::HostStatusDetail::PlanDiagnostic {
            diagnostic: crate::status::DiagnosticText::new("final"),
        }),
    };
    tx.send(status.clone()).unwrap();
    for _ in 2..OBSERVATION_QUEUE_CAPACITY {
        tx.send(SessionObservation::Row(TranscriptRow::assistant("fill")))
            .unwrap();
    }
    for _ in 0..OBSERVATION_QUEUE_CAPACITY {
        let _ = tx.send(SessionObservation::DraftAccepted(SessionIntent::Prompt(
            "next".into(),
        )));
    }
    let events = rx.try_iter().collect::<Vec<_>>();
    assert!(events.contains(&SessionObservation::SetBusy(false)));
    assert!(events.contains(&status));
    assert!(events.len() <= OBSERVATION_QUEUE_CAPACITY + 1);
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
            phase: crate::status::HostPhase::Error,
            queue_len: 1,
            detail: Some(crate::status::HostStatusDetail::PlanDiagnostic {
                diagnostic: crate::status::DiagnosticText::new("recovery required"),
            }),
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
            if matches!(phase, crate::status::HostPhase::Error) && matches!(detail, crate::status::HostStatusDetail::PlanDiagnostic { diagnostic } if diagnostic.as_str() == "recovery required")
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
            phase: crate::status::HostPhase::Idle,
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
            phase: crate::status::HostPhase::Idle,
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

fn counting_notifier(receiver: &super::ObservationReceiver<Core>) -> Arc<AtomicUsize> {
    let count = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&count);
    receiver.set_notify(move || {
        seen.fetch_add(1, Ordering::SeqCst);
    });
    count
}

fn fired(count: &AtomicUsize) -> usize {
    count.load(Ordering::SeqCst)
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

fn wait_ready(receiver: &super::IntentReceiver<Core>, within: std::time::Duration) -> bool {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(async { tokio::time::timeout(within, receiver.ready()).await.is_ok() })
}

/// An async consumer wakes on an arrival instead of polling for one: an
/// action, an urgent interrupt, a quit and the last sender leaving each end
/// the wait, and nothing else does.
#[test]
fn intent_readiness_wakes_on_every_arrival_and_on_nothing_else() {
    let short = std::time::Duration::from_millis(30);
    let long = std::time::Duration::from_secs(10);
    let (sender, receiver) = intent_channel::<Core>();
    assert!(
        !wait_ready(&receiver, short),
        "an empty mailbox is not ready"
    );
    for action in [
        SessionIntent::Prompt("arrives later".into()),
        SessionIntent::Interrupt,
        SessionIntent::Quit,
    ] {
        let late = sender.clone();
        let arrival = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            late.send(action).unwrap();
        });
        assert!(wait_ready(&receiver, long));
        arrival.join().unwrap();
        assert!(wait_ready(&receiver, short), "readiness holds until taken");
        receiver.try_recv().unwrap();
        assert!(!wait_ready(&receiver, short), "taken means not ready");
    }
    let leaving = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(sender);
    });
    assert!(
        wait_ready(&receiver, long),
        "the last sender leaving wakes it"
    );
    leaving.join().unwrap();
}
#[test]
fn child_flood_leaves_room_for_root_and_consecutive_patches_share_one_slot() {
    use super::{AGENT_OBSERVATION_SHARE, agent_observation};
    use crate::views::transcript::{
        Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptScope, TranscriptStreamDelta,
    };
    let (sender, receiver) = observation_channel::<Core>();
    let scope = TranscriptScope::Agent("child".into());
    for revision in 1..=1_000 {
        sender
            .send(SessionObservation::TranscriptPatch(TranscriptPatch {
                scope: scope.clone(),
                from_revision: revision,
                to_revision: revision + 1,
                upserts: vec![TranscriptItem {
                    key: RowKey::new(1, Lane::Runtime, 0),
                    row: TranscriptRow::assistant(revision.to_string()),
                }],
                deletes: Vec::new(),
            }))
            .unwrap();
    }
    let SessionObservation::TranscriptPatch(merged) = receiver.try_recv().unwrap() else {
        panic!("merged patch");
    };
    assert_eq!((merged.from_revision, merged.to_revision), (1, 1_001));
    assert_eq!(merged.upserts[0].row, TranscriptRow::assistant("1000"));
    assert!(receiver.try_recv().is_err());
    for index in 0..AGENT_OBSERVATION_SHARE {
        sender
            .send(SessionObservation::TranscriptStream {
                scope: scope.clone(),
                delta: TranscriptStreamDelta::Reasoning {
                    location_key: index.to_string(),
                    text: "child".into(),
                },
            })
            .unwrap();
    }
    assert!(
        sender
            .send(SessionObservation::TranscriptPatch(TranscriptPatch {
                scope: scope.clone(),
                from_revision: 1_001,
                to_revision: 1_002,
                upserts: Vec::new(),
                deletes: Vec::new(),
            }))
            .is_err()
    );
    for index in AGENT_OBSERVATION_SHARE..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::ReasoningDelta {
                location_key: index.to_string(),
                text: "root".into(),
            })
            .unwrap();
    }
    sender.send(SessionObservation::AssistantDone).unwrap();
    sender.send(SessionObservation::SetBusy(false)).unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionObservation::AssistantDone))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionObservation::SetBusy(false)))
    );
    assert!(
        events
            .iter()
            .filter(|event| agent_observation(event))
            .count()
            <= AGENT_OBSERVATION_SHARE
    );
    assert!(sender.transcript_resnapshot_required_for(&scope));
}

#[test]
fn merged_patches_fold_deletes_reinsertion_and_key_moves_across_another_scope() {
    use crate::views::transcript::{
        Lane, RowKey, TranscriptItem, TranscriptPatch, TranscriptScope, TranscriptState,
    };
    let rows = |values: &[(u64, &str)]| {
        values
            .iter()
            .map(|(key, text)| {
                (
                    RowKey::new(*key, Lane::Runtime, 0),
                    TranscriptRow::assistant(*text),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let before = rows(&[(1, "old"), (2, "delete"), (3, "keep")]);
    let middle = rows(&[(3, "keep"), (4, "moved"), (5, "transient")]);
    let after = rows(&[(1, "reinserted"), (3, "keep"), (4, "moved"), (6, "new")]);
    let (sender, receiver) = observation_channel::<Core>();
    sender
        .send(SessionObservation::TranscriptPatch(
            TranscriptPatch::between(TranscriptScope::Root, 10, 20, &before, &middle),
        ))
        .unwrap();
    sender
        .send(SessionObservation::TranscriptPatch(TranscriptPatch {
            scope: TranscriptScope::Agent("child".into()),
            from_revision: 12,
            to_revision: 25,
            upserts: Vec::new(),
            deletes: Vec::new(),
        }))
        .unwrap();
    sender
        .send(SessionObservation::TranscriptPatch(
            TranscriptPatch::between(TranscriptScope::Root, 20, 30, &middle, &after),
        ))
        .unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    assert_eq!(events.len(), 2);
    let mut folded = TranscriptState::default();
    folded
        .replace(crate::views::PresentationSnapshot {
            revision: 10,
            items: before
                .into_iter()
                .map(|(key, row)| TranscriptItem { key, row })
                .collect(),
            ..crate::views::PresentationSnapshot::default()
        })
        .unwrap();
    for event in events {
        let SessionObservation::TranscriptPatch(patch) = event else {
            panic!("patch");
        };
        if patch.scope == TranscriptScope::Root {
            folded.apply(patch).unwrap();
        }
    }
    assert_eq!(folded.revision(), 30);
    assert_eq!(folded.items(), &after);
}

#[test]
fn patches_never_merge_across_raw_or_same_scope_stream_appends() {
    use crate::views::transcript::{TranscriptPatch, TranscriptScope, TranscriptStreamDelta};
    for scope in [
        TranscriptScope::Root,
        TranscriptScope::Agent("child".into()),
    ] {
        let (sender, receiver) = observation_channel::<Core>();
        let patch = |from_revision| {
            SessionObservation::TranscriptPatch(TranscriptPatch {
                scope: scope.clone(),
                from_revision,
                to_revision: from_revision + 1,
                upserts: Vec::new(),
                deletes: Vec::new(),
            })
        };
        sender.send(patch(1)).unwrap();
        let stream = match &scope {
            TranscriptScope::Root => {
                SessionObservation::AssistantDelta("between boundaries".into())
            }
            TranscriptScope::Agent(_) => SessionObservation::TranscriptStream {
                scope: scope.clone(),
                delta: TranscriptStreamDelta::Assistant("between boundaries".into()),
            },
        };
        sender.send(stream.clone()).unwrap();
        sender.send(patch(2)).unwrap();
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            vec![patch(1), stream, patch(2)]
        );
    }
}

#[test]
fn saturated_deltas_keep_credential_wait_park_and_clear() {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::ReasoningDelta {
                location_key: index.to_string(),
                text: "x".into(),
            })
            .unwrap();
    }
    assert!(
        sender
            .send(SessionObservation::AssistantDelta("no room".into()))
            .is_err()
    );
    let park = SessionObservation::CredentialWait {
        account: Some("codex".into()),
    };
    let clear = SessionObservation::CredentialWait { account: None };
    sender.send(park.clone()).unwrap();
    sender.send(clear.clone()).unwrap();
    sender.send(SessionObservation::SetBusy(true)).unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    let waits: Vec<_> = events
        .iter()
        .filter(|event| matches!(event, SessionObservation::CredentialWait { .. }))
        .cloned()
        .collect();
    assert_eq!(waits, vec![park, clear]);
}

#[test]
fn saturated_snapshots_are_not_spent_and_a_queued_wait_survives_them() {
    use crate::intent::{Never, ObservationExtension, ProtocolExtension};

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Snap(u16);
    impl ObservationExtension for Snap {
        fn latest_state(&self) -> bool {
            true
        }
        fn same_kind(&self, other: &Self) -> bool {
            self.0 == other.0
        }
        fn kind_label(&self) -> &'static str {
            "snap"
        }
    }
    #[derive(Clone, Debug, Eq, PartialEq)]
    struct Ext;
    impl ProtocolExtension for Ext {
        type Intent = Never;
        type Observation = Snap;
    }

    let (sender, receiver) = observation_channel::<Ext>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Extension(Snap(
                u16::try_from(index).unwrap(),
            )))
            .unwrap();
    }
    let park = SessionObservation::CredentialWait {
        account: Some("codex".into()),
    };
    assert!(
        sender.send(park.clone()).is_err(),
        "a latest-state snapshot is not spent to admit a credential wait"
    );
    assert!(receiver.try_recv().is_ok(), "the snapshot queue was full");
    sender.send(park.clone()).unwrap();
    assert!(
        sender
            .send(SessionObservation::Extension(Snap(u16::MAX)))
            .is_err(),
        "a queued credential wait is not an eviction candidate"
    );
    let waits: Vec<_> = receiver
        .try_iter()
        .filter(|event| matches!(event, SessionObservation::CredentialWait { .. }))
        .collect();
    assert_eq!(waits, vec![park]);
}

fn assert_retained_park(
    label: &str,
    filler: fn(usize) -> SessionObservation<Core>,
    incoming: SessionObservation<Core>,
) {
    let park = SessionObservation::CredentialWait {
        account: Some("codex".into()),
    };
    let (sender, receiver) = observation_channel::<Core>();
    sender.send(park.clone()).unwrap();
    for index in 1..OBSERVATION_QUEUE_CAPACITY {
        sender.send(filler(index)).unwrap_or_else(|_| {
            panic!("{label} filler must enqueue beside the park");
        });
    }
    assert!(
        sender.send(incoming).is_err(),
        "{label} must be refused rather than evict the parked credential wait"
    );
    let waits: Vec<_> = receiver
        .try_iter()
        .filter(|event| matches!(event, SessionObservation::CredentialWait { .. }))
        .collect();
    assert_eq!(waits, vec![park], "{label}");
}

#[test]
fn saturated_branches_refuse_to_evict_a_queued_credential_wait() {
    // Fillers are what that branch already refuses to spend, so only the
    // retained-edge check keeps the park in the queue.
    assert_retained_park(
        "assistant done",
        |_| SessionObservation::AssistantDone,
        SessionObservation::AssistantDone,
    );
    assert_retained_park(
        "terminal error",
        |_| SessionObservation::ModelRefusal { detail: None },
        SessionObservation::Row(TranscriptRow::error(NoticeScope::Host, "terminal")),
    );
    assert_retained_park(
        "replaceable notice",
        |_| SessionObservation::AssistantDone,
        SessionObservation::Row(TranscriptRow::info(NoticeScope::Host, "note")),
    );
    assert_retained_park(
        "admission",
        |_| SessionObservation::IntentStarted(None),
        SessionObservation::IntentStarted(Some("incoming".into())),
    );
}

fn content_tool_line(index: usize) -> SessionObservation<Core> {
    SessionObservation::ToolLine {
        call_id: Some(index.to_string()),
        parent_call_id: None,
        cell_id: None,
        kind: crate::views::ActivityKind::Tool,
        name: "read_file".into(),
        state: "running".into(),
        subject: None,
        detail: None,
        duration_ms: None,
        started_at_ms: None,
        finished_at_ms: None,
        wait: None,
    }
}

fn content_patch(index: usize) -> SessionObservation<Core> {
    use crate::views::transcript::{TranscriptPatch, TranscriptScope};
    let revision = u64::try_from(index).unwrap() * 2;
    SessionObservation::TranscriptPatch(TranscriptPatch {
        scope: TranscriptScope::Root,
        from_revision: revision,
        to_revision: revision + 1,
        upserts: Vec::new(),
        deletes: Vec::new(),
    })
}

fn admit_credential_wait_by_spending(
    label: &str,
    filler: fn(usize) -> SessionObservation<Core>,
    wait: &SessionObservation<Core>,
) {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender.send(filler(index)).unwrap();
    }
    assert!(
        !sender.transcript_resnapshot_required(),
        "{label} fill must not open a gap"
    );
    sender.send(wait.clone()).unwrap();
    assert!(
        sender.transcript_resnapshot_required(),
        "{label} drop must mark the transcript for resnapshot"
    );
    let events: Vec<_> = receiver.try_iter().collect();
    assert!(events.contains(wait), "{label}");
    assert!(
        !events.contains(&filler(0)),
        "{label} spends the earliest repaired content"
    );
    assert!(events.contains(&filler(1)), "{label}");
}

#[test]
fn credential_wait_spends_gap_repaired_content_when_no_delta_is_queued() {
    let park = SessionObservation::CredentialWait {
        account: Some("codex".into()),
    };
    let clear = SessionObservation::CredentialWait { account: None };
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY - 1 {
        sender
            .send(SessionObservation::Row(TranscriptRow::assistant(format!(
                "row {index}"
            ))))
            .unwrap();
    }
    let delta = SessionObservation::ReasoningDelta {
        location_key: "k".into(),
        text: "think".into(),
    };
    sender.send(delta.clone()).unwrap();
    sender.send(park.clone()).unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    assert_eq!(events.len(), OBSERVATION_QUEUE_CAPACITY);
    assert!(events.contains(&park));
    assert!(
        !events.contains(&delta),
        "a streaming delta is spent before a transcript row"
    );
    assert!(events.contains(&SessionObservation::Row(TranscriptRow::assistant("row 0"))));

    admit_credential_wait_by_spending(
        "row",
        |index| SessionObservation::Row(TranscriptRow::assistant(format!("row {index}"))),
        &park,
    );
    admit_credential_wait_by_spending("tool", content_tool_line, &clear);
    admit_credential_wait_by_spending("patch", content_patch, &park);

    assert_unspent_credential_wait("error row", &clear, |index| {
        SessionObservation::Row(TranscriptRow::error(
            NoticeScope::Host,
            format!("e {index}"),
        ))
    });
    assert_unspent_credential_wait("info notice", &clear, |index| {
        SessionObservation::Row(TranscriptRow::info(NoticeScope::Host, format!("n {index}")))
    });
}

#[test]
fn credential_wait_spends_a_row_or_patch_before_an_earlier_tool_line() {
    use crate::views::transcript::{TranscriptPatch, TranscriptScope};
    let park = SessionObservation::CredentialWait {
        account: Some("codex".into()),
    };
    let (sender, receiver) = observation_channel::<Core>();
    let ahead = content_tool_line(0);
    sender.send(ahead.clone()).unwrap();
    for index in 1..OBSERVATION_QUEUE_CAPACITY - 1 {
        sender.send(content_tool_line(index)).unwrap();
    }
    let row = SessionObservation::Row(TranscriptRow::assistant("behind tools"));
    sender.send(row.clone()).unwrap();
    sender.send(park.clone()).unwrap();
    let events: Vec<_> = receiver.try_iter().collect();
    assert!(events.contains(&park));
    assert!(
        events.contains(&ahead),
        "a tool line queued ahead of a row is not spent"
    );
    assert!(
        !events.contains(&row),
        "the row is spent before any tool line"
    );
    assert!(sender.transcript_resnapshot_required());

    let (sender, receiver) = observation_channel::<Core>();
    let agent = TranscriptScope::Agent("child".into());
    let patch = SessionObservation::TranscriptPatch(TranscriptPatch {
        scope: agent.clone(),
        from_revision: 0,
        to_revision: 1,
        upserts: Vec::new(),
        deletes: Vec::new(),
    });
    sender.send(patch.clone()).unwrap();
    for index in 1..OBSERVATION_QUEUE_CAPACITY {
        sender
            .send(SessionObservation::Row(TranscriptRow::assistant(format!(
                "root {index}"
            ))))
            .unwrap();
    }
    assert!(!sender.transcript_resnapshot_required());
    assert!(!sender.transcript_resnapshot_required_for(&agent));
    sender.send(park).unwrap();
    assert!(
        sender.transcript_resnapshot_required_for(&agent),
        "spending the agent patch marks that scope"
    );
    assert!(
        !sender.transcript_resnapshot_required(),
        "an agent patch does not mark the root"
    );
    let events: Vec<_> = receiver.try_iter().collect();
    assert!(!events.contains(&patch));
    assert!(events.contains(&SessionObservation::Row(TranscriptRow::assistant("root 1"))));
}

fn assert_unspent_credential_wait(
    label: &str,
    wait: &SessionObservation<Core>,
    filler: fn(usize) -> SessionObservation<Core>,
) {
    let (sender, receiver) = observation_channel::<Core>();
    for index in 0..OBSERVATION_QUEUE_CAPACITY {
        sender.send(filler(index)).unwrap();
    }
    assert!(
        sender.send(wait.clone()).is_err(),
        "{label} is not spent to admit a credential wait"
    );
    assert!(!sender.transcript_resnapshot_required(), "{label}");
    assert!(
        !receiver
            .try_iter()
            .any(|event| matches!(event, SessionObservation::CredentialWait { .. })),
        "{label}"
    );
}
