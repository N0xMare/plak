#![allow(
    clippy::too_many_lines,
    reason = "Terminal publication scenarios retain paid-compaction and cached-result fixtures to verify exact notices and cursor progress."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use plakous::model::FinishReason;

#[test]
fn capacity_commit_failure_requires_recovery_and_pauses_live_drain() {
    use super::SessionQueueDrainState;
    use crate::runtime::host_loop::NodeActor;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
    use crate::runtime::{Sender, TurnHostOutcome};
    use crate::surface_protocol::HostEvent;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    for presentation in [
        super::TerminalErrorPresentation::Events,
        super::TerminalErrorPresentation::Return,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.config.compaction_enabled = true;
        host.config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        crate::sessions::queue::enqueue(&mut host.config.session.pending_queue, "queued follow-up")
            .unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        let source = host.config.session.plakous_locator;
        let submission = host
            .executor
            .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
            .unwrap()
            .unwrap();
        host.executor
            .block_on(submission.result())
            .unwrap()
            .unwrap();
        let head = host.opened.locator();
        assert_ne!(source, Some(head));
        let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            host.config.store.clone(),
            SaveFailure {
                fail: AtomicBool::new(true),
            },
        ));
        host.config.store = store.clone();
        assert!(!host.block(async |host| {
            host.publish_terminal(
                Ok(TurnHostOutcome::ContextCapacity {
                    source_checkpoint: head.checkpoint_id(),
                    recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
                    error: "synthetic overflow".into(),
                }),
                super::Instant::now(),
                presentation,
            )
            .await
        }));
        // Storage returning must not rearm the drain before result replay.
        store.faults.fail.store(false, Ordering::SeqCst);
        assert!(host.recovery.blocked());
        assert_eq!(
            host.config.session.queue_drain_state,
            SessionQueueDrainState::RestoredPaused
        );
        assert_eq!(host.config.session.pending_queue.len(), 1);

        assert_eq!(host.config.session.plakous_locator, source);
        assert_eq!(
            store
                .inner
                .load(&host.config.session.id)
                .unwrap()
                .plakous_locator,
            source
        );
        assert!(events.try_iter().any(|event| matches!(
            event,
            HostEvent::Row(row)
                if row.text().is_some_and(|text| text.contains("queue drain remains fail-closed"))
        )));
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn capacity_auto_compact_sees_the_terminal_head_binding() {
    use crate::runtime::host_loop::NodeActor;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::runtime::{Sender, TurnHostOutcome};
    use crate::surface_protocol::HostEvent;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let prepared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = std::sync::Arc::clone(&prepared);
    let store = std::sync::Arc::clone(&config.store);
    let id = config.session.id.clone();
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        std::sync::Arc::new(move |workflow, boundary| {
            if workflow == super::HostWorkflow::Compact && boundary == super::HostBoundary::Prepared
            {
                *captured.lock().unwrap() = store.load_intents(&id).unwrap().workflow_requests;
            }
        });
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.config.compaction_enabled = true;
    host.config.store.save(&mut host.config.session).unwrap();
    let source = host.config.session.plakous_locator;
    let submission = host
        .executor
        .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
        .unwrap()
        .unwrap();
    host.executor
        .block_on(submission.result())
        .unwrap()
        .unwrap();
    let head = host.opened.locator();
    assert_ne!(source, Some(head));
    // As run_prompt reaches publish_terminal after a Compactable overflow
    // whose source is the exact attached head.
    host.block(async |host| {
        host.publish_terminal(
            Ok(TurnHostOutcome::ContextCapacity {
                source_checkpoint: head.checkpoint_id(),
                recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
                error: "synthetic overflow".into(),
            }),
            super::Instant::now(),
            super::TerminalErrorPresentation::Events,
        )
        .await
    });
    let notes = events
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !notes
            .iter()
            .any(|note| note.contains("compact source does not match")),
        "capacity auto-compaction refused its own exact head: {notes:?}"
    );
    let requests = prepared.lock().unwrap();
    assert!(
        requests.iter().any(|workflow| matches!(
            workflow.kind,
            crate::sessions::SessionWorkflowKind::Compact(
                crate::sessions::ProductCompactReason::CapacityRecovery
            )
        )),
        "auto-compaction never prepared its capacity-recovery workflow"
    );
    assert_eq!(requests[0].source_locator, head);

    let views = host
        .config
        .store
        .load_views(&host.config.session.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        super::super::super::projection::head_locator(&views),
        Some(host.opened.locator())
    );
    host.block(async |host| host.release().await).unwrap();
}

/// A capacity terminal after a fold that failed after dispatch pays for no
/// capacity-recovery compaction: the segment-limit refusal whose turn-fence
/// fold failed, and the refusal of the next prompt, start none, and the
/// operator gets the guidance instead of "compacting once".
#[test]
fn capacity_auto_compact_waits_after_a_failed_paid_fold() {
    use crate::runtime::host_loop::NodeActor;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::runtime::{Sender, TurnHostOutcome};
    use crate::surface_protocol::HostEvent;
    use plakous::model::{
        Actor, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind,
        ModelId, Provenance, TextBlock,
    };
    use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("failed-fold.jsonl"),
    )
    .unwrap();
    let script: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": 1, "scheduled_compaction": true, "fold": "fail"
        }))
        .unwrap(),
    );
    crate::runtime::test_kit::scripted_mut(&mut config).runtime = Arc::new(move |_| {
        plakous_offline_crash_harness::scripted_runtime_binding(
            &workspace,
            controller.clone(),
            Arc::clone(&script),
        )
        .map_err(|error| error.to_string())
    });
    let host_compacts = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&host_compacts);
    crate::runtime::test_kit::scripted_mut(&mut config).observe =
        Arc::new(move |workflow, boundary| {
            if workflow == super::HostWorkflow::Compact && boundary == super::HostBoundary::Prepared
            {
                counted.fetch_add(1, Ordering::SeqCst);
            }
        });
    // At the refusal limit of 4,032: the turn fence pays for one segment fold,
    // which fails, and the turn is refused before its prompt appends.
    let segments = (0..4_032_u128)
        .map(|index| {
            let (actor, provenance, kind) = if index % 2 == 0 {
                (
                    Actor::User,
                    Provenance::UserInput,
                    HistorySegmentKind::RecentIntent,
                )
            } else {
                (
                    Actor::Assistant,
                    Provenance::ModelOutput,
                    HistorySegmentKind::Conversation,
                )
            };
            HistorySegment::new(
                kind,
                vec![
                    ConversationItem::new(
                        EntryId::try_from(uuid::Uuid::from_u128(0x5e6_0000 + index)).unwrap(),
                        actor,
                        vec![ContentBlock::Text(
                            TextBlock::new(format!("small turn {index}"), None).unwrap(),
                        )],
                        provenance,
                    )
                    .unwrap(),
                ],
            )
            .unwrap()
        })
        .collect();
    config.seed_history = Some(plakous::sessions::PortableSessionHistory::new(
        segments,
        "synthetic-no-replay".into(),
        ModelId::new("offline-loopback-v1").unwrap(),
    ));
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.config.compaction_enabled = true;
    host.config.store.save(&mut host.config.session).unwrap();
    for attempt in 0..2 {
        let submission = host
            .executor
            .block_on(
                host.opened
                    .agent()
                    .unwrap()
                    .prompt(format!("refused {attempt}")),
            )
            .unwrap()
            .unwrap();
        let result = host
            .executor
            .block_on(submission.result())
            .unwrap()
            .unwrap();
        let outcome = crate::runtime::turn::terminal_turn_outcome(&result);
        assert!(
            matches!(
                outcome,
                TurnHostOutcome::ContextCapacity {
                    recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
                    ..
                }
            ),
            "attempt {attempt}: {:?}",
            result.outcome()
        );
        assert!(host.opened.paid_fold_failed_since_last_round());
        host.block(async |host| {
            host.publish_terminal(
                Ok(outcome),
                super::Instant::now(),
                super::TerminalErrorPresentation::Events,
            )
            .await
        });
    }
    assert_eq!(host_compacts.load(Ordering::SeqCst), 0);
    let notes = events
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        !notes.iter().any(|note| note.contains("compacting once")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .filter(|note| note.contains("-segment limit"))
            .count()
            >= 2,
        "{notes:?}"
    );
    // The guidance is the segment limit's, not token or tool-growth advice.
    assert_eq!(
        notes
            .iter()
            .filter(|note| note.as_str() == crate::runtime::CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE)
            .count(),
        2,
        "{notes:?}"
    );
    assert!(
        !notes
            .iter()
            .any(|note| note.as_str() == crate::runtime::CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE),
        "{notes:?}"
    );
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn completed_terminal_projector_publishes_once_and_tracks_head() {
    use crate::runtime::host_loop::NodeActor;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::runtime::{Sender, TurnHostOutcome};
    use crate::sessions::projector::{Submission, SubmissionKind};
    use plakous::agent::{CommandId, TurnResultOutcome};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let records = std::sync::Arc::new(crate::session_store::MemoryStore::new());
    config.store = records.clone();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    let store = host.config.store.clone();
    let id = host.config.session.id.clone();
    let item =
        crate::sessions::queue::enqueue(&mut host.config.session.pending_queue, "root").unwrap();
    store.save(&mut host.config.session).unwrap();
    let command = format!("plak.queue.{}.{}", item.id, item.revision);
    store
        .save_submission(
            &mut host.config.session,
            &Submission {
                command_id: command.clone(),
                kind: SubmissionKind::RootStart,
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text.clone(),
                submitted_at_ms: item.submitted_at_ms,
            },
        )
        .unwrap();
    let submission = host
        .executor
        .block_on(
            host.opened
                .agent()
                .unwrap()
                .start(CommandId::new(&command).unwrap(), &item.text),
        )
        .unwrap()
        .unwrap();
    let handle = submission.into_result().unwrap();
    let result = host.executor.block_on(handle.result()).unwrap().unwrap();
    let TurnResultOutcome::Completed(response) = result.outcome() else {
        panic!("completed turn required")
    };
    let assistant = response.output_text();
    let source = host.config.session.plakous_locator;
    let head = host.opened.locator();
    host.block(async |host| {
        host.publish_terminal(
            Ok(TurnHostOutcome::Completed {
                assistant: assistant.clone(),
                finish_reason: FinishReason::Stop,
                finish_detail: None,
                wire_output_limit: None,
            }),
            super::Instant::now(),
            super::TerminalErrorPresentation::Events,
        )
        .await
    });
    let record_reads = records.record_read_count();
    host.block(async |host| host.publish_projected_transcript(true).await);
    assert_eq!(
        records.record_read_count(),
        record_reads,
        "terminal publication must not hydrate a full session record"
    );
    let stored = store.load(&id).unwrap();
    assert_eq!(host.config.session.pending_queue, stored.pending_queue);
    assert_eq!(host.config.session.queue_commands, stored.queue_commands);
    assert!(
        host.config.session.pending_queue.is_empty(),
        "consumed queue item survived publication"
    );
    assert_ne!(source, Some(head));
    assert_eq!(stored.plakous_locator, Some(head));
    host.block(async |host| host.release().await).unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    {
        let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.block(async |host| host.release().await).unwrap();
    }
    let count = store
        .load(&id)
        .unwrap()
        .transcript
        .iter()
        .filter(|row| row.text() == Some(assistant.as_str()))
        .count();
    assert_eq!(count, 1, "the terminal result was published twice");
}

#[test]
fn cached_terminal_publication_matches_the_stored_projection_and_preserves_pending_notices() {
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::runtime::{Sender, host_loop::NodeActor};
    use crate::session_store::SessionRecordStore;
    use crate::sessions::projector::{Lane, RowKey, ViewWrites};
    use crate::sessions::{MAX_TRANSCRIPT_LINES, NoticeScope, TranscriptRow};
    for pending_notice in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let records = std::sync::Arc::new(crate::session_store::MemoryStore::new());
        config.store = records.clone();
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            NodeActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
        records.save(&mut host.config.session).unwrap();
        if let Some(live) = host.live.take() {
            let target = host.live_target();
            host.executor
                .block_on(live.finish(target, None))
                .unwrap()
                .unwrap();
        }
        host.block(async |host| host.project_now().await);
        let id = host.config.session.id.clone();
        let before = records.load_views(&id).unwrap().unwrap();
        let mut views = before.clone();
        views.rows.clear();
        for ordinal in 1..=MAX_TRANSCRIPT_LINES {
            views.rows.insert(
                RowKey::new(views.cursor.get(), Lane::Runtime, ordinal as u32),
                TranscriptRow::assistant(format!("bounded row {ordinal}")),
            );
        }
        views.state.truncated = true;
        views.state.counters.root_turns = 17;
        views.state.counters.tool_calls = 31;
        views.state.counters.compactions = 2;
        views.state.row_bytes = views
            .rows
            .values()
            .map(|row| plakous_host_protocol::views::presentation::row_bytes(row) as u64)
            .sum();
        records
            .commit_projection_views(&id, &ViewWrites::between(&before, &views), &views.state)
            .unwrap();
        let mut stored = records.load(&id).unwrap();
        let kept = crate::sessions::queue::enqueue(&mut stored.pending_queue, "stored queue text")
            .unwrap();
        stored.record_notice(TranscriptRow::info(
            NoticeScope::Session,
            "at the current cursor",
        ));
        records.save(&mut stored).unwrap();
        assert_eq!(
            records
                .load_intents(&id)
                .unwrap()
                .notices
                .last()
                .unwrap()
                .anchor,
            views.cursor.get()
        );
        host.config.session = records.load(&id).unwrap();
        host.block(async |host| host.project().await);
        host.commit_notice_intents();
        host.config
            .session
            .pending_queue
            .iter_mut()
            .find(|item| item.id == kept.id)
            .unwrap()
            .text = "unsaved queue edit".into();
        host.config.session.plakous_locator = None;
        host.config.session.root_turn_count = 0;
        host.config.session.tool_call_count = 0;
        host.config.session.compaction_count = 0;
        let pending = TranscriptRow::info(NoticeScope::Session, "pending unsaved notice");
        if pending_notice {
            host.config.session.record_notice(pending.clone());
        }
        host.block(async |host| host.publish_projected_transcript(true).await);
        let mut expected = records.load(&id).unwrap();
        if pending_notice {
            expected.record_notice(pending);
        }
        assert_eq!(host.config.session.transcript, expected.transcript);
        assert_eq!(
            host.config.session.root_turn_count,
            expected.root_turn_count
        );
        assert_eq!(
            host.config.session.tool_call_count,
            expected.tool_call_count
        );
        assert_eq!(
            host.config.session.compaction_count,
            expected.compaction_count
        );
        assert_eq!(
            host.config.session.plakous_locator,
            expected.plakous_locator
        );
        assert_eq!(host.config.session.queue_commands, expected.queue_commands);
        assert_eq!(host.config.session.pending_queue, expected.pending_queue);
        assert!(host.config.session.transcript.len() <= MAX_TRANSCRIPT_LINES);
        assert_eq!(
            host.config.session.transcript.first(),
            Some(&TranscriptRow::Truncated)
        );
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn terminal_head_changes_only_after_the_projector_pass() {
    use crate::agent::TerminalExchange;
    use crate::runtime::host_loop::NodeActor;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
    use crate::runtime::{Sender, TurnHostOutcome};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    for outcome in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = NodeActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        let source = host.config.session.plakous_locator;
        let submission = host
            .executor
            .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
            .unwrap()
            .unwrap();
        host.executor
            .block_on(submission.result())
            .unwrap()
            .unwrap();
        let head = host.opened.locator();
        assert_ne!(source, Some(head));
        let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            host.config.store.clone(),
            SaveFailure {
                fail: AtomicBool::new(true),
            },
        ));
        host.config.store = store.clone();
        let drive = || {
            Ok(match outcome {
                0 => TurnHostOutcome::Failed {
                    transient: None,
                    error: "synthetic failure".into(),
                    exchange: TerminalExchange::Closed,
                    remedy: None,
                },
                1 => TurnHostOutcome::Interrupted {
                    assistant: "synthetic partial".into(),
                    exchange: TerminalExchange::Closed,
                },
                _ => TurnHostOutcome::RecoveryRequired {
                    error: "synthetic recovery".into(),
                    remedy: None,
                },
            })
        };
        host.block(async |host| {
            host.publish_terminal(
                drive(),
                super::Instant::now(),
                super::TerminalErrorPresentation::Events,
            )
            .await
        });
        assert_eq!(host.config.session.plakous_locator, source);
        assert_eq!(
            store
                .inner
                .load(&host.config.session.id)
                .unwrap()
                .plakous_locator,
            source
        );
        store.faults.fail.store(false, Ordering::SeqCst);
        host.block(async |host| {
            host.publish_terminal(
                drive(),
                super::Instant::now(),
                super::TerminalErrorPresentation::Events,
            )
            .await
        });
        assert_eq!(host.config.session.plakous_locator, source);
        host.block(async |host| host.publish_projected_transcript(true).await);
        assert_eq!(host.config.session.plakous_locator, Some(head));
        host.block(async |host| host.release().await).unwrap();
    }
}
