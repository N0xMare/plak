#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::*;
use crate::sessions::projector::{AnchoredIntent, OpenTurn, Submission, SubmissionKind};

fn lost(intents: &mut ProductIntents, anchor: u64, cause: RootInterruptCause) {
    intents.root_interrupts.push(AnchoredIntent {
        anchor,
        ordinal: 0,
        value: RootInterrupt {
            anchor: RootStopAnchor::Turn {
                turn_id: "lost-turn".to_owned(),
            },
            cause,
        },
    });
}

fn submitted(intents: &mut ProductIntents, command_id: &str, anchor: u64) {
    intents.submissions.push(Submission {
        command_id: command_id.to_owned(),
        kind: SubmissionKind::RootStart,
        queue_item_id: format!("item-{command_id}"),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: "prompt".to_owned(),
        submitted_at_ms: 1,
    });
    intents
        .submission_anchors
        .insert(command_id.to_owned(), anchor);
}

fn read(intents: &ProductIntents, views: &SessionViews) -> Option<RootLossState> {
    state(&latest_loss(intents)?, intents, views)
}

#[test]
fn root_continuation_equal_loss_positions_in_distinct_sessions_do_not_collide() {
    let directory = tempfile::tempdir().unwrap();
    let rows = crate::runtime::test_kit::MemoryRows::new();
    let store: &dyn SessionRecordStore = rows.records();
    let mut commands = Vec::new();
    for _ in 0..2 {
        let mut session = SessionRecord::new(directory.path().to_path_buf());
        store.save(&mut session).unwrap();
        record_root_handoff(
            store,
            &session,
            &LostRootTurn {
                turn_id: "equal-feed-position".into(),
                applied_seq: 0,
            },
        )
        .unwrap();
        let command = record_root_consent(store, &session).unwrap().unwrap();
        assert_eq!(
            record_root_consent(store, &session).unwrap(),
            Some(command.clone())
        );
        commands.push(command);
    }
    assert_ne!(commands[0], commands[1]);
}

#[test]
fn root_continuation_recorded_legacy_decisions_are_reused_not_rederived() {
    use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
    use crate::sessions::projector::ViewWrites;
    struct UnknownOwner;
    impl StoreFaults for UnknownOwner {
        fn before(&self, call: StoreCall, _: Option<&SessionRecord>) -> Result<(), String> {
            if call == StoreCall::SubmissionOwner {
                return Err("submission ownership cannot be verified".into());
            }
            Ok(())
        }
    }
    for consumed in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let rows = std::sync::Arc::new(crate::session_store::MemoryStore::new());
        let store: &dyn SessionRecordStore = rows.as_ref();
        let mut session = SessionRecord::new(directory.path().to_path_buf());
        store.save(&mut session).unwrap();
        record_root_handoff(
            store,
            &session,
            &LostRootTurn {
                turn_id: "legacy-turn".into(),
                applied_seq: 0,
            },
        )
        .unwrap();
        let intents = store.load_intents(&session.id).unwrap();
        let loss = latest_loss(&intents).unwrap();
        let digest = plakous_workspace::occupancy_vcs::digest_text(&loss.identity);
        let legacy = format!("{ROOT_LOSS_RESUME_PREFIX}{}", &digest[..32]);
        assert_ne!(legacy, command_id(&session.id, &loss.identity));
        store
            .write_root_loss(
                &session.id,
                RootLossIntent::Continuation(RootContinuation {
                    anchor: loss.anchor,
                    host_generation: 1,
                    cause: loss.cause,
                    round: 1,
                    decided_at_ms: 1,
                    decision: RootContinuationDecision::NewTurn {
                        command_id: legacy.clone(),
                        withdrawn: Vec::new(),
                    },
                }),
            )
            .unwrap();
        let unknown = FaultStore::new(rows.clone(), UnknownOwner);
        assert!(record_root_consent(&unknown, &session).is_err());
        assert_eq!(
            store
                .load_intents(&session.id)
                .unwrap()
                .root_continuations
                .len(),
            1
        );
        if consumed {
            let before = SessionViews::empty();
            let mut after = before.clone();
            after
                .state
                .deliveries
                .insert(legacy.clone(), Delivery::Accepted);
            store
                .commit_projection_views(
                    &session.id,
                    &ViewWrites::between(&before, &after),
                    &after.state,
                )
                .unwrap();
        }
        for _ in 0..2 {
            assert_eq!(
                record_root_consent(store, &session).unwrap(),
                (!consumed).then(|| legacy.clone())
            );
            assert_eq!(
                store
                    .load_intents(&session.id)
                    .unwrap()
                    .root_continuations
                    .len(),
                1
            );
        }
    }
}

#[test]
fn root_continuation_foreign_legacy_submission_is_superseded_once_without_touching_its_owner() {
    use crate::sessions::projector::ViewWrites;
    let directory = tempfile::tempdir().unwrap();
    let rows = crate::runtime::test_kit::MemoryRows::new();
    let store: &dyn SessionRecordStore = rows.records();
    let mut sessions = [
        SessionRecord::new(directory.path().to_path_buf()),
        SessionRecord::new(directory.path().to_path_buf()),
    ];
    let mut legacy = String::new();
    for session in &mut sessions {
        store.save(session).unwrap();
        record_root_handoff(
            store,
            session,
            &LostRootTurn {
                turn_id: "old-handoff".into(),
                applied_seq: 0,
            },
        )
        .unwrap();
        let loss = latest_loss(&store.load_intents(&session.id).unwrap()).unwrap();
        let digest = plakous_workspace::occupancy_vcs::digest_text(&loss.identity);
        legacy = format!("{ROOT_LOSS_RESUME_PREFIX}{}", &digest[..32]);
        store
            .write_root_loss(
                &session.id,
                RootLossIntent::Continuation(RootContinuation {
                    anchor: loss.anchor,
                    host_generation: 1,
                    cause: loss.cause,
                    round: 1,
                    decided_at_ms: 1,
                    decision: RootContinuationDecision::NewTurn {
                        command_id: legacy.clone(),
                        withdrawn: Vec::new(),
                    },
                }),
            )
            .unwrap();
    }
    let submission = Submission {
        command_id: legacy.clone(),
        kind: SubmissionKind::HostWake,
        queue_item_id: "legacy-item".into(),
        queue_item_revision: 1,
        target_turn_id: None,
        canonical_command: ROOT_HANDOFF_RESUME_TEXT.into(),
        submitted_at_ms: 1,
    };
    store
        .save_submission(&mut sessions[0], &submission)
        .unwrap();
    assert_eq!(
        record_root_consent(store, &sessions[0]).unwrap(),
        Some(legacy.clone())
    );
    let before = SessionViews::empty();
    let mut consumed = before.clone();
    consumed
        .state
        .deliveries
        .insert(legacy.clone(), Delivery::Accepted);
    store
        .commit_projection_views(
            &sessions[0].id,
            &ViewWrites::between(&before, &consumed),
            &consumed.state,
        )
        .unwrap();
    let replacement = record_root_consent(store, &sessions[1]).unwrap().unwrap();
    assert_ne!(legacy, replacement);
    for _ in 0..2 {
        assert_eq!(record_root_consent(store, &sessions[0]).unwrap(), None);
        assert_eq!(
            record_root_consent(store, &sessions[1]).unwrap(),
            Some(replacement.clone())
        );
        assert_eq!(
            store.submission_owner(&legacy).unwrap(),
            Some(sessions[0].id.clone())
        );
        let intents = store.load_intents(&sessions[1].id).unwrap();
        assert_eq!(intents.root_continuations.len(), 2);
        assert!(matches!(&intents.root_continuations[1].value.decision,
            RootContinuationDecision::NewTurn { command_id, withdrawn }
            if command_id == &replacement && withdrawn == std::slice::from_ref(&legacy)));
    }
}

/// The root's owner-lost interrupt holds its continuation until a
/// decision names it, and the decision is pending until its command is
/// admitted, running while its turn is open, then over.
#[test]
fn a_root_loss_holds_then_pends_then_runs_its_continuation() {
    let mut intents = ProductIntents::default();
    let mut views = SessionViews::empty();
    assert_eq!(read(&intents, &views), None);
    lost(&mut intents, 9, RootInterruptCause::OwnerLost);
    assert_eq!(read(&intents, &views), Some(RootLossState::Held));
    let loss = latest_loss(&intents).unwrap();
    let command = command_id("session-one", &loss.identity);
    assert!(is_root_loss_command(&command));
    assert!(command.len() <= 128);
    intents.root_continuations.push(AnchoredIntent {
        anchor: 9,
        ordinal: 1,
        value: RootContinuation {
            anchor: loss.anchor.clone(),
            host_generation: 1,
            cause: RootContinuationCause::OwnerLost,
            round: 1,
            decided_at_ms: 1,
            decision: RootContinuationDecision::NewTurn {
                command_id: command.clone(),
                withdrawn: Vec::new(),
            },
        },
    });
    assert_eq!(
        read(&intents, &views),
        Some(RootLossState::Pending(command.clone()))
    );
    views
        .state
        .deliveries
        .insert(command.clone(), Delivery::Accepted);
    views.state.open_turns.insert(
        "continuation-turn".to_owned(),
        OpenTurn {
            command_id: command.clone(),
            host_wake: true,
            round_anchor: None,
            cancel_requested: false,
        },
    );
    assert_eq!(read(&intents, &views), Some(RootLossState::Running));
    views.state.open_turns.clear();
    assert_eq!(read(&intents, &views), None, "the continuation ran");
}

/// A root turn admitted since the loss ends the hold; one written before
/// it does not, and a stop is never held.
#[test]
fn a_later_root_turn_ends_the_hold_and_a_stop_is_never_held() {
    let mut intents = ProductIntents::default();
    let mut views = SessionViews::empty();
    lost(&mut intents, 9, RootInterruptCause::OwnerLost);
    submitted(&mut intents, "before", 4);
    views
        .state
        .deliveries
        .insert("before".to_owned(), Delivery::Accepted);
    assert_eq!(read(&intents, &views), Some(RootLossState::Held));
    submitted(&mut intents, "after", 9);
    assert_eq!(
        read(&intents, &views),
        Some(RootLossState::Held),
        "a prompt not yet admitted does not end it"
    );
    views
        .state
        .deliveries
        .insert("after".to_owned(), Delivery::Accepted);
    assert_eq!(read(&intents, &views), None, "a new prompt ended the hold");

    let mut stopped = ProductIntents::default();
    lost(&mut stopped, 9, RootInterruptCause::OwnerLost);
    stopped.root_interrupts.push(AnchoredIntent {
        anchor: 10,
        ordinal: 0,
        value: RootInterrupt {
            anchor: RootStopAnchor::Turn {
                turn_id: "lost-turn".to_owned(),
            },
            cause: RootInterruptCause::Stopped,
        },
    });
    assert_eq!(read(&stopped, &SessionViews::empty()), None);
}

/// Through the product store: the root records its owner-lost
/// interrupt once per lost turn, and its consent once, before anything is
/// dispatched.
#[test]
fn the_root_records_its_loss_and_consent_in_the_product_store() {
    let root = tempfile::tempdir().unwrap();
    let rows = crate::runtime::test_kit::MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    rows.records().save(&mut session).unwrap();
    let store: &dyn SessionRecordStore = rows.records();
    assert_eq!(root_loss_state(store, &session).unwrap(), None);
    let lost = LostRootTurn {
        turn_id: "lost-turn".to_owned(),
        applied_seq: 0,
    };
    assert!(record_root_owner_lost(store, &session, &lost).unwrap());
    assert!(
        !record_root_owner_lost(store, &session, &lost).unwrap(),
        "a reopen records the same loss once"
    );
    let intents = store.load_intents(&session.id).unwrap();
    assert_eq!(
        intents.root_interrupts.len(),
        1,
        "one interrupt per lost turn"
    );
    assert_eq!(
        root_loss_state(store, &session).unwrap(),
        Some(RootLossState::Held)
    );
    let command = record_root_consent(store, &session).unwrap().unwrap();
    assert!(is_root_loss_command(&command));
    assert_eq!(
        record_root_consent(store, &session).unwrap(),
        Some(command.clone()),
        "a pending decision is returned as it is"
    );
    let intents = store.load_intents(&session.id).unwrap();
    assert_eq!(intents.root_continuations.len(), 1);
    assert_eq!(
        root_loss_state(store, &session).unwrap(),
        Some(RootLossState::Pending(command))
    );
}

/// The loss is written only once the views applied the lost turn's
/// terminal, so the lost turn's own submission is older than the loss;
/// until then nothing is written and the next open records it.
#[test]
fn the_loss_waits_for_the_views_to_apply_the_lost_terminal() {
    let root = tempfile::tempdir().unwrap();
    let rows = crate::runtime::test_kit::MemoryRows::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    rows.records().save(&mut session).unwrap();
    let store: &dyn SessionRecordStore = rows.records();
    let lost = LostRootTurn {
        turn_id: "lost-turn".to_owned(),
        applied_seq: 7,
    };
    assert!(!record_root_owner_lost(store, &session, &lost).unwrap());
    assert_eq!(
        store
            .load_intents(&session.id)
            .unwrap()
            .root_interrupts
            .len(),
        0,
        "nothing is written before the terminal is applied"
    );
}

/// The scan for a lost root turn starts at the earlier of the views'
/// cursor and the root's latest accepted submission, so a projection that
/// already applied the lost turn's terminal cannot hide it.
#[test]
fn the_loss_scan_starts_at_the_latest_accepted_turn_start() {
    let mut intents = ProductIntents::default();
    let mut views = SessionViews::empty();
    assert_eq!(scan_start(&views, &intents), None, "no runtime yet");
    let runtime = plakous::model::SessionId::try_from(uuid::Uuid::new_v4()).unwrap();
    views.state.runtime_session_id = Some(runtime);
    views.cursor = FeedSeq::new(40);
    let start = |views: &SessionViews, intents: &ProductIntents| {
        scan_start(views, intents).map(|scan| (scan.session, scan.after.get()))
    };
    assert_eq!(start(&views, &intents), Some((runtime, 40)));
    submitted(&mut intents, "earlier", 3);
    submitted(&mut intents, "lost", 12);
    submitted(&mut intents, "unadmitted", 30);
    for command in ["earlier", "lost"] {
        views
            .state
            .deliveries
            .insert(command.to_owned(), Delivery::Accepted);
    }
    assert_eq!(start(&views, &intents), Some((runtime, 12)));
    views.cursor = FeedSeq::new(9);
    assert_eq!(start(&views, &intents), Some((runtime, 9)));
}

#[test]
fn the_loss_scan_keeps_turn_admission_before_later_submissions() {
    let runtime = plakous::model::SessionId::try_from(uuid::Uuid::new_v4()).unwrap();
    for root_kind in [SubmissionKind::RootStart, SubmissionKind::HostWake] {
        for later_kind in [
            SubmissionKind::RootStart,
            SubmissionKind::SendNowSteer,
            SubmissionKind::HostWake,
        ] {
            for accepted in [false, true] {
                let mut intents = ProductIntents::default();
                let mut views = SessionViews::empty();
                views.state.runtime_session_id = Some(runtime);
                views.cursor = FeedSeq::new(40);
                submitted(&mut intents, "root", 12);
                intents.submissions.last_mut().unwrap().kind = root_kind;
                views
                    .state
                    .deliveries
                    .insert("root".into(), Delivery::Accepted);
                submitted(&mut intents, "later", 30);
                intents.submissions.last_mut().unwrap().kind = later_kind;
                if accepted {
                    views
                        .state
                        .deliveries
                        .insert("later".into(), Delivery::Accepted);
                }
                let expected = if accepted && later_kind != SubmissionKind::SendNowSteer {
                    30
                } else {
                    12
                };
                assert_eq!(
                    scan_start(&views, &intents).unwrap().after.get(),
                    expected,
                    "root={root_kind:?}, later={later_kind:?}, accepted={accepted}"
                );
            }
        }
    }
}

#[test]
fn the_handoff_fold_keeps_the_root_across_steering_and_control_receipts() {
    for kind in [
        FeedCommandKind::Steer,
        FeedCommandKind::Cancel,
        FeedCommandKind::EffectDecision,
        FeedCommandKind::Shutdown,
    ] {
        let mut running = None;
        fold_handed_off_turn(&mut running, 11, &scan_accepted(1));
        fold_handed_off_turn(
            &mut running,
            21,
            &FeedFact::CommandAccepted {
                command_id: plakous::agent::CommandId::new("later-control").unwrap(),
                kind,
                turn_id: Some(scan_turn(1)),
                sequence: 2,
                operation_id: None,
            },
        );
        fold_handed_off_turn(
            &mut running,
            22,
            &FeedFact::TurnTerminal {
                turn_id: scan_turn(1),
                command_id: plakous::agent::CommandId::new("command-1").unwrap(),
                outcome: FeedTurnOutcome::Cancelled {
                    cause: FeedCancelCause::Requested,
                },
                closed_exchange: false,
                steering_settled_unapplied: false,
                usage: plakous::agent::TurnUsage::default(),
            },
        );
        assert_eq!(
            running.map(|turn| (turn.turn_id, turn.applied_seq)),
            Some((scan_turn(1).to_string(), 11)),
            "{kind:?}"
        );
    }
}

/// The scan keeps only the last root terminal with no root turn admitted
/// after it, and only an abandonment with cause `process_lost` is a loss: a
/// later accepted root command with no terminal yet clears a lost turn, and
/// a turn a live owner fenced (recovery-required, or abandoned as
/// interrupted) is no loss.
#[test]
fn the_scan_keeps_only_a_process_lost_terminal_with_nothing_admitted_after() {
    use plakous::agent::{AbandonedTurnClaim, AbandonmentCause, CommandId};
    use plakous::sessions::feed::FeedCancelCause;
    let turn = |n: u128| -> plakous::model::TurnId {
        format!("00000000-0000-4000-8000-{n:012x}").parse().unwrap()
    };
    let accepted = |n: u128| FeedFact::CommandAccepted {
        command_id: CommandId::new(format!("command-{n}")).unwrap(),
        kind: FeedCommandKind::Root {
            origin: plakous::model::TurnOrigin::UserPrompt,
        },
        turn_id: Some(turn(n)),
        sequence: 1,
        operation_id: None,
    };
    let terminal = |n: u128, outcome: FeedTurnOutcome| FeedFact::TurnTerminal {
        turn_id: turn(n),
        command_id: CommandId::new(format!("command-{n}")).unwrap(),
        outcome,
        closed_exchange: false,
        steering_settled_unapplied: false,
        usage: plakous::agent::TurnUsage::default(),
    };
    let abandoned = |cause| FeedTurnOutcome::Cancelled {
        cause: FeedCancelCause::Abandoned {
            claim: AbandonedTurnClaim::ModelRequest,
            cause,
            model_round_completed: false,
            tool_round_completed: false,
        },
    };
    let scan = |facts: &[FeedFact]| {
        let mut lost = None;
        for (seq, fact) in facts.iter().enumerate() {
            fold_lost_turn(&mut lost, u64::try_from(seq).unwrap() + 1, fact);
        }
        lost.map(|lost: LostRootTurn| (lost.turn_id, lost.applied_seq))
    };
    let lost = [
        accepted(1),
        terminal(1, abandoned(AbandonmentCause::ProcessLost)),
    ];
    assert_eq!(scan(&lost), Some((turn(1).to_string(), 2)));
    let mut later = lost.to_vec();
    later.push(accepted(2));
    assert_eq!(scan(&later), None, "a later admitted turn clears the loss");
    assert_eq!(
        scan(&[accepted(1), terminal(1, FeedTurnOutcome::RecoveryRequired)]),
        None,
        "a fenced turn is no loss"
    );
    assert_eq!(
        scan(&[
            accepted(1),
            terminal(1, abandoned(AbandonmentCause::Interrupted))
        ]),
        None,
        "a fenced turn's closure is no loss"
    );
}

fn scan_turn(n: u128) -> plakous::model::TurnId {
    format!("00000000-0000-4000-8000-{n:012x}").parse().unwrap()
}

fn scan_accepted(n: u128) -> FeedFact {
    FeedFact::CommandAccepted {
        command_id: plakous::agent::CommandId::new(format!("command-{n}")).unwrap(),
        kind: FeedCommandKind::Root {
            origin: plakous::model::TurnOrigin::UserPrompt,
        },
        turn_id: Some(scan_turn(n)),
        sequence: 1,
        operation_id: None,
    }
}

fn scan_process_lost(n: u128) -> FeedFact {
    FeedFact::TurnTerminal {
        turn_id: scan_turn(n),
        command_id: plakous::agent::CommandId::new(format!("command-{n}")).unwrap(),
        outcome: FeedTurnOutcome::Cancelled {
            cause: FeedCancelCause::Abandoned {
                claim: plakous::agent::AbandonedTurnClaim::ModelRequest,
                cause: plakous::agent::AbandonmentCause::ProcessLost,
                model_round_completed: false,
                tool_round_completed: false,
            },
        },
        closed_exchange: false,
        steering_settled_unapplied: false,
        usage: plakous::agent::TurnUsage::default(),
    }
}

/// A scan start below the feed's retention restarts at the retained start:
/// a lost turn whose terminal is still retained is found there.
#[test]
fn a_scan_below_retention_restarts_and_finds_a_retained_terminal() {
    let mut scanner = LossScanner::new(FeedSeq::new(5));
    // A candidate read before the retention answer is not kept.
    scanner.record(FeedSeq::new(6), &[scan_accepted(9), scan_process_lost(9)]);
    assert!(scanner.behind_retention(10), "the first answer restarts");
    assert_eq!(scanner.cursor, FeedSeq::new(9), "at the retained start");
    assert_eq!(scanner.lost, None);
    scanner.record(FeedSeq::new(10), &[scan_accepted(1)]);
    scanner.record(FeedSeq::new(11), &[scan_process_lost(1)]);
    assert_eq!(
        scanner.lost.map(|lost| (lost.turn_id, lost.applied_seq)),
        Some((scan_turn(1).to_string(), 11))
    );
}

/// A lost turn whose terminal is itself behind retention is not found, so
/// it is not held; a second retention answer ends the scan.
#[test]
fn a_terminal_behind_retention_is_no_loss() {
    let mut scanner = LossScanner::new(FeedSeq::new(5));
    assert!(scanner.behind_retention(20));
    scanner.record(FeedSeq::new(20), &[]);
    assert_eq!(scanner.lost, None, "the lost turn's terminal was pruned");
    assert!(!scanner.behind_retention(30), "the scan restarts once");
}
