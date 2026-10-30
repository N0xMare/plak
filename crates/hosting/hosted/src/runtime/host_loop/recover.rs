//! Recover a fenced session in place: release the dead attachment and reopen.
//!
//! A turn that ends recovery-required leaves its driver stopped, and nothing
//! can revive that driver. What a relaunch would do is open a new owner of the
//! same session, whose open abandons the unfinished turn exactly once (spec 01
//! §13.1). This does the same thing without quitting: the one reopen path the
//! crash matrix already covers, taken in place.

use crate::surface_protocol::ContentEvent;
use plakous::agent::AbandonedTurnReport;
use plakous::sessions::feed::FeedSeq;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

use super::{
    CliFailure, CliResult, ContextQuery, HostLoop, HostStep, SessionEvent,
    build_session_from_config, durable_tree_id, emit_queue_snapshot, emit_restored_paused_note,
    emit_unavailable_context, log_ev, native_compact_facet, query_context, report_context,
    restored_paused_status_suffix,
};
use crate::context::ContextDisposition;
use crate::sessions::{NoticeScope, TranscriptRow};

/// What the recovery was asked by.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecoveryTrigger {
    /// An action that needs the session: a prompt, `/compact`, a rebind.
    Action,
    /// The explicit control: `/recover`, JSONL `recover`.
    Explicit,
}

pub(super) struct ReopenedTurn {
    pub(super) abandoned: Option<AbandonedTurnReport>,
    terminal_published: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    /// Reopens this session in place and publishes what the reopen closed.
    ///
    /// On failure the old attachment stays released and `recovery_blocked`
    /// stays set, so the next action tries again; nothing is lost and nothing
    /// ends in `/clear`.
    #[allow(
        clippy::too_many_lines,
        reason = "Recovery distinguishes retained approvals, abandoned turns, pending projection debt, and the exact source binding before reopening."
    )]
    pub(super) fn recover_session(&mut self, trigger: RecoveryTrigger) -> CliResult<HostStep> {
        let _ = self.events.send(ContentEvent::SetBusy(true));
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::RecoveringInterruptedTurn),
        });
        let recovered = self.reopen_in_place();
        let restored_working = recovered.is_ok() && self.opened.has_recovered_turn();
        if !restored_working {
            let _ = self.events.send(ContentEvent::SetBusy(false));
        }
        let ReopenedTurn {
            abandoned: report,
            terminal_published,
        } = match recovered {
            Ok(report) => report,
            Err(failure) => {
                log_ev(
                    self.log.as_ref(),
                    &self.config,
                    SessionEvent::new("session.recover_failed").str("msg", failure.message()),
                );
                let _ = self.events.send(ContentEvent::Status {
                    phase: HostPhase::Error,
                    queue_len: self.config.session.pending_queue.len(),
                    detail: Some(HostStatusDetail::RecoveryIncomplete),
                });
                return Err(CliFailure::new(format!(
                    "the session could not be reopened to recover the interrupted turn ({}); \
                     nothing was lost, and the next prompt or /recover retries",
                    failure.message()
                )));
            }
        };
        self.recovery_blocked = false;
        let (claim, cause, operations) =
            report.as_ref().map_or((None, None, Vec::new()), |report| {
                (
                    Some(report.claim().as_str().to_owned()),
                    Some(report.cause().as_str().to_owned()),
                    report.operations().to_vec(),
                )
            });
        let mut record = SessionEvent::new("session.recovered").str(
            "trigger",
            match trigger {
                RecoveryTrigger::Action => "action",
                RecoveryTrigger::Explicit => "explicit",
            },
        );
        if let Some(report) = &report {
            record = record
                .str("claim", report.claim().as_str())
                .str("cause", report.cause().as_str());
        }
        log_ev(self.log.as_ref(), &self.config, record);
        if let Some(report) = &report {
            log_abandoned_report(self.log.as_ref(), &self.config, report);
            if !terminal_published {
                let _ = self.events.send(ContentEvent::Row(TranscriptRow::info(
                    NoticeScope::Recovery,
                    super::super::events::abandoned_turn_operator_line(report),
                )));
            }
        }
        if report.is_none() && !restored_working {
            let _ = self.events.send(ContentEvent::Row(TranscriptRow::info(NoticeScope::Recovery,
                if terminal_published {
                    "plak: reopened the session and published the saved turn's result; the conversation continues from its saved state".to_owned()
                } else {
                    "plak: reopened the session; the interrupted turn left nothing outstanding, and the conversation continues from its saved state".to_owned()
                },
            )));
        }
        let _ = self.events.send(ContentEvent::Recovered {
            claim,
            cause,
            operations,
        });
        if let Err(error) = report_context(
            &self.executor,
            &self.opened,
            &self.events,
            "after recovery",
            self.config
                .target
                .native_provider()
                .is_some_and(native_compact_facet),
            self.config.session.context_counters(),
        ) {
            emit_unavailable_context(&self.events);
            let _ = self.events.send(ContentEvent::Row(TranscriptRow::error(
                NoticeScope::Recovery,
                error.message().to_owned(),
            )));
        }
        let _ = self.events.send(ContentEvent::AgentAttached(true));
        self.spawn_tree_recovery();
        // What was queued behind the fenced turn was written for a turn whose
        // outcome nobody saw, so it stays paused (spec 02 §4.2); the note says
        // exactly how to resume it, and a prompt typed now still runs at once.
        let rows = self.config.topology_rows();
        emit_queue_snapshot(&self.events, &self.config.session);
        emit_restored_paused_note(
            &self.events,
            &rows,
            &self.config.session,
            self.config.interactive,
        );
        if restored_working {
            return self.drive_restored_turn();
        }
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: self.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::Ready {
                model_id: self.config.model_id.clone(),
                pause: restored_paused_status_suffix(&rows, &self.config.session),
            }),
        });
        Ok(HostStep::Continue)
    }

    /// Starts this tree's one child recovery pass in the background. It never
    /// waits on a lease, and what it settled or refused reaches the user as
    /// one system note when it finishes.
    pub(super) fn spawn_tree_recovery(&self) {
        let tree_id = durable_tree_id(&self.config.topology_rows(), &self.config.session);
        let events = self.events.clone();
        let session_id = self.config.session.id.clone();
        let _ = self.executor.block_on(async move {
            super::super::tree::start_child_recovery(&events, &session_id, tree_id, false, None);
        });
    }

    /// An explicit `/recover` with no interrupted root turn: starts the tree's
    /// recovery pass in the background and says so; its report, or
    /// `nothing` when it had nothing to do, follows as a note. Never waits on
    /// the pass.
    pub(super) fn recover_tree_children(&mut self, nothing: Option<String>) {
        let tree_id = durable_tree_id(&self.config.topology_rows(), &self.config.session);
        let events = self.events.clone();
        let session_id = self.config.session.id.clone();
        let for_pass = nothing.clone();
        let started = self
            .executor
            .block_on(async move {
                super::super::tree::start_child_recovery(
                    &events,
                    &session_id,
                    tree_id,
                    true,
                    for_pass,
                )
            })
            .unwrap_or(crate::agent::RecoveryPassStart::NoTree);
        log_ev(
            self.log.as_ref(),
            &self.config,
            SessionEvent::new("topology.children_recovery_requested")
                .str("start", format!("{started:?}")),
        );
        if started == crate::agent::RecoveryPassStart::NoTree
            && let Some(nothing) = nothing
        {
            let _ = self
                .events
                .notice(TranscriptRow::info(NoticeScope::Recovery, nothing));
        }
    }

    /// Releases the stopped driver's attachment, then opens a new owner of the
    /// same session. The old attachment keeps its topology lease until the new
    /// one holds its own, so a recycle never shuts a running agent tree.
    #[allow(
        clippy::too_many_lines,
        reason = "In-place reopen restores the canonical session and reattaches its events without losing the existing host queue and notices."
    )]
    pub(super) fn reopen_in_place(&mut self) -> CliResult<ReopenedTurn> {
        let previous_cursor = self
            .config
            .store
            .load_views(&self.config.session.id)
            .map_err(CliFailure::new)?
            .map_or(FeedSeq::ZERO, |views| views.cursor);
        match self
            .executor
            .block_on(self.opened.detach_for_transfer())
            .map_err(|_| CliFailure::new("application execution failed"))?
        {
            Ok(_) => {}
            // Another owner opened the session after this one lost its lease.
            // There is nothing of this owner's left to release; the reopen
            // below either succeeds once that owner is gone or says it holds
            // the session.
            Err(error) if error.kind() == plakous::sessions::AgentSessionsErrorKind::Ownership => {
                log_ev(
                    self.log.as_ref(),
                    &self.config,
                    SessionEvent::new("session.lease_taken_over"),
                );
            }
            Err(error) if error.released_locator().is_some() => {}
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "the stopped session did not release: {error}"
                )));
            }
        }
        if self.config.session.store_revision == 0 {
            self.config
                .store
                .save(&mut self.config.session)
                .map_err(CliFailure::new)?;
        }
        // Transfer reopens the projected head, just like a fresh attachment.
        let views = self
            .executor
            .block_on(super::super::projection::project_to_tail(
                self.opened.sessions(),
                &self.config.store,
                &self.config.session.id,
                self.opened.locator(),
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        self.config.session.plakous_locator = super::super::projection::head_locator(&views);
        let mut replacement = self
            .executor
            .block_on(build_session_from_config(
                &mut self.config,
                &mut self.stderr_buf,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))??;
        replacement.inherit_topology_owner(&mut self.opened);
        let report = replacement
            .agent()
            .and_then(plakous::agent::Agent::abandoned_turn)
            .cloned();
        let previous = std::mem::replace(&mut self.opened, replacement);
        // Spec 02 §4.2 step 2: apply what the reopen committed.
        self.project_now();
        let applied_cursor = self
            .config
            .store
            .load_views(&self.config.session.id)
            .map_err(CliFailure::new)?
            .map_or(previous_cursor, |views| views.cursor);
        let locator = self.opened.locator();
        let terminal_published = report.is_none()
            && self
                .executor
                .block_on(crate::driver::reopen::terminal_published_between(
                    self.opened.sessions(),
                    locator,
                    previous_cursor,
                    applied_cursor,
                ))
                .map_err(|_| CliFailure::new("application execution failed"))?
                .map_err(|error| CliFailure::new(error.to_string()))?;
        let released = match self.executor.block_on(previous.release()) {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => Some(error.to_string()),
            Err(_) => Some("application execution failed".to_owned()),
        };
        if let Some(error) = released {
            let _ = self.events.notice(TranscriptRow::error(
                NoticeScope::Recovery,
                format!("the stopped session's attachment did not release cleanly: {error}"),
            ));
        }
        self.executor
            .block_on(super::super::result_mail::settle_waits(
                &self.config.topology_rows(),
                &self.config.session,
                &self.opened,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        self.config
            .session
            .bind_plakous_locator(self.opened.locator())
            .map_err(CliFailure::new)?;
        self.config.session.touch();
        self.config
            .store
            .save(&mut self.config.session)
            .map_err(CliFailure::new)?;
        if query_context(&self.executor, &self.opened, ContextQuery::Current).is_ok_and(
            |projection| projection.disposition() == ContextDisposition::RecoveryRequired,
        ) {
            return Err(CliFailure::new(
                "the reopened session still has an unfinished operation this release cannot close",
            ));
        }
        Ok(ReopenedTurn {
            abandoned: report,
            terminal_published,
        })
    }
}

pub(super) fn log_abandoned_report<P: plakous_host_protocol::ProtocolExtension>(
    log: Option<&crate::session_log::SessionLog>,
    config: &super::HostConfig<P>,
    report: &AbandonedTurnReport,
) {
    let mut record = SessionEvent::new("turn.abandoned")
        .str("id", &config.session.id)
        .str("claim", report.claim().as_str())
        .str("cause", report.cause().as_str())
        .u64("operations", report.operations().len() as u64);
    for (index, operation) in report.operations().iter().enumerate() {
        record = record.u64(&format!("operation_{index}"), *operation);
    }
    log_ev(log, config, record);
}

#[cfg(test)]
mod tests;
