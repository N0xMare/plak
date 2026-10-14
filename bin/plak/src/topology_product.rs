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
    // A carried-forward source lives in another build's store.
    let mut next_source = if session.carried_forward_from().is_some() {
        None
    } else {
        session.source_product_session_id.clone()
    };
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
        next_source = if source.carried_forward_from().is_some() {
            None
        } else {
            source.source_product_session_id.clone()
        };
        candidate = source.id;
    }
    Err(plakous_hosted::CliFailure::new(
        "topology session lineage exceeds its inspection bound",
    ))
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

/// `--resume-tree` is meaningful only when this session has a durable tree.
/// A root-only session resumes normally; a store error is not mistaken for an
/// absent tree.
pub(crate) fn requested_tree_resume(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
    requested: bool,
) -> plakous_hosted::CliResult<bool> {
    if !requested {
        return Ok(false);
    }
    crate::store::tree::tree_exists(paths, tree_id).map_err(plakous_hosted::CliFailure::new)
}

/// True when a live tree epoch still has non-terminal descendants or pending
/// spawns, or the tree is RestoredPaused (ResumeTree required before rebind).
/// CLI `run --session --resume-tree -- <prompt>` is explicit ResumeTree.
/// TUI attach sends `ResumeTopologyTree` (tree unpause). Crash-restored
/// queue drain stays `RestoredPaused` until the operator resumes it.
///
/// # Errors
///
/// Returns a redaction-safe failure when the tree cannot be resumed.
pub fn resume_topology_tree_for_cli_prompt(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
) -> plakous_hosted::CliResult<()> {
    resume_tree_for_cli_prompt(paths, tree_id).map_err(plakous_hosted::CliFailure::new)
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

    fn save_tree(&self, tree: &mut ProductAgentTree) -> Result<(), String> {
        crate::store::tree::save_tree(self, tree)
    }
}

#[cfg(test)]
pub(crate) fn epoch_blocks_rebind(paths: &crate::home::paths::PlakPaths, tree_id: &str) -> bool {
    plakous_hosted::runtime::epoch_rebind_refusal(paths, tree_id).is_some()
}

/// Next CLI prompt on a restored session is explicit ResumeTree via `--resume-tree`.
///
/// The hub's runtime owns the store now; `_paths` stays in the signature
/// for the product callers until they are handed the hub.
pub(crate) fn resume_tree_for_cli_prompt(
    _paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
) -> Result<(), String> {
    plakous_hosted::runtime::resume_tree(tree_id)
}

pub(crate) fn install_successor_epoch(
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
}

/// Why a roster or orchestration apply was refused. Every arm
/// reuses an existing distinct refusal string; `RootBusy` and
/// `StoreUnavailable` are the added ones.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RosterApplyRefusal {
    RestoredPaused,
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
            Self::RestoredPaused => ProductAgentTree::TREE_EPOCH_REBIND_PAUSED,
            Self::PendingSpawns => ProductAgentTree::TREE_EPOCH_REBIND_PENDING_SPAWNS,
            Self::LiveDescendants => ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS,
            Self::RootBusy => Self::ROOT_BUSY,
            Self::StoreUnavailable(message) => message.as_str(),
        }
    }

    const ROOT_BUSY: &'static str = "cannot apply a roster while the root turn is running";

    const fn from_epoch(refusal: EpochRebindRefusal) -> Self {
        match refusal {
            EpochRebindRefusal::RestoredPaused => Self::RestoredPaused,
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
pub fn apply_roster(
    paths: &crate::home::paths::PlakPaths,
    bind: Option<&RosterApplyRootBind>,
    config: &crate::product_config::ProductConfig,
    scope: crate::product_config::ConfigWriteScope,
) -> Result<RosterApplyOutcome, RosterApplyError> {
    if let Some(refusal) = roster_apply_refusal(paths, bind) {
        return Err(RosterApplyError::Refused(refusal));
    }
    let durability = match config.save_scoped(paths, scope) {
        Ok(()) => None,
        Err(crate::product_config::ConfigSaveError::BeforePublication(error)) => {
            return Err(RosterApplyError::BeforePublication(error));
        }
        Err(crate::product_config::ConfigSaveError::PublishedDurabilityUncertain(error)) => {
            Some(error)
        }
    };
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
    .map_err(|error| {
        RosterApplyError::PublishedWithoutEpoch(format!(
            "config.toml was saved but the epoch was not replaced: {error}; the next session picks it up at genesis"
        ))
    })?;
    Ok(durability.map_or(RosterApplyOutcome::Applied, |error| {
        RosterApplyOutcome::AppliedDurabilityUncertain(error)
    }))
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
/// descendant. `RestoredPaused` is deliberately not consulted here: every
/// persisted tree reopens in that state, so judging it would refuse
/// `plak config set` for ever after the first session, with no CLI verb able to
/// lift the fence. A vacuous singleton tree passes.
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
    apply_roster(paths, Some(bind), config, scope)
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
        return crate::store::tree::save_tree(paths, &mut tree);
    };
    let closed_ids = runtime.mutate_then_emit(false, |tree| {
        let root = tree.root_agent_id.clone();
        let closed = tree
            .stop_all(&root, Some(&operation_id))
            .map_err(|error| error.message)?;
        Ok(closed
            .into_iter()
            .map(|(id, _)| id)
            .collect::<Vec<String>>())
    })?;
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

/// Hub first, then the product store. Used by `plak agent --session` so
/// RestoredPaused cannot attach without explicit `--resume-tree`.
#[must_use]
pub(crate) fn tree_requires_explicit_resume(
    paths: &crate::home::paths::PlakPaths,
    tree_id: &str,
) -> bool {
    plakous_hosted::runtime::requires_explicit_resume(paths, tree_id)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::product_config::ProductConfig;

    use plakous_hosted::topology_runtime::test_support::{bootstrap, unique_tree_id};
    use plakous_hosted::topology_runtime::{force_release, insert_root, insert_root_with};

    /// Every test here writes the global scope; project-scope apply is
    /// covered by the product-config save tests.
    fn apply_roster(
        paths: &crate::home::paths::PlakPaths,
        bind: Option<&RosterApplyRootBind>,
        config: &ProductConfig,
    ) -> Result<RosterApplyOutcome, RosterApplyError> {
        super::apply_roster(
            paths,
            bind,
            config,
            crate::product_config::ConfigWriteScope::User,
        )
    }

    async fn stop_all_then_apply(
        paths: &crate::home::paths::PlakPaths,
        bind: &RosterApplyRootBind,
        config: &ProductConfig,
    ) -> Result<RosterApplyOutcome, RosterApplyError> {
        super::stop_all_then_apply(
            paths,
            bind,
            config,
            crate::product_config::ConfigWriteScope::User,
        )
        .await
    }

    /// The reopened file set: every template ever saved into the home stays a
    /// file, so a reload carries the union of what the tests saved.
    fn assert_saved_config_matches(
        paths: &crate::home::paths::PlakPaths,
        expected: &ProductConfig,
    ) {
        let loaded = ProductConfig::load(paths).unwrap();
        assert_eq!(loaded.working_set, expected.working_set);
        assert_eq!(loaded.root_agent, expected.root_agent);
        for (name, template) in &expected.templates {
            assert_eq!(loaded.templates.get(name), Some(template), "{name}");
        }
    }

    /// A one-template working set built in memory; `save` writes the template
    /// file beside `config.toml`.
    fn roster_config(_class: &str, template: &str, account: &str, model: &str) -> ProductConfig {
        let mut config = ProductConfig::default_product();
        config.templates.insert(
            template.to_owned(),
            crate::product_config::AgentTemplate {
                account: account.to_owned(),
                model: model.to_owned(),
                description: format!("{template} worker"),
                ..crate::product_config::AgentTemplate::default()
            },
        );
        config.working_set = plakous_hosted::agent_tree_protocol::WorkingSet::parse([template])
            .expect("working set");
        config
    }

    /// The headless `plak config set` successor epoch and a live hub in
    /// another process share one store row; whichever publishes second is
    /// refused by the compare-and-swap, in either order, and the store
    /// keeps the winner's rows.
    #[tokio::test]
    async fn headless_successor_epoch_and_a_live_hub_cannot_overwrite_each_other() {
        use plakous_hosted::agent_tree_protocol::TopologyErrorCode;
        use plakous_hosted::orchestration::OrchestrationSnapshot;
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let store = crate::store::tree::tree_store(&paths).unwrap();
        let attach = |tree_id: &str| {
            let mut genesis = bootstrap(tree_id, paths.home().to_path_buf());
            genesis.acknowledge_revision(store.save(&genesis).unwrap());
            let hub_copy = ProductAgentTree::reopen(store.load(tree_id).unwrap().unwrap());
            insert_root_with(tree_id.to_owned(), hub_copy, Arc::clone(&store), None).unwrap()
        };

        // The headless process publishes first: the hub's next save is
        // refused and the successor epoch stays.
        let tree_id = unique_tree_id();
        let runtime = attach(&tree_id);
        plakous_hosted::runtime::install_successor_epoch_headless(
            &paths,
            OrchestrationSnapshot::unpinned("grok-4.6"),
            &tree_id,
            &|_| {},
        )
        .unwrap();
        let refused = {
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.record_fault("root", "moved after the headless publish");
            runtime.driver().save(&mut tree).unwrap_err()
        };
        assert_eq!(refused.code, TopologyErrorCode::OperationRecoveryRequired);
        assert!(
            refused.message.contains("refusing to overwrite"),
            "{}",
            refused.message
        );
        force_release(&tree_id).await;
        let rows = store.load(&tree_id).unwrap().unwrap();
        assert_eq!(rows.rows().selected_tuple, "grok-4.6");

        // The hub publishes first: the headless handle that loaded before it
        // moved is refused, and the hub's rows stay.
        let tree_id = unique_tree_id();
        let runtime = attach(&tree_id);
        let mut stale = crate::store::tree::load_tree(&paths, &tree_id)
            .unwrap()
            .unwrap();
        {
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.record_fault("root", "moved before the headless publish");
            runtime.driver().save(&mut tree).unwrap();
        }
        stale
            .begin_successor_epoch(OrchestrationSnapshot::unpinned("grok-4.6"))
            .unwrap();
        let refused = crate::store::tree::save_tree(&paths, &mut stale).unwrap_err();
        assert!(refused.contains("refusing to overwrite"), "{refused}");
        assert_ne!(
            store.load(&tree_id).unwrap().unwrap().rows().selected_tuple,
            "grok-4.6"
        );
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn cli_prompt_resumes_restored_paused_tree() {
        let tree_id = unique_tree_id();
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        let mut tree = bootstrap(&tree_id, root.path().to_path_buf());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        tree.restore_paused();
        let runtime = insert_root(tree_id.clone(), tree, None).unwrap();
        assert_eq!(
            runtime.tree_handle_for_test().lock().unwrap().state,
            plakous_hosted::agent_tree::TreeAdmissionState::RestoredPaused
        );
        assert!(plakous_hosted::topology_runtime::is_restored_paused(
            &tree_id
        ));
        resume_tree_for_cli_prompt(&paths, &tree_id).unwrap();
        assert_eq!(
            runtime.tree_handle_for_test().lock().unwrap().state,
            plakous_hosted::agent_tree::TreeAdmissionState::Live
        );
        assert!(!plakous_hosted::topology_runtime::is_restored_paused(
            &tree_id
        ));
        force_release(&tree_id).await;
    }

    #[test]
    fn resume_tree_hub_miss_is_fail_closed() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::home::paths::PlakPaths::explicit(root.path().join("p"));
        paths.ensure_layout().unwrap();
        let error = resume_tree_for_cli_prompt(&paths, "missing-tree").unwrap_err();
        assert!(error.contains("missing from the hub"));
    }

    #[test]
    fn requested_tree_resume_only_resumes_a_persisted_tree() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let tree_id = unique_tree_id();
        assert!(!requested_tree_resume(&paths, &tree_id, true).unwrap());

        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(requested_tree_resume(&paths, &tree_id, true).unwrap());
        assert!(!requested_tree_resume(&paths, &tree_id, false).unwrap());
    }

    #[test]
    fn durable_tree_id_walks_rebind_source_to_persisted_tree() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let genesis = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        let mut tree = bootstrap(&genesis.id, paths.home().to_path_buf());
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let mut rebound = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        rebound.source_product_session_id = Some(genesis.id.clone());
        assert_eq!(
            durable_topology_tree_id(&crate::shell::host::product_state_roots(&paths), &rebound),
            genesis.id
        );
        assert_eq!(
            durable_topology_tree_id(&crate::shell::host::product_state_roots(&paths), &genesis),
            genesis.id
        );
    }

    #[test]
    fn read_only_tree_inspection_walks_rebind_lineage_and_keeps_child_rows() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let mut product = crate::store::ProductStore::open(&paths).unwrap();
        let mut genesis = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        product.commit(&mut genesis).unwrap();
        let mut tree = bootstrap(&genesis.id, paths.home().to_path_buf());
        let child = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let mut rebound = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        rebound.source_product_session_id = Some(genesis.id.clone());
        rebound.source_workflow_operation_id = Some("inspect:rebind".to_owned());
        product.commit(&mut rebound).unwrap();

        let (tree_id, observed) = inspect_session_tree(&paths, &rebound).unwrap();
        assert_eq!(tree_id, genesis.id);
        let TreeInspection::Tree(snapshot) = observed else {
            panic!("persisted predecessor tree was not inspected");
        };
        assert!(snapshot.rows().nodes().contains_key(&child.agent_id));
        assert_eq!(snapshot.rows().state, tree.state);
        assert_eq!(snapshot.rows().revision, tree.revision);
    }

    #[test]
    fn read_only_tree_inspection_refuses_cyclic_lineage() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let _tree_store = SqliteTreeStore::open(&paths.subagents_database()).unwrap();
        let mut product = crate::store::ProductStore::open(&paths).unwrap();
        let mut first = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        let mut second = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        first.source_product_session_id = Some(second.id.clone());
        first.source_workflow_operation_id = Some("inspect:cycle:first".to_owned());
        second.source_product_session_id = Some(first.id.clone());
        second.source_workflow_operation_id = Some("inspect:cycle:second".to_owned());
        product.commit(&mut first).unwrap();
        product.commit(&mut second).unwrap();
        let error = inspect_session_tree(&paths, &first).unwrap_err();
        assert!(error.message().contains("cycle"));
    }

    // Successor-epoch apply: no pending state, terminal tree plus idle root only.

    fn bind(tree_id: &str, root_idle: bool) -> RosterApplyRootBind {
        RosterApplyRootBind {
            tree_id: tree_id.to_owned(),
            account: "grok".to_owned(),
            tuple_id: "selected-tuple".to_owned(),
            effort: Some("low".to_owned()),
            root_idle,
        }
    }

    #[tokio::test]
    async fn save_is_rejected_on_a_live_epoch_and_leaves_the_file_byte_identical() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let before = roster_config("review", "critic", "claude", "claude-opus-5");
        before.save(&paths).unwrap();
        let original = std::fs::read(paths.config_file()).unwrap();

        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        let runtime = insert_root(tree_id.clone(), tree, None).unwrap();

        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");

        // Live descendants.
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &next),
            Err(RosterApplyError::Refused(
                RosterApplyRefusal::LiveDescendants
            ))
        );
        // Root busy is its own refusal.
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, false)), &next),
            Err(RosterApplyError::Refused(RosterApplyRefusal::RootBusy))
        );
        // Pending spawns and restored-paused are distinct strings.
        assert_ne!(
            RosterApplyRefusal::PendingSpawns.message(),
            RosterApplyRefusal::LiveDescendants.message()
        );
        assert_ne!(
            RosterApplyRefusal::RestoredPaused.message(),
            RosterApplyRefusal::RootBusy.message()
        );
        {
            let mut tree = runtime.tree_handle_for_test().lock().unwrap();
            tree.restore_paused();
        }
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &next),
            Err(RosterApplyError::Refused(
                RosterApplyRefusal::RestoredPaused
            ))
        );

        assert_eq!(std::fs::read(paths.config_file()).unwrap(), original);
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn a_terminal_tree_with_an_idle_root_applies_a_successor_epoch_at_once() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        roster_config("review", "critic", "claude", "claude-opus-5")
            .save(&paths)
            .unwrap();
        let tree_id = unique_tree_id();
        let runtime = insert_root(
            tree_id.clone(),
            bootstrap(&tree_id, paths.home().to_path_buf()),
            None,
        )
        .unwrap();
        let before = runtime
            .tree_handle_for_test()
            .lock()
            .unwrap()
            .snapshot
            .clone();

        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &next),
            Ok(RosterApplyOutcome::Applied)
        );

        {
            let tree = runtime.tree_handle_for_test().lock().unwrap();
            assert_eq!(
                tree.state,
                plakous_hosted::agent_tree::TreeAdmissionState::Live
            );
            assert_ne!(tree.snapshot.digest, before.digest);
            assert!(tree.snapshot.digest_matches());
            let binding = tree
                .snapshot
                .binding(
                    &plakous_hosted::agent_tree_protocol::TemplateName::parse("scout").unwrap(),
                )
                .unwrap();
            assert_eq!(binding.template, "scout");
            assert_eq!(binding.account, "codex");
        }
        assert_saved_config_matches(&paths, &next);
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn a_failed_write_installs_no_epoch() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        let before = tree.snapshot.digest.clone();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let runtime = insert_root_with(
            tree_id.clone(),
            tree,
            crate::store::tree::tree_store(&paths).unwrap(),
            None,
        )
        .unwrap();

        // An invalid roster never publishes and never installs an epoch.
        let mut invalid = ProductConfig::default_product();
        invalid.working_set = plakous_hosted::agent_tree_protocol::WorkingSet::parse(["absent"])
            .expect("working set");
        assert!(matches!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &invalid),
            Err(RosterApplyError::BeforePublication(_))
        ));
        assert!(!paths.config_file().is_file());
        assert_eq!(
            runtime
                .tree_handle_for_test()
                .lock()
                .unwrap()
                .snapshot
                .digest,
            before
        );
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn stop_all_then_apply_closes_descendants_then_applies_the_same_save() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        roster_config("review", "critic", "claude", "claude-opus-5")
            .save(&paths)
            .unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        let child = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        let runtime = insert_root(tree_id.clone(), tree, None).unwrap();

        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        // The plain save is fenced while the child is live.
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &next),
            Err(RosterApplyError::Refused(
                RosterApplyRefusal::LiveDescendants
            ))
        );
        assert_eq!(
            stop_all_then_apply(&paths, &bind(&tree_id, true), &next).await,
            Ok(RosterApplyOutcome::Applied)
        );
        {
            let tree = runtime.tree_handle_for_test().lock().unwrap();
            assert!(tree.node(&child.agent_id).unwrap().status.is_terminal());
            assert_eq!(
                tree.snapshot
                    .binding(
                        &plakous_hosted::agent_tree_protocol::TemplateName::parse("scout").unwrap()
                    )
                    .unwrap()
                    .template,
                "scout"
            );
        }
        force_release(&tree_id).await;
    }

    /// On a restored-paused tree the first `s`
    /// refuses and offers `a stop all agents, then apply`. Taking that offer
    /// closed every descendant and then refused the very save it exists to
    /// unblock, with byte-identical text — so from the operator's seat the
    /// Agents tab went silent for ever. Stop-all is the operator's explicit
    /// close of every agent; once it has run there is nothing left to resume,
    /// and every later save answers too.
    #[tokio::test]
    async fn stop_all_then_apply_lifts_a_restored_pause_and_later_saves_still_answer() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        roster_config("review", "critic", "claude", "claude-opus-5")
            .save(&paths)
            .unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        let child = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        // Reopened after a quit or an interrupt: the tree is paused and its
        // running child is demoted to a non-terminal `paused`.
        tree.restore_paused();
        let runtime = insert_root(tree_id.clone(), tree, None).unwrap();

        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        // The plain save is refused, naming the pause and not the descendants.
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &next),
            Err(RosterApplyError::Refused(
                RosterApplyRefusal::RestoredPaused
            ))
        );

        // The offered compound action applies the same save.
        assert_eq!(
            stop_all_then_apply(&paths, &bind(&tree_id, true), &next).await,
            Ok(RosterApplyOutcome::Applied)
        );
        {
            let tree = runtime.tree_handle_for_test().lock().unwrap();
            assert!(tree.node(&child.agent_id).unwrap().status.is_terminal());
            // The successor epoch returns the tree to Live with the new
            // frozen roster; the pause bit is gone either way.
            assert_eq!(
                tree.state,
                plakous_hosted::agent_tree::TreeAdmissionState::Live
            );
            assert_eq!(tree.epoch_rebind_refusal_kind(), None);
            assert_eq!(
                tree.snapshot
                    .binding(
                        &plakous_hosted::agent_tree_protocol::TemplateName::parse("scout").unwrap()
                    )
                    .unwrap()
                    .template,
                "scout"
            );
        }

        // The operator edits the draft again and presses `s`. A plain save now
        // answers `Applied` on its own: nothing is left holding the fence up.
        let after = roster_config("review", "critic", "claude", "claude-opus-5");
        assert_eq!(
            apply_roster(&paths, Some(&bind(&tree_id, true)), &after),
            Ok(RosterApplyOutcome::Applied)
        );
        assert_eq!(
            runtime
                .tree_handle_for_test()
                .lock()
                .unwrap()
                .snapshot
                .binding(
                    &plakous_hosted::agent_tree_protocol::TemplateName::parse("critic").unwrap()
                )
                .unwrap()
                .template,
            "critic"
        );
        force_release(&tree_id).await;
    }

    #[tokio::test]
    async fn a_partial_close_leaves_the_fence_up_and_the_save_rejected() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let before = roster_config("review", "critic", "claude", "claude-opus-5");
        before.save(&paths).unwrap();
        let original = std::fs::read(paths.config_file()).unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        // A pending spawn is not closed by stop-all: the fence stays up.
        tree.set_pending_spawns_for_test(1);
        let runtime = insert_root(tree_id.clone(), tree, None).unwrap();

        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        assert_eq!(
            stop_all_then_apply(&paths, &bind(&tree_id, true), &next).await,
            Err(RosterApplyError::Refused(RosterApplyRefusal::PendingSpawns))
        );
        assert_eq!(std::fs::read(paths.config_file()).unwrap(), original);
        drop(runtime);
        force_release(&tree_id).await;
    }

    #[test]
    fn a_headless_config_set_fences_every_persisted_tree() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        // No trees at all: the save lands with no epoch to install it into.
        assert_eq!(
            apply_roster(&paths, None, &next),
            Ok(RosterApplyOutcome::Saved)
        );
        assert_saved_config_matches(&paths, &next);

        // A vacuous singleton every TUI session leaves behind at genesis. It
        // reopens `RestoredPaused`, but it is quiescent: judging the reopen
        // state would fence `plak config set` for ever after the first session
        // with no CLI verb able to lift it.
        let quiescent_id = unique_tree_id();
        let mut quiescent = bootstrap(&quiescent_id, paths.home().to_path_buf());
        crate::store::tree::save_tree(&paths, &mut quiescent).unwrap();
        let other = roster_config("review", "critic", "claude", "claude-opus-5");
        assert_eq!(
            apply_roster(&paths, None, &other),
            Ok(RosterApplyOutcome::Saved)
        );
        assert_saved_config_matches(&paths, &other);

        // A persisted tree that still has a non-terminal descendant is not
        // quiescent, and that fence stands.
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        let third = roster_config("review", "scribe", "codex", "gpt-5.6-luna");
        assert_eq!(
            apply_roster(&paths, None, &third),
            Err(RosterApplyError::Refused(
                RosterApplyRefusal::LiveDescendants
            ))
        );
        assert_saved_config_matches(&paths, &other);
    }

    /// A store that cannot be read is never reported as live descendants.
    #[test]
    fn a_store_that_cannot_be_read_names_itself_and_not_live_descendants() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        // A directory where the tree's store file belongs: `open` fails, and
        // the operator must not be told the tree still has live descendants.
        std::fs::create_dir_all(paths.subagents_database()).unwrap();
        let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
        let Err(RosterApplyError::Refused(RosterApplyRefusal::StoreUnavailable(message))) =
            apply_roster(&paths, None, &next)
        else {
            panic!("an unreadable store must refuse as StoreUnavailable");
        };
        assert!(message.contains("could not be read"), "{message}");
        assert!(!message.contains("live descendants"), "{message}");
    }

    #[test]
    fn epoch_blocks_rebind_reads_store_when_hub_misses() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        let spawned = tree
            .spawn_agent(
                "root",
                &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .unwrap();
        assert!(!tree.epoch_allows_rebind());
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(lookup(&tree_id).is_none());
        assert!(epoch_blocks_rebind(&paths, &tree_id));
        // Store load always restore_paused(); hub miss cannot see Live descendants.
        assert_eq!(
            plakous_hosted::runtime::epoch_rebind_refusal(&paths, &tree_id),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
        );
        let _ = spawned;
    }

    #[test]
    fn epoch_blocks_rebind_when_restored_paused() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        assert!(tree.epoch_allows_rebind());
        tree.spawn_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::SpawnAgentRequest::test_review(),
            None,
        )
        .unwrap();
        tree.restore_paused();
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(epoch_blocks_rebind(&paths, &tree_id));
        assert_eq!(
            plakous_hosted::runtime::epoch_rebind_refusal(&paths, &tree_id),
            Some(ProductAgentTree::TREE_EPOCH_REBIND_PAUSED)
        );
    }

    #[test]
    fn install_successor_epoch_persists_when_hub_misses() {
        let (_tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let tree_id = unique_tree_id();
        let mut tree = bootstrap(&tree_id, paths.home().to_path_buf());
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert!(lookup(&tree_id).is_none());
        install_successor_epoch(&paths, &tree_id, "grok", "grok-4.6", Some("low"), None).unwrap();
        let loaded = crate::store::tree::load_tree(&paths, &tree_id)
            .unwrap()
            .unwrap();
        assert_eq!(loaded.selected_tuple, "grok-4.6");
        assert_eq!(loaded.snapshot.root_tuple, "grok-4.6");
        assert_eq!(loaded.snapshot.root_account, "grok");
        assert!(loaded.snapshot.digest_matches());
    }
}
