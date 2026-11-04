//! The product's half of the topology hub.
//!
//!
//! Durable tree identity across rebinds, the roster/orchestration apply flows,
//! successor-epoch install, and the CLI-facing pause/resume/interrupt marks:
//! everything the hub does that needs a `PlakPaths`, a `ProductConfig`, a
//! `ProductStore`, or a product session record. `plakous-recursive-subagents`
//! keeps the hub itself — the working copy, the lease, the driver — and knows
//! none of this.

use std::sync::Arc;

use crate::telemetry_sink::TelemetryWriter;
use plakous_hosted::agent_tree::{EpochRebindRefusal, ProductAgentTree};
use plakous_hosted::topology_runtime::lookup;
use plakous_recursive_subagents_store::{SqliteTreeStore, TreeInspection};

/// Inspects the persisted topology tree of this product session and its
/// bounded rebind lineage. Unlike runtime resolution, every store/read error
/// is reported, never treated as absence. This does not attach or reopen a tree.
pub(crate) fn inspect_session_tree(
    paths: &crate::home::paths::PlakPaths,
    session: &crate::store::sessions::SessionRecord,
) -> plakous_hosted::CliResult<(String, TreeInspection)> {
    let mut candidate = session.id.clone();
    let mut next_source = inherited_tree_source(session);
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..64 {
        if !seen.insert(candidate.clone()) {
            return Err(plakous_hosted::CliFailure::new(
                "topology session lineage contains a cycle",
            ));
        }
        let observed = SqliteTreeStore::inspect(&paths.subagents_database(), &candidate)
            .map_err(|error| plakous_hosted::CliFailure::new(error.message()))?;
        match observed {
            TreeInspection::Tree(_) | TreeInspection::StoreAbsent => {
                return Ok((candidate, observed));
            }
            TreeInspection::TreeAbsent => {}
        }
        let Some(source_id) = next_source.take() else {
            return Ok((candidate, TreeInspection::TreeAbsent));
        };
        let source = crate::store::ProductStore::inspect_session_read_only(paths, &source_id)
            .map_err(|error| {
                plakous_hosted::CliFailure::new(format!(
                    "could not inspect topology lineage: {error}"
                ))
            })?;
        next_source = inherited_tree_source(&source);
        candidate = source.id;
    }
    Err(plakous_hosted::CliFailure::new(
        "topology session lineage exceeds its inspection bound",
    ))
}

/// The source whose tree `session` may still be on, as the host resolves
/// it: none for a record that opened a tree of its own (its node is the root
/// of the tree named after it) or carried forward from another build's
/// store.
fn inherited_tree_source(session: &crate::store::sessions::SessionRecord) -> Option<String> {
    if session.node.tree_id == session.id || session.carried_forward_from().is_some() {
        return None;
    }
    session.source_product_session_id.clone()
}

/// Stable tree identity across rebind branches. Rebind walks
/// `source_product_session_id`
/// until a hub or store tree exists, then that genesis id.
pub fn durable_topology_tree_id(
    roots: &plakous_hosted::agent::ProductStateRoots,
    session: &crate::store::sessions::SessionRecord,
) -> String {
    plakous_hosted::runtime::durable_topology_tree_id(
        &plakous_hosted::runtime::HostRows::new(roots, roots.session_records().as_ref()),
        session,
    )
}

/// Tree-only queries for headless flows. Session ancestry uses the record
/// service supplied to the state roots, never a database reopened from paths.
impl plakous_hosted::runtime::TreeRows for crate::home::paths::PlakPaths {
    fn tree(&self, tree_id: &str) -> Result<Option<ProductAgentTree>, String> {
        crate::store::tree::load_tree(self, tree_id)
    }

    fn tree_exists(&self, tree_id: &str) -> Result<bool, String> {
        crate::store::tree::tree_exists(self, tree_id)
    }

    fn save_tree(
        &self,
        tree: &mut ProductAgentTree,
    ) -> Result<(), plakous_hosted::runtime::TreeWriteError> {
        crate::store::tree::save_tree(self, tree)
    }
}

#[cfg(test)]
pub(crate) fn epoch_blocks_rebind(paths: &crate::home::paths::PlakPaths, tree_id: &str) -> bool {
    plakous_hosted::runtime::epoch_rebind_refusal(paths, tree_id).is_some()
}

pub(crate) async fn install_successor_epoch(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
    account: &str,
    tuple_id: &str,
    effort: Option<&str>,
    product_telemetry: Option<&Arc<TelemetryWriter>>,
) -> Result<(), String> {
    let config = crate::product_config::ProductConfig::load(paths)?;
    let snapshot = crate::roster_input::snapshot_from_config(
        &config,
        account,
        tuple_id,
        effort.map(str::to_owned),
    );
    plakous_hosted::runtime::install_successor_epoch(paths, snapshot, tree_id, &|observation| {
        crate::telemetry_sink::try_emit_product(product_telemetry, observation);
    })
    .await
}

/// Why a roster or orchestration apply was refused. Every arm
/// reuses an existing distinct refusal string; `RootBusy` and
/// `StoreUnavailable` are the added ones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RosterApplyRefusal {
    PendingSpawns,
    LiveDescendants,
    RootBusy,
    /// The topology store could not be opened or read, so quiescence is
    /// unknown. Reporting that as live descendants would name a condition the
    /// host never observed.
    StoreUnavailable(String),
}

impl RosterApplyRefusal {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::PendingSpawns => ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS,
            Self::LiveDescendants => ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS,
            Self::RootBusy => Self::ROOT_BUSY,
            Self::StoreUnavailable(message) => message.as_str(),
        }
    }

    const ROOT_BUSY: &'static str = "cannot apply a roster while the root turn is running";

    const fn from_epoch(refusal: EpochRebindRefusal) -> Self {
        match refusal {
            EpochRebindRefusal::PendingSpawns => Self::PendingSpawns,
            EpochRebindRefusal::LiveDescendants => Self::LiveDescendants,
        }
    }
}

/// A refused apply changed nothing; a failed one may have published the file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RosterApplyError {
    Refused(RosterApplyRefusal),
    /// Nothing was written and no epoch was installed.
    BeforePublication(String),
    /// `config.toml` is saved; the epoch was not replaced. The next session
    /// picks the file up at genesis.
    PublishedWithoutEpoch(String),
}

impl RosterApplyError {
    #[must_use]
    pub fn into_message(self) -> String {
        match self {
            Self::Refused(refusal) => refusal.message().to_owned(),
            Self::BeforePublication(message) | Self::PublishedWithoutEpoch(message) => message,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RosterApplyOutcome {
    /// File written and a successor epoch installed on the named tree.
    Applied,
    /// File written; no epoch was attached to install it into. The next
    /// session reads it at genesis.
    Saved,
    /// File published but the parent-directory sync failed.
    AppliedDurabilityUncertain(String),
}

/// Root bind carried into the successor epoch.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RosterApplyRootBind {
    pub tree_id: String,
    pub account: String,
    pub tuple_id: String,
    pub effort: Option<String>,
    /// False while the root turn is running.
    pub root_idle: bool,
}

/// accepted only from a terminal tree with an idle root; then
/// writes `config.toml` atomically and installs a successor epoch at once.
/// There is no pending, deferred, staged, or queued roster state.
///
/// `scope` names which file the save lands in — the global home or the
/// trusted project override. The installed epoch always carries the
/// *effective* config either way.
pub async fn apply_roster(
    paths: &crate::home::paths::PlakPaths,
    bind: Option<&RosterApplyRootBind>,
    config: &crate::product_config::ProductConfig,
    scope: crate::product_config::ConfigWriteScope,
) -> Result<RosterApplyOutcome, RosterApplyError> {
    let durability = save_roster(paths, bind, config, scope)?;
    let Some(bind) = bind else {
        return Ok(RosterApplyOutcome::Saved);
    };
    // The writer here is the headless branch's alone: an attached tree
    // announces its own install through the runtime's observer, and a roster
    // apply from this process carries no writer of its own.
    install_successor_epoch(
        paths,
        &bind.tree_id,
        &bind.account,
        &bind.tuple_id,
        bind.effort.as_deref(),
        None,
    )
    .await
    .map_err(|error| {
        RosterApplyError::PublishedWithoutEpoch(format!(
            "config.toml was saved but the epoch was not replaced: {error}; the next session picks it up at genesis"
        ))
    })?;
    Ok(durability.map_or(RosterApplyOutcome::Applied, |error| {
        RosterApplyOutcome::AppliedDurabilityUncertain(error)
    }))
}

/// [`apply_roster`] for a headless `plak config set`, which names no tree:
/// every persisted tree must be quiescent, and only the file is saved.
pub fn apply_unbound_roster(
    paths: &crate::home::paths::PlakPaths,
    config: &crate::product_config::ProductConfig,
    scope: crate::product_config::ConfigWriteScope,
) -> Result<RosterApplyOutcome, RosterApplyError> {
    save_roster(paths, None, config, scope)?;
    Ok(RosterApplyOutcome::Saved)
}

/// The admitted save of an apply; `Some` names a published save whose
/// durability is uncertain.
fn save_roster(
    paths: &crate::home::paths::PlakPaths,
    bind: Option<&RosterApplyRootBind>,
    config: &crate::product_config::ProductConfig,
    scope: crate::product_config::ConfigWriteScope,
) -> Result<Option<String>, RosterApplyError> {
    if let Some(refusal) = roster_apply_refusal(paths, bind) {
        return Err(RosterApplyError::Refused(refusal));
    }
    match config.save_scoped(paths, scope) {
        Ok(()) => Ok(None),
        Err(crate::product_config::ConfigSaveError::BeforePublication(error)) => {
            Err(RosterApplyError::BeforePublication(error))
        }
        Err(crate::product_config::ConfigSaveError::PublishedDurabilityUncertain(error)) => {
            Ok(Some(error))
        }
    }
}

/// `None` when the apply is admitted. With no attached tree every persisted
/// tree is fenced, because a headless `plak config set` names none.
#[must_use]
pub fn roster_apply_refusal(
    paths: &crate::home::paths::PlakPaths,
    bind: Option<&RosterApplyRootBind>,
) -> Option<RosterApplyRefusal> {
    let Some(bind) = bind else {
        return every_tree_refusal(paths);
    };
    if !bind.root_idle {
        return Some(RosterApplyRefusal::RootBusy);
    }
    match epoch_rebind_refusal_kind(paths, &bind.tree_id) {
        Ok(refusal) => refusal.map(RosterApplyRefusal::from_epoch),
        Err(message) => Some(store_unavailable(&message)),
    }
}

/// Headless gate: a session-less apply names no tree, so every
/// persisted tree must be *quiescent* — no pending spawn and no non-terminal
/// descendant. A vacuous singleton tree passes.
fn every_tree_refusal(paths: &crate::home::paths::PlakPaths) -> Option<RosterApplyRefusal> {
    let tree_ids = match crate::store::tree::tree_ids(paths) {
        Ok(tree_ids) => tree_ids,
        Err(error) => return Some(store_unavailable(&error)),
    };
    for tree_id in tree_ids {
        match persisted_quiescence_refusal(paths, &tree_id) {
            Ok(Some(refusal)) => return Some(RosterApplyRefusal::from_epoch(refusal)),
            Ok(None) => {}
            Err(message) => return Some(store_unavailable(&message)),
        }
    }
    None
}

fn store_unavailable(detail: &str) -> RosterApplyRefusal {
    RosterApplyRefusal::StoreUnavailable(format!(
        "cannot judge roster quiescence: the topology store could not be read: {detail}"
    ))
}

/// Quiescence of one tree, hub-live or persisted. `Err` means the store could
/// not answer, which is never reported as live descendants.
fn persisted_quiescence_refusal(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
) -> Result<Option<EpochRebindRefusal>, String> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .read(ProductAgentTree::quiescence_refusal)
            .map_err(|_| "topology tree lock is poisoned".to_owned());
    }
    match crate::store::tree::load_tree(paths, tree_id) {
        Ok(Some(tree)) => Ok(tree.quiescence_refusal()),
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    }
}

/// `Err` when the store could not answer, so the caller can name that instead
/// of asserting live descendants it never observed.
fn epoch_rebind_refusal_kind(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
) -> Result<Option<EpochRebindRefusal>, String> {
    if let Some(runtime) = lookup(tree_id) {
        return runtime
            .read(ProductAgentTree::epoch_rebind_refusal_kind)
            .map_err(|_| "topology tree lock is poisoned".to_owned());
    }
    match crate::store::tree::load_tree(paths, tree_id) {
        Ok(Some(tree)) => Ok(tree.epoch_rebind_refusal_kind()),
        Ok(None) => Ok(None),
        Err(error) => Err(error),
    }
}

/// The confirmed compound action: host `stop-all` (postorder
/// close of every non-root non-terminal node) followed by the same save. A
/// partial close does not lift the fence: the apply then refuses.
pub async fn stop_all_then_apply(
    paths: &crate::home::paths::PlakPaths,
    bind: &RosterApplyRootBind,
    config: &crate::product_config::ProductConfig,
    scope: crate::product_config::ConfigWriteScope,
) -> Result<RosterApplyOutcome, RosterApplyError> {
    stop_all_now(paths, &bind.tree_id)
        .await
        .map_err(RosterApplyError::BeforePublication)?;
    apply_roster(paths, Some(bind), config, scope).await
}

async fn stop_all_now(paths: &crate::home::paths::PlakPaths, tree_id: &str) -> Result<(), String> {
    let operation_id = uuid::Uuid::new_v4().to_string();
    let Some(runtime) = lookup(tree_id) else {
        let Some(mut tree) = crate::store::tree::load_tree(paths, tree_id)? else {
            return Ok(());
        };
        let root = tree.root_agent_id.clone();
        tree.stop_all(&root, Some(&operation_id))
            .map_err(|error| error.message)?;
        return crate::store::tree::save_tree(paths, &mut tree).map_err(|error| error.message());
    };
    let closed_ids = runtime
        .mutate_then_emit(false, move |tree| {
            let root = tree.root_agent_id.clone();
            let closed = tree
                .stop_all(&root, Some(&operation_id))
                .map_err(|error| error.message)?;
            Ok(closed
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<String>>())
        })
        .await?;
    let cancel = runtime.cancel_live_many(&closed_ids).await;
    if !cancel.is_clean() {
        let message = cancel
            .first_failure
            .unwrap_or_else(|| "child turn could not be cancelled".to_owned());
        return Err(format!(
            "stop-all applied; live cancel failed for {}: {message}",
            cancel
                .failed
                .iter()
                .chain(&cancel.joiner_expired)
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
