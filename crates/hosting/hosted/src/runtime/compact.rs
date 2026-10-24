//! Manual and scheduled compaction and capacity recovery.

use std::time::Instant;

use plakous::agent::UserInput;
use plakous::application::ApplicationExecutor;
use plakous::sessions::{
    AgentSessionsErrorKind, CompactReason, CompactSessionOutcome, ContextQuery, SessionOperationId,
};

use crate::agent::AttachedCodingSession;
use crate::context::ContextDisposition;
use crate::interrupt::{InterruptWatch, run_watched};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{self, QueueDrainState as SessionQueueDrainState, SessionRecord};
use crate::{CliFailure, CliResult};

use super::events::{
    compact_row_token_pair, compact_terminal_state, compact_terminal_subject,
    compaction_summary_clause, publish_compact_activity,
};
use super::factory::observe_boundary;
use super::logging::{
    compact_reason_label, log_compact_finished, log_compact_finished_with_detail,
    log_compact_started, log_ev,
};
use super::rebind::interrupted_before_dispatch;
use super::startup::host_runtime_binding;
use super::usage::{emit_context_projection, last_enrolled_source_bytes, query_context};
use super::{
    COMPACT_COMMITTED_DETAIL, COMPACT_COMMITTED_STILL_OVERFLOWS_DETAIL, COMPACT_FAILED_DETAIL,
    COMPACT_NO_ELIGIBLE_RANGE_DETAIL, COMPACT_NOT_NEEDED_DETAIL, COMPACT_PROVIDER_QUIET_NOTE,
    COMPACT_RECOVERY_REQUIRED_DETAIL, COMPACT_UNAVAILABLE_DETAIL,
    CONTEXT_CAPACITY_COMPACTABLE_GUIDANCE, CONTEXT_CAPACITY_NATIVE_COMPACT_GUIDANCE,
    CONTEXT_CAPACITY_PROTECTED_GUIDANCE, CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE,
    CONTEXT_CAPACITY_UNVERIFIED_GUIDANCE, HostConfig, HostEvent, RuntimeRole, Sender, queue,
};
use super::{HostBoundary, HostWorkflow};
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
        && !projection.compaction_verdict().observation_headroom()
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
                let mut over_window = after.compaction_verdict().overflow();
                let drop_mail = over_window && bare_fallback;
                if drop_mail {
                    over_window = bare(|_| true)?
                        .is_none_or(|projection| projection.compaction_verdict().overflow());
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
        if pass == CompactPass::Committed
            && follow_ons_used < crate::context::SCHEDULED_COMPACT_FOLLOW_ON_LIMIT
            && after.compaction_verdict().is_due()
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
                && !projection.compaction_verdict().is_due()
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
    let runtime = host_runtime_binding(config, stderr_buf, RuntimeRole::Compact)?;
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
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Compact,
        HostBoundary::Prepared,
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
    observe_boundary(
        config.runtime_factory.as_ref(),
        HostWorkflow::Compact,
        HostBoundary::RuntimeReturned,
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

/// `segment_limit`: the attached head holds at least the segment count at
/// which Plakous refuses the next round (chapter 01 §10.3).
pub(super) const fn context_capacity_recovery_guidance(
    recovery: plakous_host_protocol::ContextCapacityRecovery,
    exact_source: bool,
    native_compact_facet: bool,
    segment_limit: bool,
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
        ContextCapacityRecovery::Compactable if segment_limit => {
            CONTEXT_CAPACITY_SEGMENT_LIMIT_GUIDANCE
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
mod tests;
