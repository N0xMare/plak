//! `/new` rebuild, publication, and resume session switching.

use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};
use std::time::Instant;

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
use super::source_release::{Settlement, SourceRelease, release_source_attachment};
use super::startup::{cached_api_runtime_for_record, observe_phase, open_session_for_host};
use super::usage::report_context;
use super::{HostConfig, HostLifecyclePhase, HostTargetAuthority, RuntimeRole, Sender};
use crate::sessions::{NoticeScope, TranscriptRow};

/// New product session in the same workspace; rebuild agent for a clean slate.
///
/// Builds the replacement agent **before** shutting down the live one so a
/// failed rebuild never leaves the host without a usable agent.
#[allow(
    clippy::too_many_lines,
    reason = "Clear rebuilds the candidate before detaching the source and preserves rollback at every publication boundary."
)]
pub async fn clear_session_rebuild<P: plakous_host_protocol::ProtocolExtension>(
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
        operation_id: format!("plak:new:{}", uuid::Uuid::new_v4()),
        kind: sessions::SessionWorkflowKind::New,
        source_locator: attached_locator,
    };
    config
        .store
        .submit_workflow_request(&mut config.session, &request)
        .map_err(CliFailure::new)?;
    let target_product_id = request.successor_product_id();
    freeze_reserved_profile(config, &target_product_id)?;
    let create_operation = sessions::new_session_create_operation_id(&request.operation_id)
        .map_err(CliFailure::new)?;
    let runtime_sessions = config.state_roots.runtime_sessions()?;
    let (new_session, fresh_resolved) = build_clear_candidate(
        config,
        opened,
        &target_product_id,
        &create_operation,
        stderr_buf,
        events,
        started,
    )
    .await?;
    observe_phase(
        events,
        HostLifecyclePhase::ClearBuildCandidate,
        started,
        config.session.pending_queue.len(),
    );
    let publication = super::projection::project_to_tail(
        &runtime_sessions,
        &config.store,
        &target_product_id,
        new_session.locator(),
    )
    .await
    .map_err(String::from)
    .and_then(|_| config.store.load(&target_product_id));
    let next = match publication {
        Ok(next) => next,
        Err(error) => {
            let release = new_session
                .release()
                .await
                .map_err(|error| error.to_string());
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
    super::surface::leaving_session(config.surface.as_deref(), &source.id, &next.id);
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
        SourceRelease::Shutdown,
        config.runtime_factory.clone(),
    ));
    persist(&*config.store, &mut config.session);
    emit_agent_plan_notes(events, config, stderr_buf);

    send_binding(events, config);
    emit_prompt_history(events, &config.session);
    emit_queue_snapshot(events, &config.session);
    let _ = events.send(ContentEvent::ClearView);
    // The new session's header, as a restore of it would show.
    let _ = events.replace_transcript(project_session_snapshot(&config.session));
    send_last_user_prompt_at(events, config, opened);
    if let Err(error) = report_context(
        opened,
        events,
        "after clear",
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    )
    .await
    {
        emit_unavailable_context(events);
        // An unreadable context is state every switch re-announces.
        let _ = events.send(ContentEvent::Row(
            TranscriptRow::error(NoticeScope::Session, error.message().to_owned())
                .with_optional_remedy(error.remedy()),
        ));
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
    let _ = events.send(ContentEvent::SetBusy(false));
    let _ = events.send(ContentEvent::Status {
        phase: HostPhase::Idle,
        queue_len: 0,
        detail: Some(HostStatusDetail::Ready {
            model_id: config.model_id.clone(),
            pause: plakous_host_protocol::status::HostPause::default(),
        }),
    });
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "Takes the attached source session, the new product session id and create operation id, and the host output channels and start instant that the clear reports through."
)]
pub(super) async fn production_build_clear_candidate<
    P: plakous_host_protocol::ProtocolExtension,
>(
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

    if config.target.is_api() {
        let mut target = config.session.clone();
        target_product_session_id.clone_into(&mut target.id);
        create_operation_id.clone_into(&mut target.create_operation_id);
        target.plakous_locator = None;
        target.transcript.clear();
        target.prompt_history.clear();
        target.pending_queue.clear();
        let runtime = cached_api_runtime_for_record(config, &target, false).await?;
        observe_phase(
            events,
            HostLifecyclePhase::ClearResolveAuthority,
            started,
            config.session.pending_queue.len(),
        );
        let operation = SessionOperationId::new(create_operation_id)
            .map_err(|_| CliFailure::new("OpenRouter clear operation identity is invalid"))?;
        let session = opened
            .create_with_runtime(operation, opened.clear_job_counter(runtime)?)
            .await
            .map_err(|error| *error.failure)?;
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
            account: config.session.account_ref()?,
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
            continue_after_loss: config.resume_after_loss
                == crate::runtime::ResumeAfterLoss::Continue,
        },
        stderr_buf,
    )
    .await?;
    let operation = SessionOperationId::new(create_operation_id)
        .map_err(|_| CliFailure::new("native clear operation identity is invalid"))?;
    let session = opened
        .create_with_runtime(operation, opened.clear_job_counter(runtime)?)
        .await
        .map_err(|error| *error.failure)?;
    Ok((session, None))
}

/// Writes the profile the new session's genesis freezes (the home's, for its
/// tuple) into its reservation before the runtime create, so no created
/// session lacks one.
fn freeze_reserved_profile<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    target_product_id: &str,
) -> CliResult<()> {
    let mut target = config
        .store
        .load(target_product_id)
        .map_err(CliFailure::new)?;
    if target.root_profile.is_some() {
        return Ok(());
    }
    target.root_profile = super::startup::genesis_root_profile(&config.state_roots, &target)?;
    config.store.save(&mut target).map_err(CliFailure::new)
}

pub(super) async fn delete_unpublished_runtime(
    state_roots: &crate::agent::ProductStateRoots,
    locator: plakous::sessions::SessionLocator,
) -> CliResult<()> {
    let sessions = state_roots.runtime_sessions()?;
    let archived = sessions
        .archive(locator)
        .await
        .map_err(|error| CliFailure::new(format!("unpublished runtime archive failed: {error}")))?;
    sessions
        .delete(archived.session_id(), archived.archived_revision())
        .await
        .map_err(|error| {
            CliFailure::new(format!("unpublished runtime deletion failed: {error}"))
        })?;
    Ok(())
}

/// Load a session by uuid and rebuild the live agent (same provider/model attach).
///
/// Refuses when the saved session is bound to a different account than this
/// host process (cross-account resume needs a fresh `plak resume <uuid>` launch).
pub(super) async fn resume_session_switch<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    session_id: &str,
) -> CliResult<()> {
    super::jobs_transition_guard(config, opened)?;
    let mut waited = false;
    let switched =
        switch_onto_session(config, opened, stderr_buf, events, session_id, &mut waited).await;
    if switched.is_err() && waited {
        let _ = events.send(ContentEvent::SetBusy(false));
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::Ready {
                model_id: config.model_id.clone(),
                pause: plakous_host_protocol::status::HostPause::default(),
            }),
        });
    }
    switched
}

/// Wait for this host's old attachment, then load and project before opening.
async fn load_settled_source<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    session_id: &str,
    waited: &mut bool,
) -> CliResult<SessionRecord> {
    let queue_len = config.session.pending_queue.len();
    if config.source_releases.settle(session_id, || {
        *waited = true;
        let _ = events.send(ContentEvent::SetBusy(true));
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Resume,
            queue_len,
            detail: Some(HostStatusDetail::ReleasingPreviousAttachment),
        });
    }) == Settlement::StillRunning
    {
        return Err(CliFailure::new(format!(
            "session {session_id} is still finishing its previous switch; try again"
        )));
    }
    let mut record = config.store.load(session_id).map_err(CliFailure::new)?;
    let sessions = config.state_roots.runtime_sessions()?;
    if let Some(head) = super::projection::project_before_open(&sessions, &config.store, &record)
        .await
        .map_err(CliFailure::new)?
    {
        record = config.store.load(session_id).map_err(CliFailure::new)?;
        record.bind_plakous_locator(head).map_err(CliFailure::new)?;
    }
    Ok(record)
}

async fn switch_onto_session<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    session_id: &str,
    waited: &mut bool,
) -> CliResult<()> {
    // A source this host left (a clear, fork or rebind) may still be
    // releasing its attachment; read its record only after the join.
    let next = load_settled_source(config, events, session_id, waited).await?;
    let probe = config.state_roots.workspace_probe();
    let probe = probe.as_ref();
    let unavailable = crate::process_environment::probe_unavailable;
    let requested = probe
        .canonicalize(&config.cwd)
        .map_err(unavailable)?
        .unwrap_or_else(|| config.cwd.clone());
    let recorded = probe
        .canonicalize(&next.workspace_root)
        .map_err(unavailable)?
        .unwrap_or_else(|| next.workspace_root.clone());
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
    let replacement =
        open_session_for_host(config, &next, locator, stderr_buf, RuntimeRole::Primary)
            .await
            .map_err(|error| *error.failure)?;
    let source = config.session.clone();
    let previous = std::mem::replace(opened, replacement);
    super::surface::leaving_session(config.surface.as_deref(), &source.id, &next.id);
    config.session = next;
    let mut release = None;
    publish_restored_frame_before_source_release(events, config, stderr_buf, || {
        release = Some(release_source_attachment(
            previous,
            source.id,
            events.pinned_to(&config.session.id),
            "resume",
            SourceRelease::Shutdown,
            config.runtime_factory.clone(),
        ));
    });
    if let Some(release) = release {
        config.source_releases.track(release);
    }
    send_last_user_prompt_at(events, config, opened);
    if let Err(error) = report_context(
        opened,
        events,
        "after session switch",
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    )
    .await
    {
        emit_unavailable_context(events);
        // An unreadable context is state every switch re-announces.
        let _ = events.send(ContentEvent::Row(
            TranscriptRow::error(NoticeScope::Session, error.message().to_owned())
                .with_optional_remedy(error.remedy()),
        ));
    }
    if !opened.has_recovered_turn() {
        let _ = events.send(ContentEvent::SetBusy(false));
        let _ = events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::Ready {
                model_id: config.model_id.clone(),
                pause: plakous_host_protocol::status::HostPause::default(),
            }),
        });
    }
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
    send_binding(events, config);
    let _ = events.replace_transcript(project_session_snapshot(&config.session));
    emit_prompt_history(events, &config.session);
    emit_queue_snapshot(events, &config.session);
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

#[allow(
    clippy::too_many_arguments,
    reason = "Clear candidate construction forwards the explicit runtime factory inputs without replacing the source authority."
)]
pub(super) async fn build_clear_candidate<P: plakous_host_protocol::ProtocolExtension>(
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
    let factory = config.runtime_factory.clone();
    factory
        .clear(
            config,
            opened,
            target_product_session_id,
            create_operation_id,
            stderr_buf,
            events,
            started,
        )
        .await
}

#[cfg(test)]
mod tests;
