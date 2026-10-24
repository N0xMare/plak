//! The idle scheduled pass before a prompt and its follow-on.

use super::views::{emit_compact_meter, publish_compact_row};
use super::{CompactPass, elapsed_ms, handle_compact};
use crate::agent::AttachedCodingSession;
use crate::context::ContextDisposition;
use crate::interrupt::InterruptWatch;
use crate::runtime::logging::log_compact_finished;
use crate::runtime::usage::{emit_context_projection, last_enrolled_source_bytes, query_context};
use crate::runtime::{HostConfig, HostEvent, Sender};
use crate::session_log::SessionLog;
use crate::sessions::{NoticeScope, QueueDrainState as SessionQueueDrainState, TranscriptRow};
use crate::{CliFailure, CliResult, sessions};
use plakous::agent::UserInput;
use plakous::application::ApplicationExecutor;
use plakous::sessions::ContextQuery;

/// What an idle scheduled pass leaves for the prompt it ran before.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum ScheduledPrompt {
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
pub(in crate::runtime) fn run_scheduled_compaction_until_fits<
    P: plakous_host_protocol::ProtocolExtension,
>(
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
const SCHEDULED_COMPACT_FAILED_FITS_NOTE: &str =
    "the prompt fits without compaction and is sent as is";

/// Clause for a prompt sent without the child-mail batch that did not fit.
const SCHEDULED_COMPACT_MAIL_UNREAD_CLAUSE: &str =
    "sub-agent mail does not fit with it and stays unread for the next prompt";

/// Operator note when compaction made room for the prompt but not its mail.
const SCHEDULED_COMPACT_MAIL_DEFERRED_NOTE: &str = "the prompt fits after scheduled compaction and is sent; sub-agent mail does not fit with it and stays unread for the next prompt";

/// Operator note when a scheduled pass found nothing to compact.
const SCHEDULED_COMPACT_NOTHING_ELIGIBLE_NOTE: &str =
    "nothing could be compacted before this prompt; it is sent as is";

/// Whether a scheduled pass's command already fits below every trigger, so
/// the pass reports `already fits` and pays for nothing.
pub(super) fn scheduled_pass_already_fits<P: plakous_host_protocol::ProtocolExtension>(
    run: &super::CompactRun<'_, P>,
    config: &mut HostConfig<P>,
    pending_user_input: Option<&UserInput>,
) -> bool {
    let qualification = pending_user_input.map_or(ContextQuery::Current, |input| {
        ContextQuery::WithPendingUserInput(input.clone())
    });
    // Manual compact is eligible-range gated. Scheduled compact
    // skips only when occupancy gauge Fits below the capacity-derived idle trigger.
    // Capacity recovery must still compact a Fits envelope.
    if let Ok(projection) = query_context(run.executor, run.opened, qualification) {
        if projection.disposition() == ContextDisposition::Fits
            && !projection.compaction_verdict().is_due()
        {
            publish_compact_row(
                config,
                run.events,
                &run.call_id,
                "done",
                Some("already fits".to_owned()),
                Some(elapsed_ms(run.started_at)),
            );
            log_compact_finished(run.log, config, run.started_at, "not_needed");
            emit_compact_meter(run.executor, run.opened, config, run.events);
            return true;
        }
    }
    false
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
