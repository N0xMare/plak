//! A profile switch of the root node onto its own line (live succession,
//! spec 02 §5.5, spec 03 §1.2): what changes in the tree, and the tree steps
//! of the switch's write order.
//!
//! The order is product intent (1), tree `epoch_pending` (2), runtime
//! publication (3), product projection (4), tree settlement (5), host
//! activation (6). Steps 2 and 5 exist only when the tree has rows and the
//! roster changes; a virtual tree carries the successor epoch's bytes in the
//! product intent instead, and a roster that is unchanged moves only the
//! tree's view of the root binding.

use std::sync::Arc;

use super::{CliFailure, CliResult, HostConfig, durable_tree_id, sessions, topology_state};
use crate::orchestration::OrchestrationSnapshot;

/// What a switch changes in the tree.
#[derive(Clone, Debug)]
pub(in crate::runtime) enum SuccessionChange {
    /// A tree with rows takes the successor epoch: recorded pending before
    /// the publication, appended after it.
    Append(Arc<OrchestrationSnapshot>),
    /// The roster is unchanged: only the tree's view of the root binding
    /// moves, with no tree intent.
    RootBind,
    /// A virtual tree of one: the successor epoch's bytes travel in the
    /// product intent and no tree rows are written.
    Virtual(Arc<OrchestrationSnapshot>),
}

/// One switch's tree half.
#[derive(Clone, Debug)]
pub(in crate::runtime) struct Succession {
    /// The profile switched to, as the Agents list names it.
    pub(in crate::runtime) profile: String,
    /// The tree the root node belongs to.
    pub(in crate::runtime) tree_id: String,
    /// The successor epoch the profile freezes.
    pub(in crate::runtime) snapshot: Arc<OrchestrationSnapshot>,
    /// Its root template and working set.
    root: sessions::RootProfile,
    /// What changes in that tree.
    pub(in crate::runtime) change: SuccessionChange,
}

impl Succession {
    /// Decides what switching to `profile`, frozen as `roster`, changes in
    /// the session's tree.
    pub(in crate::runtime) fn plan<P: plakous_host_protocol::ProtocolExtension>(
        config: &HostConfig<P>,
        profile: &str,
        roster: &crate::orchestration::RosterInput,
    ) -> CliResult<Self> {
        let rows = config.topology_rows();
        let tree_id = durable_tree_id(&rows, &config.session);
        let snapshot = Arc::new(OrchestrationSnapshot::freeze(roster.clone()));
        let change =
            match topology_state::current_epoch_of(&rows, &tree_id).map_err(CliFailure::new)? {
                None => SuccessionChange::Virtual(Arc::clone(&snapshot)),
                Some(current)
                    if current
                        .with_root_bind(
                            snapshot.root_account.clone(),
                            snapshot.root_tuple.clone(),
                            snapshot.root_effort.clone(),
                        )
                        .digest
                        == snapshot.digest =>
                {
                    SuccessionChange::RootBind
                }
                Some(_) => SuccessionChange::Append(Arc::clone(&snapshot)),
            };
        Ok(Self {
            profile: profile.to_owned(),
            tree_id,
            snapshot,
            root: sessions::RootProfile::from_roster(roster),
            change,
        })
    }

    /// The epoch the successor's root compiles against before its
    /// publication, when it is not the tree's current one.
    pub(in crate::runtime) fn pending_epoch(&self) -> Option<Arc<OrchestrationSnapshot>> {
        match &self.change {
            SuccessionChange::Append(snapshot) => Some(Arc::clone(snapshot)),
            SuccessionChange::RootBind | SuccessionChange::Virtual(_) => None,
        }
    }

    /// The product `Switch` workflow for this switch onto `rebind`.
    pub(in crate::runtime) fn workflow_kind(
        &self,
        rebind: sessions::RebindSelection,
    ) -> CliResult<sessions::SessionWorkflowKind> {
        let pending_snapshot = match &self.change {
            SuccessionChange::Virtual(snapshot) => Some(
                serde_json::to_string(snapshot.as_ref())
                    .map_err(|_| CliFailure::new("the profile's epoch could not be encoded"))?,
            ),
            SuccessionChange::Append(_) | SuccessionChange::RootBind => None,
        };
        Ok(sessions::SessionWorkflowKind::Switch(Box::new(
            sessions::SwitchSelection {
                rebind,
                profile: self.profile.clone(),
                root: self.root.clone(),
                roster_digest: self.snapshot.digest.clone(),
                pending_snapshot,
            },
        )))
    }

    /// Step 2: records the successor epoch's bytes in the tree, after the
    /// product intent and before the publication.
    pub(in crate::runtime) async fn record<P: plakous_host_protocol::ProtocolExtension>(
        &self,
        config: &HostConfig<P>,
        operation: &str,
    ) -> CliResult<()> {
        let SuccessionChange::Append(snapshot) = &self.change else {
            return Ok(());
        };
        topology_state::record_switch_epoch(
            &config.topology_rows(),
            &self.tree_id,
            operation,
            snapshot.as_ref().clone(),
        )
        .await
        .map_err(|error| CliFailure::new(format!("cannot switch profile: {error}")))
    }

    /// Withdraws step 2 for a switch whose publication definitely did not
    /// land.
    pub(in crate::runtime) async fn withdraw<P: plakous_host_protocol::ProtocolExtension>(
        &self,
        config: &HostConfig<P>,
        operation: &str,
    ) -> CliResult<()> {
        if !matches!(self.change, SuccessionChange::Append(_)) {
            return Ok(());
        }
        topology_state::withdraw_switch_epoch(&config.topology_rows(), &self.tree_id, operation)
            .await
            .map_err(CliFailure::new)
    }

    /// Step 5: once the successor is published and projected, the tree takes
    /// its epoch from the recorded bytes, or its root-binding view moves.
    pub(in crate::runtime) async fn settle<P: plakous_host_protocol::ProtocolExtension>(
        &self,
        config: &HostConfig<P>,
        operation: &str,
        account: &str,
        model: &str,
        effort: Option<&str>,
    ) -> CliResult<()> {
        let rows = config.topology_rows();
        match &self.change {
            SuccessionChange::Append(_) => {
                topology_state::settle_switch_epoch(&rows, &self.tree_id, operation)
                    .await
                    .map_err(CliFailure::new)?;
            }
            SuccessionChange::RootBind => {
                topology_state::record_root_bind_view(&rows, &self.tree_id, account, model, effort)
                    .await
                    .map_err(CliFailure::new)?;
            }
            SuccessionChange::Virtual(_) => {}
        }
        Ok(())
    }
}

/// Before a root opens: a switch successor whose tree still records its
/// epoch as pending (a crash after the publication) installs it from those
/// bytes, so the successor compiles against exactly what it was published
/// with.
pub(in crate::runtime) async fn settle_before_open<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<()> {
    let rows = config.topology_rows();
    let tree_id = durable_tree_id(&rows, &config.session);
    topology_state::settle_switch_epoch_on_open(&rows, &tree_id, &config.session)
        .await
        .map_err(|error| {
            CliFailure::recovery_required(format!(
                "the profile switch's epoch could not be installed ({error}); RecoveryRequired"
            ))
        })
}

/// After a root opened at its line's head: any epoch still pending belongs
/// to a switch that never published onto this head, so it is withdrawn.
pub(in crate::runtime) async fn withdraw_after_open<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> Result<Vec<String>, String> {
    let rows = config.topology_rows();
    let tree_id = durable_tree_id(&rows, &config.session);
    topology_state::withdraw_stale_switch_epochs(&rows, &tree_id).await
}
