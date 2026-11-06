//! A host's idle release (spec 02 §4.4). A host keeps a session resident
//! while anything needs it: a turn, a pending command or recovery debt, a
//! pending continuation, a background process, a resident child in its
//! tree, or an attachment or a connection's hold. Once none has held for
//! the host's idle release, the session's own actor releases it through its
//! joined quit. Every condition is read here, in the actor, at the moment
//! of the decision, so a job terminal racing the timer is never lost. An
//! intent is not lost either: the actor reads the attachment count's word
//! before the receive whose timeout leads to the decision, and the decision
//! ends by marking the session releasing only from exactly that word, with
//! nothing counted ([`AttachmentCount::begin_release`]). Any attach or hold
//! since, even one that sent an intent and is already gone, changed the
//! word, so the release waits afresh and the intent is received; and after
//! the mark an attach or a hold is refused and finds the session released,
//! and its host opens it again.
//!
//! [`AttachmentCount::begin_release`]: crate::host_service::AttachmentCount::begin_release

use std::time::Instant;

use super::{HostStep, RootActor};
use crate::host_service::CountSeen;

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// The attachment count's word, read before a receive whose timeout
    /// may lead to [`Self::idle_release_due`].
    pub(super) fn residency_seen(&self) -> Option<CountSeen> {
        self.role
            .config
            .idle_release
            .as_ref()
            .map(|release| release.attachments.seen())
    }

    /// Whether the host releases the session now: it releases idle
    /// sessions, nothing has kept this one resident for the whole idle
    /// release, and nothing was counted since `seen` was read. Anything that
    /// keeps it resident restarts the wait.
    pub(super) async fn idle_release_due(&mut self, seen: Option<CountSeen>) -> bool {
        let (Some(release), Some(seen)) = (self.role.config.idle_release.clone(), seen) else {
            return false;
        };
        if seen.count() > 0 || !self.residency_clear().await {
            self.role.idle_since = None;
            return false;
        }
        let since = *self.role.idle_since.get_or_insert_with(Instant::now);
        if since.elapsed() < release.after {
            return false;
        }
        // An attach or a hold since the word was read wins, even one that
        // has already ended: what it sent waits in the mailbox.
        if !release.attachments.begin_release(seen) {
            self.role.idle_since = None;
            return false;
        }
        true
    }

    /// Whether nothing but an attachment would keep the session resident.
    async fn residency_clear(&self) -> bool {
        let role = &self.role;
        let pending = role.pending_start.is_some()
            || role.pending_resume.is_some()
            || role.root_loss_resume_due
            || role.config.continuation.is_some()
            || role.recovery.blocked()
            || role.interrupt_state.borrow().has_deferred()
            || role.interrupt_state.borrow().has_stops()
            || !role.config.session.pending_queue.is_empty();
        if pending || role.config.state_roots.job_registry().running_count() > 0 {
            return false;
        }
        let tree_id = super::durable_tree_id(&role.config.topology_rows(), &role.config.session);
        match crate::topology_runtime::lookup(&tree_id) {
            Some(runtime) => runtime.children_quiet().await,
            None => true,
        }
    }

    /// Releases the idle session: the joined quit releases its runtime
    /// lease and holder record, then the root's hub entry, and the tree's
    /// custody last, since nothing else in it is resident.
    pub(super) fn release_idle(&mut self) -> HostStep {
        self.quit_started_at.get_or_insert_with(Instant::now);
        HostStep::Quit
    }
}

#[cfg(test)]
mod tests;
