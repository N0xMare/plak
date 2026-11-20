use plakous_host_protocol::{ProtocolExtension, SessionObservation};

use super::{HostService, KeptActivity, tree_nodes};

impl<P: ProtocolExtension> HostService<P> {
    /// What this service kept of the session that a directory shows: whether
    /// a turn runs, whether an approval or a credential wait needs the
    /// operator, and how many background processes run.
    #[must_use]
    pub fn kept_activity(&self) -> KeptActivity {
        let state = self.lock();
        let snapshot = &state.snapshot;
        let mut kept = KeptActivity {
            awaiting_operator: !snapshot.approvals.is_empty()
                || snapshot.credential_wait.is_some()
                || state.count.attention().is_some(),
            ..KeptActivity::default()
        };
        for observation in &snapshot.states {
            match observation {
                SessionObservation::SetBusy(busy) => {
                    kept.busy = *busy;
                    kept.turn_busy = *busy;
                }
                SessionObservation::Processes { running, .. } => kept.processes = *running,
                SessionObservation::ContextCapacityRecovery(Some(_)) => {
                    kept.awaiting_operator = true;
                }
                _ => {}
            }
        }
        drop(state);
        kept.awaiting_operator |= crate::runtime::tree_needs_operator(&self.root().tree_id);
        (kept.children, kept.held_children) = tree_nodes::child_counts(&self.root().tree_id);
        kept.busy |= kept.children > 0;
        kept
    }
}
