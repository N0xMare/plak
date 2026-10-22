//! Manual and scheduled compaction and capacity recovery.

use std::time::Instant;

use plakous::agent::UserInput;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessionsErrorKind, CompactReason, CompactSessionOutcome, ContextQuery, SessionOperationId,
};

use crate::agent::AttachedCodingSession;
use crate::context::{
    ContextDisposition, occupancy_requires_scheduled_compact, scheduled_compact_should_follow_on,
};
use crate::interrupt::{InterruptWatch, run_watched};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, QueueDrainState as SessionQueueDrainState, SessionRecord};
use crate::{CliFailure, CliResult};

use super::events::{
    compact_row_token_pair, compact_terminal_state, compact_terminal_subject,
    compaction_summary_clause, publish_compact_activity,
};
use super::logging::{
    compact_reason_label, log_compact_finished, log_compact_finished_with_detail,
    log_compact_started, log_ev,
};
use super::rebind::interrupted_before_dispatch;
use super::startup::host_runtime_binding;
#[cfg(any(test, feature = "test-support"))]
use super::startup::observe_test_boundary;
use super::usage::{emit_context_projection, last_enrolled_source_bytes, query_context};
use super::{
    COMPACT_COMMITTED_DETAIL, COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL, COMPACT_FAILED_DETAIL,
    COMPACT_NO_ELIGIBLE_RANGE_DETAIL, COMPACT_NOT_NEEDED_DETAIL, COMPACT_PROVIDER_QUIET_NOTE,
    COMPACT_RECOVERY_REQUIRED_DETAIL, COMPACT_UNAVAILABLE_DETAIL,
    CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE, CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE,
    CONTEXT_CAPACITY_PROTECTED_GUIDANCE, CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE, HostConfig,
    HostEvent, Sender, TestRuntimeRole, queue,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow};
use crate::sessions::{NoticeScope, TranscriptRow};

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

/// What an idle scheduled pass leaves for the prompt it ran before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScheduledPrompt {
    /// The prompt goes to turn admission. `over_window` says its remeasured
    /// pending projection overflows, so its turn will withhold it.
    /// `drop_mail` says the decision was made on the bare prompt because its
    /// child-mail batch did not fit; the batch stays unread.
    Send { over_window: bool, drop_mail: bool },
    /// The prompt stays queued and drain is paused.
    Parked,
}

/// Runs idle scheduled compact, then at most one new-op follow-on.
///
/// Every pass qualifies, folds and remeasures on `command`, the exact input
/// the host would admit (the prompt with any child-mail batch, and its
/// images). `bare` is the prompt without a freshly selected batch: when the batch cannot be
/// made to fit, the send-or-park decision falls back to it and the batch is
/// dropped, so a prompt that fits is never parked behind its own mail.
/// Returns whether the prompt is sent or parked.
pub(super) fn run_scheduled_compaction_until_fits<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    log: Option<&SessionLog>,
    command: &UserInput,
    bare: Option<&UserInput>,
) -> CliResult<ScheduledPrompt> {
    run_scheduled_compaction_until_fits_with(
        watch,
        config,
        events,
        last_enrolled_source_bytes(opened),
        bare.is_some(),
        |config| {
            handle_compact(
                executor,
                watch,
                config,
                opened,
                stderr_buf,
                events,
                log,
                sessions::ProductCompactReason::Scheduled,
                Some(command),
            )
        },
        |use_bare| {
            let input = bare.filter(|_| use_bare).unwrap_or(command);
            query_context(
                executor,
                opened,
                ContextQuery::WithPendingUserInput(input.clone()),
            )
        },
    )
}

/// Whether a remeasured prompt goes to admission without another fold.
fn scheduled_prompt_sendable(projection: crate::context::ContextProjection) -> bool {
    projection.disposition() == ContextDisposition::Fits
        && !projection.observation_headroom_requires_compaction()
}

fn run_scheduled_compaction_until_fits_with<P: plakous_host_protocol::ProtocolExtension>(
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    enrolled_source_bytes: u64,
    bare_fallback: bool,
    mut compact: impl FnMut(&mut HostConfig<P>) -> CliResult<CompactPass>,
    mut project_pending: impl FnMut(bool) -> CliResult<crate::context::ContextProjection>,
) -> CliResult<ScheduledPrompt> {
    let mut follow_ons_used = 0u32;
    loop {
        let pass = compact(config)?;
        if watch.observe_now() {
            // an interrupted scheduled compaction never follows on.
            let _ = events.notice(TranscriptRow::error(NoticeScope::Compaction,
                "scheduled compaction was interrupted; the prompt remains queued and drain stays paused"
                    ,
            ));
            return Ok(ScheduledPrompt::Parked);
        }
        if config.session.queue_drain_state != SessionQueueDrainState::LiveAutoDrain {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Compaction,
                SCHEDULED_COMPACT_PARKED_NOTE,
            ));
            return Ok(ScheduledPrompt::Parked);
        }
        let after = project_pending(false)?;
        emit_context_projection(
            events,
            &after,
            config.session.context_counters(),
            enrolled_source_bytes,
        );
        // The bare prompt, measured only once its mail-bearing command has
        // run out of folds this pass may pay for.
        let mut bare = |admits: fn(crate::context::ContextProjection) -> bool| {
            if bare_fallback {
                project_pending(true).map(|projection| admits(projection).then_some(projection))
            } else {
                Ok(None)
            }
        };
        match pass {
            CompactPass::NothingEligible => {
                // Nothing was paid and nothing changed, so another pass would
                // find the same nothing. The prompt goes to ordinary turn
                // admission. One that does not fit is withheld by the turn,
                // which restores the draft with the resend-shorter remedy;
                // that remedy is then the one note, so "sent as is" is said
                // only of a prompt that is.
                let mut over_window =
                    crate::context::should_schedule_compaction(after.disposition());
                let drop_mail = over_window && bare_fallback;
                if drop_mail {
                    over_window = bare(|_| true)?.is_none_or(|projection| {
                        crate::context::should_schedule_compaction(projection.disposition())
                    });
                }
                if !over_window {
                    let _ = events.notice(TranscriptRow::info(
                        NoticeScope::Compaction,
                        with_mail_clause(SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE, drop_mail),
                    ));
                }
                return Ok(ScheduledPrompt::Send {
                    over_window,
                    drop_mail,
                });
            }
            CompactPass::Failed { note } => {
                // A definite failure is deterministic for this source: a
                // follow-on or a resume would pay for the same refused fold.
                // The feed owns the failure line. Only the product's send or
                // park decision authors a notice intent.
                let fits = |projection: crate::context::ContextProjection| {
                    projection.disposition() == ContextDisposition::Fits
                };
                let drop_mail = !fits(after);
                if !drop_mail || bare(fits)?.is_some() {
                    let _ = events.send(HostEvent::Row(TranscriptRow::info(
                        NoticeScope::Compaction,
                        note,
                    )));
                    let _ = events.notice(TranscriptRow::info(
                        NoticeScope::Compaction,
                        with_mail_clause(SCHEDULED_COMPACT_FAILED_FITS_NOTE, drop_mail),
                    ));
                    return Ok(ScheduledPrompt::Send {
                        over_window: false,
                        drop_mail,
                    });
                }
                config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                config
                    .store
                    .save(&mut config.session)
                    .map_err(CliFailure::new)?;
                let _ = events.send(HostEvent::Row(TranscriptRow::info(
                    NoticeScope::Compaction,
                    note,
                )));
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Compaction,
                    format!(
                        "{}: {SCHEDULED_COMPACT_PARKED_NOTE}",
                        after.disposition().label()
                    ),
                ));
                return Ok(ScheduledPrompt::Parked);
            }
            CompactPass::Committed | CompactPass::Unchanged => {}
        }
        if scheduled_prompt_sendable(after) {
            return Ok(ScheduledPrompt::Send {
                over_window: false,
                drop_mail: false,
            });
        }
        // Only a pass that changed the head can make a follow-on worth paying
        // for; an unchanged one would measure the same again.
        let observation_headroom_low = after.observation_headroom_requires_compaction();
        let still_triggered = occupancy_requires_scheduled_compact(after);
        if pass == CompactPass::Committed
            && (((still_triggered || observation_headroom_low)
                && follow_ons_used < crate::context::SCHEDULED_COMPACT_FOLLOW_ON_LIMIT)
                || scheduled_compact_should_follow_on(follow_ons_used, after.disposition()))
        {
            let _ = events.notice(TranscriptRow::info(NoticeScope::Compaction,
                "successor still meets the idle compact trigger; compacting once more with a new operation"
                    ,
            ));
            follow_ons_used += 1;
            continue;
        }
        if bare(scheduled_prompt_sendable)?.is_some() {
            let _ = events.notice(TranscriptRow::info(
                NoticeScope::Compaction,
                SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE,
            ));
            return Ok(ScheduledPrompt::Send {
                over_window: false,
                drop_mail: true,
            });
        }
        config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
        config
            .store
            .save(&mut config.session)
            .map_err(CliFailure::new)?;
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Compaction,
            format!(
                "{} after scheduled compaction; prompt remains queued",
                after.disposition().label()
            ),
        ));
        return Ok(ScheduledPrompt::Parked);
    }
}

/// Appends why a prompt goes without its child-mail batch.
fn with_mail_clause(note: &str, drop_mail: bool) -> String {
    if drop_mail {
        format!("{note}; {SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE}")
    } else {
        note.to_owned()
    }
}

/// Operator note when a scheduled pass parked its prompt without a compacted
/// checkpoint.
const SCHEDULED_COMPACT_PARKED_NOTE: &str = "scheduled compaction produced no compacted checkpoint for the queued prompt; the prompt remains queued and drain is paused. Review the preceding compaction result. After correcting the cause, resume the queue with Tab then r in the TUI or resume_queue in plak agent; retrying compaction is a new paid operation. You can also use /clear in the TUI or clear in plak agent to start a fresh conversation";

/// Product decision following the feed-owned failed fold outcome.
pub(super) const SCHEDULED_COMPACT_FAILED_FITS_NOTE: &str =
    "the prompt fits without compaction and is sent as is";

/// Clause for a prompt sent without the child-mail batch that did not fit.
const SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE: &str =
    "sub-agent mail does not fit with it and stays unread for the next prompt";

/// Operator note when compaction made room for the prompt but not its mail.
pub(super) const SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE: &str = "the prompt fits after scheduled compaction and is sent; sub-agent mail does not fit with it and stays unread for the next prompt";

/// Operator note when a scheduled pass found nothing to compact.
pub(super) const SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE: &str =
    "nothing could be compacted before this prompt; it is sent as is";

/// Publishes the §5.4 activity for a compaction, and clears it.
///
/// A compaction — manual `/compact` or the capacity-triggered scheduled one — is one
/// paid round-trip the operator waits on, and the host published nothing
/// while it ran: the HUD stayed empty for the whole dispatch and
/// the TUI's `Compacting…` branch was unreachable. The
/// row has zero height unless the host says it is working, so busy is set
/// with the label and cleared on every exit arm — including the refusals
/// `handle_compact` returns.
pub(super) fn observe_compact<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    queue_len: usize,
    running: bool,
) {
    if running {
        let _ = events.send(HostEvent::SetBusy(true));
        let _ = events.send(HostEvent::Status {
            phase: "compacting".into(),
            queue_len,
            detail: None,
        });
    } else {
        let _ = events.send(HostEvent::Status {
            phase: "idle".into(),
            queue_len,
            detail: None,
        });
        let _ = events.send(HostEvent::SetBusy(false));
    }
}

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
    let qualification = pending_user_input
        .as_ref()
        .map_or(ContextQuery::Current, |input| {
            ContextQuery::WithPendingUserInput(input.clone())
        });
    // Manual compact is eligible-range gated. Scheduled compact
    // skips only when occupancy gauge Fits below the capacity-derived idle trigger.
    // Capacity recovery must still compact a Fits envelope.
    if reason == sessions::ProductCompactReason::Scheduled {
        if let Ok(projection) = query_context(executor, opened, qualification) {
            if projection.disposition() == ContextDisposition::Fits
                && !occupancy_requires_scheduled_compact(projection)
                && !projection.observation_headroom_requires_compaction()
            {
                publish_compact_row(
                    config,
                    events,
                    &call_id,
                    "done",
                    Some("already fits".to_owned()),
                    Some(elapsed_ms(compact_started_at)),
                );
                log_compact_finished(log, config, compact_started_at, "not_needed");
                emit_compact_meter(executor, opened, config, events);
                return Ok(CompactPass::Unchanged);
            }
        }
    }
    if (reason == sessions::ProductCompactReason::Manual
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
    stderr_buf.clear();
    let runtime = host_runtime_binding(config, stderr_buf, TestRuntimeRole::Compact)?;
    let operation_id = SessionOperationId::new(&operation)
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
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Compact,
        TestHostBoundary::Prepared,
    );
    let occupancy_before = unanchored_occupancy(executor, opened);
    publish_compact_row(config, events, &call_id, "running", None, None);
    let outcome = match run_watched(
        executor,
        watch,
        opened.compact_from(
            operation_id,
            source,
            facade_reason,
            pending_user_input.clone(),
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
                &operation,
                &call_id,
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
                &call_id,
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
                &call_id,
                "failed",
                Some(detail.clone()),
                Some(elapsed_ms(compact_started_at)),
            );
            return Err(CliFailure::new(detail));
        }
    };
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Compact,
        TestHostBoundary::RuntimeReturned,
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
            let successor_fits = match pending_user_input.as_ref() {
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
    let settled_row = refresh_compact_views(
        executor, opened, config, log, &operation, &call_id, unadmitted,
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
    #[cfg(any(test, feature = "test-support"))]
    {
        // The runtime fact settles the request; this save holds host intents.
        // These two observations intentionally share a fence.
        observe_test_boundary(
            config.test_injection.as_deref(),
            TestHostWorkflow::Compact,
            TestHostBoundary::ProductPublished,
        );
        observe_test_boundary(
            config.test_injection.as_deref(),
            TestHostWorkflow::Compact,
            TestHostBoundary::ProductCommitted,
        );
    }
    let _ = events.send(HostEvent::Status {
        phase: "idle".into(),
        queue_len: config.session.pending_queue.len(),
        detail: None,
    });
    publish_compact_row(
        config,
        events,
        &call_id,
        compact_terminal_state(compacted, successor_fits),
        size_subject,
        Some(elapsed_ms(compact_started_at)),
    );
    // The preview includes qualification time; the feed owns settled timing.
    if let Some(row) = settled_row {
        if let TranscriptRow::Tool(tool) = &row {
            config.session.upsert_tool_activity(tool.clone());
        }
        let _ = events.send(HostEvent::Row(row));
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
        let _ = events.send(HostEvent::Row(TranscriptRow::info(
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
        let _ = events.send(HostEvent::ContextCapacityRecovery(None));
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

fn refresh_compact_views<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    config: &mut HostConfig<P>,
    log: Option<&SessionLog>,
    operation: &str,
    call_id: &str,
    unadmitted: bool,
) -> Option<TranscriptRow> {
    let views = executor
        .block_on(super::projection::project_to_tail(
            opened.sessions(),
            &config.store,
            &config.session.id,
            opened.locator(),
        ))
        .unwrap_or_else(|_| Err("application execution failed".to_owned().into()))
        .map_err(String::from);
    let settled_row = views.as_ref().ok().and_then(|views| {
        views.rows.values().find(|row| matches!(row,
            TranscriptRow::Tool(tool) if tool.call_id == call_id && tool.finished_at_ms.is_some()
        )).cloned()
    });
    apply_compact_views(config, log, operation, unadmitted, views);
    settled_row
}

fn apply_compact_views<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    log: Option<&SessionLog>,
    operation: &str,
    unadmitted: bool,
    views: Result<sessions::projector::SessionViews, String>,
) {
    let result = views.and_then(|views| {
        config.session.root_turn_count = views.state.counters.root_turns;
        config.session.tool_call_count = views.state.counters.tool_calls;
        config.session.compaction_count = views.state.counters.compactions;
        if unadmitted {
            config
                .store
                .discard_unadmitted_workflow(&mut config.session, operation)?;
        }
        Ok(())
    });
    if let Err(error) = result {
        log_ev(
            log,
            config,
            SessionEvent::new("projector.deferred").str("error", &error),
        );
    }
}

fn publish_compact_row<P: plakous_host_protocol::ProtocolExtension>(
    config: &mut HostConfig<P>,
    events: &Sender<P>,
    call_id: &str,
    state: &str,
    subject: Option<String>,
    duration_ms: Option<u64>,
) {
    publish_compact_activity(
        &mut config.session,
        events,
        call_id,
        state,
        subject,
        duration_ms,
    );
}

pub(super) const fn compact_committed_detail(successor_fits: bool) -> &'static str {
    if successor_fits {
        COMPACT_COMMITTED_DETAIL
    } else {
        COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL
    }
}

pub(super) fn compact_operator_detail(base: &str, diagnostic: Option<&str>) -> String {
    match diagnostic {
        Some(text) if !text.is_empty() => format!("{base}: {text}"),
        _ => base.to_owned(),
    }
}

/// Transcript copy: never dump a Responses deadline diagnostic into the TUI.
pub(super) fn compact_operator_note(base: &str, diagnostic: Option<&str>) -> String {
    if diagnostic.is_some_and(compact_diagnostic_is_provider_quiet) {
        COMPACT_PROVIDER_QUIET_NOTE.to_owned()
    } else {
        compact_operator_detail(base, diagnostic)
    }
}

/// Core wording for a paid fold that measured no shrink. The session log keeps
/// this diagnostic; the transcript uses `manual_compact_operator_text`.
const COMPACT_SHRINK_REFUSAL_DIAGNOSTIC: &str =
    "compaction successor did not prove net input shrink";

const COMPACT_SHRINK_REFUSAL_NOTE: &str =
    "nothing was compacted: the summary would not be smaller than the conversation it replaces";

/// Operator sentence for a manual compact note. A shrink refusal is told in
/// plain words; every other detail is unchanged. The session log keeps the
/// diagnostic string.
fn manual_compact_operator_text(detail: &str) -> String {
    if detail.contains(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC) {
        COMPACT_SHRINK_REFUSAL_NOTE.to_owned()
    } else {
        detail.to_owned()
    }
}

fn compact_diagnostic_is_provider_quiet(diagnostic: &str) -> bool {
    diagnostic.contains("deadline elapsed") || diagnostic.contains("no authoritative completion")
}

fn unanchored_occupancy(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
) -> Option<u64> {
    query_context(executor, opened, ContextQuery::Current)
        .ok()?
        .unanchored_occupied_tokens()
}

fn emit_compact_meter<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    config: &HostConfig<P>,
    events: &Sender<P>,
) {
    if let Ok(projection) = query_context(executor, opened, ContextQuery::Current) {
        emit_context_projection(
            events,
            &projection,
            config.session.context_counters(),
            last_enrolled_source_bytes(opened),
        );
    }
}

pub(super) const fn context_capacity_recovery_guidance(
    recovery: plakous_host_protocol::ContextCapacityRecovery,
    exact_source: bool,
    native_compact_facet: bool,
) -> &'static str {
    use plakous_host_protocol::ContextCapacityRecovery;
    match recovery {
        ContextCapacityRecovery::ResendShorter => {
            "prompt withheld at the context limit; shorten the restored draft and resend explicitly"
        }
        ContextCapacityRecovery::Protected => CONTEXT_CAPACITY_PROTECTED_GUIDANCE,
        ContextCapacityRecovery::Compactable if !exact_source => {
            CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE
        }
        ContextCapacityRecovery::Compactable if native_compact_facet => {
            CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE
        }
        ContextCapacityRecovery::Compactable => CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE,
    }
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

/// A committed scheduled compact, one that found nothing to compact, or a
/// definite failure leaves drain live so the caller can remeasure the queued
/// prompt. A scheduled pass that was abandoned or unavailable still parks.
pub(super) const fn scheduled_compact_queue_drain_state(
    pass: &CompactPass,
) -> SessionQueueDrainState {
    match pass {
        CompactPass::Committed | CompactPass::NothingEligible | CompactPass::Failed { .. } => {
            SessionQueueDrainState::LiveAutoDrain
        }
        CompactPass::Unchanged => SessionQueueDrainState::RestoredPaused,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;
    use crate::runtime::test_kit::ProbeIntent;

    use super::*;
    use crate::runtime::*;

    use crate::runtime::events::*;

    use crate::context::{
        ContextDisposition, scheduled_compaction_disabled_note,
        scheduled_compaction_parks_when_disabled, should_schedule_compaction,
    };
    use crate::interrupt::{InterruptWatch, InterruptWatchState};
    use crate::runtime::test_support::*;
    use crate::runtime::usage::*;
    use crate::sessions::{self, QueueDrainState as SessionQueueDrainState};
    use plakous::agent::CompactionEventOutcome;
    use plakous::application::ApplicationExecutor;
    use plakous::subscriptions::NativeProvider;
    use std::cell::RefCell;

    #[test]
    fn scheduled_compaction_without_a_token_trigger_requires_overflow() {
        assert!(should_schedule_compaction(
            ContextDisposition::WouldOverflow
        ));
        assert!(should_schedule_compaction(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        // The placeholder rung acts on a protected-only tail without a facet.
        assert!(should_schedule_compaction(
            ContextDisposition::ProtectedOverflow
        ));
        assert!(!scheduled_compaction_parks_when_disabled(
            ContextDisposition::Fits
        ));
        assert!(scheduled_compaction_parks_when_disabled(
            ContextDisposition::WouldOverflow
        ));
        for disposition in [
            ContextDisposition::Fits,
            ContextDisposition::Unavailable,
            ContextDisposition::RecoveryRequired,
        ] {
            assert!(!should_schedule_compaction(disposition));
        }
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        // Occupancy gauge, not envelope-only: 38% envelope would skip; 88%
        // provider occupancy must schedule.
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert!(!should_schedule_compaction(ContextDisposition::Fits));
        assert_eq!(
            scheduled_compaction_disabled_note(ContextDisposition::WouldOverflow),
            "authoritative context would overflow; scheduled compaction is disabled"
        );
        assert_eq!(
            scheduled_compaction_disabled_note(ContextDisposition::Fits),
            "scheduled compaction is disabled at the context trigger"
        );
        assert!(native_compact_facet(NativeProvider::Codex));
        assert!(!native_compact_facet(NativeProvider::Xai));
        assert!(!native_compact_facet(NativeProvider::Claude));
        // OpenRouter is not a NativeProvider; its compact host uses the
        // coding binding via api_compaction_runtime_binding.
    }

    /// Protected overflow asks for capacity recovery on every route.
    ///
    /// The facet used to decide this, which made `/compact` refuse before the
    /// ladder ran while the capacity guidance promised it would replace. The
    /// "only protected material" wording now belongs to the exhausted ladder.
    #[test]
    fn protected_overflow_asks_for_capacity_recovery_on_every_route() {
        use plakous_host_protocol::ContextCapacityRecovery::{
            Compactable, Protected, ResendShorter,
        };

        assert_eq!(
            compact_reason_for_disposition(ContextDisposition::WouldOverflow),
            sessions::ProductCompactReason::CapacityRecovery
        );
        assert_eq!(
            compact_reason_for_disposition(ContextDisposition::ProtectedOverflow),
            sessions::ProductCompactReason::CapacityRecovery
        );
        assert_eq!(
            compact_reason_for_disposition(ContextDisposition::ProtectedOverflow),
            sessions::ProductCompactReason::CapacityRecovery,
            "the placeholder rung can act here, so the key must not refuse first"
        );
        assert_eq!(
            compact_reason_for_disposition(ContextDisposition::Fits),
            sessions::ProductCompactReason::Manual
        );
        assert_eq!(
            compact_reason_for_disposition(ContextDisposition::Unavailable),
            sessions::ProductCompactReason::Manual
        );
        assert_eq!(
            live_capacity_recovery(ContextDisposition::ProtectedOverflow, false),
            Some(Protected)
        );
        assert_eq!(
            live_capacity_recovery(ContextDisposition::ProtectedOverflow, true),
            None
        );
        assert_eq!(
            live_capacity_recovery(ContextDisposition::WouldOverflow, true),
            None
        );
        assert_eq!(
            context_capacity_recovery_guidance(Compactable, true, false),
            CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE
        );
        assert_eq!(
            context_capacity_recovery_guidance(Compactable, true, true),
            CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE
        );
        assert_eq!(
            context_capacity_recovery_guidance(Protected, true, true),
            CONTEXT_CAPACITY_PROTECTED_GUIDANCE
        );
        assert_eq!(
            context_capacity_recovery_guidance(Compactable, false, true),
            CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE
        );
        assert!(
            context_capacity_recovery_guidance(ResendShorter, true, true)
                .contains("shorten the restored draft")
        );
        assert!(
            !COMPACT_UNAVAILABLE_DETAIL.contains("unavailable on this route"),
            "Unavailable compact copy must describe this attempt, not the route"
        );
        assert!(COMPACT_UNAVAILABLE_DETAIL.contains("retry is a new paid operation"));
        assert_eq!(compact_committed_detail(true), COMPACT_COMMITTED_DETAIL);
        assert_eq!(
            compact_committed_detail(false),
            COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL
        );
        assert!(compact_committed_detail(false).contains("still overflows"));
        assert_eq!(format_compact_tokens(199_700), "199.7k");
        assert_eq!(format_compact_tokens(178_075), "178.1k");
        assert_eq!(format_compact_tokens(400), "400");
        assert_eq!(
            compact_terminal_subject(Some(199_700), Some(178_075), true, false).as_deref(),
            Some("199.7k → 178.1k · still over capacity")
        );
    }

    #[test]
    fn capacity_recovery_queue_resumes_only_after_compacted_and_fresh_exact_fit() {
        assert!(compact_may_resume_queue(
            true,
            false,
            SessionQueueDrainState::LiveAutoDrain
        ));
        assert!(!compact_may_resume_queue(
            true,
            true,
            SessionQueueDrainState::LiveAutoDrain
        ));
        assert!(!compact_may_resume_queue(
            false,
            false,
            SessionQueueDrainState::LiveAutoDrain
        ));
        assert_eq!(
            compact_queue_drain_state(true, true),
            SessionQueueDrainState::LiveAutoDrain
        );
        for (compacted, fits) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                compact_queue_drain_state(compacted, fits),
                SessionQueueDrainState::RestoredPaused
            );
        }
        for pass in [
            CompactPass::Committed,
            CompactPass::NothingEligible,
            CompactPass::Failed {
                note: String::new(),
            },
        ] {
            assert_eq!(
                scheduled_compact_queue_drain_state(&pass),
                SessionQueueDrainState::LiveAutoDrain
            );
        }
        assert_eq!(
            scheduled_compact_queue_drain_state(&CompactPass::Unchanged),
            SessionQueueDrainState::RestoredPaused
        );
    }

    /// A scheduled pass that finds nothing to compact never pays for a
    /// follow-on, and the prompt goes to ordinary turn admission whether or
    /// not it fits: the turn withholds an over-window prompt with the
    /// resend-shorter remedy, never the misleading "retry compaction" guidance.
    #[test]
    fn a_scheduled_pass_with_nothing_eligible_sends_the_prompt() {
        let triggered = crate::context::ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(99_000),
            100_000,
            100_000,
            None,
        )
        .with_anchored_occupied_tokens(99_000);
        assert!(triggered.observation_headroom_requires_compaction());
        assert!(occupancy_requires_scheduled_compact(triggered));
        let note = SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE;
        for projection in [
            triggered,
            triggered.with_disposition(ContextDisposition::WouldOverflow),
            triggered.with_disposition(ContextDisposition::ProtectedOverflow),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut config, _) = fresh_injected_attached_host(dir.path());
            queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
            config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
            let (events, rx) = host_event_sink();
            let (_actions, actions_rx) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let interrupt_state = RefCell::new(InterruptWatchState::default());
            let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
            let mut passes = 0;
            let admitted = run_scheduled_compaction_until_fits_with(
                watch,
                &mut config,
                &events,
                0,
                false,
                |config| {
                    passes += 1;
                    config.session.queue_drain_state =
                        scheduled_compact_queue_drain_state(&CompactPass::NothingEligible);
                    Ok(CompactPass::NothingEligible)
                },
                |_| Ok(projection),
            )
            .unwrap();
            let over_window = projection.disposition() != ContextDisposition::Fits;
            assert_eq!(
                admitted,
                ScheduledPrompt::Send {
                    over_window,
                    drop_mail: false
                },
                "{:?}",
                projection.disposition()
            );
            assert_eq!(passes, 1, "nothing eligible never follows on");
            assert_eq!(
                config.session.queue_drain_state,
                SessionQueueDrainState::LiveAutoDrain
            );
            let notes: Vec<String> = rx
                .try_iter()
                .filter_map(|event| match event {
                    HostEvent::Row(TranscriptRow::Notice(
                        plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                    )) => Some(text),
                    _ => None,
                })
                .collect();
            // A withheld prompt's resend remedy is its one note.
            assert_eq!(
                notes.iter().any(|text| text == note),
                !over_window,
                "{notes:?}"
            );
            assert!(
                !notes
                    .iter()
                    .any(|text| text.contains("retrying compaction is a new paid operation")),
                "{notes:?}"
            );
        }
    }

    /// A definite scheduled failure is deterministic for its source. A prompt
    /// that fits is sent without a follow-on; one that does not parks with
    /// the paid-retry note.
    #[test]
    fn a_definite_scheduled_failure_sends_a_fitting_prompt_and_parks_one_that_does_not() {
        let triggered = crate::context::ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(99_000),
            100_000,
            100_000,
            None,
        )
        .with_anchored_occupied_tokens(99_000);
        for (projection, expected) in [
            (
                triggered,
                ScheduledPrompt::Send {
                    over_window: false,
                    drop_mail: false,
                },
            ),
            (
                triggered.with_disposition(ContextDisposition::WouldOverflow),
                ScheduledPrompt::Parked,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (mut config, _) = fresh_injected_attached_host(dir.path());
            queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
            config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
            let (events, rx) = host_event_sink();
            let (_actions, actions_rx) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let interrupt_state = RefCell::new(InterruptWatchState::default());
            let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
            let mut passes = 0;
            let outcome = run_scheduled_compaction_until_fits_with(
                watch,
                &mut config,
                &events,
                0,
                false,
                |config| {
                    passes += 1;
                    let pass = CompactPass::Failed {
                        note: COMPACT_FAILED_DETAIL.to_owned(),
                    };
                    config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
                    Ok(pass)
                },
                |_| Ok(projection),
            )
            .unwrap();
            assert_eq!(outcome, expected, "{:?}", projection.disposition());
            assert_eq!(passes, 1, "a definite failure never follows on");
            let notes: Vec<String> = rx
                .try_iter()
                .filter_map(|event| match event {
                    HostEvent::Row(TranscriptRow::Notice(
                        plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                    )) => Some(text),
                    _ => None,
                })
                .collect();
            let parked = expected == ScheduledPrompt::Parked;
            assert_eq!(
                config.session.queue_drain_state,
                if parked {
                    SessionQueueDrainState::RestoredPaused
                } else {
                    SessionQueueDrainState::LiveAutoDrain
                }
            );
            assert_eq!(
                notes
                    .iter()
                    .any(|text| text.contains("retrying compaction is a new paid operation")),
                parked,
                "{notes:?}"
            );
            if !parked {
                assert_eq!(
                    notes,
                    [COMPACT_FAILED_DETAIL, SCHEDULED_COMPACT_FAILED_FITS_NOTE]
                );
            }
            assert!(events.take_notice_intents().iter().all(|intent| {
                !matches!(&intent.row, TranscriptRow::Notice(notice) if notice.text.contains(COMPACT_FAILED_DETAIL))
            }), "a recorded failure is presentation only, not another notice intent");
        }
    }

    /// Runs the scheduled loop over scripted passes, with the mail-bearing
    /// command measuring `with_mail` and the bare prompt `bare`.
    fn run_scripted_scheduled_loop(
        passes: Vec<CompactPass>,
        with_mail: crate::context::ContextProjection,
        bare: Option<crate::context::ContextProjection>,
    ) -> (ScheduledPrompt, usize, Vec<String>, SessionQueueDrainState) {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(dir.path());
        queue::enqueue(&mut config.session.pending_queue, "carry on".to_owned()).unwrap();
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let (events, rx) = host_event_sink();
        let (_actions, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let mut scripted = passes.into_iter();
        let mut ran = 0;
        let outcome = run_scheduled_compaction_until_fits_with(
            watch,
            &mut config,
            &events,
            0,
            bare.is_some(),
            |config| {
                ran += 1;
                let pass = scripted.next().expect("an unscripted follow-on pass");
                // "Already fits" returns Unchanged before any dispatch, so it
                // leaves drain as it was.
                if pass != CompactPass::Unchanged {
                    config.session.queue_drain_state = scheduled_compact_queue_drain_state(&pass);
                }
                Ok(pass)
            },
            |is_bare| Ok(if is_bare { bare.unwrap() } else { with_mail }),
        )
        .unwrap();
        let notes = rx
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                )) => Some(text),
                _ => None,
            })
            .collect();
        (outcome, ran, notes, config.session.queue_drain_state)
    }

    /// A prompt that fits is never parked behind its own child-mail batch:
    /// when no pass can make room for the batch, the decision falls back to
    /// the bare prompt and the batch stays unread. Without a bare fallback
    /// (a wake, or a replayed batch) the same measures park.
    #[test]
    fn a_fitting_prompt_is_sent_without_mail_that_no_pass_can_fit() {
        let fits = crate::context::ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(10_000),
            100_000,
            100_000,
            None,
        );
        assert!(scheduled_prompt_sendable(fits));
        let over = fits.with_disposition(ContextDisposition::WouldOverflow);
        let failed = || CompactPass::Failed {
            note: COMPACT_FAILED_DETAIL.to_owned(),
        };
        let sent_bare = ScheduledPrompt::Send {
            over_window: false,
            drop_mail: true,
        };
        let (outcome, ran, notes, _) =
            run_scripted_scheduled_loop(vec![CompactPass::NothingEligible], over, Some(fits));
        assert_eq!((outcome, ran), (sent_bare, 1));
        assert_eq!(
            notes,
            [format!(
                "{SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE}; {SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE}"
            )]
        );
        let (outcome, _, notes, _) =
            run_scripted_scheduled_loop(vec![CompactPass::NothingEligible], over, Some(over));
        assert_eq!(
            outcome,
            ScheduledPrompt::Send {
                over_window: true,
                drop_mail: true
            }
        );
        assert!(notes.is_empty(), "the turn's resend remedy is the one note");
        let (outcome, ran, notes, drain) =
            run_scripted_scheduled_loop(vec![failed()], over, Some(fits));
        assert_eq!((outcome, ran), (sent_bare, 1));
        assert_eq!(drain, SessionQueueDrainState::LiveAutoDrain);
        assert_eq!(
            notes,
            [
                COMPACT_FAILED_DETAIL.to_owned(),
                format!(
                    "{SCHEDULED_COMPACT_FAILED_FITS_NOTE}; {SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE}"
                ),
            ]
        );
        let (outcome, ran, notes, _) = run_scripted_scheduled_loop(
            vec![CompactPass::Committed, CompactPass::Committed],
            over,
            Some(fits),
        );
        assert_eq!((outcome, ran), (sent_bare, 2), "{notes:?}");
        assert_eq!(notes.last().unwrap(), SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE);
        for bare in [Some(over), None] {
            let (outcome, _, _, drain) = run_scripted_scheduled_loop(
                vec![CompactPass::Committed, CompactPass::Committed],
                over,
                bare,
            );
            assert_eq!(outcome, ScheduledPrompt::Parked);
            assert_eq!(drain, SessionQueueDrainState::RestoredPaused);
            let (outcome, _, _, _) = run_scripted_scheduled_loop(vec![failed()], over, bare);
            assert_eq!(outcome, ScheduledPrompt::Parked);
        }
    }

    /// A pass that left the head unchanged ("already fits") never pays for a
    /// follow-on or says the successor still meets the trigger: it would
    /// measure the same again.
    #[test]
    fn an_unchanged_pass_never_follows_on() {
        let fits = crate::context::ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(10_000),
            100_000,
            100_000,
            None,
        );
        let over = fits.with_disposition(ContextDisposition::WouldOverflow);
        let (outcome, ran, notes, _) =
            run_scripted_scheduled_loop(vec![CompactPass::Unchanged], fits, None);
        assert_eq!(
            (outcome, ran),
            (
                ScheduledPrompt::Send {
                    over_window: false,
                    drop_mail: false
                },
                1
            )
        );
        assert!(notes.is_empty(), "{notes:?}");
        let (outcome, ran, notes, drain) =
            run_scripted_scheduled_loop(vec![CompactPass::Unchanged], over, None);
        assert_eq!((outcome, ran), (ScheduledPrompt::Parked, 1));
        assert_eq!(drain, SessionQueueDrainState::RestoredPaused);
        assert!(
            !notes
                .iter()
                .any(|note| note.contains("successor still meets")),
            "{notes:?}"
        );
    }

    #[test]
    fn scheduled_compaction_admits_a_fitting_successor_above_the_occupancy_trigger() {
        use plakous::agent::{CommandId, TurnResultOutcome};
        use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
        use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};

        let root = tempfile::tempdir().unwrap();
        let script: ScriptPlan = serde_json::from_value(serde_json::json!({
            "version": SCRIPT_VERSION,
            "maximum_projected_input": 100_000,
            "content_sensitive_projection": true,
            "steps": [
                {"output": {"text": "x".repeat(70_000)}},
                {"output": {"text": "latest protected answer"}},
                {"output": {"text": "queued prompt admitted"}}
            ]
        }))
        .unwrap();
        let script = Arc::new(script);
        let (mut config, evidence) =
            recover_tests::scripted_host_with(root.path(), Arc::clone(&script));
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                evidence.clone(),
            )
            .unwrap();
        let workspace = config.cwd.clone();
        let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
        config.test_injection = Some(Arc::new(TestHostInjection {
            rebind_target: config
                .test_injection
                .as_ref()
                .unwrap()
                .rebind_target
                .clone(),
            runtime: Arc::new(move |role| {
                if role == TestRuntimeRole::Compact {
                    plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                        &workspace,
                        controller.clone(),
                        &script,
                        None,
                        None,
                        Vec::new(),
                    )
                } else {
                    plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                        &workspace,
                        controller.clone(),
                        Arc::clone(&script),
                        Arc::clone(&cursor),
                    )
                }
                .map_err(|error| error.to_string())
            }),
            observe: Arc::new(|_, _| {}),
            child_runtime: None,
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        }));
        config.compaction_enabled = true;
        let executor = ApplicationExecutor::new().unwrap();
        let runtime =
            host_runtime_binding(&config, &mut Vec::new(), TestRuntimeRole::Primary).unwrap();
        let opened = executor
            .block_on(crate::agent::build_or_open_injected_session(
                config.state_roots.runtime_sessions().unwrap(),
                None,
                None,
                SessionOperationId::new("scheduled-fits:create").unwrap(),
                runtime,
            ))
            .unwrap()
            .unwrap_or_else(|error| panic!("{}", error.failure.message()));
        for (index, prompt) in ["older intent", "latest intent"].into_iter().enumerate() {
            let result = executor
                .block_on(async {
                    opened
                        .agent()
                        .unwrap()
                        .start(
                            CommandId::new(format!("scheduled-fits:seed:{index}")).unwrap(),
                            prompt,
                        )
                        .await
                        .unwrap()
                        .into_turn()
                        .unwrap()
                        .result()
                        .await
                        .unwrap()
                })
                .unwrap();
            assert!(
                matches!(result.outcome(), TurnResultOutcome::Completed(_)),
                "{:?}",
                result.outcome()
            );
        }
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        let user_text = "p".repeat(50_000);
        queue::enqueue(&mut config.session.pending_queue, user_text.clone()).unwrap();
        config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
        let source = opened.locator();
        let after = crate::context::ContextProjection::test_fits(
            crate::context::PreparedInputCount::UpperBound(20_000),
            100_000,
            100_000,
            None,
        )
        .with_anchored_occupied_tokens(90_000)
        .with_auto_compact_input(Some(80_000));
        assert_eq!(after.disposition(), ContextDisposition::Fits);
        assert!(!after.observation_headroom_requires_compaction());
        assert!(occupancy_requires_scheduled_compact(after));
        let (events, rx) = host_event_sink();
        let (_actions, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let mut compacts = 0;
        // Run a real committed scheduled fold; pin the post-fold control policy
        // with a Fits projection whose occupancy independently meets the trigger.
        let admitted = run_scheduled_compaction_until_fits_with(
            watch,
            &mut config,
            &events,
            last_enrolled_source_bytes(&opened),
            false,
            |config| {
                compacts += 1;
                assert_eq!(
                    compacts, 1,
                    "a fitting successor must not start a follow-on fold"
                );
                handle_compact(
                    &executor,
                    watch,
                    config,
                    &opened,
                    &mut Vec::new(),
                    &events,
                    None,
                    sessions::ProductCompactReason::Scheduled,
                    Some(&UserInput::from(user_text.clone())),
                )
            },
            |_| Ok(after),
        )
        .unwrap();
        assert_eq!(
            admitted,
            ScheduledPrompt::Send {
                over_window: false,
                drop_mail: false
            }
        );
        assert_eq!(compacts, 1);
        assert_ne!(opened.locator(), source, "the scheduled fold committed");
        executor
            .block_on(projection::project_to_tail(
                opened.sessions(),
                &config.store,
                &config.session.id,
                opened.locator(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            config
                .store
                .load(&config.session.id)
                .unwrap()
                .compaction_count,
            1,
            "the projector counts a committed host-level fold exactly once"
        );
        assert_eq!(
            config.session.queue_drain_state,
            SessionQueueDrainState::LiveAutoDrain
        );
        assert!(!rx.try_iter().any(|event| matches!(event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("compacting once more"))));
        let result = executor
            .block_on(async {
                opened
                    .agent()
                    .unwrap()
                    .start(
                        CommandId::new("scheduled-fits:admitted").unwrap(),
                        user_text,
                    )
                    .await
                    .unwrap()
                    .into_turn()
                    .unwrap()
                    .result()
                    .await
                    .unwrap()
            })
            .unwrap();
        assert!(
            matches!(result.outcome(), TurnResultOutcome::Completed(_)),
            "{:?}",
            result.outcome()
        );
        assert_eq!(
            read_evidence(&evidence)
                .unwrap()
                .iter()
                .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
                .count(),
            4,
            "two seed rounds, one fold and the admitted prompt only"
        );
        executor.block_on(opened.release()).unwrap().unwrap();
    }

    #[test]
    fn attached_no_effort_openrouter_usage_and_compact_use_real_host_workflows() {
        let root = tempfile::tempdir().unwrap();
        let config = no_effort_openrouter_injected_host(root.path());
        let events = run_attached_actions(
            config,
            vec![
                UserAction::Extension(ProbeIntent::InspectUsage),
                UserAction::Compact,
            ],
        );
        assert_eq!(usage_statuses(&events), ["fits"]);
        assert!(
            events.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("canonical compact failed")
            ) || matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.contains("compaction not needed")
                        || note.contains("nothing in this conversation can be compacted yet")
            ) || matches!(
                event,
                HostEvent::ToolLine { name, .. } if name == "compact"
            )),
            "openrouter compact must run the host workflow, events={events:?}"
        );
        assert!(events.iter().all(|event| !matches!(
            event,
            HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("effort")
        )));
    }

    /// A manual `/compact` published no activity at
    /// all, so the HUD stayed empty for the whole paid dispatch and
    /// the TUI's `Compacting…` branch was unreachable. Spec 02 §5.4 segment 2.
    /// Status and busy are latest-state events, so this drains between the two
    /// phases exactly as the frame loop does.
    #[test]
    fn a_manual_compact_names_itself_on_the_turn_status_row_and_clears_after() {
        let (events, rx) = host_event_sink();
        observe_compact(&events, 2, true);
        let during = rx.try_iter().collect::<Vec<_>>();
        assert!(
            during.contains(&HostEvent::SetBusy(true)),
            "the row has zero height unless the host says it is working: {during:?}"
        );
        assert!(
            during.iter().any(|event| matches!(
                event,
                HostEvent::Status { phase, queue_len, .. }
                    if phase == "compacting" && *queue_len == 2
            )),
            "the activity label names the compaction: {during:?}"
        );

        observe_compact(&events, 2, false);
        let after = rx.try_iter().collect::<Vec<_>>();
        assert!(after.contains(&HostEvent::SetBusy(false)));
        assert!(
            after.iter().any(|event| matches!(
                event,
                HostEvent::Status { phase, .. } if phase == "idle"
            )),
            "the activity clears on every exit arm: {after:?}"
        );
    }

    #[test]
    fn compact_on_fitting_session_stays_manual_and_reports_no_eligible_range() {
        let root = tempfile::tempdir().unwrap();
        let script = Arc::new(
            serde_json::from_value::<plakous_offline_crash_harness::script::ScriptPlan>(
                serde_json::json!({
                    "version": 1,
                    "steps": [{"output": {"text": "unused"}}]
                }),
            )
            .unwrap(),
        );
        let (mut config, _) = recover_tests::scripted_host_with(root.path(), Arc::clone(&script));
        let runtime = Arc::clone(&config.test_injection.as_ref().unwrap().runtime);
        let workspace = config.cwd.clone();
        let controller =
            plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("compact-evidence.jsonl"),
            )
            .unwrap();
        Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .runtime = Arc::new(move |role| {
            if role == TestRuntimeRole::Compact {
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
        config.compaction_enabled = true;
        let session_id = config.session.id.clone();
        let store = Arc::clone(&config.store);
        let prepared = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = Arc::clone(&prepared);
        let observed_store = Arc::clone(&store);
        let observed_id = session_id.clone();
        Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .observe = Arc::new(move |workflow, boundary| {
            if workflow == TestHostWorkflow::Compact && boundary == TestHostBoundary::Prepared {
                *captured.lock().unwrap() = observed_store
                    .load_intents(&observed_id)
                    .unwrap()
                    .workflow_requests;
            }
        });
        let (actions, receiver, worker) = spawn_host_worker(config).unwrap();
        actions.send(UserAction::Compact).unwrap();
        let mut events = Vec::new();
        let mut compact_started = false;
        loop {
            let event = receiver.recv_timeout(Duration::from_secs(20)).unwrap();
            compact_started |= matches!(event, HostEvent::SetBusy(true));
            let finished = compact_started && matches!(event, HostEvent::SetBusy(false));
            events.push(event);
            if finished {
                break;
            }
        }
        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
        events.extend(receiver.try_iter());
        assert!(
            store
                .load_intents(&session_id)
                .unwrap()
                .workflow_requests
                .is_empty(),
            "a no-eligible-range answer leaves no pending compact request: {events:?}"
        );
        let requests = prepared.lock().unwrap();
        assert_eq!(requests.len(), 1, "events={events:?}");
        assert!(matches!(
            requests[0].kind,
            sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual)
        ));

        assert!(
            events.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.contains("nothing in this conversation can be compacted yet")
            )),
            "fitting manual compact must answer no eligible range, events={events:?}"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.contains("compaction not needed: the selected prefix already fits")
            )),
            "manual compact must not Fits-short-circuit, events={events:?}"
        );
        if let Ok(Some(views)) = store.load_views(&session_id) {
            assert_eq!(views.state.counters.compactions, 0);
        }
    }

    /// Grok protected overflow now attempts the compact instead of refusing.
    ///
    /// `/compact` used to be refused before the ladder ran, on a key that
    /// asked the native facet. It now dispatches and the outcome decides.
    /// This fixture's injected runtime is not exact for its own overflow
    /// source, so the attempt is refused at runtime admission rather than
    /// reaching the rung; what this pins is that the pre-ladder refusal is
    /// gone and that nothing is committed. The exhausted-ladder wording is
    /// pinned by `context_capacity_recovery_guidance` above and by the
    /// agent-level refusal test.
    #[test]
    fn overflow_usage_and_grok_compact_attempts_the_ladder() {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = overflow_injected_attached_host(root.path(), NativeProvider::Xai);
        let session_id = config.session.id.clone();
        let store = Arc::clone(&config.store);
        let events = run_attached_actions(
            config,
            vec![
                UserAction::Extension(ProbeIntent::InspectUsage),
                UserAction::Compact,
            ],
        );
        let statuses = usage_statuses(&events);
        assert!(
            !statuses.is_empty()
                && statuses
                    .iter()
                    .all(|status| *status == "protected-overflow"),
            "empty-session Grok overflow must report protected-overflow, got {statuses:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("only protected context remains")
            )),
            "the pre-ladder refusal must be gone, events={events:?}"
        );
        assert!(
            events.iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.contains("compact")
            )),
            "the compact must be attempted and reported, events={events:?}"
        );
        if let Ok(Some(views)) = store.load_views(&session_id) {
            assert_eq!(
                views.state.counters.compactions, 0,
                "an unrescuable overflow commits no compaction"
            );
        }
    }

    #[test]
    fn compact_binding_failure_leaves_no_workflow_request() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let executor = ApplicationExecutor::new().unwrap();
        let runtime =
            host_runtime_binding(&config, &mut Vec::new(), TestRuntimeRole::Primary).unwrap();
        let opened = executor
            .block_on(crate::agent::build_or_open_injected_session(
                config.state_roots.runtime_sessions().unwrap(),
                None,
                None,
                SessionOperationId::new("binding-failure:create").unwrap(),
                runtime,
            ))
            .unwrap()
            .unwrap_or_else(|error| panic!("{}", error.failure.message()));
        Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .runtime = Arc::new(|_| Err("compact binding unavailable".into()));
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        let (events, _rx) = host_event_sink();
        let (_actions, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&rx, &state);
        let error = handle_compact(
            &executor,
            watch,
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            sessions::ProductCompactReason::Manual,
            None,
        )
        .unwrap_err();
        assert!(
            error.message().contains("compact binding unavailable"),
            "{}",
            error.message()
        );
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            [] as [sessions::projector::WorkflowRequest; 0]
        );
    }

    #[test]
    fn unknown_without_a_fact_stays_pending_but_unavailable_discards() {
        use sessions::projector::{SessionViews, ViewWrites, WorkflowRequest};
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let source = memory_attached_stub().locator();
        config.session.bind_plakous_locator(source).unwrap();
        config.store.save(&mut config.session).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:compact:no-fact".into(),
            source_locator: source,
            kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();
        let views = SessionViews::empty();
        config
            .store
            .commit_projection_views(
                &config.session.id,
                &ViewWrites {
                    successors: Vec::new(),
                    expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                    cursor: plakous::sessions::feed::FeedSeq::ZERO,
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                    calls: Vec::new(),
                    open_turns: Vec::new(),
                    processes: Vec::new(),
                },
                &views.state,
            )
            .unwrap();
        apply_compact_views(
            &mut config,
            None,
            &request.operation_id,
            compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown),
            Ok(views.clone()),
        );
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            vec![request.clone()]
        );
        let answer = CompactSessionOutcome::Unavailable {
            operation_id: SessionOperationId::new(&request.operation_id).unwrap(),
            source,
            reason: plakous::sessions::SessionWorkflowUnavailable::Compaction,
        };
        assert!(compact_answer_is_unadmitted(&answer, source));
        apply_compact_views(
            &mut config,
            None,
            &request.operation_id,
            compact_answer_is_unadmitted(&answer, source),
            Ok(views),
        );
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            [] as [WorkflowRequest; 0]
        );
    }

    #[test]
    fn unknown_after_admission_keeps_the_request_and_refresh_debt_is_nonfatal() {
        use sessions::projector::{SessionViews, ViewWrites, WorkflowCompletion, WorkflowRequest};
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let opened = memory_attached_stub();
        let source = opened.locator();
        config.session.bind_plakous_locator(source).unwrap();
        config.store.save(&mut config.session).unwrap();
        let request = WorkflowRequest {
            operation_id: "plak:compact:lost-reply".into(),
            source_locator: source,
            kind: sessions::SessionWorkflowKind::Compact(sessions::ProductCompactReason::Manual),
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();
        let mut views = SessionViews::empty();
        views.state.runtime_session_id = Some(source.session_id());
        views.state.workflows.insert(
            request.operation_id.clone(),
            WorkflowCompletion {
                admitted_at: Some(1),
                settled: None,
            },
        );
        config
            .store
            .commit_projection_views(
                &config.session.id,
                &ViewWrites {
                    successors: Vec::new(),
                    expected_cursor: plakous::sessions::feed::FeedSeq::ZERO,
                    cursor: plakous::sessions::feed::FeedSeq::ZERO,
                    upserts: Vec::new(),
                    deletes: Vec::new(),
                    calls: Vec::new(),
                    open_turns: Vec::new(),
                    processes: Vec::new(),
                },
                &views.state,
            )
            .unwrap();
        let unadmitted = compact_error_is_unadmitted(AgentSessionsErrorKind::OutcomeUnknown);
        assert!(!unadmitted);
        let revision = config.session.store_revision;
        apply_compact_views(
            &mut config,
            None,
            &request.operation_id,
            unadmitted,
            Ok(views.clone()),
        );
        assert_eq!(config.session.store_revision, revision);
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            vec![request.clone()]
        );
        // Even a wrong caller classification cannot erase the admitted request.
        apply_compact_views(&mut config, None, &request.operation_id, true, Ok(views));
        apply_compact_views(
            &mut config,
            None,
            &request.operation_id,
            true,
            Err("transient projection fault".into()),
        );
        assert_eq!(config.session.store_revision, revision);
        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            vec![request]
        );
        config.store.save(&mut config.session).unwrap();
    }

    #[test]
    fn interrupt_refuses_compact_before_its_single_dispatch() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let opened = memory_attached_stub();
        let source = opened.locator();
        config.session.bind_plakous_locator(source).unwrap();
        let (events, _rx) = host_event_sink();
        let (actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        actions_tx.send(UserAction::Interrupt).unwrap();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

        let error = handle_compact(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &opened,
            &mut Vec::new(),
            &events,
            None,
            sessions::ProductCompactReason::Manual,
            None,
        )
        .expect_err("an interrupted compact must not dispatch");

        assert!(
            error.message().contains("interrupted"),
            "compact refusal must name the interrupt: {}",
            error.message()
        );
        // no dispatch happened, so no workflow and no redispatch.

        assert_eq!(
            config
                .store
                .load_intents(&config.session.id)
                .unwrap()
                .workflow_requests,
            [] as [sessions::projector::WorkflowRequest; 0]
        );
        assert_eq!(config.session.plakous_locator, Some(source));
    }

    /// Spec section 9: every compaction outcome is distinct operator text,
    /// including the ones the tool-group fence produces mid-turn, which were
    /// logged and then dropped instead of reaching the transcript at all.
    #[test]
    fn every_compaction_outcome_has_its_own_operator_text() {
        let outcomes = [
            CompactionEventOutcome::Committed,
            CompactionEventOutcome::NotNeeded,
            CompactionEventOutcome::NoEligibleRange,
            CompactionEventOutcome::Unavailable,
            CompactionEventOutcome::Failed,
            CompactionEventOutcome::RecoveryRequired,
        ];
        let mut seen = std::collections::BTreeSet::new();
        for outcome in outcomes {
            let detail = compaction_event_outcome_detail(outcome);
            assert_ne!(detail, "");
            assert!(seen.insert(detail), "duplicate operator text: {detail}");
        }
        assert_eq!(seen.len(), outcomes.len());
    }

    #[test]
    fn a_quiet_provider_does_not_dump_the_responses_deadline_into_the_transcript() {
        let dumped = compact_operator_note(
            COMPACT_FAILED_DETAIL,
            Some(
                "Timeout: Responses operation deadline elapsed; no authoritative completion received; observed_events=4 last_observation_age_ms=30002",
            ),
        );
        assert!(
            !dumped.contains("deadline elapsed"),
            "raw deadline dump must not reach the transcript: {dumped}"
        );
        assert!(
            !dumped.contains("observed_events"),
            "Responses observation dump must not reach the transcript: {dumped}"
        );
        assert!(
            dumped.contains("/compact"),
            "the note must name the retry: {dumped}"
        );
        assert_eq!(
            compact_operator_note(COMPACT_FAILED_DETAIL, Some("model refused")),
            format!("{COMPACT_FAILED_DETAIL}: model refused")
        );
        assert_eq!(
            compact_operator_note(COMPACT_FAILED_DETAIL, None),
            COMPACT_FAILED_DETAIL
        );
    }

    #[test]
    fn a_shrink_refusal_is_plain_in_the_transcript_and_kept_in_the_log_detail() {
        let logged = compact_operator_detail(
            COMPACT_FAILED_DETAIL,
            Some(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC),
        );
        assert!(logged.contains(COMPACT_SHRINK_REFUSAL_DIAGNOSTIC));
        assert_eq!(
            manual_compact_operator_text(&logged),
            COMPACT_SHRINK_REFUSAL_NOTE
        );
        assert!(!COMPACT_SHRINK_REFUSAL_NOTE.contains("did not prove"));
        let other = format!("{COMPACT_FAILED_DETAIL}: model refused");
        assert_eq!(manual_compact_operator_text(&other), other);
    }
}
