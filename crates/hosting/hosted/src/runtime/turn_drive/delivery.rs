//! Submit one immutable steer and let the feed projector settle its queue item.

use super::super::admission::{QueuePersistAction, report_queue_persist_failed};
use super::{
    CommandId, HostEvent, TurnDrive, emit_queue_snapshot, queue, reject_stale_queue_action,
    sessions,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow, observe_test_boundary};
use crate::sessions::projector::{Submission, SubmissionKind};
use crate::sessions::{NoticeScope, TranscriptRow};

impl<P: plakous_host_protocol::ProtocolExtension> TurnDrive<'_, P> {
    pub(super) async fn send_queued_now(&mut self, id: String, revision: u64) -> bool {
        if (self.host.session.queue_projection_debt
            || self.host.session.queue_commands.has_pending())
            && let Ok(commands) = super::super::projection::queue_join(
                self.host.projection_store,
                &self.host.session.id,
            )
            .await
        {
            self.apply_queue_join(commands);
        }
        let Some(item) = queue::get(&self.host.session.pending_queue, &id, revision) else {
            reject_stale_queue_action(self.host.events, self.host.session);
            return true;
        };
        let existing = self.host.session.queue_commands.pending_for(&item).cloned();
        if self.host.session.queue_projection_debt
            || (self.host.session.queue_commands.has_pending() && existing.is_none())
            || existing
                .as_ref()
                .is_some_and(|submission| submission.kind != SubmissionKind::SendNowSteer)
        {
            self.note_queue_projection_wait();
            return true;
        }
        let was_pending = existing.is_some();
        let intent = existing.unwrap_or_else(|| Submission {
            command_id: format!("plak.send-now.{id}.{revision}"),
            kind: SubmissionKind::SendNowSteer,
            queue_item_id: id,
            queue_item_revision: revision,
            target_turn_id: Some(self.control.turn_id().to_string()),
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        });
        let dispatch = (|| -> Result<_, String> {
            let command_id =
                CommandId::new(&intent.command_id).map_err(|error| error.to_string())?;
            let turn_id = intent
                .target_turn_id
                .as_deref()
                .ok_or_else(|| "stored steer has no target turn".to_owned())?
                .parse()
                .map_err(|_| "stored steer target is invalid".to_owned())?;
            self.host
                .store
                .save_submission(self.host.session, &intent)?;
            Ok(plakous::agent::TurnCommand::Steer {
                command_id,
                turn_id,
                input: item.user_input(&intent.canonical_command),
            })
        })();
        let command = match dispatch {
            Ok(command) => command,
            Err(error) => {
                report_queue_persist_failed(
                    self.host.events,
                    self.host.home,
                    self.host.session,
                    QueuePersistAction::SendNow,
                    &error,
                );
                return true;
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            self.host.test_injection,
            TestHostWorkflow::SendNow,
            TestHostBoundary::Prepared,
        );
        let submission = match self.host.job_agent.command(command).await {
            Ok(submission) => submission,
            Err(error) => {
                if error.kind() == plakous::agent::ControlErrorKind::Backpressure && !was_pending {
                    if let Err(save_error) = self.host.store.discard_submission(
                        self.host.session,
                        &intent.command_id,
                        None,
                    ) {
                        report_queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::SendNow,
                            &save_error,
                        );
                    }
                } else {
                    self.host.session.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
                }
                let _ = self.host.events.notice(TranscriptRow::error(
                    NoticeScope::Queue,
                    format!("steer submission failed: {error}"),
                ));
                return false;
            }
        };
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            self.host.test_injection,
            TestHostWorkflow::SendNow,
            TestHostBoundary::RuntimeReturned,
        );
        if let Some(followed) = self.host.session.plakous_locator {
            let projected = match self.host.roots.runtime_sessions() {
                Ok(runtime) => {
                    super::super::projection::queue_after_ack(
                        &runtime,
                        self.host.projection_store,
                        &self.host.session.id,
                        followed,
                    )
                    .await
                }
                Err(error) => Err(error.message().to_owned()),
            };
            match projected {
                Ok(commands) => {
                    self.apply_queue_join(commands);
                }
                Err(_) => self.host.session.queue_projection_debt = true,
            }
        }
        let ack = submission.acknowledgement();
        let consumed = if ack
            .rejection()
            .is_some_and(|error| error.kind() == plakous::agent::ControlErrorKind::Reclaimed)
        {
            true
        } else if let Some(locator) = self.host.session.plakous_locator {
            let checked = match self.host.roots.runtime_sessions() {
                Ok(runtime) => {
                    super::admission::duplicate_consumed(
                        &runtime,
                        self.host.projection_store,
                        &self.host.session.id,
                        &submission,
                        locator,
                    )
                    .await
                }
                Err(error) => Err(error.message().to_owned()),
            };
            match checked {
                Ok(consumed) => consumed,
                Err(error) => {
                    self.host.session.queue_projection_debt = true;
                    let _ = self
                        .host
                        .events
                        .notice(TranscriptRow::error(NoticeScope::Queue, error));
                    return false;
                }
            }
        } else {
            false
        };
        if consumed {
            let mut candidate = self.host.session.clone();
            candidate.pending_queue.retain(|queued| {
                queued.id != intent.queue_item_id || queued.revision != intent.queue_item_revision
            });
            let row = TranscriptRow::info(
                NoticeScope::Queue,
                "queued prompt already ran; its result is no longer kept",
            );
            candidate.record_notice(row.clone());
            match self
                .host
                .store
                .discard_submission(&mut candidate, &intent.command_id, None)
            {
                Ok(()) => {
                    *self.host.session = candidate;
                    let _ = self.host.events.send(HostEvent::Row(row));
                }
                Err(error) => report_queue_persist_failed(
                    self.host.events,
                    self.host.home,
                    self.host.session,
                    QueuePersistAction::SendNow,
                    &error,
                ),
            }
        } else if let Some(error) = ack.rejection() {
            self.host.session.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
            let has_fact = match self.host.store.load_views(&self.host.session.id) {
                Ok(views) => views
                    .is_some_and(|views| views.state.deliveries.contains_key(&intent.command_id)),
                Err(error) => {
                    self.host.session.queue_projection_debt = true;
                    report_queue_persist_failed(
                        self.host.events,
                        self.host.home,
                        self.host.session,
                        QueuePersistAction::SendNow,
                        &error,
                    );
                    return false;
                }
            };
            if ack.feed_seq().is_none()
                && !has_fact
                && !matches!(
                    error.kind(),
                    plakous::agent::ControlErrorKind::OutcomeUnknown
                        | plakous::agent::ControlErrorKind::Internal
                        | plakous::agent::ControlErrorKind::Shutdown
                        | plakous::agent::ControlErrorKind::LedgerFull
                )
            {
                let cause = sessions::projector::HostPauseCause::UnrecordedRefusal {
                    reason: error.to_string(),
                };
                if let Err(save_error) = self.host.store.discard_submission(
                    self.host.session,
                    &intent.command_id,
                    Some(cause),
                ) {
                    report_queue_persist_failed(
                        self.host.events,
                        self.host.home,
                        self.host.session,
                        QueuePersistAction::SendNow,
                        &save_error,
                    );
                }
            }
            let _ = self.host.events.notice(TranscriptRow::error(NoticeScope::Queue,
                "send-now was rejected; the exact queue item was retained and automatic drain paused"));
        } else {
            let _ = self.host.events.send(HostEvent::Status {
                phase: "running".into(),
                queue_len: self.host.session.pending_queue.len(),
                detail: Some("steering...".into()),
            });
        }
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            self.host.test_injection,
            TestHostWorkflow::SendNow,
            TestHostBoundary::ProductPublished,
        );
        emit_queue_snapshot(self.host.events, self.host.session);
        false
    }
}
