#![allow(
    clippy::too_many_lines,
    reason = "Clear scenarios exercise durable intent publication, successor projection, and source preservation across the complete rebuild."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]
use crate::runtime::test_kit::BlockHere as _;

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

use super::*;
use crate::runtime::*;

use crate::agent::build_or_open_bound_session;
use crate::interrupt::InterruptWatch;
use crate::runtime::shell::*;
use crate::runtime::test_support::*;
use crate::sessions::{self, SessionRecord};
use plakous::application::ApplicationExecutor;
use plakous::sessions::SessionOperationId;
use std::time::Instant;

#[test]
fn unreadable_topology_refuses_even_an_injected_clear_candidate() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.topology_admission = Some(Arc::new(|| Err(CliFailure::new("named config error"))));
    let (events, _) = host_event_sink();
    let result = build_clear_candidate(
        &ApplicationExecutor::new().unwrap(),
        &mut config,
        &memory_attached_stub(),
        "target",
        "create-target",
        &mut Vec::new(),
        &events,
        Instant::now(),
    );
    assert!(
        result
            .err()
            .is_some_and(|error| error.message() == "named config error")
    );
}

#[test]
fn restored_frame_is_published_before_source_release_latency_boundary() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config
        .session
        .append_transcript(TranscriptRow::assistant("restored immediately"));
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let release_observed = std::cell::Cell::new(false);

    publish_restored_frame_before_source_release(&events, &config, &[], || {
        let frame = rx.try_iter().collect::<Vec<_>>();
        assert!(
            matches!(frame.first(), Some(HostEvent::Binding { session_id: Some(id), .. }) if id == &config.session.id),
            "the target binding must precede its restored state"
        );
        assert!(
            frame
                .iter()
                .any(|event| matches!(event, HostEvent::PresentationSnapshot(_))),
            "first restored frame must cross the UI boundary before source release starts"
        );
        assert!(
            frame
                .iter()
                .any(|event| matches!(event, HostEvent::PromptHistorySnapshot(_)))
        );
        assert!(
            frame
                .iter()
                .any(|event| matches!(event, HostEvent::QueueSnapshot { .. }))
        );
        release_observed.set(true);
    });

    assert!(release_observed.get());
}

#[test]
fn clear_creation_fact_settles_intent_and_rebuilds_without_second_runtime() {
    clear_intent_projection(true, false);
}

#[test]
fn unadmitted_clear_is_withdrawn_once_without_creating_runtime() {
    clear_intent_projection(false, false);
}

#[test]
fn clear_creation_join_rebuilds_the_fork_source_lineage() {
    clear_intent_projection(true, true);
}

fn clear_intent_projection(admit: bool, fork_source: bool) {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let store = Arc::new(crate::session_store::MemoryStore::new());
    config.store = store.clone();
    let executor = ApplicationExecutor::new().unwrap();
    let create = SessionOperationId::new(&config.session.create_operation_id).unwrap();
    let mut opened = executor
        .block_on(build_or_open_bound_session(
            config.state_roots.runtime_sessions().unwrap(),
            None,
            None,
            create,
            config
                .runtime_factory
                .runtime(&config, RuntimeRole::Primary, &mut Vec::new())
                .block_here()
                .unwrap(),
        ))
        .unwrap()
        .unwrap_or_else(|error| panic!("source create failed: {}", error.failure.message()));
    config
        .session
        .bind_plakous_locator(opened.locator())
        .unwrap();
    config.store.save(&mut config.session).unwrap();
    if fork_source {
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&receiver, &interrupt_state);
        fork::handle_fork(
            &executor,
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &Sender::over(crate::surface_protocol::host_event_channel::<TestExtension>().0),
            None,
        )
        .unwrap();
    }
    let request = sessions::projector::WorkflowRequest {
        operation_id: "plak:clear:crash-after-intent".into(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Clear,
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .unwrap();
    let target_id = request.successor_product_id();
    assert!(config.store.load(&target_id).unwrap().transcript.is_empty());
    if admit {
        let create_operation = sessions::clear_create_operation_id(&request.operation_id).unwrap();
        let (created, _) = build_clear_candidate(
            &executor,
            &mut config,
            &opened,
            &target_id,
            &create_operation,
            &mut Vec::new(),
            &Sender::over(crate::surface_protocol::host_event_channel::<TestExtension>().0),
            Instant::now(),
        )
        .unwrap();
        assert_eq!(
            config.store.load(&target_id).unwrap().plakous_locator,
            Some(created.locator())
        );
        executor.block_on(created.release()).unwrap().unwrap();
    }
    let sessions = config.state_roots.runtime_sessions().unwrap();
    let views = executor
        .block_on(projection::project_to_tail(
            &sessions,
            &config.store,
            &config.session.id,
            opened.locator(),
        ))
        .unwrap()
        .unwrap();
    let refusing: Arc<dyn SessionRecordStore> = Arc::new(fault_store::FaultStore::new(
        store.clone(),
        SlowProjection {
            delay: Duration::ZERO,
            refuse: true,
        },
    ));
    assert_eq!(
        executor
            .block_on(projection::project_to_tail(
                &sessions,
                &refusing,
                &config.session.id,
                opened.locator(),
            ))
            .unwrap()
            .expect("an idle clear source needs no product commit"),
        views
    );
    let notices = config
        .store
        .withdraw_unadmitted_workflows(&mut config.session)
        .unwrap();
    if admit {
        assert_eq!(notices, [] as [TranscriptRow; 0]);
        assert!(
            views.state.workflows[&request.operation_id]
                .settled
                .is_some()
        );

        assert_eq!(
            durable_authority_count(&config.product_home.join("runtime.sqlite3"), "sessions"),
            2
        );
        let target_views = config.store.load_views(&target_id).unwrap().unwrap();
        let target_locator = config
            .store
            .load(&target_id)
            .unwrap()
            .plakous_locator
            .unwrap();
        assert_eq!(
            executor
                .block_on(projection::project_to_tail(
                    &sessions,
                    &refusing,
                    &target_id,
                    target_locator,
                ))
                .unwrap()
                .expect("an already-joined idle clear target needs no product commit"),
            target_views
        );
        store.delete_views(&config.session.id);
        executor
            .block_on(projection::project_to_tail(
                &sessions,
                &config.store,
                &target_id,
                target_locator,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            config
                .store
                .load_views(&config.session.id)
                .unwrap()
                .unwrap()
                .state
                .workflows[&request.operation_id],
            views.state.workflows[&request.operation_id],
            "an idle target repairs a deleted source's clear settlement"
        );
        let rebuilt = executor
            .block_on(projection::project_to_tail(
                &sessions,
                &config.store,
                &config.session.id,
                opened.locator(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            rebuilt, views,
            "source rebuild rejoins an already-projected target"
        );
        store.delete_views(&target_id);
        store.delete_views(&config.session.id);
        executor
            .block_on(projection::project_to_tail(
                &sessions,
                &config.store,
                &target_id,
                target_locator,
            ))
            .unwrap()
            .unwrap();
        let rebuilt = executor
            .block_on(projection::project_to_tail(
                &sessions,
                &config.store,
                &config.session.id,
                opened.locator(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            rebuilt, views,
            "deleting both views preserves the clear join"
        );
        assert_eq!(
            config.store.load_views(&target_id).unwrap().unwrap(),
            target_views
        );
        let mut stale = rebuilt.clone();
        stale.state.workflows.remove(&request.operation_id);
        config
            .store
            .commit_projection_views(
                &config.session.id,
                &sessions::projector::ViewWrites::between(&rebuilt, &stale),
                &stale.state,
            )
            .unwrap();
        assert_eq!(
            config
                .store
                .load_views(&config.session.id)
                .unwrap()
                .unwrap(),
            rebuilt,
            "a same-cursor source applier cannot erase a target's settlement"
        );
    } else {
        assert_eq!(notices.len(), 1);
        assert!(
            config.store.load(&target_id).is_err(),
            "withdrawal removes the empty reservation"
        );
        assert!(
            matches!(&notices[0], TranscriptRow::Notice(row) if row.text == "clear did not start; request a fresh conversation again explicitly" && row.remedy == Some(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {}))
        );
        assert_eq!(
            config
                .store
                .withdraw_unadmitted_workflows(&mut config.session)
                .unwrap(),
            [] as [TranscriptRow; 0]
        );
        assert_eq!(
            durable_authority_count(&config.product_home.join("runtime.sqlite3"), "sessions"),
            1
        );
    }
    executor.block_on(opened.release()).unwrap().unwrap();
}

#[test]
fn detached_clear_refuses_to_publish_when_the_durable_save_fails() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut failed_source = SessionRecord::new(workspace);
    failed_source.account = Some("codex".into());
    failed_source.model = Some("gpt-test".into());
    failed_source.effort = Some("high".into());
    let failed_id = failed_source.id.clone();
    let failed_revision = failed_source.store_revision;
    let error = commit_detached_clear(&mut failed_source, |_| {
        Err("injected durable save refusal".into())
    })
    .unwrap_err();
    assert_eq!(error, "injected durable save refusal");
    assert_eq!(failed_source.id, failed_id);
    assert_eq!(failed_source.store_revision, failed_revision);
}
