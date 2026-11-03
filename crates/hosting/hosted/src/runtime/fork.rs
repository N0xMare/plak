//! `/fork` workflow.

use crate::surface_protocol::ContentEvent;
use plakous::sessions::{AgentSessionsErrorKind, ForkSessionOutcome, SessionOperationId};

use crate::agent::AttachedCodingSession;
use crate::interrupt::{InterruptWatch, watched};
use crate::sessions::{self};
use crate::{CliFailure, CliResult};

use super::admission::emit_queue_snapshot;
use super::factory::observe_boundary;
use super::info::{send_binding, send_last_user_prompt_at};
use super::rebind::{
    durable_tree_id, instruction_sources, interrupted_before_dispatch, note_reread_instructions,
    note_reread_instructions_at, restore_detached_source,
};
use super::startup::{api_runtime_for_record, host_runtime_binding, open_session_for_host};
use super::{HostBoundary, HostWorkflow};
use super::{HostConfig, RuntimeRole, Sender};
use crate::sessions::{NoticeScope, TranscriptRow};

#[allow(
    clippy::too_many_lines,
    reason = "Fork publication keeps preparation, source detachment, canonical outcome reconciliation, and target activation ordered."
)]
pub(super) async fn handle_fork<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    requested_account: Option<&str>,
) -> CliResult<()> {
    super::jobs_transition_guard(config, opened)?;
    watch.begin();
    if requested_account.is_some_and(|account| config.session.account.as_deref() != Some(account)) {
        return Err(CliFailure::new(
            "cross-binding fork requires the confirmed rebind workflow; source unchanged",
        ));
    }
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("fork"));
    }
    if config.session.store_revision == 0 {
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
    }
    project_fork_source(watch, config, opened, events, false)
        .await
        .map_err(|error| {
            if watch.interrupted() {
                interrupted_before_dispatch("fork")
            } else {
                error
            }
        })?;
    let intents = config
        .store
        .load_intents(&config.session.id)
        .map_err(CliFailure::new)?;
    let views = config
        .store
        .load_views(&config.session.id)
        .map_err(CliFailure::new)?
        .ok_or_else(|| CliFailure::new("fork requires projected views"))?;
    if !config.session.pending_queue.is_empty()
        || !sessions::projector::pending_command_ids(&views, &intents).is_empty()
        || intents.workflow_requests.iter().any(|request| {
            views
                .state
                .workflows
                .get(&request.operation_id)
                .is_none_or(|workflow| workflow.settled.is_none())
        })
    {
        return Err(CliFailure::new(
            "fork requires an idle session with no queued or unresolved product work",
        ));
    }
    let attached_locator = opened.locator();
    if config.session.plakous_locator != Some(attached_locator) {
        return Err(CliFailure::new(
            "fork source does not match the exact durable product binding",
        ));
    }
    // nothing durable has happened yet, so an intent observed
    // here refuses outright and the exact source stays attached.
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("fork"));
    }
    // A fork is a successor of its source: it keeps whether the source's
    // skills are on, and re-reads the roots under that (spec 02 §6.1.1).
    let inherited_skills = super::rebind::source_skills_on(opened).await?;
    let operation_id = format!("plak:fork:{}", uuid::Uuid::new_v4());
    let facade_operation = SessionOperationId::new(&operation_id)
        .map_err(|_| CliFailure::new("fork operation identity is invalid"))?;
    let instructions_before = instruction_sources(opened);
    let source_locator = watched(watch, opened.detach())
        .await
        .map_err(|error| CliFailure::new(format!("fork source release failed: {error}")))?;
    if watch.observe_now() {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        return Err(interrupted_before_dispatch("fork"));
    }
    let request = sessions::projector::WorkflowRequest {
        operation_id: operation_id.clone(),
        source_locator,
        kind: sessions::SessionWorkflowKind::Fork,
    };
    if let Err(error) = config
        .store
        .submit_workflow_request(&mut config.session, &request)
    {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        return Err(CliFailure::new(error));
    }
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Fork,
        HostBoundary::Prepared,
    );
    if watch.observe_now() {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        discard_unstarted_fork(watch, config, opened, events, &operation_id).await?;
        return Err(interrupted_before_dispatch("fork"));
    }
    stderr_buf.clear();
    config.state_roots.set_inherited_skills(inherited_skills);
    let built = fork_runtime_binding(config, stderr_buf).await;
    config.state_roots.set_inherited_skills(None);
    let runtime = match built {
        Ok(runtime) => runtime,
        Err(error) => {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            discard_unstarted_fork(watch, config, opened, events, &operation_id).await?;
            return Err(error);
        }
    };
    if watch.observe_now() {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        discard_unstarted_fork(watch, config, opened, events, &operation_id).await?;
        return Err(interrupted_before_dispatch("fork"));
    }
    let outcome = match watched(watch, Box::pin(opened.fork(facade_operation, runtime))).await {
        Ok(outcome) => outcome,
        // Refused before anything canonical happened: nothing was forked.
        Err(error)
            if matches!(
                error.kind(),
                AgentSessionsErrorKind::InvalidInput
                    | AgentSessionsErrorKind::IdentityConflict
                    | AgentSessionsErrorKind::Revalidation
                    | AgentSessionsErrorKind::StoragePressure
            ) =>
        {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            discard_unstarted_fork(watch, config, opened, events, &operation_id).await?;
            if let Some(refusal) = fork_refusal(error.kind(), &error.to_string()) {
                return Err(CliFailure::new(refusal));
            }
            let raw = format!("canonical fork failed: {error}");
            return Err(CliFailure::new(
                crate::storage_pressure::sessions_storage_pressure(&error, &raw).unwrap_or(raw),
            )
            .with_optional_remedy(crate::storage_pressure::sessions_remedy(&error).as_ref()));
        }
        // Possibly dispatched: the workflow stays unresolved for exact
        // reconciliation and is never replayed.
        Err(error) => {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            return Err(CliFailure::new(format!("canonical fork failed: {error}")));
        }
    };
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Fork,
        HostBoundary::RuntimeReturned,
    );
    let target_locator = match outcome {
        ForkSessionOutcome::Forked {
            source: exact_source,
            target,
            ..
        } if exact_source == source_locator => target,
        _ => {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            discard_unstarted_fork(watch, config, opened, events, &operation_id).await?;
            return Err(
                CliFailure::new(request.withdrawal_note()).with_remedy(request.withdrawal_remedy())
            );
        }
    };
    // The branch fact and request, not this runtime receipt, publish the
    // successor and settle the source atomically under the projector cursor.
    // Finish that publication before honouring an interrupt on successor catch-up.
    if let Err(error) = project_fork_source(watch, config, opened, events, true).await {
        restore_detached_source(config, opened, source_locator, stderr_buf).await?;
        return Err(CliFailure::new(format!(
            "fork committed but could not publish its product views ({}); this session stays attached",
            error.message()
        )));
    }
    let target_id = request.successor_product_id();
    let target_result = async {
        let projected = watched(
            watch,
            super::projection::project_to_tail_interruptible(
                opened.sessions(),
                &config.store,
                &target_id,
                target_locator,
                || watch.observe_now(),
                |applied, tail| fork_progress(events, applied, tail),
            ),
        )
        .await;
        end_fork_progress(events);
        projected.map_err(CliFailure::new)?;
        config.store.load(&target_id).map_err(CliFailure::new)
    }
    .await;
    let target = match target_result {
        Ok(target) => target,
        Err(error) => {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            if watch.interrupted() {
                return Err(CliFailure::new(format!(
                    "fork stopped; saved as session {target_id}; this session stays attached; the saved session can be opened explicitly"
                )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeSavedSession { session_id: target_id.parse().map_err(|e| CliFailure::new(format!("invalid saved session identity: {e}")))? }));
            }
            return Err(CliFailure::new(format!(
                "the fork was saved as session {target_id} but could not project ({}); this session stays attached; the saved fork can be opened explicitly",
                error.message(),
            )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeSavedSession { session_id: target_id.parse().map_err(|e| CliFailure::new(format!("invalid saved session identity: {e}")))? }));
        }
    };
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Fork,
        HostBoundary::ProductPublished,
    );
    stderr_buf.clear();
    let replacement = match watched(
        watch,
        open_session_for_host(
            config,
            &target,
            target_locator,
            stderr_buf,
            RuntimeRole::Primary,
        ),
    )
    .await
    {
        Ok(replacement) => replacement,
        Err(error) => {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            let detail = error.failure.message().to_owned();
            note_reread_instructions_at(config, events, instructions_before, target_locator).await;
            return Err(CliFailure::new(format!(
                "the fork was saved as session {target_id} but could not attach ({detail}); this session stays attached; the saved fork can be opened explicitly"
            )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeSavedSession { session_id: target_id.parse().map_err(|e| CliFailure::new(format!("invalid saved session identity: {e}")))? }));
        }
    };
    let previous = std::mem::replace(opened, replacement);
    config.session = target;
    let release_error = match watched(watch, previous.release()).await {
        Ok(_) => None,
        Err(error) => Some(error.to_string()),
    };
    if let Some(error) = release_error {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Session,
            format!("fork target is active; source release could not be joined: {error}"),
        ));
    }
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Fork,
        HostBoundary::ProductCommitted,
    );
    send_binding(events, config);
    super::admission::emit_prompt_history(events, &config.session);
    emit_queue_snapshot(events, &config.session);
    let _ = events.replace_transcript(super::logging::project_session_snapshot(&config.session));
    send_last_user_prompt_at(events, config, opened);
    note_reread_instructions(events, instructions_before, opened);
    if watch.observe_now() {
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Session,
            "fork attached before the stop could be applied; source remains resumable",
        ));
    }
    let _ = events.notice(TranscriptRow::info(
        NoticeScope::Session,
        format!(
            "forked session {}…; source remains resumable",
            &config.session.id[..8]
        ),
    ));
    Ok(())
}

/// The `/fork` wording for a refusal the operator can act on. A fork
/// recompiles the session's own configuration; when its tools changed
/// (Plakous: "fork changed canonical tool authority") only a new session can
/// use them.
pub(super) fn fork_refusal(kind: AgentSessionsErrorKind, message: &str) -> Option<&'static str> {
    (kind == AgentSessionsErrorKind::Revalidation
        && message.contains("fork changed canonical tool authority"))
    .then_some("cannot fork: this session's tools changed; start a new session")
}

/// The runtime a fork recompiles from: this session's own binding and tree,
/// built from the files as they are now. It takes no topology lease.
pub(super) async fn production_fork_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    if config.session.target_selection.api_target().is_none() {
        return host_runtime_binding(config, stderr_buf, RuntimeRole::Primary).await;
    }
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let mut attached = None;
    api_runtime_for_record(
        config,
        &config.session,
        false,
        crate::topology_runtime::TopologyHubAttach::LookupOnly,
        Some(&tree_id),
        &mut attached,
    )
    .await
}

fn fork_progress<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    applied: plakous::sessions::feed::FeedSeq,
    tail: plakous::sessions::feed::FeedSeq,
) {
    let _ = events.send(ContentEvent::Activity {
        label: format!("preparing fork: {}/{}", applied.get(), tail.get()),
        active: applied < tail,
    });
}

fn end_fork_progress<P: plakous_host_protocol::ProtocolExtension>(events: &Sender<P>) {
    let _ = events.send(ContentEvent::Activity {
        label: "preparing fork".into(),
        active: false,
    });
}

async fn project_fork_source<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    finish_publication: bool,
) -> CliResult<()> {
    let result = watched(
        watch,
        super::projection::project_to_tail_interruptible(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
            || !finish_publication && watch.observe_now(),
            |applied, tail| fork_progress(events, applied, tail),
        ),
    )
    .await
    .map(|_| ())
    .map_err(CliFailure::new);
    end_fork_progress(events);
    result
}

async fn discard_unstarted_fork<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    operation: &str,
) -> CliResult<()> {
    project_fork_source(watch, config, opened, events, true).await?;
    config
        .store
        .discard_unadmitted_workflow(&mut config.session, operation)
        .map_err(CliFailure::new)
}

pub(super) async fn fork_runtime_binding<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    stderr: &mut Vec<u8>,
) -> CliResult<plakous::sessions::SessionRuntimeBinding> {
    config.runtime_factory.fork_runtime(config, stderr).await
}

#[cfg(test)]
mod tests;
