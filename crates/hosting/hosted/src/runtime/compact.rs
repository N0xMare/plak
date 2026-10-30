//! Manual and scheduled compaction and capacity recovery.

use super::events::{
    compact_row_token_pair, compact_terminal_state, compact_terminal_subject,
    compaction_summary_clause,
};
use super::factory::observe_boundary;
use super::logging::{
    compact_reason_label, log_compact_finished, log_compact_finished_with_detail,
    log_compact_started,
};
use super::rebind::interrupted_before_dispatch;
use super::startup::host_runtime_binding;
use super::usage::query_context;
use super::{
    COMPACT_COMMITTED_DETAIL, COMPACT_FAILED_DETAIL, COMPACT_NO_ELIGIBLE_RANGE_DETAIL,
    COMPACT_NOT_NEEDED_DETAIL, COMPACT_RECOVERY_REQUIRED_DETAIL, COMPACT_UNAVAILABLE_DETAIL,
    CONTEXT_CAPACITY_PROTECTED_GUIDANCE, HostBoundary, HostConfig, HostWorkflow, RuntimeRole,
    Sender, queue,
};
use crate::agent::AttachedCodingSession;
use crate::context::ContextDisposition;
use crate::interrupt::{InterruptWatch, run_watched};
use crate::session_log::SessionLog;
use crate::sessions::{
    NoticeScope, QueueDrainState as SessionQueueDrainState, SessionRecord, TranscriptRow,
};
use crate::surface_protocol::ContentEvent;
use crate::{CliFailure, CliResult, sessions};
use plakous::agent::UserInput;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessionsErrorKind, CompactReason, CompactSessionOutcome, ContextQuery, SessionOperationId,
};
use std::time::Instant;

mod scheduled;
mod views;

pub(super) use scheduled::{ScheduledPrompt, run_scheduled_compaction_until_fits};
use scheduled::{scheduled_compact_queue_drain_state, scheduled_pass_already_fits};
use views::{
    compact_committed_detail, compact_operator_note, emit_compact_meter,
    manual_compact_operator_text, publish_compact_row, refresh_compact_views, unanchored_occupancy,
};
pub(super) use views::{context_capacity_recovery_guidance, observe_compact};

/// What one host compaction pass did to the conversation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CompactPass {
    /// A compacted successor checkpoint is now the head.
    Committed,
    /// The ladder found nothing to compact. No operation was paid and the
    /// conversation is unchanged.
    NothingEligible,
    /// The fold settled as a definite failure; the head is unchanged. A retry
    /// of the same source would pay for the same refused fold again. A
    /// scheduled pass leaves `note` to its caller, which knows whether the
    /// prompt is sent.
    Failed { note: String },
    /// Not needed, abandoned or unavailable; the head is unchanged.
    Unchanged,
}

/// One compact pass's fixed facts, shared by its steps.
struct CompactRun<'a, P: plakous_host_protocol::ProtocolExtension> {
    executor: &'a ApplicationExecutor,
    opened: &'a AttachedCodingSession,
    events: &'a Sender<P>,
    log: Option<&'a SessionLog>,
    reason: sessions::ProductCompactReason,
    operation: String,
    call_id: String,
    started_at: Instant,
}

/// The canonical answer to one pass, classified for the operator.
struct CompactAnswer {
    pass: CompactPass,
    successor_fits: bool,
    detail: String,
    outcome: &'static str,
    size_subject: Option<String>,
    unadmitted: bool,
}

/// One compact pass, for an operator `/compact`, a scheduled pass or a
/// capacity recovery: the boundary checks, the single dispatch, the
/// classified answer and its publication.
#[allow(
    clippy::too_many_arguments,
    reason = "Manual compaction binds the attached session, durable reason, interrupt watch, and host event sink at dispatch."
)]
pub(super) fn handle_compact<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
    reason: sessions::ProductCompactReason,
    scheduled_command: Option<&UserInput>,
) -> CliResult<CompactPass> {
    watch.begin();
    let _credential_notices = if let (Some(agent), Some(account)) =
        (opened.agent().cloned(), config.session.account.clone())
    {
        Some(
            executor
                .block_on(async {
                    super::credential_wait::CredentialNotices::start(agent, account, events.clone())
                })
                .map_err(|_| CliFailure::new("application execution failed"))?,
        )
    } else {
        None
    };
    let _compact_span = tracing::info_span!(
        "compact",
        session_id = config.session.id.as_str(),
        reason = compact_reason_label(reason),
    )
    .entered();
    let compact_started_at = Instant::now();
    let operation = format!(
        "plak:{}compact:{}",
        if reason == sessions::ProductCompactReason::Scheduled {
            "scheduled-"
        } else if reason == sessions::ProductCompactReason::CapacityRecovery {
            "capacity-recovery-"
        } else {
            ""
        },
        uuid::Uuid::new_v4()
    );
    let call_id = compact_call_id(&operation, opened.locator())?;
    log_compact_started(log, config, reason);
    // A scheduled pass qualifies and folds on the exact command its caller
    // will measure and admit, child mail included.
    let pending_user_input = scheduled_command
        .filter(|_| reason == sessions::ProductCompactReason::Scheduled)
        .cloned();
    let run = CompactRun {
        executor,
        opened,
        events,
        log,
        reason,
        operation,
        call_id,
        started_at: compact_started_at,
    };
    if reason == sessions::ProductCompactReason::Scheduled
        && scheduled_pass_already_fits(&run, config, pending_user_input.as_ref())
    {
        return Ok(CompactPass::Unchanged);
    }
    let source = compact_source(&run, watch, config)?;
    let (outcome, occupancy_before) = dispatch_compact(
        &run,
        watch,
        config,
        stderr_buf,
        source,
        pending_user_input.clone(),
    )?;
    let answer = classify_compact_answer(
        &run,
        config,
        source,
        outcome,
        pending_user_input.as_ref(),
        occupancy_before,
    )?;
    finish_compact(&run, watch, config, answer)
}

/// The exact source a pass compacts, once no product work, workflow request
/// or interrupt stands in its way.
fn compact_source<P: plakous_host_protocol::ProtocolExtension>(
    run: &CompactRun<'_, P>,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
) -> CliResult<plakous::sessions::SessionLocator> {
    let (executor, opened, log, compact_started_at) =
        (run.executor, run.opened, run.log, run.started_at);
    if (run.reason == sessions::ProductCompactReason::Manual
        && !config.session.pending_queue.is_empty())
        || config.session.queue_commands.has_pending()
    {
        let detail = "compact requires a safe boundary with no unresolved product work";
        log_compact_finished_with_detail(log, config, compact_started_at, "failed", Some(detail));
        return Err(CliFailure::new(detail));
    }
    if watch.observe_now() {
        log_compact_finished(log, config, compact_started_at, "not_needed");
        return Err(interrupted_before_dispatch("compact"));
    }
    if config.session.store_revision == 0 {
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
    }
    executor
        .block_on(super::projection::project_to_tail(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
        ))
        .map_err(|_| CliFailure::new("application execution failed"))??;
    let intents = config
        .store
        .load_intents(&config.session.id)
        .map_err(CliFailure::new)?;
    let views = config
        .store
        .load_views(&config.session.id)
        .map_err(CliFailure::new)?
        .ok_or_else(|| CliFailure::new("compact requires projected views"))?;
    config.session.plakous_locator = super::projection::head_locator(&views);
    if intents.workflow_requests.iter().any(|request| {
        views
            .state
            .workflows
            .get(&request.operation_id)
            .is_none_or(|workflow| workflow.settled.is_none())
    }) {
        return Err(CliFailure::new(
            "compact requires a safe boundary with no unresolved workflow request",
        ));
    }
    let source = opened.locator();
    if config.session.plakous_locator != Some(source) {
        let detail = "compact source does not match the exact durable product binding";
        log_compact_finished_with_detail(log, config, compact_started_at, "failed", Some(detail));
        return Err(CliFailure::new(detail));
    }
    // refusing before the single dispatch is lawful and unpaid.
    if watch.observe_now() {
        log_compact_finished(log, config, compact_started_at, "not_needed");
        return Err(interrupted_before_dispatch("compact"));
    }
    Ok(source)
}

/// The pass's single dispatch: its workflow request, then the canonical
/// compact run to its own receipt. Returns the answer and the occupancy
/// measured before it.
#[allow(
    clippy::too_many_lines,
    reason = "Compaction dispatch preserves the ordered interrupt, source release, canonical dispatch, and reconciliation cuts."
)]
fn dispatch_compact<P: plakous_host_protocol::ProtocolExtension>(
    run: &CompactRun<'_, P>,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    stderr_buf: &mut Vec<u8>,
    source: plakous::sessions::SessionLocator,
    pending_user_input: Option<UserInput>,
) -> CliResult<(CompactSessionOutcome, Option<u64>)> {
    let (executor, opened, events, log, reason) =
        (run.executor, run.opened, run.events, run.log, run.reason);
    let (operation, call_id, compact_started_at) = (&run.operation, &run.call_id, run.started_at);
    stderr_buf.clear();
    let runtime = host_runtime_binding(config, stderr_buf, RuntimeRole::Compact)?;
    let operation_id = SessionOperationId::new(operation)
        .map_err(|_| CliFailure::new("compact operation identity is invalid"))?;
    let facade_reason = match reason {
        sessions::ProductCompactReason::Manual => CompactReason::Manual,
        sessions::ProductCompactReason::Scheduled => CompactReason::Scheduled,
        sessions::ProductCompactReason::CapacityRecovery => CompactReason::Manual,
    };
    if watch.observe_now() {
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
        log_compact_finished(log, config, compact_started_at, "not_needed");
        return Err(interrupted_before_dispatch("compact"));
    }
    // Exactly one dispatch. The round-trip runs to its own
    // receipt even under an interrupt — abandoning it mid-flight is what turns
    // a paid compaction into an unrecoverable ambiguity.
    config
        .store
        .submit_workflow_request(
            &mut config.session,
            &sessions::projector::WorkflowRequest {
                operation_id: operation.clone(),
                source_locator: source,
                kind: sessions::SessionWorkflowKind::Compact(reason),
            },
        )
        .map_err(CliFailure::new)?;
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Compact,
        HostBoundary::Prepared,
    );
    let occupancy_before = unanchored_occupancy(executor, opened);
    publish_compact_row(config, events, call_id, "running", None, None);
    let outcome = match run_watched(
        executor,
        watch,
        opened.compact_from(
            operation_id,
            source,
            facade_reason,
            pending_user_input,
            runtime,
        ),
    ) {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(error)) => {
            let _ = refresh_compact_views(
                executor,
                opened,
                config,
                log,
                operation,
                call_id,
                compact_error_is_unadmitted(error.kind()),
            );
            config
                .store
                .save(&mut config.session)
                .map_err(CliFailure::new)?;
            let detail = format!("canonical compact failed: {error}");
            log_compact_finished_with_detail(
                log,
                config,
                compact_started_at,
                "failed",
                Some(&detail),
            );
            publish_compact_row(
                config,
                events,
                call_id,
                "failed",
                Some(detail.clone()),
                Some(elapsed_ms(compact_started_at)),
            );
            return Err(CliFailure::new(detail));
        }
        Err(error) => {
            config
                .store
                .save(&mut config.session)
                .map_err(CliFailure::new)?;
            let detail = format!("canonical compact executor failed: {error}");
            log_compact_finished_with_detail(
                log,
                config,
                compact_started_at,
                "failed",
                Some(&detail),
            );
            publish_compact_row(
                config,
                events,
                call_id,
                "failed",
                Some(detail.clone()),
                Some(elapsed_ms(compact_started_at)),
            );
            return Err(CliFailure::new(detail));
        }
    };
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Compact,
        HostBoundary::RuntimeReturned,
    );
    Ok((outcome, occupancy_before))
}

/// Classifies the canonical answer to the pass's dispatch, binding the
/// session to its successor or exact source.
#[allow(
    clippy::too_many_lines,
    reason = "Each canonical compaction outcome determines its own source restoration, notice, and reconciliation action."
)]
fn classify_compact_answer<P: plakous_host_protocol::ProtocolExtension>(
    run: &CompactRun<'_, P>,
    config: &mut HostConfig<P>,
    source: plakous::sessions::SessionLocator,
    outcome: CompactSessionOutcome,
    pending_user_input: Option<&UserInput>,
    occupancy_before: Option<u64>,
) -> CliResult<CompactAnswer> {
    let (executor, opened, log, reason, compact_started_at) = (
        run.executor,
        run.opened,
        run.log,
        run.reason,
        run.started_at,
    );
    let mut size_subject = None;
    let unadmitted = compact_answer_is_unadmitted(&outcome, source);
    let (pass, successor_fits, detail, compact_outcome) = match outcome {
        CompactSessionOutcome::Compacted {
            source: exact,
            target,
            summary,
            ..
        } if exact == source && opened.locator() == target => {
            config
                .session
                .bind_plakous_locator(target)
                .map_err(CliFailure::new)?;
            let successor_fits = match pending_user_input {
                Some(command) => query_context(
                    executor,
                    opened,
                    ContextQuery::WithPendingUserInput(command.clone()),
                )
                .is_ok_and(|projection| projection.disposition() == ContextDisposition::Fits),
                None => exact_pending_context_fits(executor, opened, &config.session),
            };
            let base = if reason == sessions::ProductCompactReason::Scheduled {
                COMPACT_COMMITTED_DETAIL
            } else {
                compact_committed_detail(successor_fits)
            };
            // The figures replace the bare "committed" when the successor
            // records them, and the overflow warning is kept beside them,
            // because "it compacted" and "it still does not fit" are two
            // different things an operator needs.
            let detail = match summary {
                Some(summary) if base == COMPACT_COMMITTED_DETAIL => {
                    compaction_summary_clause(summary)
                }
                Some(summary) => {
                    format!("{}; {base}", compaction_summary_clause(summary))
                }
                None => base.to_owned(),
            };
            let occupancy_after = unanchored_occupancy(executor, opened);
            // The record's own figures first: they are what telemetry reports,
            // and both sides are unanchored so they compare like for like.
            let (before, after) = match (
                summary
                    .and_then(|item| item.occupancy_tokens_before)
                    .or(occupancy_before),
                summary
                    .and_then(|item| item.occupancy_tokens_after)
                    .or(occupancy_after),
            ) {
                (Some(before), Some(after)) => (Some(before), Some(after)),
                _ => compact_row_token_pair(summary),
            };
            size_subject = compact_terminal_subject(before, after, true, successor_fits);
            (CompactPass::Committed, successor_fits, detail, "committed")
        }
        CompactSessionOutcome::Failed {
            source: exact,
            target,
            diagnostic,
            ..
        } if exact == source && opened.locator() == target => {
            config
                .session
                .bind_plakous_locator(target)
                .map_err(CliFailure::new)?;
            let note = compact_operator_note(COMPACT_FAILED_DETAIL, diagnostic.as_deref());
            (
                CompactPass::Failed { note: note.clone() },
                false,
                note,
                "failed",
            )
        }
        CompactSessionOutcome::Abandoned {
            source: exact,
            target,
            ..
        } if exact == source && opened.locator() == target => {
            config
                .session
                .bind_plakous_locator(target)
                .map_err(CliFailure::new)?;
            (
                CompactPass::Unchanged,
                false,
                COMPACT_FAILED_DETAIL.to_owned(),
                "failed",
            )
        }
        CompactSessionOutcome::NotNeeded { source: exact, .. } if exact == source => (
            CompactPass::Unchanged,
            false,
            COMPACT_NOT_NEEDED_DETAIL.to_owned(),
            "not_needed",
        ),
        CompactSessionOutcome::NoEligibleRange { source: exact, .. } if exact == source => {
            // The ladder ran out: no facet, no eligible whole group, and even
            // replacing every tool-result body left the projection over
            // capacity. That is the one case the protected wording is still
            // true for, so the operator gets it here rather than at the
            // failed turn, where placeholders had not been tried yet.
            let detail =
                if query_context(executor, opened, ContextQuery::Current).is_ok_and(|projection| {
                    projection.disposition() == ContextDisposition::ProtectedOverflow
                }) {
                    CONTEXT_CAPACITY_PROTECTED_GUIDANCE.to_owned()
                } else {
                    COMPACT_NO_ELIGIBLE_RANGE_DETAIL.to_owned()
                };
            (
                CompactPass::NothingEligible,
                false,
                detail,
                "no_eligible_range",
            )
        }
        CompactSessionOutcome::Unavailable { source: exact, .. } if exact == source => (
            CompactPass::Unchanged,
            false,
            COMPACT_UNAVAILABLE_DETAIL.to_owned(),
            "unavailable",
        ),
        CompactSessionOutcome::RecoveryRequired { diagnostic, .. } => {
            config
                .store
                .save(&mut config.session)
                .map_err(CliFailure::new)?;
            let detail =
                compact_operator_note(COMPACT_RECOVERY_REQUIRED_DETAIL, diagnostic.as_deref());
            log_compact_finished_with_detail(
                log,
                config,
                compact_started_at,
                "recovery_required",
                Some(&detail),
            );
            return Err(CliFailure::new(detail));
        }
        _ => {
            let detail = "canonical compact outcome contradicts its exact source";
            log_compact_finished_with_detail(
                log,
                config,
                compact_started_at,
                "failed",
                Some(detail),
            );
            return Err(CliFailure::new(detail));
        }
    };
    Ok(CompactAnswer {
        pass,
        successor_fits,
        detail,
        outcome: compact_outcome,
        size_subject,
        unadmitted,
    })
}

/// Settles the pass in the product: its views, the queue's drain, its
/// activity row and the operator notes.
#[allow(
    clippy::too_many_lines,
    reason = "Compaction completion reconciles the workflow before restoring the binding, context counters, and queue snapshot."
)]
fn finish_compact<P: plakous_host_protocol::ProtocolExtension>(
    run: &CompactRun<'_, P>,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    answer: CompactAnswer,
) -> CliResult<CompactPass> {
    let (executor, opened, events, log, reason, compact_started_at) = (
        run.executor,
        run.opened,
        run.events,
        run.log,
        run.reason,
        run.started_at,
    );
    let (operation, call_id) = (&run.operation, &run.call_id);
    let CompactAnswer {
        pass,
        successor_fits,
        detail,
        outcome: compact_outcome,
        size_subject,
        unadmitted,
    } = answer;
    let settled_row = refresh_compact_views(
        executor, opened, config, log, operation, call_id, unadmitted,
    );
    let compacted = pass == CompactPass::Committed;
    let resumes_queue = matches!(
        reason,
        sessions::ProductCompactReason::Scheduled
            | sessions::ProductCompactReason::CapacityRecovery
    );
    let interrupted = watch.observe_now();
    if interrupted {
        // The single dispatch is committed above. An interrupt
        // never redispatches and never resumes automatic drain; it parks the
        // queue so the next paid step is an explicit operator decision.
        config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
    } else if resumes_queue {
        config.session.queue_drain_state = if reason == sessions::ProductCompactReason::Scheduled {
            scheduled_compact_queue_drain_state(&pass)
        } else {
            compact_queue_drain_state(compacted, successor_fits)
        };
    }
    config
        .store
        .save(&mut config.session)
        .map_err(CliFailure::new)?;
    {
        // The runtime fact settles the request; this save holds host intents.
        // These two observations intentionally share a fence.
        observe_boundary(
            config.runtime_factory.as_ref(),
            HostWorkflow::Compact,
            HostBoundary::ProductPublished,
        );
        observe_boundary(
            config.runtime_factory.as_ref(),
            HostWorkflow::Compact,
            HostBoundary::ProductCommitted,
        );
    }
    let _ = events.send(ContentEvent::Status {
        phase: "idle".into(),
        queue_len: config.session.pending_queue.len(),
        detail: None,
    });
    publish_compact_row(
        config,
        events,
        call_id,
        compact_terminal_state(compacted, successor_fits),
        size_subject,
        Some(elapsed_ms(compact_started_at)),
    );
    // The preview includes qualification time; the feed owns settled timing.
    if let Some(row) = settled_row {
        if let TranscriptRow::Tool(tool) = &row {
            config.session.upsert_tool_activity(tool.clone());
        }
        let _ = events.send(ContentEvent::Row(row));
    }
    // The structured Compact row is the successful outcome. Show explanatory
    // notes only for no-change/failure cases and quiet providers; repeating a
    // successful detail exposed internal prepared-input accounting below it.
    // A scheduled pass with nothing eligible or a definite failure leaves its
    // one note to the caller, which knows whether the prompt is sent.
    let caller_owns_note = reason == sessions::ProductCompactReason::Scheduled
        && matches!(
            pass,
            CompactPass::NothingEligible | CompactPass::Failed { .. }
        );
    if !caller_owns_note
        && (!compacted
            && matches!(
                reason,
                sessions::ProductCompactReason::Manual | sessions::ProductCompactReason::Scheduled
            )
            || detail.contains("provider went quiet"))
    {
        // Plain sentence for the operator. `detail` stays the session-log diagnostic.
        let _ = events.send(ContentEvent::Row(TranscriptRow::info(
            NoticeScope::Compaction,
            manual_compact_operator_text(&detail),
        )));
    }
    if interrupted {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Compaction,
            "interrupt observed during compaction; the dispatched operation was committed and automatic drain is paused"
                ,
        ));
    }
    log_compact_finished_with_detail(
        log,
        config,
        compact_started_at,
        compact_outcome,
        Some(detail.as_str()),
    );
    if successor_fits || !compacted {
        let _ = events.send(ContentEvent::ContextCapacityRecovery(None));
    }
    emit_compact_meter(executor, opened, config, events);
    Ok(pass)
}

pub(super) fn exact_pending_context_fits(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    session: &SessionRecord,
) -> bool {
    let query = queue::next_drainable(session).map_or(ContextQuery::Current, |item| {
        ContextQuery::WithPendingUserInput(item.user_input(&item.text))
    });
    query_context(executor, opened, query)
        .is_ok_and(|projection| projection.disposition() == ContextDisposition::Fits)
}

fn compact_call_id(
    operation: &str,
    source: plakous::sessions::SessionLocator,
) -> CliResult<String> {
    // The runtime workflow digest is also the projector row's identity.
    let workflow = plakous::model::Digest::sha256(
        plakous::model::DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/session-compaction-workflow-v1",
            operation.as_bytes(),
            source.session_id().as_uuid().as_bytes(),
            source.branch_id().as_uuid().as_bytes(),
            source.checkpoint_id().as_uuid().as_bytes(),
        ],
    )
    .map_err(|_| CliFailure::new("compact workflow identity is invalid"))?;
    Ok(sessions::projector::compaction_call_id(workflow.bytes()))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn compact_answer_is_unadmitted(
    outcome: &CompactSessionOutcome,
    source: plakous::sessions::SessionLocator,
) -> bool {
    // The post-projection fact check, not the reply variant, guards removal.
    // Unavailable proves no admission; a lost reply is OutcomeUnknown instead.
    matches!(outcome,
        CompactSessionOutcome::NotNeeded { source: exact, .. }
            | CompactSessionOutcome::NoEligibleRange { source: exact, .. }
            | CompactSessionOutcome::Unavailable { source: exact, .. }
            if *exact == source)
}

fn compact_error_is_unadmitted(kind: AgentSessionsErrorKind) -> bool {
    // OutcomeUnknown and other errors do not prove that no commit occurred.
    matches!(
        kind,
        AgentSessionsErrorKind::Busy | AgentSessionsErrorKind::StaleLocator
    )
}

pub(super) const fn compact_may_resume_queue(
    capacity_recovery: bool,
    attached_one_shot: bool,
    drain: SessionQueueDrainState,
) -> bool {
    capacity_recovery
        && !attached_one_shot
        && matches!(drain, SessionQueueDrainState::LiveAutoDrain)
}

pub(super) const fn compact_queue_drain_state(
    compacted: bool,
    exact_pending_context_fits: bool,
) -> SessionQueueDrainState {
    if compacted && exact_pending_context_fits {
        SessionQueueDrainState::LiveAutoDrain
    } else {
        SessionQueueDrainState::RestoredPaused
    }
}

#[cfg(test)]
mod tests;
