use super::super::presentation::{PresentingStore, ProcessUsage};
use super::*;
use crate::runtime::{Sender, SessionLedger};
use crate::session_home::SessionHome;
use crate::session_log::SessionLog;
use std::sync::Mutex;

#[test]
fn resumed_fork_and_rebind_do_not_present_inherited_terminal_usage() {
    use crate::interrupt::{InterruptWatch, InterruptWatchState};
    use crate::runtime::test_support::host_event_sink;
    use std::cell::RefCell;

    for rebind in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        let source = config.session.id.clone();
        run_prompts(config, &["previous process"]);
        let before = store.projected_usage_emissions();
        assert_eq!(before.len(), 1);
        let mut config = reopened(root.path(), &store, &source);
        config.ready_catalog =
            crate::runtime::test_support::code_mode_source_with_a_prefix(root.path()).ready_catalog;
        if rebind {
            let workspace = config.cwd.clone();
            let controller =
                plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                    plakous_offline_crash_harness::protocol::CrashBoundary::None,
                    root.path().join("rebind-usage-evidence.jsonl"),
                )
                .unwrap();
            let injection = Arc::get_mut(config.test_injection.as_mut().unwrap()).unwrap();
            injection.rebind_target = plakous_offline_crash_harness::runtime_binding_for_account(
                &workspace,
                controller.clone(),
                b"successor-account",
            )
            .unwrap()
            .target_binding();
            let primary = injection.runtime.clone();
            injection.runtime = Arc::new(move |role| {
                if role == crate::runtime::TestRuntimeRole::Rebind {
                    plakous_offline_crash_harness::runtime_binding_for_account(
                        &workspace,
                        controller.clone(),
                        b"successor-account",
                    )
                    .map_err(|error| error.to_string())
                } else {
                    primary(role)
                }
            });
        }
        let locator = config.session.plakous_locator.unwrap();
        let process = Arc::new(Mutex::new(ProcessUsage::default()));
        process.lock().unwrap().target = Some(locator.session_id());
        config.store = Arc::new(PresentingStore {
            inner: store.clone(),
            usage: process.clone(),
        });
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let mut opened = executor
            .block_on(crate::runtime::startup::open_session_for_host(
                &config,
                &config.session,
                locator,
                &mut Vec::new(),
                crate::runtime::TestRuntimeRole::Primary,
            ))
            .unwrap()
            .unwrap_or_else(|_| panic!("source opens"));
        let (events, rx) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&receiver, &state);
        if rebind {
            crate::runtime::rebind::handle_rebind(
                &executor,
                watch,
                &mut config,
                &mut opened,
                &mut Vec::new(),
                &events,
                "codex",
                "offline-loopback-v2",
                Some("medium"),
                None,
                None,
                None,
                &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
            )
            .unwrap();
        } else {
            crate::runtime::fork::handle_fork(
                &executor,
                watch,
                &mut config,
                &mut opened,
                &mut Vec::new(),
                &events,
                None,
            )
            .unwrap();
        }
        assert_ne!(config.session.id, source);
        let sessions = config.state_roots.runtime_sessions().unwrap();
        executor.block_on(opened.release()).unwrap().unwrap();
        // A fresh process has no local turn-id suppression to hide a replay.
        let home = SessionHome::new(root.path().join("successor-log"));
        let log = SessionLog::open(&home, &config.session.id).unwrap();
        let ledger = SessionLedger::default();
        for terminal in std::mem::take(&mut process.lock().unwrap().pending) {
            crate::runtime::usage::emit_projected_turn_usage(
                &events,
                &config.session.id,
                terminal.turn_id,
                terminal.usage,
                Some(&log),
                &ledger,
            );
        }
        assert_eq!(store.projected_usage_emissions(), before, "rebind={rebind}");
        assert_eq!(ledger.project(), SessionLedger::default().project());
        assert!(
            !rx.try_iter()
                .any(|event| matches!(event, HostEvent::TurnUsage { .. }))
        );
        assert!(
            crate::session_log::read_tail(&home, &config.session.id, 10)
                .unwrap()
                .is_empty()
        );

        // Opening a successor with no projected views must obey the same law.
        store.delete_views(&config.session.id);
        executor
            .block_on(super::super::project_before_open(
                &sessions,
                &config.store,
                &config.session,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(store.projected_usage_emissions(), before);
        assert!(process.lock().unwrap().pending.is_empty());
    }
}

#[test]
fn non_live_passes_present_usage_once_to_ledger_events_and_log() {
    // These are the common entries used by admission/duplicate, open,
    // compact/clear/fork/rebind and lease-free product commands.
    for entry in ["after_ack", "before_open", "workflow", "product_command"] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        let id = config.session.id.clone();
        run_prompts(config, &["non-live usage"]);
        let terminal = store.projected_usage_emissions()[0];
        let record = store.load(&id).unwrap();
        let locator = record.plakous_locator.unwrap();
        let (fresh, _) = fresh_injected_attached_host(root.path());
        let sessions = fresh.state_roots.runtime_sessions().unwrap();
        let process = Arc::new(Mutex::new(ProcessUsage::default()));
        process.lock().unwrap().target = Some(locator.session_id());
        let presenting: Arc<dyn SessionRecordStore> = Arc::new(PresentingStore {
            inner: store.clone(),
            usage: process.clone(),
        });
        let home = SessionHome::new(root.path().join("presentation-log"));
        let log = SessionLog::open(&home, &id).unwrap();
        let ledger = SessionLedger::default();
        let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        store.delete_views(&id);
        for replay in 0..3 {
            if replay == 2 {
                // Reprojection may upsert telemetry, but not replay presentation.
                store.delete_views(&id);
            }
            executor
                .block_on(async {
                    match entry {
                        "after_ack" => {
                            super::super::queue_after_ack(&sessions, &presenting, &id, locator)
                                .await
                                .unwrap();
                        }
                        "before_open" => {
                            super::super::project_before_open(&sessions, &presenting, &record)
                                .await
                                .unwrap();
                        }
                        "product_command" => {
                            super::super::project_session(&sessions, &presenting, &record)
                                .await
                                .unwrap();
                        }
                        _ => {
                            project_to_tail(&sessions, &presenting, &id, locator)
                                .await
                                .unwrap();
                        }
                    }
                })
                .unwrap();
            let pending = std::mem::take(&mut process.lock().unwrap().pending);
            assert_eq!(
                pending.len(),
                usize::from(replay == 0),
                "{entry}, replay {replay}"
            );
            for terminal in pending {
                crate::runtime::usage::emit_projected_turn_usage(
                    &events,
                    &id,
                    terminal.turn_id,
                    terminal.usage,
                    Some(&log),
                    &ledger,
                );
            }
        }
        let expected = SessionLedger::default();
        expected.record_turn_usage(terminal.usage);
        assert_eq!(ledger.project(), expected.project(), "{entry}");
        assert_eq!(
            rx.try_iter()
                .filter(|event| matches!(event, HostEvent::TurnUsage { terminal: true, .. }))
                .count(),
            1,
            "{entry}"
        );
        let rows = crate::session_log::read_tail(&home, &id, 10).unwrap();
        assert_eq!(rows.len(), 1, "{entry}");
        let row: serde_json::Value = serde_json::from_str(&rows[0]).unwrap();
        assert_eq!(row["kind"], "turn.usage");
        assert_eq!(row["turn_id"], terminal.turn_id.to_string());
    }
}

#[test]
fn a_late_old_session_pass_does_not_feed_the_new_session() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["old session"]);
    let record = store.load(&id).unwrap();
    let process = Arc::new(Mutex::new(ProcessUsage::default()));
    process.lock().unwrap().target =
        Some(plakous::model::SessionId::try_from(uuid::Uuid::new_v4()).unwrap());
    let presenting: Arc<dyn SessionRecordStore> = Arc::new(PresentingStore {
        inner: store.clone(),
        usage: process.clone(),
    });
    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    store.delete_views(&id);
    let before = store.projected_usage_emissions().len();
    executor
        .block_on(project_to_tail(
            &sessions,
            &presenting,
            &id,
            record.plakous_locator.unwrap(),
        ))
        .unwrap()
        .unwrap();
    assert_eq!(
        store.projected_usage_emissions().len(),
        before + 1,
        "telemetry still receives the old session's terminal"
    );
    assert!(
        process.lock().unwrap().pending.is_empty(),
        "the new attachment receives no old-session usage"
    );
}
