#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::sync::Arc;
use std::time::Duration;

use plakous_host_protocol::views::transcript::TranscriptRow;

use super::project_to_tail;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::{fresh_injected_attached_host, send_attached_action};
use crate::session_store::{MemoryStore, SessionRecordStore};
use crate::sessions::projector::SessionViews;

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

#[cfg(test)]
mod sealed_reasoning;
#[cfg(test)]
mod steer_live_only;

#[cfg(test)]
mod idempotence;
#[cfg(test)]
mod non_live_usage;
#[cfg(test)]
mod one_entry;
#[cfg(test)]
mod terminal_usage;

#[cfg(test)]
mod perf_cold;

#[test]
fn an_intent_written_during_the_feed_read_is_joined_to_that_page() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["one prompt"]);
    let mut record = store.load(&id).unwrap();
    let request = crate::sessions::projector::WorkflowRequest {
        operation_id: "plak:compact:written-during-read".to_owned(),
        source_locator: record.plakous_locator.unwrap(),
        kind: crate::sessions::SessionWorkflowKind::Compact(
            crate::sessions::ProductCompactReason::Scheduled,
        ),
    };
    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let records: Arc<dyn SessionRecordStore> = store.clone();
    let (_, intents) = executor
        .block_on(super::page_with_intents(&records, &id, &[], async {
            store.submit_workflow_request(&mut record, &request)?;
            sessions
                .feed_page(plakous::sessions::feed::FeedPageRequest::after(
                    plakous::sessions::feed::FeedCursor::start(request.source_locator.session_id()),
                ))
                .await
                .map_err(|error| crate::error::ReadFailure::from(error.to_string()))
        }))
        .unwrap()
        .unwrap();
    assert_eq!(
        intents.workflow_request(&request.operation_id),
        Some(&request)
    );
}

#[test]
fn a_pruned_feed_reopen_consumes_historical_intents_without_withdrawing_clear() {
    use crate::runtime::test_support::run_attached_actions;

    for delete_views in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let script = crate::runtime::recover_tests::script(serde_json::json!([
            {"output": {"text": "historical answer"}}
        ]));
        let (mut config, _) =
            crate::runtime::recover_tests::scripted_host_with(root.path(), script.clone());
        let runtime = Arc::clone(&crate::runtime::test_kit::scripted(&config).runtime);
        let workspace = config.cwd.clone();
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("retained-compact-evidence.jsonl"),
            )
            .unwrap();
        crate::runtime::test_kit::scripted_mut(&mut config).runtime = Arc::new(move |role| {
            if role == crate::runtime::RuntimeRole::Compact {
                plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                    &workspace,
                    controller.clone(),
                    &script,
                    None,
                    None,
                    Vec::new(),
                )
                .map_err(|error| error.to_string())
            } else {
                runtime(role)
            }
        });
        let injection = config.runtime_factory.clone();
        let database = config.product_home.join("runtime.sqlite3");
        let store = Arc::new(MemoryStore::new());
        config.store = store.clone();
        let id = config.session.id.clone();
        let reopen = || {
            let mut config = reopened(root.path(), &store, &id);
            config.runtime_factory = injection.clone();
            config
        };
        run_prompts(config, &["historical root"]);
        let events = run_attached_actions(reopen(), vec![UserAction::ClearSession]);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HostEvent::ClearView))
        );
        let successor = store
            .records()
            .into_iter()
            .find(|record| record.source_product_session_id.as_deref() == Some(&id))
            .expect("clear successor");
        assert!(store.load_views(&successor.id).unwrap().is_some());
        assert_ne!(
            store.load_intents(&id).unwrap().workflow_requests,
            [] as [crate::sessions::projector::WorkflowRequest; 0]
        );
        assert_ne!(
            store.load_intents(&id).unwrap().submissions,
            [] as [crate::sessions::projector::Submission; 0]
        );
        let mut connection = rusqlite::Connection::open(database).unwrap();
        let tx = connection.transaction().unwrap();
        // Retain the runtime heads and receipts, but none of their feed facts.
        tx.execute("DELETE FROM session_feed", []).unwrap();
        tx.execute(
            "UPDATE sessions SET feed_retained_from=feed_tail+1,feed_bytes=0",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
        drop(connection);
        if delete_views {
            store.delete_views(&id);
            store.delete_views(&successor.id);
        }
        let events = run_attached_actions(
            reopen(),
            vec![
                UserAction::Compact,
                UserAction::ForkSession {
                    account: None,
                    compact: false,
                },
            ],
        );
        let notes: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(notice)) => Some(notice.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            !notes.iter().any(|text| text.contains("did not start")),
            "{notes:?}"
        );
        assert!(
            notes.iter().any(|text| text.contains("forked session")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|text| text.contains("compaction") || text.contains("compact")),
            "{notes:?}"
        );
        assert!(
            store.load(&successor.id).is_ok(),
            "a consumed clear never deletes its successor"
        );
        let views = store.load_views(&id).unwrap().unwrap();
        let intents = store.load_intents(&id).unwrap();
        assert_eq!(
            crate::sessions::projector::pending_command_ids(&views, &intents),
            [] as [String; 0]
        );
        assert_eq!(
            crate::sessions::projector::workflow_withdrawals(&views, &intents),
            [] as [(String, crate::sessions::projector::OperatorNotice); 0]
        );
        assert!(
            intents
                .notices
                .iter()
                .all(|notice| !notice.notice_id.starts_with("withdrawn-workflow:"))
        );
    }
}

/// Runs one attached host over `config` until its prompts' turns end, then
/// quits it.
pub(super) fn run_prompts(config: crate::runtime::HostConfig<TestExtension>, prompts: &[&str]) {
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    for prompt in prompts {
        send_attached_action(&actions, UserAction::Prompt((*prompt).to_owned().into()));
        loop {
            let event = events
                .recv_timeout(Duration::from_secs(30))
                .expect("a host event");
            if matches!(&event, HostEvent::TurnLifecycle(lifecycle)
                if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted)
            {
                break;
            }
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

/// The views after one more pass to the feed's tail, through a fresh view of
/// the same homes.
fn caught_up(root: &std::path::Path, store: &Arc<MemoryStore>, id: &str) -> SessionViews {
    let store: Arc<dyn SessionRecordStore> = store.clone();
    let (config, _) = fresh_injected_attached_host(root);
    let record = store.load(id).unwrap();
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(project_to_tail(
            &sessions,
            &store,
            id,
            record.plakous_locator.unwrap(),
        ))
        .unwrap()
        .unwrap()
}

/// A host over the same homes and store, opening the stored record.
fn reopened(
    root: &std::path::Path,
    store: &Arc<MemoryStore>,
    id: &str,
) -> crate::runtime::HostConfig<TestExtension> {
    let (mut config, _) = fresh_injected_attached_host(root);
    config.store = store.clone();
    config.session = store.load(id).unwrap();
    config
}

fn texts(views: &SessionViews) -> Vec<String> {
    views
        .rows
        .values()
        .map(|row| match row {
            TranscriptRow::User { text, .. } => format!("user:{text}"),
            TranscriptRow::Assistant { text } => format!("assistant:{text}"),
            other => format!("{:?}", std::mem::discriminant(other)),
        })
        .collect()
}

#[test]
fn an_idle_projector_pass_does_not_commit_product_views() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["one prompt"]);
    let expected = caught_up(root.path(), &store, &id);
    let record = store.load(&id).unwrap();
    let refusing: Arc<dyn SessionRecordStore> =
        Arc::new(crate::runtime::fault_store::FaultStore::new(
            store.clone(),
            crate::runtime::test_support::SlowProjection {
                delay: Duration::ZERO,
                refuse: true,
            },
        ));
    let (config, _) = fresh_injected_attached_host(root.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    for _ in 0..3 {
        let actual = executor
            .block_on(project_to_tail(
                &sessions,
                &refusing,
                &id,
                record.plakous_locator.unwrap(),
            ))
            .unwrap()
            .expect("an idle pass needs no product commit");
        assert_eq!(actual, expected);
    }
}

#[test]
fn cached_projector_passes_reload_views_only_after_a_cursor_race() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["initial prompt"]);
    let record = store.load(&id).unwrap();
    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let records: Arc<dyn SessionRecordStore> = store.clone();
    let pass = |cached| {
        executor
            .block_on(super::project_to_tail_cached(
                &sessions,
                &records,
                &id,
                record.plakous_locator.unwrap(),
                cached,
            ))
            .unwrap()
            .unwrap()
            .views
    };
    let reads = store.view_read_count();
    let mut cached = pass(None);
    assert_eq!(
        store.view_read_count(),
        reads + 1,
        "the first pass loads once"
    );
    for _ in 0..5 {
        let records = store.record_read_count();
        let intents = store.intent_read_count();
        let inherited = store.inherited_read_count();
        cached = pass(Some(cached));
        assert_eq!(
            store.inherited_read_count(),
            inherited + 1,
            "one narrow ancestry snapshot per pass"
        );
        assert_eq!(
            store.record_read_count(),
            records,
            "idle ticks never load a full record"
        );
        assert_eq!(
            store.intent_read_count(),
            intents + 1,
            "one own-intent snapshot per idle tick"
        );
    }
    assert_eq!(
        store.view_read_count(),
        reads + 1,
        "idle passes use the task's views"
    );
    run_prompts(
        reopened(root.path(), &store, &id),
        &["a racing host advances the cursor"],
    );
    let reads = store.view_read_count();
    let actual = pass(Some(cached));
    assert_eq!(
        store.view_read_count(),
        reads + 1,
        "a lost cursor CAS reloads once"
    );
    assert_eq!(store.load_views(&id).unwrap(), Some(actual));
}

/// Spec 02 §2.6: the open and live passes write the views, and deleting
/// them and reopening through the real host rebuilds the same views: rows,
/// keys, counters and head.
#[test]
fn live_views_equal_the_views_a_reopen_rebuilds_from_empty() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _injection) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["first prompt", "second prompt"]);

    let live = store
        .load_views(&id)
        .unwrap()
        .expect("the live ticks wrote views");
    assert!(
        texts(&live).contains(&"user:second prompt".to_owned()),
        "the live tick projected the second turn: {:?}",
        texts(&live)
    );
    assert_eq!(live.state.counters.root_turns, 2);
    let live = caught_up(root.path(), &store, &id);

    store.delete_views(&id);
    run_prompts(reopened(root.path(), &store, &id), &[]);
    let rebuilt = caught_up(root.path(), &store, &id);
    assert!(rebuilt.cursor >= live.cursor);
    assert_eq!(
        rebuilt
            .rows
            .iter()
            .filter(|(key, _)| key.anchor <= live.cursor.get())
            .map(|(key, row)| (*key, row.clone()))
            .collect::<Vec<_>>(),
        live.rows
            .iter()
            .map(|(key, row)| (*key, row.clone()))
            .collect::<Vec<_>>(),
        "the rebuilt rows up to the live cursor are the live rows, key for key"
    );
    assert_eq!(rebuilt.state.counters, live.state.counters);
}

/// Every record of `session`'s feed.
pub(super) fn feed_records(
    sessions: &plakous::sessions::AgentSessions,
    session: plakous::model::SessionId,
) -> Vec<plakous::sessions::feed::FeedRecord> {
    use plakous::sessions::feed::{FeedCursor, FeedPage, FeedPageRequest};
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let mut cursor = FeedCursor::start(session);
    let mut all = Vec::new();
    loop {
        let page = executor
            .block_on(sessions.feed_page(FeedPageRequest::after(cursor)))
            .unwrap()
            .unwrap();
        let FeedPage::Records { records, .. } = page else {
            panic!("a fresh feed is never behind retention");
        };
        if records.is_empty() {
            return all;
        }
        cursor = records.iter().fold(cursor, FeedCursor::advanced_past);
        all.extend(records);
    }
}

/// K4 open issue (a): a direct effect's call block reaches history only
/// with its result, so `feed_call` of its operation at the record that
/// dispatched it reads the block from the operation record's request.
#[test]
fn a_direct_effect_reads_its_call_block_from_its_request_at_dispatch() {
    use plakous::sessions::feed::{
        FeedCall, FeedCallOutcome, FeedCallRequest, FeedCallTarget, FeedFact, FeedOperationKind,
        FeedOperationState,
    };
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = crate::runtime::recover_tests::scripted_host_with(
        root.path(),
        crate::runtime::recover_tests::script(serde_json::json!([
            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": "the effect ran"}}
        ])),
    );
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let id = config.session.id.clone();
    run_prompts(config, &["run the effect"]);

    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let locator = store.load(&id).unwrap().plakous_locator.unwrap();
    let records = feed_records(&sessions, locator.session_id());
    let (record, operation_id) = records
        .iter()
        .find_map(|record| {
            record.facts().iter().find_map(|fact| match fact {
                FeedFact::OperationAdvanced {
                    operation_id,
                    kind: FeedOperationKind::Effect { .. },
                    state: FeedOperationState::Dispatched,
                    ..
                } => Some((record, *operation_id)),
                _ => None,
            })
        })
        .expect("the effect's dispatch record");
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let outcome = executor
        .block_on(sessions.feed_call(FeedCallRequest {
            session: record.header().session_id,
            branch: record.header().branch_id,
            checkpoint: crate::sessions::projector::record_checkpoint(record).unwrap(),
            call: FeedCallTarget::Operation(operation_id),
        }))
        .unwrap()
        .unwrap();
    let FeedCallOutcome::Call {
        call: FeedCall::Tool { call, result },
        ..
    } = outcome
    else {
        panic!("the dispatch record holds the call: {outcome:?}");
    };
    assert_eq!(call.exposed_name().as_str(), "loopback_effect");
    assert!(result.is_none(), "the result is not committed at dispatch");

    let views = store.load_views(&id).unwrap().unwrap();
    let tool = views
        .rows
        .values()
        .find_map(TranscriptRow::as_tool)
        .expect("the effect's row");
    assert_eq!(tool.name, "loopback_effect");
}

/// Spec 02 §2.6 (live tick): the host loop never waits on a projector pass,
/// and release waits for its final pass only within its bound. The debt a
/// slow projector leaves is applied by the next open.
#[test]
fn a_slow_projector_never_holds_the_host_loop_and_the_next_open_catches_up() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _injection) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    let delay = Duration::from_secs(10);
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        crate::runtime::test_support::SlowProjection {
            delay,
            refuse: false,
        },
    ));
    let id = config.session.id.clone();
    let started = std::time::Instant::now();
    let (actions, events, worker) = crate::runtime::startup::spawn_host_worker(config).unwrap();
    send_attached_action(&actions, UserAction::Prompt("first prompt".into()));
    loop {
        if matches!(events.recv_timeout(delay).unwrap(), HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted)
        {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Prompt("second prompt".into()));
    loop {
        match events.recv_timeout(delay).unwrap() {
            HostEvent::PromptAdmitted(_) => break,
            HostEvent::TurnLifecycle(_) => panic!("the next admission must wait for the join"),
            _ => {}
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    assert!(
        started.elapsed() < delay,
        "admission and release waited on the projector: {:?}",
        started.elapsed()
    );

    run_prompts(reopened(root.path(), &store, &id), &[]);
    let views = caught_up(root.path(), &store, &id);
    assert_eq!(views.state.counters.root_turns, 1);
    assert!(!texts(&views).contains(&"user:second prompt".to_owned()));
    assert_eq!(
        store.load(&id).unwrap().pending_queue[0].text,
        "second prompt"
    );

    let (actions, events, worker) =
        crate::runtime::startup::spawn_host_worker(reopened(root.path(), &store, &id)).unwrap();
    send_attached_action(&actions, UserAction::ResumeQueueDrain);
    loop {
        if matches!(events.recv_timeout(delay).unwrap(), HostEvent::TurnLifecycle(lifecycle)
            if lifecycle.phase != plakous_host_protocol::TurnPhase::Admitted)
        {
            break;
        }
    }
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let views = caught_up(root.path(), &store, &id);
    assert!(
        texts(&views).contains(&"user:second prompt".to_owned()),
        "{:?}",
        texts(&views)
    );
    assert_eq!(views.state.counters.root_turns, 2);
}

#[test]
fn background_projection_releases_the_next_admission_without_resending_the_previous_command() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        crate::runtime::test_support::SlowProjection {
            delay: Duration::from_secs(1),
            refuse: false,
        },
    ));
    let id = config.session.id.clone();
    run_prompts(config, &["first prompt", "second prompt"]);
    let views = caught_up(root.path(), &store, &id);
    assert_eq!(views.state.counters.root_turns, 2);
    for prompt in ["first prompt", "second prompt"] {
        assert_eq!(
            texts(&views)
                .iter()
                .filter(|text| **text == format!("user:{prompt}"))
                .count(),
            1
        );
    }
    assert_eq!(
        store.load(&id).unwrap().pending_queue,
        [] as [crate::sessions::queue::QueueItem; 0]
    );
}

#[test]
fn fork_and_rebind_interrupt_between_pages_keep_source_and_resumable_target() {
    use crate::interrupt::{InterruptWatch, InterruptWatchState};
    use crate::runtime::{HostBoundary, HostWorkflow, ScriptedRuntimeFactory};
    use std::cell::RefCell;

    for rebind in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executor = plakous::application::ApplicationExecutor::new().unwrap();
        let (mut config, injection) = fresh_injected_attached_host(root.path());
        config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
            plakous_host_protocol::views::ProviderSection {
                account: "codex".into(),
                label: "Codex".into(),
                ready: true,
                entries: vec![plakous_host_protocol::views::ModelEntry {
                    id: "offline-loopback-v2".into(),
                    tag: "offline".into(),
                    reasoning: true,
                    effort_options: vec!["medium".into()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                }],
            },
        ]);
        let mut opened = executor
            .block_on(crate::runtime::startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        for turn in 0..40 {
            let result = executor
                .block_on(async {
                    opened
                        .agent()
                        .unwrap()
                        .prompt(format!("history turn {turn}"))
                        .await
                        .unwrap()
                        .result()
                        .await
                        .unwrap()
                })
                .unwrap();
            assert!(matches!(
                result.outcome(),
                plakous::agent::TurnResultOutcome::Completed(_)
            ));
        }
        let source = opened.locator();
        config.session.bind_plakous_locator(source).unwrap();
        config.store.save(&mut config.session).unwrap();
        executor
            .block_on(super::project_session(
                opened.sessions(),
                &config.store,
                &config.session,
            ))
            .unwrap()
            .unwrap();
        // Make the source seed-ineligible so the target genuinely owes pages.
        let previous = config
            .store
            .load_views(&config.session.id)
            .unwrap()
            .unwrap();
        let mut retained = previous.clone();
        retained.state.retained_from = Some(1);
        let writes = crate::sessions::projector::ViewWrites::between(&previous, &retained);
        config
            .store
            .commit_projection_views(&config.session.id, &writes, &retained.state)
            .unwrap();
        let source_id = config.session.id.clone();
        let store = Arc::clone(&config.store);
        let observed_source = source_id.clone();
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let resuming = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resumed_target = Arc::clone(&resuming);
        let primary = Arc::clone(&injection.runtime);
        let workspace = config.cwd.clone();
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("rebind-evidence.jsonl"),
            )
            .unwrap();
        let alternate = plakous_offline_crash_harness::runtime_binding_for_account(
            &workspace,
            controller.clone(),
            b"projection-target-account",
        )
        .unwrap()
        .target_binding();
        config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
            runtime: Arc::new(move |role| {
                if rebind
                    && (role == crate::runtime::RuntimeRole::Rebind
                        || resumed_target.load(std::sync::atomic::Ordering::SeqCst))
                {
                    plakous_offline_crash_harness::runtime_binding_for_account(
                        &workspace,
                        controller.clone(),
                        b"projection-target-account",
                    )
                    .map_err(|error| error.to_string())
                } else {
                    primary(role)
                }
            }),
            rebind_target: if rebind {
                alternate
            } else {
                injection.rebind_target.clone()
            },
            observe: Arc::new(move |workflow, boundary| {
                if boundary == HostBoundary::RuntimeReturned
                    && workflow
                        == if rebind {
                            HostWorkflow::Rebind
                        } else {
                            HostWorkflow::Fork
                        }
                {
                    let request = store
                        .load_intents(&observed_source)
                        .unwrap()
                        .workflow_requests
                        .last()
                        .unwrap()
                        .clone();
                    let target = request.successor_product_id();
                    super::probe::track(&target);
                    let actions = actions.clone();
                    super::probe::after_next_write(target, move || {
                        actions.send(UserAction::Interrupt).unwrap();
                    });
                }
            }),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        });
        let state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&receiver, &state);
        let (events, _events_rx) = crate::runtime::test_support::host_event_sink();
        let error = if rebind {
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
            .unwrap_err()
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
            .unwrap_err()
        };
        assert!(watch.interrupted(), "rebind={rebind}: {error:?}");
        assert!(
            error.message().contains("fork stopped; saved as session"),
            "{error:?}"
        );
        assert!(error.message().contains("/resume"), "{error:?}");
        assert_eq!(config.session.id, source_id);
        assert!(
            opened.agent().is_some(),
            "the source must still be attached"
        );
        assert_eq!(opened.locator().branch_id(), source.branch_id());
        let request = config
            .store
            .load_intents(&source_id)
            .unwrap()
            .workflow_requests
            .last()
            .unwrap()
            .clone();
        let target_id = request.successor_product_id();
        assert_eq!(super::probe::take(&target_id).writes, 1);
        let partial = config.store.load_views(&target_id).unwrap().unwrap();
        assert!(partial.cursor.get() > 0);
        let tail = feed_records(opened.sessions(), source.session_id())
            .last()
            .unwrap()
            .header()
            .seq;
        assert!(
            partial.cursor < tail,
            "the interrupted target must still owe at least one page"
        );
        executor.block_on(opened.release()).unwrap().unwrap();
        config.session = config.store.load(&target_id).unwrap();
        resuming.store(true, std::sync::atomic::Ordering::SeqCst);
        let resumed = executor
            .block_on(crate::runtime::startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        assert_ne!(resumed.locator().branch_id(), source.branch_id());
        assert_eq!(config.session.id, target_id);
        executor.block_on(resumed.release()).unwrap().unwrap();
    }
}

#[test]
fn pruned_lineage_rebuild_from_zero_equals_incremental_views() {
    use crate::runtime::test_support::run_attached_actions;
    use plakous::sessions::PruneHistoryOutcome;

    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(MemoryStore::new());
    config.store = store.clone();
    let source_id = config.session.id.clone();
    run_prompts(config, &["first prompt", "second prompt"]);
    run_attached_actions(
        reopened(root.path(), &store, &source_id),
        vec![UserAction::ForkSession {
            account: None,
            compact: false,
        }],
    );
    let request = store
        .load_intents(&source_id)
        .unwrap()
        .workflow_requests
        .last()
        .unwrap()
        .clone();
    let target_id = request.successor_product_id();
    let incremental = caught_up(root.path(), &store, &target_id);
    assert_eq!(incremental.state.lineage.len(), 2);
    let (config, _) = fresh_injected_attached_host(root.path());
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let mut pruned = 0;
    for id in [&source_id, &target_id] {
        let locator = store.load(id).unwrap().plakous_locator.unwrap();
        let PruneHistoryOutcome::Pruned(report) = executor
            .block_on(sessions.prune_history(locator))
            .unwrap()
            .unwrap()
        else {
            panic!("released session must prune")
        };
        pruned += report.pruned_checkpoints();
    }
    assert!(pruned > 0);
    // A fresh facade has no process-local published-checkpoint shortcuts.
    let (fresh, _) = fresh_injected_attached_host(root.path());
    let sessions = fresh.state_roots.runtime_sessions().unwrap();
    let records = feed_records(&sessions, incremental.state.runtime_session_id.unwrap());
    let initial = SessionViews::empty_with_lineage(incremental.state.lineage.clone());
    assert_eq!(initial.cursor.get(), 0);
    let mut intents = store.load_intents(&target_id).unwrap();
    intents
        .submissions
        .extend(store.load_inherited_submissions(&target_id).unwrap());
    let incremental = executor
        .block_on(crate::sessions::projector::run(
            &sessions,
            incremental,
            &intents,
            &records,
        ))
        .unwrap()
        .unwrap()
        .views;
    let rebuilt = executor
        .block_on(crate::sessions::projector::run(
            &sessions, initial, &intents, &records,
        ))
        .unwrap()
        .unwrap();
    assert_eq!(rebuilt.views, incremental);
}
