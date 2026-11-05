//! The root's Resume after a lost host (spec 03 §14.1): one Resume resumes
//! every held child, the root's own held continuation and its restored
//! queue. The root's continuation is a host-instruction submission under
//! the decision's command id, recorded after consent and before dispatch.

use super::{
    CliFailure, CliResult, RootActor, SessionQueueDrainState, SessionQueueItem, UserAction,
    commit_session_update, dispatch_topology_navigator, durable_tree_id, emit_queue_snapshot,
    queue, record_emitted_tree_revision,
};
use crate::runtime::admission::{
    QueuePersistAction, report_queue_persist_failed as queue_persist_failed,
};
use crate::runtime::root_loss::{
    ROOT_HANDOFF_RESUME_TEXT, ROOT_LOSS_RESUME_TEXT, RootLossState, record_root_consent,
    root_loss_is_handoff, root_loss_state,
};
use crate::sessions::{NoticeScope, TranscriptRow};

/// The live notice of a root a lost host left held.
pub(super) const ROOT_LOSS_HELD_NOTE: &str =
    "the turn the lost host was running is held; nothing continues until you resume it";

/// The live notice of a stop that ended the root's continuation.
pub(super) const ROOT_LOSS_STOPPED_NOTE: &str =
    "stopped: the turn the lost host was running stays interrupted and nothing continues it";

/// What the idle dispatcher admits next: a prompt text, its queue item and
/// whether it was already echoed.
type NextPrompt = (String, Option<SessionQueueItem>, bool);

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    fn root_loss(&self) -> Option<RootLossState> {
        root_loss_state(self.role.config.store.as_ref(), &self.role.config.session)
            .ok()
            .flatten()
    }

    /// Whether the root holds a continuation after a loss, or has one
    /// pending: what makes `/recover` the root's Resume.
    pub(super) fn root_loss_held(&self) -> bool {
        matches!(
            self.root_loss(),
            Some(RootLossState::Held | RootLossState::Pending(_))
        )
    }

    fn root_loss_handoff(&self) -> bool {
        root_loss_is_handoff(self.role.config.store.as_ref(), &self.role.config.session)
            .unwrap_or(false)
    }

    /// At open: a decision a crash left pending is resubmitted, and a held
    /// continuation a planned handoff left, or any held one under
    /// `resume_after_loss = "continue"`, runs by itself and the restored
    /// queue drains. Under hold, one attention notice names what the lost
    /// host left held: the root's turn and each child, with the one Resume.
    /// The open's scan run sends it once it has marked every child the lost
    /// host left (a child whose turn was running is marked there, not at
    /// load); with no run to carry it, the open sends it itself.
    pub(super) fn arm_root_loss_resume(&mut self) {
        let continues =
            self.role.config.resume_after_loss == crate::runtime::ResumeAfterLoss::Continue;
        let root_held = match self.root_loss() {
            Some(RootLossState::Pending(_)) => {
                self.role.root_loss_resume_due = true;
                false
            }
            Some(RootLossState::Held) if continues || self.root_loss_handoff() => {
                self.role.root_loss_resume_due = true;
                self.resume_restored_queue();
                false
            }
            Some(RootLossState::Held) => true,
            _ => false,
        };
        let root = root_held.then_some(ROOT_LOSS_HELD_NOTE);
        let notice = if continues {
            crate::runtime::tree::HeldNotice::Silent
        } else {
            crate::runtime::tree::HeldNotice::Name { root }
        };
        if self.finish_attach_scan(notice) || continues {
            return;
        }
        let tree_id = durable_tree_id(&self.role.config.topology_rows(), &self.role.config.session);
        // Live only: it is shown again by the next open while the hold
        // stands, and never stored.
        if let Some(note) = crate::runtime::tree::loss_attention_note(&tree_id, root) {
            let _ = self
                .role
                .events
                .send(crate::surface_protocol::ContentEvent::Row(
                    TranscriptRow::warning(NoticeScope::Recovery, note).with_remedy(
                        plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeAfterLoss {},
                    ),
                ));
        }
    }

    /// A stop of everything while the root holds a continuation after a
    /// loss, or has one pending, ends it: stops stay stopped (spec 03
    /// §14.1). An idle interrupt does not.
    pub(super) fn stop_root_continuation(
        &mut self,
        cause: crate::sessions::projector::RootInterruptCause,
    ) {
        self.role.root_loss_resume_due = false;
        match crate::runtime::root_loss::stop_root_continuation(
            self.role.config.store.as_ref(),
            &mut self.role.config.session,
            cause,
        ) {
            Ok(true) => {
                emit_queue_snapshot(&self.role.events, &self.role.config.session);
                let _ = self
                    .role
                    .events
                    .send(crate::surface_protocol::ContentEvent::Row(
                        TranscriptRow::info(NoticeScope::Recovery, ROOT_LOSS_STOPPED_NOTE),
                    ));
            }
            Ok(false) => {}
            Err(error) => {
                let _ = self.role.events.notice(TranscriptRow::error(
                    NoticeScope::Recovery,
                    format!("the stop could not be recorded: {error}"),
                ));
            }
        }
    }

    /// Lets a restored, paused queue drain again. Returns whether it is
    /// now draining.
    fn resume_restored_queue(&mut self) -> bool {
        match commit_session_update(
            &*self.role.config.store,
            &self.role.config.home,
            &mut self.role.config.session,
            |candidate| {
                candidate.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
                true
            },
        ) {
            Ok(true) => {
                emit_queue_snapshot(&self.role.events, &self.role.config.session);
                true
            }
            Ok(false) => false,
            Err(error) => {
                queue_persist_failed(
                    &self.role.events,
                    &self.role.config.home,
                    &self.role.config.session,
                    QueuePersistAction::ResumeDrain,
                    &error,
                );
                false
            }
        }
    }

    /// The root's continuation turn after a loss: consent is recorded
    /// first, then its instruction submission under the decision's command
    /// id. A submission a crash left prepared is that queued item. `None`
    /// when nothing is held, or while the root is not free to run it.
    pub(super) async fn root_loss_resume_delivery(&mut self) -> CliResult<Option<NextPrompt>> {
        self.role.root_loss_resume_due = false;
        if self.role.recovery.blocked() || self.role.pending_start.is_some() {
            return Ok(None);
        }
        let rows = self.role.config.topology_rows();
        let text = if self.root_loss_handoff() {
            ROOT_HANDOFF_RESUME_TEXT
        } else {
            ROOT_LOSS_RESUME_TEXT
        };
        let Some(command) =
            record_root_consent(self.role.config.store.as_ref(), &self.role.config.session)
                .map_err(CliFailure::new)?
        else {
            return Ok(None);
        };
        let intents = self
            .role
            .config
            .store
            .load_intents(&self.role.config.session.id)
            .map_err(CliFailure::new)?;
        if let Some(submission) = intents.submission(&command) {
            return Ok(self
                .role
                .config
                .session
                .pending_queue
                .iter()
                .find(|item| item.id == submission.queue_item_id)
                .cloned()
                .map(|item| (item.text.clone(), Some(item), true)));
        }
        let mut candidate = self.role.config.session.clone();
        let item = candidate
            .enqueue_live(text)
            .map_err(|_| CliFailure::new("the continuation was refused by the queue"))?;
        // The continuation is the root's next turn, so it carries the
        // children the lost host interrupted.
        let command_text = crate::runtime::result_mail::with_root_loss_notice(
            &rows,
            &self.role.config.session,
            text,
        );
        self.role.config.session = crate::runtime::result_mail::prepare_with_command(
            &rows,
            self.role.config.store.as_ref(),
            &candidate,
            &item,
            &command_text,
            None,
            &command,
            crate::driver::ledger::CommandKind::HostWake,
        )
        .await
        .map_err(CliFailure::from)?;
        Ok(Some((item.text.clone(), Some(item), true)))
    }

    /// The operator's one Resume after a lost host: every held child, the
    /// root's own continuation, and the restored queue. Returns what the
    /// root runs next.
    pub(super) async fn resume_after_loss_all(&mut self) -> Option<NextPrompt> {
        let rows = self.role.config.topology_rows();
        let tree_id = durable_tree_id(&rows, &self.role.config.session);
        // A paused queue alone is resumed without a tree note.
        if crate::topology_runtime::lookup(&tree_id)
            .is_some_and(|runtime| crate::agent::children::holds_after_loss(&runtime))
        {
            dispatch_topology_navigator(
                &rows,
                &self.role.config.state_roots,
                &self.role.config.session,
                &self.role.events,
                &UserAction::ResumeLossContinuation { agent_id: None },
                &mut self.role.last_tree_revision,
            )
            .await;
            record_emitted_tree_revision(
                &rows,
                &self.role.config.session,
                &mut self.role.last_tree_revision,
            );
        }
        let draining = self.resume_restored_queue();
        match self.root_loss_resume_delivery().await {
            Ok(Some(next)) => Some(next),
            Ok(None) => draining
                .then(|| {
                    queue::next_drainable(&self.role.config.session)
                        .map(|item| (item.text.clone(), Some(item), false))
                })
                .flatten(),
            Err(failure) => {
                let _ = self.role.events.notice(
                    TranscriptRow::error(NoticeScope::Session, failure.message().to_owned())
                        .with_optional_remedy(failure.remedy()),
                );
                None
            }
        }
    }
}
