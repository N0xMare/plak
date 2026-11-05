//! A host's idle release (spec 02 §4.4). A host keeps a session resident
//! while anything needs it: a turn, a pending command or recovery debt, a
//! pending continuation, a background process, a resident child in its
//! tree, or an attachment. Once none has held for the host's idle release,
//! the session's own actor releases it through its joined quit. Every
//! condition is read here, in the actor, at the moment of the decision, so
//! a job terminal, an attach or an intent racing the timer is never lost:
//! each either lands before the check and keeps the session, or after the
//! release and opens it again.

use std::time::Instant;

use super::{HostStep, RootActor};

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Whether the host releases the session now: it releases idle
    /// sessions, and nothing has kept this one resident for the whole idle
    /// release. Anything that keeps it resident restarts the wait.
    pub(super) async fn idle_release_due(&mut self) -> bool {
        let Some(release) = self.role.config.idle_release.clone() else {
            return false;
        };
        if release.attachments.get() > 0 || !self.residency_clear().await {
            self.role.idle_since = None;
            return false;
        }
        let since = *self.role.idle_since.get_or_insert_with(Instant::now);
        since.elapsed() >= release.after
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
