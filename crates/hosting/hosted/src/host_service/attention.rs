use plakous_host_protocol::{ProtocolExtension, SessionObservation};

use super::{HostService, Snapshot, tree_nodes};

/// Current published root conditions, without approval or credential details.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct KeptDirectoryAttention {
    /// The first host observation of the current approval category.
    pub approval: Option<u64>,
    /// The first host observation of the current credential wait.
    pub credential: Option<u64>,
    /// The first host observation of the current context block.
    pub capacity: Option<u64>,
    /// The account and first host observation of the active quota refusal.
    pub usage_limit: Option<(String, u64)>,
    /// A runtime-only root blocker, if any.
    pub runtime: Option<crate::runtime::operator_attention::AttentionReason>,
    /// A descendant needs operator input.
    pub child: bool,
}

#[derive(Default)]
pub(super) struct LiveAttention {
    kept: KeptDirectoryAttention,
    credential_account: Option<String>,
}

impl LiveAttention {
    pub(super) fn observe<P: ProtocolExtension>(
        &mut self,
        snapshot: &Snapshot<P>,
        event: &SessionObservation<P>,
    ) {
        self.apply(snapshot, event, plakous::model::now_unix_secs);
    }

    fn apply<P: ProtocolExtension>(
        &mut self,
        snapshot: &Snapshot<P>,
        event: &SessionObservation<P>,
        now: impl Fn() -> u64,
    ) {
        use plakous_host_protocol::observation::TurnPhase;
        match event {
            SessionObservation::SessionOpening | SessionObservation::SessionOpened { .. } => {
                *self = Self::default();
            }
            SessionObservation::ApprovalRequired { .. }
            | SessionObservation::ApprovalResolved { .. } => {
                active_since(&mut self.kept.approval, !snapshot.approvals.is_empty(), now);
            }
            SessionObservation::CredentialWait { account, .. } => {
                if self.credential_account != *account {
                    self.kept.credential = None;
                    self.credential_account.clone_from(account);
                }
                active_since(&mut self.kept.credential, account.is_some(), now);
            }
            SessionObservation::ContextCapacityRecovery(recovery) => {
                active_since(&mut self.kept.capacity, recovery.is_some(), now);
            }
            SessionObservation::TurnLifecycle(lifecycle)
                if lifecycle.phase == TurnPhase::Admitted =>
            {
                self.kept.usage_limit = None;
            }
            SessionObservation::TurnLifecycle(lifecycle) => {
                if let Some(limit) = lifecycle.usage_limit.as_ref()
                    && let Ok(account) = limit.account.parse::<crate::account::AccountRef>()
                    && self
                        .kept
                        .usage_limit
                        .as_ref()
                        .is_none_or(|(current, _)| current != account.as_str())
                {
                    self.kept.usage_limit = Some((account.to_string(), now()));
                }
            }
            _ => {}
        }
    }
}

fn active_since(since: &mut Option<u64>, active: bool, now: impl FnOnce() -> u64) {
    if active {
        since.get_or_insert_with(now);
    } else {
        *since = None;
    }
}

/// What a session's host service kept that its host's directory shows
/// ([`HostService::kept_activity`]).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct KeptActivity {
    /// A turn runs, or a child's work is live.
    pub busy: bool,
    /// The root's own turn runs, whatever its children do.
    pub turn_busy: bool,
    /// An approval, credential wait or context-capacity decision needs the operator.
    pub awaiting_operator: bool,
    /// Background processes running or stopping.
    pub processes: usize,
    /// Children whose work is live, including parked and sealing children.
    pub children: usize,
    /// Descendants pending, running, or sealing; excludes parked work.
    pub running_children: usize,
    /// Children awaiting consent to continue after host loss.
    pub held_children: usize,
}

impl<P: ProtocolExtension> HostService<P> {
    /// Typed resident facts for the draft directory projection.
    #[must_use]
    pub fn kept_directory_attention(&self) -> KeptDirectoryAttention {
        let mut kept = {
            let state = self.lock();
            let mut kept = state.directory_attention.kept.clone();
            kept.runtime = state.count.attention().map(|attention| attention.reason);
            kept
        };
        kept.child = crate::runtime::tree_needs_operator(&self.root().tree_id);
        kept
    }

    /// What this service kept of the session that a directory shows: whether
    /// a turn runs, whether an approval or a credential wait needs the
    /// operator, and how many background processes run.
    #[must_use]
    pub fn kept_activity(&self) -> KeptActivity {
        let tree_id = &self.root().tree_id;
        let mut state = self.lock();
        let children = tree_nodes::try_child_work_counts(tree_id);
        if let Some(counts) = children {
            state.directory_children = Some((tree_id.clone(), counts));
        }
        let snapshot = &state.snapshot;
        let mut kept = KeptActivity {
            awaiting_operator: !snapshot.approvals.is_empty()
                || snapshot.credential_wait.is_some()
                || state.count.attention().is_some(),
            ..KeptActivity::default()
        };
        let counts = state
            .directory_children
            .as_ref()
            .filter(|(tree, _)| tree == tree_id)
            .map_or_default(|(_, counts)| *counts);
        kept.children = counts.work;
        kept.running_children = counts.running;
        kept.held_children = counts.held;
        for (observation, _) in &snapshot.states {
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
        kept.awaiting_operator |= crate::runtime::tree_needs_operator(tree_id);
        kept.busy |= kept.children > 0;
        kept
    }
}
