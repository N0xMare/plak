use super::*;

#[test]
fn projection_store_wrappers_forward_terminal_usage() {
    use crate::sessions::projector::ProjectedTurnUsage;
    let store = Arc::new(MemoryStore::new());
    let wrapper = crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        crate::runtime::test_support::SlowProjection {
            delay: Duration::ZERO,
            refuse: false,
        },
    );
    let terminal = ProjectedTurnUsage {
        session_id: plakous::model::SessionId::try_from(uuid::Uuid::new_v4()).unwrap(),
        branch_id: plakous::model::BranchId::try_from(uuid::Uuid::new_v4()).unwrap(),
        turn_id: plakous::model::TurnId::try_from(uuid::Uuid::new_v4()).unwrap(),
        committed_at_ms: 0,
        usage: plakous::agent::TurnUsage::default(),
    };
    wrapper.emit_projected_turn_usages(&[terminal]);
    assert_eq!(store.projected_usage_emissions(), vec![terminal]);
}

#[test]
fn behind_retention_emits_an_unapplied_terminal_in_the_retained_window() {
    retained_window_usage(false);
}

#[test]
fn retained_history_replay_does_not_duplicate_the_rebuilt_prose() {
    retained_window_usage(true);
}

fn retained_window_usage(retain_history: bool) {
    use plakous::sessions::feed::FeedFact;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let database = config.product_home.join("runtime.sqlite3");
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["retained usage"]);
    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();
    let records = feed_records(&sessions, locator.session_id());
    let terminal = records
        .iter()
        .find(|record| {
            record
                .facts()
                .iter()
                .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
        })
        .unwrap();
    let retained = if retain_history {
        2
    } else {
        i64::try_from(terminal.header().seq.get()).unwrap()
    };
    assert!(retained > 1);
    let mut connection = rusqlite::Connection::open(database).unwrap();
    let tx = connection.transaction().unwrap();
    tx.execute("DELETE FROM session_feed WHERE seq < ?1", [retained])
        .unwrap();
    tx.execute("UPDATE sessions SET feed_retained_from=?1,feed_bytes=(SELECT COALESCE(SUM(length(record)),0) FROM session_feed)", [retained]).unwrap();
    tx.commit().unwrap();
    store.delete_views(&id);
    let before = store.projected_usage_emissions();
    let views = caught_up(root.path(), &store, &id);
    let after = store.projected_usage_emissions();
    assert_eq!(
        after.len(),
        before.len() + 1,
        "retained terminals must use the normal Records path"
    );
    assert_eq!(after.last(), before.last());
    assert_eq!(views.cursor, records.last().unwrap().header().seq);
    assert_eq!(
        views
            .rows
            .values()
            .filter(
                |row| matches!(row, TranscriptRow::User { text, .. } if text == "retained usage")
            )
            .count(),
        1,
        "replayed retained history is already in the head's canonical tail"
    );
    caught_up(root.path(), &store, &id);
    assert_eq!(store.projected_usage_emissions(), after);
}

#[test]
fn every_projected_terminal_is_presented_once_before_idle() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("usage root".to_owned()));
    let mut observed = Vec::new();
    let mut started = false;
    loop {
        let event = events.recv_timeout(Duration::from_secs(30)).unwrap();
        started |= matches!(event, HostEvent::SetBusy(true));
        let idle = started && matches!(event, HostEvent::SetBusy(false));
        observed.push(event);
        if idle {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let events = observed;
    let terminal: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, HostEvent::TurnUsage { terminal: true, .. }))
        .collect();
    assert_eq!(terminal.len(), 1);
    let done = events
        .iter()
        .position(|event| matches!(event, HostEvent::AssistantDone))
        .unwrap();
    let idle = events
        .iter()
        .rposition(|event| matches!(event, HostEvent::SetBusy(false)))
        .unwrap();
    assert!(done < terminal[0].0 && terminal[0].0 < idle);
}

#[test]
fn reopening_emits_a_new_abandoned_terminal_but_not_an_applied_one() {
    non_live_recovery_terminal(false, false);
}

#[test]
fn an_interrupted_terminal_is_presented_by_after_ack_once() {
    non_live_recovery_terminal(true, false);
}

#[test]
fn a_duplicate_command_pass_presents_the_interrupted_terminal_once() {
    non_live_recovery_terminal(true, true);
}

fn non_live_recovery_terminal(interrupted: bool, duplicate: bool) {
    use plakous::agent::CommandId;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
        OpenSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous_offline_crash_harness::protocol::CrashBoundary;
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding_with_cursor};
    use std::sync::atomic::AtomicU64;

    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let script: Arc<plakous_offline_crash_harness::script::ScriptPlan> = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "stall_ignores_cancellation": !interrupted,
            "steps": [{"stall_ms": 30_000, "output": {"text": "unobserved"}}]
        }))
        .unwrap(),
    );
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("usage-recovery.jsonl"),
    )
    .unwrap();
    let cursor = Arc::new(AtomicU64::new(1));
    let binding = || {
        scripted_runtime_binding_with_cursor(
            &config.cwd,
            controller.clone(),
            script.clone(),
            cursor.clone(),
        )
        .unwrap()
    };
    let sessions =
        AgentSessions::new(SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap())
            .unwrap();
    let store = Arc::new(MemoryStore::new());
    let process = Arc::new(std::sync::Mutex::new(
        super::super::presentation::ProcessUsage::default(),
    ));
    let records: Arc<dyn SessionRecordStore> =
        Arc::new(super::super::presentation::PresentingStore {
            inner: store.clone(),
            usage: process.clone(),
        });
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("usage:recover:create").unwrap(),
                    binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("created")
            };
            let mut feed = sessions.subscribe_feed(session.locator().session_id()).await.unwrap();
            let turn = session.agent().start(CommandId::new("usage:recover:first").unwrap(), "lost root")
                .await.unwrap().into_turn().unwrap();
            let turn_id = turn.control().turn_id();
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let page = sessions.feed_page(plakous::sessions::feed::FeedPageRequest::after(
                        plakous::sessions::feed::FeedCursor::start(session.locator().session_id()),
                    )).await.unwrap();
                    if let plakous::sessions::feed::FeedPage::Records { records, .. } = page
                        && records.iter().flat_map(plakous::sessions::feed::FeedRecord::facts).any(|fact| matches!(fact,
                            plakous::sessions::feed::FeedFact::OperationAdvanced {
                                kind: plakous::sessions::feed::FeedOperationKind::ProviderAttempt { .. },
                                state: plakous::sessions::feed::FeedOperationState::Dispatched, ..
                            }
                        )) {
                        break;
                    }
                    feed.changed().await.unwrap();
                }
            }).await.unwrap();
            let mut duplicate_submission = None;
            let locator = if interrupted {
                turn.control().request_cancel().await.unwrap();
                assert!(matches!(turn.result().await.unwrap().outcome(), plakous::agent::TurnResultOutcome::Cancelled(_)));
                if duplicate {
                    duplicate_submission = Some(session.agent().start(CommandId::new("usage:recover:first").unwrap(), "lost root").await.unwrap());
                }
                session.release().await.unwrap()
            } else {
                session.abandon_and_release(&[], Duration::from_secs(1)).await.unwrap()
            };
            process.lock().unwrap().target = Some(locator.session_id());
            let mut record = crate::sessions::SessionRecord::new(config.cwd.clone());
            record.plakous_locator = Some(locator);
            store.save(&mut record).unwrap();
            if interrupted {
                for _ in 0..3 {
                    if let Some(submission) = &duplicate_submission {
                        assert!(!crate::runtime::turn_drive::duplicate_consumed(&sessions, &records, &record.id, submission, locator).await.unwrap());
                    } else {
                        super::super::after_ack_pass(&sessions, &records, &record.id, locator).await.unwrap();
                    }
                }
                assert_eq!(store.projected_usage_emissions().len(), 1);
                assert_recovery_presentation(&config.home, &record.id, &process, turn_id);
                return;
            }
            // Open's step 0 catches up only to the unfinished head.
            super::super::project_before_open(&sessions, &records, &record)
                .await
                .unwrap();
            let before_open = store.projected_usage_emissions();
            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("usage:recover:open").unwrap(),
                    locator,
                    binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("opened")
            };
            assert!(session.agent().abandoned_turn().is_some());
            // Step 2 applies the closure the fresh open committed.
            project_to_tail(&sessions, &records, &record.id, session.locator())
                .await
                .unwrap();
            let after_open = store.projected_usage_emissions();
            assert_eq!(after_open.len(), before_open.len() + 1);
            assert_eq!(after_open.last().unwrap().turn_id, turn_id);
            let locator = session.release().await.unwrap();
            super::super::project_before_open(&sessions, &records, &record)
                .await
                .unwrap();
            project_to_tail(&sessions, &records, &record.id, locator)
                .await
                .unwrap();
            assert_eq!(store.projected_usage_emissions(), after_open);
            assert_recovery_presentation(&config.home, &record.id, &process, turn_id);
        })
        .unwrap();
}

fn assert_recovery_presentation(
    home: &crate::session_home::SessionHome,
    id: &str,
    process: &std::sync::Mutex<super::super::presentation::ProcessUsage>,
    turn_id: plakous::model::TurnId,
) {
    let pending = std::mem::take(&mut process.lock().unwrap().pending);
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].turn_id, turn_id);
    let ledger = crate::runtime::SessionLedger::default();
    let expected = crate::runtime::SessionLedger::default();
    expected.record_turn_usage(pending[0].usage);
    let log = crate::session_log::SessionLog::open(home, id).unwrap();
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    crate::runtime::usage::emit_projected_turn_usage(
        &crate::runtime::Sender::over(tx),
        id,
        turn_id,
        pending[0].usage,
        Some(&log),
        &ledger,
    );
    assert_eq!(ledger.project(), expected.project());
    assert_eq!(
        rx.try_iter()
            .filter(|event| matches!(event, HostEvent::TurnUsage { terminal: true, .. }))
            .count(),
        1
    );
    let rows = crate::session_log::read_tail(home, id, 10).unwrap();
    assert_eq!(rows.len(), 1);
    let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(row["kind"], "turn.usage");
    assert_eq!(row["turn_id"], turn_id.to_string());
}

#[test]
fn terminal_usage_emits_only_for_the_winning_cursor_and_rebuilds_identically() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["usage root"]);
    caught_up(root.path(), &store, &id);
    let emissions = store.projected_usage_emissions();
    assert_eq!(emissions.len(), 1);
    let (config, _) = fresh_injected_attached_host(root.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let records: Arc<dyn SessionRecordStore> = store.clone();
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();
    // An applier holding the old cursor loses its CAS, reloads and emits nothing.
    executor
        .block_on(super::super::project_to_tail_cached(
            &sessions,
            &records,
            &id,
            locator,
            Some(SessionViews::empty()),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(store.projected_usage_emissions(), emissions);
    run_prompts(reopened(root.path(), &store, &id), &[]);
    assert_eq!(store.projected_usage_emissions(), emissions);
    let live = caught_up(root.path(), &store, &id);
    store.delete_views(&id);
    let rebuilt = caught_up(root.path(), &store, &id);
    assert_eq!(rebuilt, live);
    let rebuilt_emissions = store.projected_usage_emissions();
    assert_eq!(rebuilt_emissions.len(), 2);
    assert_eq!(rebuilt_emissions[0], rebuilt_emissions[1]);
}
