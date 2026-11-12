//! A planned handoff's record (spec 03 §14.1): before the releasing host
//! lets go, each node with work in flight is interrupted with cause
//! `handoff`, so the next host continues it once by itself. Only the
//! releasing host writes it, inside its own release and before any lease is
//! released. A write that fails leaves the release as it was: the next open
//! then reads an ordinary loss and holds, and nothing continues by itself
//! without a durable handoff row.

use super::{RootActor, SessionEvent, durable_tree_id, log_ev};
use crate::runtime::root_loss::{
    RootInterruptWrite, handed_off_root_turn, loss_scan, record_root_handoff,
};

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    pub(super) async fn record_planned_handoff(&mut self) {
        let rows = self.role.config.topology_rows();
        let tree_id = durable_tree_id(&rows, &self.role.config.session);
        if let Some(runtime) = crate::topology_runtime::lookup(&tree_id) {
            // A handoff covers the turn it interrupts: its fold position has
            // to be past that turn's acceptance, or the turn reads as one
            // admitted after the handoff and the child is held instead.
            if let Err(error) = crate::agent::project_running_children(&runtime).await {
                self.log_unrecorded_handoff("projection", &error);
            }
            let recorded = runtime
                .mutate_then_emit(false, |tree| {
                    tree.record_handoff().map_err(|error| error.message)
                })
                .await;
            if let Err(error) = recorded {
                self.log_unrecorded_handoff("tree", &error);
            }
        }
        // A quit that reached a running root turn: that turn is handed over.
        if self.role.handoff_root_due && self.role.config.session.store_revision != 0 {
            crate::test_support_runner::cut(crate::test_support_runner::HostCut::BeforeRootHandoff);
            self.record_root_handoff().await;
        }
        #[cfg(test)]
        hooks::recorded();
    }

    async fn record_root_handoff(&self) {
        let store = self.role.config.store.as_ref();
        let session = &self.role.config.session;
        let recorded = match loss_scan(store, session) {
            Ok(Some(scan)) => match handed_off_root_turn(self.opened.sessions(), scan).await {
                Ok(Some(turn)) => match record_root_handoff(store, session, &turn) {
                    // The next open reads the turn as a loss and holds it.
                    Ok(RootInterruptWrite::ViewsBehind) => {
                        Err("the session's views had not applied the turn's admission".to_owned())
                    }
                    Ok(_) => Ok(()),
                    Err(error) => Err(error),
                },
                Ok(None) => Ok(()),
                Err(error) => Err(error),
            },
            Ok(None) => Ok(()),
            Err(error) => Err(error),
        };
        if let Err(error) = recorded {
            self.log_unrecorded_handoff("root", &error);
        }
    }

    fn log_unrecorded_handoff(&self, node: &str, error: &str) {
        log_ev(
            self.role.log.as_ref(),
            &self.role.config,
            SessionEvent::new("session.handoff_unrecorded")
                .str("node", node)
                .str("error", error),
        );
    }
}

/// A probe the host-level handoff tests read the stores through, at the
/// point the handoff is recorded and before any lease is released.
#[cfg(test)]
pub(in crate::runtime) mod hooks {
    type Hook = Box<dyn Fn() + Send>;
    static RECORDED: std::sync::Mutex<Option<Hook>> = std::sync::Mutex::new(None);

    /// Runs `hook` when the releasing host has recorded its handoff.
    pub(in crate::runtime) fn after_record(hook: impl Fn() + Send + 'static) {
        *RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Box::new(hook));
    }

    pub(super) fn recorded() {
        if let Some(hook) = RECORDED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            hook();
        }
    }
}
