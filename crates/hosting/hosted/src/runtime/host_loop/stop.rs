//! A host's stop of the session's work (spec 02 §4.4, spec 03 §14.1): the
//! wire's `stop_session`, which reaches the actor as
//! [`UserAction::StopSession`](super::UserAction::StopSession). The root's
//! stop record is the commit point, written last. Child-wake suppression is
//! durable first, then every running or held child is interrupted with
//! cause `stopped` and its live turn cancelled, then the queue is paused
//! and background processes are stopped, and only then is a held or pending
//! root continuation recorded `stopped`. A crash before that root record
//! is an ordinary loss. After it, with no root turn admitted since, the
//! whole tree reads stopped and nothing continues it; the next prompt's
//! admission clears the wake suppression. A running turn signals that tree
//! stop without joining, so its cancel is not held behind the children's
//! retirement, and the idle boundary joins them and sweeps again for
//! anything spawned in between. The
//! ticket is settled once all of it is durable, before the stop's notice,
//! so the wire answers `stopped` only then. A repeated stop records nothing
//! new and is settled at once.

use plakous_host_protocol::StopTicket;

use super::{RootActor, SessionQueueDrainState, commit_session_update, durable_tree_id};
use crate::sessions::projector::RootInterruptCause;
use crate::sessions::{NoticeScope, TranscriptRow};

/// The live notice of a host's stop.
pub(super) const SESSION_STOPPED_NOTE: &str =
    "stopped: the session's work ended, and nothing continues it until it is prompted again";

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Stops the session's work and settles `ticket`: `Ok` once every stop
    /// record is durable, or why one is not. `unrecorded` is why a running
    /// turn's root stop was not recorded, when it was not.
    pub(super) async fn stop_session(&mut self, ticket: StopTicket, unrecorded: Option<String>) {
        let mut failures: Vec<String> = unrecorded.into_iter().collect();
        let before = self.stop_durable_shape();
        // Suppression first, so a child that finishes in a later crash
        // cannot wake the root before the root record exists.
        self.cut("wake");
        if let Err(failure) = self.suppress_child_wake(None) {
            failures.push(failure.message().to_owned());
        }
        self.cut("tree");
        let tree_id = durable_tree_id(&self.role.config.topology_rows(), &self.role.config.session);
        if let Err(error) =
            super::super::topology_state::mark_root_interrupted_for_run(&tree_id).await
        {
            failures.push(error);
        }
        if let Err(error) = self.role.config.state_roots.finish_child_process_stops(&tree_id).await {
            failures.push(error.message().to_owned());
        }
        // What would start by itself, before the root record: a crash here
        // is still an ordinary loss.
        self.cancel_transient_resume();
        self.role.config.continuation = None;
        self.role.pending_start = None;
        self.cut("queue");
        if !self.role.config.session.pending_queue.is_empty() {
            // The live drain stops now, including a session not stored yet.
            // A stored session records the same pause.
            self.role.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            if self.role.config.session.store_revision != 0
                && let Err(error) = commit_session_update(
                    &*self.role.config.store,
                    &self.role.config.home,
                    &mut self.role.config.session,
                    |candidate| {
                        candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                        true
                    },
                )
            {
                failures.push(format!("the queue was not paused: {error}"));
            }
        }
        self.cut("process");
        if let Err(error) = self.role.config.state_roots.job_registry().stop_user(None) {
            failures.push(error);
        }
        self.update_background_process_count();
        // The commit point. Before this write a lost host is an ordinary
        // loss; after it the tree is stopped and nothing continues it.
        self.cut("root");
        if let Err(error) = self.stop_root_continuation(RootInterruptCause::Stopped) {
            failures.push(error);
        }
        self.cut("after");
        let outcome = if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        };
        let fresh = self.stop_durable_shape() != before;
        tracing::info!(
            target: "plak::host",
            session = %self.role.config.session.id,
            recorded = outcome.is_ok(),
            "session stopped"
        );
        self.finish_stop(ticket, outcome, fresh);
    }

    /// What a stop records, so a repeat that changed nothing is not announced
    /// again. A new transcript row would be a new write.
    fn stop_durable_shape(&self) -> (usize, usize, usize, u64) {
        let id = &self.role.config.session.id;
        let Ok(intents) = self.role.config.store.load_intents(id) else {
            return (0, 0, 0, self.role.config.session.store_revision);
        };
        let revision = self
            .role
            .config
            .store
            .load(id)
            .map(|record| record.store_revision)
            .unwrap_or(0);
        (
            intents.wake_suppressions.len(),
            intents.pause_causes.len(),
            intents.root_interrupts.len(),
            revision,
        )
    }

    /// Settles `ticket`, then tells the session. The settle wakes whoever
    /// is waiting on the answer, so it does not depend on this notice. A
    /// stop that recorded nothing new is not announced again.
    fn finish_stop(&mut self, ticket: StopTicket, outcome: Result<(), String>, fresh: bool) {
        #[cfg(test)]
        if let Some(delay) = crate::runtime::test_seams::notice_delay(&self.role.config.session.id)
        {
            if fresh {
                self.emit_stop_notice(&outcome);
            }
            std::thread::sleep(delay);
            self.role.actions.settle_stop(ticket, outcome);
            return;
        }
        self.role.actions.settle_stop(ticket, outcome.clone());
        if fresh {
            self.emit_stop_notice(&outcome);
        }
    }

    fn emit_stop_notice(&mut self, outcome: &Result<(), String>) {
        let row = match outcome {
            Ok(()) => TranscriptRow::info(NoticeScope::Session, SESSION_STOPPED_NOTE),
            Err(detail) => TranscriptRow::error(
                NoticeScope::Session,
                format!("the stop is not fully recorded: {detail}"),
            ),
        };
        let _ = self.role.events.notice(row);
    }

    #[cfg(test)]
    fn cut(&self, at: &str) {
        assert!(
            !crate::runtime::test_seams::stop_cut(&self.role.config.session.id, at),
            "injected stop cut at {at}"
        );
    }

    #[cfg(not(test))]
    fn cut(&self, _at: &str) {}
}
