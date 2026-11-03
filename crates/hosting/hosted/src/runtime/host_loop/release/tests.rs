#![allow(
    clippy::too_many_lines,
    reason = "Release scenarios commit a terminal during shutdown and verify its projection wins before ownership is dropped."
)]

use super::*;
use crate::runtime::Sender;
use crate::runtime::startup::build_session_from_config;
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn release_aborts_and_joins_every_task_the_session_spawned() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    host.block(async |host| host.project().await);
    let tasks = host.role.executor.tasks().clone();
    // Work started through the session's roots belongs to the same set. The
    // live projector spawns and finishes its own tasks concurrently, so the
    // set's size is no evidence: only the set aborting this task drops its
    // flag.
    let dropped = Arc::new(AtomicBool::new(false));
    let flag = DropFlag(Arc::clone(&dropped));
    let roots = host.role.config.state_roots.clone();
    host.role
        .executor
        .block_on(async move {
            roots.session_tasks().spawn(async move {
                let _flag = flag;
                std::future::pending::<()>().await;
            });
        })
        .unwrap();
    assert!(!dropped.load(Ordering::SeqCst));

    host.block(async |host| host.release().await).unwrap();

    assert!(tasks.is_closed(), "release closed the session's task set");
    assert!(tasks.is_empty(), "no session task outlives release");
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn quit_projection_stages_share_the_original_deadline() {
    use crate::runtime::test_support::SlowProjection;
    use std::time::{Duration, Instant};
    for publish in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.role
            .config
            .store
            .save(&mut host.role.config.session)
            .unwrap();
        host.role.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            host.role.config.store.clone(),
            SlowProjection {
                delay: Duration::from_secs(1),
                refuse: false,
            },
        ));
        host.quit_started_at = Instant::now().checked_sub(Duration::from_millis(900));
        let started = Instant::now();
        if publish {
            host.block(async |host| host.publish_projected_transcript(false).await);
        } else {
            host.block(async |host| host.finish_projection(false).await)
                .unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "quit projection stacked another bound: {:?}",
            started.elapsed()
        );
        let started = Instant::now();
        host.block(async |host| host.finish_projection(false).await)
            .unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "finish stacked another bound: {:?}",
            started.elapsed()
        );
        // Release itself is durable lease and store work, unbounded by
        // the quit deadline and slow on a disk-backed TMPDIR; what must
        // not happen is a post-release pass started on a spent budget.
        let id = host.role.config.session.id.clone();
        crate::runtime::projection::probe::track(&id);
        host.block(async |host| host.release().await).unwrap();
        assert_eq!(
            crate::runtime::projection::probe::take(&id).release,
            0,
            "post-release pass started on a spent quit budget"
        );
    }
}

#[test]
fn terminal_committed_during_release_is_projected_before_shutdown_returns() {
    use plakous::agent::AgentEventKind;
    use plakous::model::ModelEvent;
    use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest};

    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version":1,
            "steps":[{"stream":[{"text":"release prefix"},{"stall":30000}]}]
        }))
        .unwrap(),
    );
    let (config, _) = crate::runtime::recover_tests::scripted_host_with(root.path(), plan);
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    let sessions = host.opened.shared_sessions();
    let runtime_id = host.opened.locator().session_id();
    let agent = host.opened.agent().unwrap().clone();
    let turn = host.role.executor.block_on(async {
        let mut turn = agent.prompt("release while active").await.unwrap();
        let mut events = turn.take_events().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = events.recv().await.unwrap();
                if matches!(event.kind(), AgentEventKind::Model { event: Some(model), .. }
                    if matches!(model.as_ref(), ModelEvent::TextDelta(text) if text.bytes() == b"release prefix")) {
                    break;
                }
            }
        }).await.unwrap();
        turn
    }).unwrap();
    host.block(async |host| host.finish_projection(false).await)
        .unwrap();
    let before = store.load_views(&id).unwrap().unwrap();
    assert!(before.state.head_working);
    assert!(before.state.open_turns.contains_key(&turn.id().to_string()));
    super::super::super::projection::probe::track(&id);
    let home = host.role.config.home.clone();
    let ledger = host.role.session_ledger.clone();
    let before_ledger = ledger.project();
    events.try_iter().for_each(drop);
    host.block(async |host| host.release().await).unwrap();
    assert!(
        !events
            .try_iter()
            .any(|event| matches!(event, crate::surface_protocol::HostEvent::TurnUsage { .. }))
    );
    assert_eq!(
        ledger.project(),
        before_ledger,
        "post-release drain is log-only"
    );
    let rows = crate::session_log::read_tail(&home, &id, 100).unwrap();
    let usage = rows
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|row| row["kind"] == "turn.usage")
        .collect::<Vec<_>>();
    assert_eq!(
        usage.len(),
        1,
        "release terminal usage must reach the session log"
    );
    assert_eq!(usage[0]["turn_id"], turn.id().to_string());
    let counts = super::super::super::projection::probe::take(&id);
    assert_eq!(counts.release, 1);
    assert!(counts.writes > 0);
    let after = store.load_views(&id).unwrap().unwrap();
    assert!(after.state.released);
    assert!(!after.state.head_working);
    assert!(after.state.open_turns.is_empty());
    assert!(
        after
            .rows
            .values()
            .any(|row| *row == crate::sessions::TranscriptRow::Interrupted)
    );
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let FeedPage::Records { records, tail, .. } = sessions
                .feed_page(FeedPageRequest::after(FeedCursor::new(
                    runtime_id,
                    before.cursor,
                )))
                .await
                .unwrap()
            else {
                panic!("fixture feed pruned")
            };
            assert!(records.iter().flat_map(plakous::sessions::feed::FeedRecord::facts).any(|fact|
            matches!(fact, FeedFact::TurnTerminal { turn_id, .. } if *turn_id == turn.id())));
            assert_eq!(after.cursor, tail);
            assert!(matches!(
                turn.result().await.unwrap().outcome(),
                plakous::agent::TurnResultOutcome::Cancelled(_)
            ));
        })
        .unwrap();
}

#[test]
fn failed_pre_open_projection_does_not_fence_a_current_head() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let memory = Arc::new(crate::session_store::MemoryStore::new());
    config.store = memory.clone();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let inner = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    host.block(async |host| host.release().await).unwrap();

    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session = inner.load(&id).unwrap();
    config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        inner.clone(),
        crate::runtime::test_support::SlowProjection {
            delay: std::time::Duration::ZERO,
            refuse: true,
        },
    ));
    // Force a write on the next pre-open pass, even if release caught up.
    memory.delete_views(&id);
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    // The recorded head is current: deterministic debt must not fence open.
    let mut opened = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    opened.role.config.store = inner.clone();
    opened.block(async |opened| opened.release().await).unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session = inner.load(&id).unwrap();
    config.store = inner;
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    {
        let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn failed_projection_with_a_stale_head_names_debt_without_recovery_loop() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let memory = Arc::new(crate::session_store::MemoryStore::new());
    config.store = memory.clone();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let recorded = host.opened.locator();
    let inner = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    let agent = host.opened.agent().unwrap().clone();
    host.role
        .executor
        .block_on(async {
            let turn = agent.prompt("advance the canonical head").await.unwrap();
            turn.result().await.unwrap();
        })
        .unwrap();
    host.block(async |host| host.release().await).unwrap();
    memory.delete_views(&id);
    for _ in 0..2 {
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session = inner.load(&id).unwrap();
        config.session.plakous_locator = Some(recorded);
        config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            inner.clone(),
            crate::runtime::test_support::SlowProjection {
                delay: std::time::Duration::ZERO,
                refuse: true,
            },
        ));
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let error = match RootActor::open_blocking(config, &rx, Sender::over(tx), None) {
            Ok(_) => panic!("a stale head opened despite failed projection"),
            Err(error) => error,
        };
        assert!(!error.is_recovery_required(), "{error}");
        assert!(
            error.message().contains("recorded session head is stale"),
            "{error}"
        );
        assert!(
            error
                .message()
                .contains("injected projector commit refusal"),
            "{error}"
        );
        assert!(!error.message().contains("another process"), "{error}");
    }
}

#[test]
fn post_release_projection_refusal_is_cleanup_and_preserves_release_failure() {
    for fail_child in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        if fail_child {
            let executor = plakous::application::ApplicationExecutor::new().unwrap();
            executor
                .block_on(crate::runtime::test_kit::install_failing_child_shutdown(
                    &config.session.id,
                    &config.session.workspace_root,
                ))
                .unwrap();
        }
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.role
            .config
            .store
            .save(&mut host.role.config.session)
            .unwrap();
        let inner = host.role.config.store.clone();
        let id = host.role.config.session.id.clone();
        host.role.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
            inner.clone(),
            crate::runtime::test_support::SlowProjection {
                delay: std::time::Duration::ZERO,
                refuse: true,
            },
        ));
        let error = host.block(async |host| host.release().await).unwrap_err();
        let failures = error.shutdown_failures().unwrap();
        assert!(
            failures
                .cleanup
                .as_ref()
                .unwrap()
                .message()
                .contains("injected projector commit refusal")
        );
        assert_eq!(failures.release.is_some(), fail_child);
        if fail_child {
            assert!(
                failures
                    .release
                    .as_ref()
                    .unwrap()
                    .message()
                    .contains("injected child release refusal")
            );
        }
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = inner.clone();
        config.session = inner.load(&id).unwrap();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        {
            let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
            host.block(async |host| host.release().await).unwrap();
        }
    }
}

#[test]
fn child_retirement_failure_releases_without_a_product_save() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(crate::runtime::test_kit::install_failing_child_shutdown(
            &config.session.id,
            &config.session.workspace_root,
        ))
        .unwrap();
    let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    // A stale cache cannot select abandonment instead of the clean receipt.
    host.quit_abandoned_turn = true;
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    host.role.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        store.clone(),
        SaveFailure {
            fail: AtomicBool::new(true),
        },
    ));
    let error = host.block(async |host| host.release().await).unwrap_err();
    let failures = error.shutdown_failures().unwrap();
    assert!(failures.cleanup.is_none());
    assert!(
        failures
            .release
            .as_ref()
            .unwrap()
            .message()
            .contains("injected child release refusal")
    );
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = store.clone();
    config.session = store.load(&id).unwrap();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    {
        let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn release_without_a_product_save_fences_a_retained_agent_and_reopens() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let (actions, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    host.role
        .config
        .store
        .save(&mut host.role.config.session)
        .unwrap();
    let old_agent = host.opened.agent().unwrap().clone();
    let (mut contender, _) = fresh_injected_attached_host(root.path());
    contender.store = host.role.config.store.clone();
    contender.session = contender.store.load(&host.role.config.session.id).unwrap();
    let error = host
        .role
        .executor
        .block_on(build_session_from_config(&mut contender, &mut Vec::new()))
        .unwrap()
        .err()
        .expect("a live owner must exclude a second owner");
    assert!(error.message().contains("ownership"), "{error:?}");
    let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
        host.role.config.store.clone(),
        SaveFailure {
            fail: AtomicBool::new(true),
        },
    ));
    host.role.config.store = store.clone();
    let id = host.role.config.session.id.clone();
    host.block(async |host| host.release().await).unwrap();
    store.faults.fail.store(false, Ordering::SeqCst);
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store = store.inner.clone();
    config.session = config.store.load(&id).unwrap();
    let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened = RootActor::open_blocking(config, &rx, Sender::over(tx), None).unwrap();
    assert!(
        reopened
            .role
            .executor
            .block_on(old_agent.prompt("must not run"))
            .unwrap()
            .is_err()
    );
    drop(actions);
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
}
