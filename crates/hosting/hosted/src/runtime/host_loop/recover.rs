//! Recover a fenced session in place: release the dead attachment and reopen.
//!
//! A turn that ends recovery-required leaves its driver stopped, and nothing
//! can revive that driver. What a relaunch would do is open a new owner of the
//! same session, whose open abandons the unfinished turn exactly once (spec 01
//! §13.1). This does the same thing without quitting: the one reopen path the
//! crash matrix already covers, taken in place.
use super::RootRecovery;

use crate::surface_protocol::ContentEvent;
use plakous::agent::AbandonedTurnReport;
use plakous::sessions::feed::FeedSeq;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

use super::{
    CliFailure, CliResult, ContextQuery, HostStep, RootActor, SessionEvent,
    build_session_from_config, durable_tree_id, emit_queue_pause_note, emit_queue_snapshot,
    emit_unavailable_context, log_ev, native_compact_facet, query_context, queue_pause,
    report_context,
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
    /// The actor itself, at idle entry after the fence (design §6.0).
    Eager,
}

pub(super) struct ReopenedTurn {
    pub(super) abandoned: Option<AbandonedTurnReport>,
    terminal_published: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Reopens this session in place and publishes what the reopen closed.
    ///
    /// On failure the old attachment stays released and the recovery overlay
    /// stays set, so the next action tries again; nothing is lost and nothing
    /// ends in `/new`.
    #[allow(
        clippy::too_many_lines,
        reason = "Recovery distinguishes retained approvals, abandoned turns, pending projection debt, and the exact source binding before reopening."
    )]
    pub(super) async fn recover_session(
        &mut self,
        trigger: RecoveryTrigger,
    ) -> CliResult<HostStep> {
        let _ = self.role.events.send(ContentEvent::SetBusy(true));
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::RecoveringInterruptedTurn),
        });
        let recovered = self.reopen_in_place().await;
        let restored_working = recovered.is_ok() && self.opened.has_recovered_turn();
        if !restored_working {
            let _ = self.role.events.send(ContentEvent::SetBusy(false));
        }
        let ReopenedTurn {
            abandoned: report,
            terminal_published,
        } = match recovered {
            Ok(report) => report,
            Err(failure) => {
                log_ev(
                    self.role.log.as_ref(),
                    &self.role.config,
                    SessionEvent::new("session.recover_failed").str("msg", failure.message()),
                );
                let _ = self.role.events.send(ContentEvent::Status {
                    phase: HostPhase::Error,
                    queue_len: self.role.config.session.pending_queue.len(),
                    detail: Some(HostStatusDetail::RecoveryIncomplete),
                });
                return Err(CliFailure::new(format!(
                    "the session could not be reopened to recover the interrupted turn ({}); nothing was lost, and the next prompt or explicit recovery retries",
                    failure.message()
                )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::RecoverSession {}));
            }
        };
        self.role.recovery = RootRecovery::Clear;
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
                RecoveryTrigger::Eager => "eager",
            },
        );
        if let Some(report) = &report {
            record = record
                .str("claim", report.claim().as_str())
                .str("cause", report.cause().as_str());
        }
        log_ev(self.role.log.as_ref(), &self.role.config, record);
        if let Some(report) = &report {
            if !terminal_published {
                let _ = self.role.events.send(ContentEvent::Row(TranscriptRow::info(
                    NoticeScope::Recovery,
                    super::super::events::abandoned_turn_operator_line(report),
                )));
            }
        }
        if report.is_none() && !restored_working {
            let _ = self.role.events.send(ContentEvent::Row(TranscriptRow::info(NoticeScope::Recovery,
                if terminal_published {
                    "plak: reopened the session and published the saved turn's result; the conversation continues from its saved state".to_owned()
                } else {
                    "plak: reopened the session; the interrupted turn left nothing outstanding, and the conversation continues from its saved state".to_owned()
                },
            )));
        }
        let _ = self.role.events.send(ContentEvent::Recovered {
            claim,
            cause,
            operations,
        });
        if let Err(error) = report_context(
            &self.opened,
            &self.role.events,
            "after recovery",
            self.role
                .config
                .target
                .native_provider()
                .is_some_and(native_compact_facet),
            self.role.config.session.context_counters(),
        )
        .await
        {
            emit_unavailable_context(&self.role.events);
            let _ = self.role.events.send(ContentEvent::Row(
                TranscriptRow::error(NoticeScope::Recovery, error.message().to_owned())
                    .with_optional_remedy(error.remedy()),
            ));
        }
        let _ = self.role.events.send(ContentEvent::AgentAttached(true));
        self.spawn_tree_recovery();
        // What was queued behind the fenced turn was written for a turn whose
        // outcome nobody saw, so it stays paused (spec 02 §4.2); the note says
        // exactly how to resume it, and a prompt typed now still runs at once.
        emit_queue_snapshot(&self.role.events, &self.role.config.session);
        emit_queue_pause_note(&self.role.events, &self.role.config.session);
        if restored_working {
            return self.drive_restored_turn().await;
        }
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Idle,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: Some(HostStatusDetail::Ready {
                model_id: self.role.config.model_id.clone(),
                pause: queue_pause(&self.role.config.session),
            }),
        });
        Ok(HostStep::Continue)
    }

    /// Finishes the tree's custody-attach scan at the host's open (design
    /// section 6.6).
    pub(super) fn finish_attach_scan(&self) {
        super::super::tree::finish_custody_attach_scan(&self.role.config, &self.role.events);
    }

    /// Starts a `recover_node` run over this tree's children in the
    /// background. It never waits on a lease, and what it settled or refused
    /// reaches the user as one system note when it finishes.
    pub(super) fn spawn_tree_recovery(&self) {
        let tree_id = durable_tree_id(&self.role.config.topology_rows(), &self.role.config.session);
        let events = self.role.events.clone();
        let session_id = self.role.config.session.id.clone();
        super::super::tree::start_child_recovery(&events, &session_id, tree_id, false, None);
    }

    /// An explicit `/recover` with no interrupted root turn: starts a
    /// `recover_node` run over the tree's children in the background and says
    /// so; its report, or `nothing` when it had nothing to do, follows as a
    /// note. Never waits on the run.
    pub(super) fn recover_tree_children(&mut self, nothing: Option<String>) {
        let tree_id = durable_tree_id(&self.role.config.topology_rows(), &self.role.config.session);
        let events = self.role.events.clone();
        let session_id = self.role.config.session.id.clone();
        let for_pass = nothing.clone();
        let started =
            super::super::tree::start_child_recovery(&events, &session_id, tree_id, true, for_pass);
        log_ev(
            self.role.log.as_ref(),
            &self.role.config,
            SessionEvent::new("topology.children_recovery_requested")
                .str("start", format!("{started:?}")),
        );
        if started == crate::agent::TreeRecoveryStart::NoTree
            && let Some(nothing) = nothing
        {
            let _ = self
                .role
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
    pub(super) async fn reopen_in_place(&mut self) -> CliResult<ReopenedTurn> {
        let previous_cursor = self
            .role
            .config
            .store
            .load_views(&self.role.config.session.id)
            .map_err(CliFailure::new)?
            .map_or(FeedSeq::ZERO, |views| views.cursor);
        match self.opened.detach_for_transfer().await {
            Ok(_) => {}
            // Another owner opened the session after this one lost its lease.
            // There is nothing of this owner's left to release; the reopen
            // below either succeeds once that owner is gone or says it holds
            // the session.
            Err(error) if error.kind() == plakous::sessions::AgentSessionsErrorKind::Ownership => {
                log_ev(
                    self.role.log.as_ref(),
                    &self.role.config,
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
        if self.role.config.session.store_revision == 0 {
            self.role
                .config
                .store
                .save(&mut self.role.config.session)
                .map_err(CliFailure::new)?;
        }
        // Transfer reopens the projected head, just like a fresh attachment.
        let views = super::super::projection::project_to_tail(
            self.opened.sessions(),
            &self.role.config.store,
            &self.role.config.session.id,
            self.opened.locator(),
        )
        .await
        .map_err(CliFailure::new)?;
        self.role.config.session.plakous_locator = super::super::projection::head_locator(&views);
        let mut replacement =
            build_session_from_config(&mut self.role.config, &mut self.role.stderr_buf).await?;
        replacement.inherit_topology_owner(&mut self.opened);
        let mut report = replacement
            .agent()
            .and_then(plakous::agent::Agent::abandoned_turn)
            .cloned();
        // Spec 02 §4.2: the claim is logged before the projector applies the
        // closure, so no crash between them loses it; the next open then
        // derives the notice and may log it a second time.
        if let Some(report) = &report {
            log_abandoned_report(self.role.log.as_ref(), &self.role.config, report);
        }
        let previous = std::mem::replace(&mut self.opened, replacement);
        // Spec 02 §4.2 step 2: apply what the reopen committed.
        self.project_now().await;
        // A closure an earlier, failed reopen committed is applied here and
        // reported now; one this reopen committed was reported above.
        let projected = super::take_projected_abandonments(
            &self.role.pending_usage,
            self.opened.locator().session_id(),
        );
        if report.is_none()
            && let Some(abandonment) = projected.into_iter().next()
        {
            log_abandoned_report(
                self.role.log.as_ref(),
                &self.role.config,
                &abandonment.report,
            );
            report = Some(abandonment.report);
        }
        let applied_cursor = self
            .role
            .config
            .store
            .load_views(&self.role.config.session.id)
            .map_err(CliFailure::new)?
            .map_or(previous_cursor, |views| views.cursor);
        let locator = self.opened.locator();
        let terminal_published = report.is_none()
            && crate::driver::reopen::terminal_published_between(
                self.opened.sessions(),
                locator,
                previous_cursor,
                applied_cursor,
            )
            .await
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let released = previous
            .release()
            .await
            .err()
            .map(|error| error.to_string());
        if let Some(error) = released {
            let _ = self.role.events.notice(TranscriptRow::error(
                NoticeScope::Recovery,
                format!("the stopped session's attachment did not release cleanly: {error}"),
            ));
        }
        super::super::result_mail::settle_waits(
            &self.role.config.topology_rows(),
            &self.role.config.session,
            &self.opened,
        )
        .await
        .map_err(CliFailure::from)?;
        self.role
            .config
            .session
            .bind_plakous_locator(self.opened.locator())
            .map_err(CliFailure::new)?;
        self.role.config.session.touch();
        self.role
            .config
            .store
            .save(&mut self.role.config.session)
            .map_err(CliFailure::new)?;
        if query_context(&self.opened, ContextQuery::Current)
            .await
            .is_ok_and(|projection| {
                projection.disposition() == ContextDisposition::RecoveryRequired
            })
        {
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
