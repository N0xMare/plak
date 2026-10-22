//! `/clear` rebuild, publication, and resume session switching.

use std::time::Instant;

use plakous::application::ApplicationExecutor;
use plakous::sessions::SessionOperationId;
use plakous::subscriptions::ResolvedProvider;

use crate::agent::{AttachedCodingSession, ProductAgentSpec};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, SessionRecord};
use crate::{CliFailure, CliResult};

use super::admission::{emit_prompt_history, emit_queue_snapshot, emit_unavailable_context};
use super::info::{emit_agent_plan_notes, send_binding, send_last_user_prompt_at};
use super::logging::{log_ev, project_session_snapshot};
use super::models::native_compact_facet;
use super::reconcile::persist;
use super::source_release::{Settlement, release_source_attachment};
#[cfg(any(test, feature = "test-support"))]
use super::startup::injected_runtime;
use super::startup::{cached_api_runtime_for_record, observe_phase, open_session_for_host};
use super::usage::report_context;
use super::{
    HostConfig, HostEvent, HostLifecyclePhase, HostTargetAuthority, Sender, TestRuntimeRole,
};
use crate::sessions::{NoticeScope, TranscriptRow};

/// New product session in the same workspace; rebuild agent for a clean slate.
///
/// Builds the replacement agent **before** shutting down the live one so a
/// failed rebuild never leaves the host without a usable agent.
pub fn clear_session_rebuild<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
) -> CliResult<()> {
    config.live_topology_enabled()?;
    super::jobs_transition_guard(config, opened)?;
    let started = Instant::now();
    let attached_locator = opened.locator();
    if config.session.plakous_locator != Some(attached_locator) {
        return Err(CliFailure::new(
            "clear source does not match the exact durable product binding",
        ));
    }
    let request = sessions::projector::WorkflowRequest {
        operation_id: format!("plak:clear:{}", uuid::Uuid::new_v4()),
        kind: sessions::SessionWorkflowKind::Clear,
        source_locator: attached_locator,
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .map_err(CliFailure::new)?;
    let target_product_id = request.successor_product_id();
    let create_operation =
        sessions::clear_create_operation_id(&request.operation_id).map_err(CliFailure::new)?;
    let runtime_sessions = config.state_roots.runtime_sessions()?;
    let (new_session, fresh_resolved) = build_clear_candidate(
        executor,
        config,
        opened,
        &target_product_id,
        &create_operation,
        stderr_buf,
        events,
        started,
    )?;
    observe_phase(
        events,
        HostLifecyclePhase::ClearBuildCandidate,
        started,
        config.session.pending_queue.len(),
    );
    let publication = executor
        .block_on(super::projection::project_to_tail(
            &runtime_sessions,
            &config.store,
            &target_product_id,
            new_session.locator(),
        ))
        .map_err(|_| "application execution failed".to_owned())
        .and_then(|result| result.map_err(String::from))
        .and_then(|_| config.store.load(&target_product_id));
    let next = match publication {
        Ok(next) => next,
        Err(error) => {
            let release = executor
                .block_on(new_session.release())
                .map_err(|_| "clear target release join failed".to_owned())
                .and_then(|result| result.map_err(|error| error.to_string()));
            return Err(CliFailure::new(format!(
                "clear projection failed: {error}{}",
                release
                    .err()
                    .map(|error| format!("; target release failed: {error}"))
                    .unwrap_or_default(),
            )));
        }
    };
    observe_phase(
        events,
        HostLifecyclePhase::ClearPublishCandidate,
        started,
        config.session.pending_queue.len(),
    );
    let source = config.session.clone();
    let previous = std::mem::replace(opened, new_session);
    config.session = next;
    if let Some(resolved) = fresh_resolved
        && let HostTargetAuthority::Native {
            resolved: current, ..
        } = &mut config.target
    {
        *current = resolved;
    }
    observe_phase(events, HostLifecyclePhase::ClearActivate, started, 0);
    config.source_releases.track(release_source_attachment(
        previous,
        source.id,
        events.pinned_to(&config.session.id),
        "clear",
        config.test_injection.clone(),
    ));
    persist(&*config.store, &mut config.session);
    emit_agent_plan_notes(events, config, stderr_buf);

    let _ = events.send(HostEvent::ClearView);
    // The new session's header, as a restore of it would show.
    let _ = events.send(project_session_snapshot(&config.session));
    send_binding(events, config);
    send_last_user_prompt_at(events, opened);
    if let Err(error) = report_context(
        executor,
        opened,
        events,
        "after clear",
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    ) {
        emit_unavailable_context(events);
        // An unreadable context is state every switch re-announces.
        let _ = events.send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Session,
            error.message().to_owned(),
        )));
    }
    log_ev(
        log,
        config,
        SessionEvent::new("session.cleared").str("id", &config.session.id),
    );
    let short = config.session.id.chars().take(8).collect::<String>();
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!("new session {short}…"),
    ));
    let _ = events.send(HostEvent::SetBusy(false));
    let _ = events.send(HostEvent::Status {
        phase: "idle".into(),
        queue_len: 0,
        detail: Some(format!("ready · {}", config.model_id)),
    });
    Ok(())
}

pub(super) fn build_clear_candidate<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    target_product_session_id: &str,
    create_operation_id: &str,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    started: Instant,
) -> CliResult<(
    AttachedCodingSession,
    Option<std::sync::Arc<ResolvedProvider>>,
)> {
    let topology_enabled = config.live_topology_enabled()?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(runtime) = injected_runtime(config, TestRuntimeRole::Primary) {
        let operation = SessionOperationId::new(create_operation_id)
            .map_err(|_| CliFailure::new("injected clear operation identity is invalid"))?;
        let session = executor
            .block_on(opened.create_with_runtime(operation, opened.clear_job_counter(runtime?)?))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(|error| error.failure)?;
        observe_phase(
            events,
            HostLifecyclePhase::ClearResolveAuthority,
            started,
            config.session.pending_queue.len(),
        );
        return Ok((session, None));
    }

    if config.target.is_api() {
        let mut target = config.session.clone();
        target_product_session_id.clone_into(&mut target.id);
        create_operation_id.clone_into(&mut target.create_operation_id);
        target.plakous_locator = None;
        target.transcript.clear();
        target.prompt_history.clear();
        target.pending_queue.clear();
        let runtime = executor
            .block_on(cached_api_runtime_for_record(config, &target, false))
            .map_err(|_| CliFailure::new("application execution failed"))??;
        observe_phase(
            events,
            HostLifecyclePhase::ClearResolveAuthority,
            started,
            config.session.pending_queue.len(),
        );
        let operation = SessionOperationId::new(create_operation_id)
            .map_err(|_| CliFailure::new("OpenRouter clear operation identity is invalid"))?;
        let session = executor
            .block_on(opened.create_with_runtime(operation, opened.clear_job_counter(runtime)?))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(|error| error.failure)?;
        return Ok((session, None));
    }

    // Clear changes conversation identity, not provider binding. Reuse the
    // already admitted exact provider/configuration aggregate; every physical
    // model attempt still performs its mandatory fresh authority revalidation,
    // and the turn-scoped gate issues a distinct nonrenewable root lease.
    let resolved = config
        .target
        .native_resolved()
        .cloned()
        .ok_or_else(|| CliFailure::new("current exact provider authority is missing"))?;
    observe_phase(
        events,
        HostLifecyclePhase::ClearResolveAuthority,
        started,
        config.session.pending_queue.len(),
    );
    // Never place a cancellation timeout around durable create. The exact
    // operation is already persisted above, and this call must run through to
    // an authoritative outcome rather than being dropped across commit.
    let runtime = crate::agent::coding_session_runtime_binding_attaching_root(
        ProductAgentSpec {
            prompt_host_facts: config.prompt_host_facts(),
            product_home: Some(config.product_home.clone()),
            tool_selection: super::startup::root_tool_selection(&config.session),
            provider: config
                .target
                .native_provider()
                .ok_or_else(|| CliFailure::new("native provider authority is missing"))?,
            resolved: std::sync::Arc::clone(&resolved),
            effort: config.effort,
            effort_explicit: config.effort_explicit,
            workspace_root: config.session.workspace_root.clone(),
            cwd: config.cwd.clone(),
            plan_state_authority: target_product_session_id.to_owned(),
            composition: config.session.composition,
            topology_tree_id: Some(target_product_session_id.to_owned()),
            topology_caller_id: None,
            state_roots: config.state_roots.clone(),
            scheduled_compaction: config.compaction_enabled,
            max_model_rounds: config.max_model_rounds,
            topology_enabled,
        },
        stderr_buf,
    )?;
    let operation = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("native clear operation identity is invalid"))?;
    let session = executor
        .block_on(opened.create_with_runtime(operation, opened.clear_job_counter(runtime)?))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| error.failure)?;
    Ok((session, None))
}

pub(super) fn delete_unpublished_runtime(
    executor: &ApplicationExecutor,
    state_roots: &crate::agent::ProductStateRoots,
    locator: plakous::sessions::SessionLocator,
) -> CliResult<()> {
    let sessions = state_roots.runtime_sessions()?;
    let archived = executor
        .block_on(sessions.archive(locator))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| CliFailure::new(format!("unpublished runtime archive failed: {error}")))?;
    executor
        .block_on(sessions.delete(archived.session_id(), archived.archived_revision()))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| {
            CliFailure::new(format!("unpublished runtime deletion failed: {error}"))
        })?;
    Ok(())
}

/// Load a session by uuid and rebuild the live agent (same provider/model attach).
///
/// Refuses when the saved session is bound to a different account than this
/// host process (cross-account resume needs a fresh `plak resume <uuid>` launch).
pub(super) fn resume_session_switch<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    session_id: &str,
) -> CliResult<()> {
    super::jobs_transition_guard(config, opened)?;
    let mut waited = false;
    let switched = switch_onto_session(
        executor,
        config,
        opened,
        stderr_buf,
        events,
        session_id,
        &mut waited,
    );
    if switched.is_err() && waited {
        let _ = events.send(HostEvent::SetBusy(false));
        let _ = events.send(HostEvent::Status {
            phase: "idle".into(),
            queue_len: config.session.pending_queue.len(),
            detail: Some(format!("ready · {}", config.model_id)),
        });
    }
    switched
}

/// Wait for this host's old attachment, then load and project before opening.
fn load_settled_source<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    session_id: &str,
    waited: &mut bool,
) -> CliResult<SessionRecord> {
    let queue_len = config.session.pending_queue.len();
    if config.source_releases.settle(session_id, || {
        *waited = true;
        let _ = events.send(HostEvent::SetBusy(true));
        let _ = events.send(HostEvent::Status {
            phase: "resume".into(),
            queue_len,
            detail: Some("releasing the previous session attachment".into()),
        });
    }) == Settlement::StillRunning
    {
        return Err(CliFailure::new(format!(
            "session {session_id} is still finishing its previous switch; try again"
        )));
    }
    let mut record = config.store.load(session_id).map_err(CliFailure::new)?;
    let sessions = config.state_roots.runtime_sessions()?;
    if let Some(head) = executor
        .block_on(super::projection::project_before_open(
            &sessions,
            &config.store,
            &record,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?
    {
        record = config.store.load(session_id).map_err(CliFailure::new)?;
        record.bind_plakous_locator(head).map_err(CliFailure::new)?;
    }
    Ok(record)
}

fn switch_onto_session<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    session_id: &str,
    waited: &mut bool,
) -> CliResult<()> {
    // A source this host left (a clear, fork or rebind) may still be
    // releasing its attachment; read its record only after the join.
    let next = load_settled_source(executor, config, events, session_id, waited)?;
    let requested = std::fs::canonicalize(&config.cwd).unwrap_or_else(|_| config.cwd.clone());
    let recorded =
        std::fs::canonicalize(&next.workspace_root).unwrap_or_else(|_| next.workspace_root.clone());
    if requested != recorded
        || !exact_resume_target_matches(&config.session, &next, &config.model_id)
    {
        return Err(CliFailure::new(
            "exact resume binding or workspace differs; use confirmed fork/rebind",
        ));
    }
    let locator = next
        .plakous_locator
        .ok_or_else(|| CliFailure::new("session has no canonical Plakous locator"))?;
    stderr_buf.clear();
    let replacement = executor
        .block_on(open_session_for_host(
            config,
            &next,
            locator,
            stderr_buf,
            TestRuntimeRole::Primary,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|error| error.failure)?;
    let source = config.session.clone();
    let previous = std::mem::replace(opened, replacement);
    config.session = next;
    let mut release = None;
    publish_restored_frame_before_source_release(events, config, stderr_buf, || {
        release = Some(release_source_attachment(
            previous,
            source.id,
            events.pinned_to(&config.session.id),
            "resume",
            config.test_injection.clone(),
        ));
    });
    if let Some(release) = release {
        config.source_releases.track(release);
    }
    send_last_user_prompt_at(events, opened);
    if let Err(error) = report_context(
        executor,
        opened,
        events,
        "after session switch",
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    ) {
        emit_unavailable_context(events);
        // An unreadable context is state every switch re-announces.
        let _ = events.send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Session,
            error.message().to_owned(),
        )));
    }
    let _ = events.send(HostEvent::SetBusy(false));
    let _ = events.send(HostEvent::Status {
        phase: "idle".into(),
        queue_len: config.session.pending_queue.len(),
        detail: Some(format!("ready · {}", config.model_id)),
    });
    Ok(())
}

pub(super) fn publish_restored_frame_before_source_release<
    P: plakous_host_protocol::ProtocolExtension,
>(
    events: &Sender<P>,
    config: &HostConfig<P>,
    stderr_buf: &[u8],
    release_source: impl FnOnce(),
) {
    // The selected exact runtime is already active. Publish its first frame
    // before joining the unrelated idle source; attachment release retains
    // its own durable locator and reports any ambiguity without rolling the
    // active target back.
    let _ = events.send(project_session_snapshot(&config.session));
    emit_prompt_history(events, &config.session);
    emit_queue_snapshot(events, &config.session);
    send_binding(events, config);
    emit_agent_plan_notes(events, config, stderr_buf);
    release_source();
}

pub(super) fn exact_resume_target_matches(
    current: &SessionRecord,
    candidate: &SessionRecord,
    current_model: &str,
) -> bool {
    candidate.account == current.account
        && candidate.model.as_deref() == Some(current_model)
        && candidate.effort == current.effort
        && candidate.composition == current.composition
        && candidate.target_selection == current.target_selection
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::shell::*;

    #[cfg(any(test, feature = "test-support"))]
    use crate::agent::build_or_open_injected_session;
    use crate::interrupt::InterruptWatch;
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
            assert!(
                matches!(rx.try_recv(), Ok(HostEvent::PresentationSnapshot(_))),
                "first restored frame must cross the UI boundary before source release starts"
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
            .block_on(build_or_open_injected_session(
                config.state_roots.runtime_sessions().unwrap(),
                None,
                None,
                create,
                injected_runtime(&config, TestRuntimeRole::Primary)
                    .unwrap()
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
            let (_actions, receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
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
            let create_operation =
                sessions::clear_create_operation_id(&request.operation_id).unwrap();
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
                matches!(&notices[0], TranscriptRow::Notice(row) if row.text == "clear did not start; run /clear again")
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
}
