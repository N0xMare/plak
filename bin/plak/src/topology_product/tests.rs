use super::*;
use crate::product_config::ProductConfig;
use plakous_hosted::runtime::test_kit::{BlockHere as _, BlockOn as _};

use plakous_hosted::topology_runtime::test_support::{bootstrap, unique_tree_id};
use plakous_hosted::topology_runtime::{force_release, insert_root, insert_root_with};

/// Every test here writes the global scope; project-scope apply is
/// covered by the product-config save tests.
async fn apply_roster(
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
    .await
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
fn assert_saved_config_matches(paths: &crate::home::paths::PlakPaths, expected: &ProductConfig) {
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
    config.working_set =
        plakous_hosted::agent_tree_protocol::WorkingSet::parse([template]).expect("working set");
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
        genesis.acknowledge_revision(store.save_in_custody(&genesis).unwrap());
        let hub_copy = ProductAgentTree::reopen(store.load(tree_id).unwrap().unwrap());
        insert_root_with(tree_id.to_owned(), hub_copy, Arc::clone(&store), None)
            .block()
            .unwrap()
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
    assert!(
        refused.message().contains("refusing to overwrite"),
        "{refused:?}"
    );
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
    let runtime = insert_root(tree_id.clone(), tree, None).await.unwrap();
    assert_eq!(
        runtime.tree_handle_for_test().lock().unwrap().state,
        plakous_hosted::agent_tree::TreeAdmissionState::RestoredPaused
    );
    assert!(plakous_hosted::topology_runtime::is_restored_paused(
        &tree_id
    ));
    resume_tree_for_cli_prompt(&paths, &tree_id).await.unwrap();
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
    let error = resume_tree_for_cli_prompt(&paths, "missing-tree")
        .block()
        .unwrap_err();
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
    // A rebind that stayed on its source's tree carries the source's node.
    rebound.node = genesis.node.clone();
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
    // A rebind that stayed on its source's tree carries the source's node.
    rebound.node = genesis.node.clone();
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
    // Both claim to stay on a tree that is not their own.
    let shared = plakous_hosted::sessions::SessionNode::genesis("shared-tree");
    first.node = shared.clone();
    second.node = shared;
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
    let runtime = insert_root(tree_id.clone(), tree, None).await.unwrap();

    let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");

    // Live descendants.
    assert_eq!(
        apply_roster(&paths, Some(&bind(&tree_id, true)), &next).await,
        Err(RosterApplyError::Refused(
            RosterApplyRefusal::LiveDescendants
        ))
    );
    // Root busy is its own refusal.
    assert_eq!(
        apply_roster(&paths, Some(&bind(&tree_id, false)), &next).await,
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
        apply_roster(&paths, Some(&bind(&tree_id, true)), &next).await,
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
    .await
    .unwrap();
    let before = runtime
        .tree_handle_for_test()
        .lock()
        .unwrap()
        .current_epoch()
        .clone();

    let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
    assert_eq!(
        apply_roster(&paths, Some(&bind(&tree_id, true)), &next).await,
        Ok(RosterApplyOutcome::Applied)
    );

    {
        let tree = runtime.tree_handle_for_test().lock().unwrap();
        assert_eq!(
            tree.state,
            plakous_hosted::agent_tree::TreeAdmissionState::Live
        );
        assert_ne!(tree.current_epoch().digest, before.digest);
        assert!(tree.current_epoch().digest_matches());
        let binding = tree
            .current_epoch()
            .binding(&plakous_hosted::agent_tree_protocol::TemplateName::parse("scout").unwrap())
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
    let before = tree.current_epoch().digest.clone();
    crate::store::tree::save_tree(&paths, &mut tree).unwrap();
    let runtime = insert_root_with(
        tree_id.clone(),
        tree,
        crate::store::tree::tree_store(&paths).unwrap(),
        None,
    )
    .await
    .unwrap();

    // An invalid roster never publishes and never installs an epoch.
    let mut invalid = ProductConfig::default_product();
    invalid.working_set =
        plakous_hosted::agent_tree_protocol::WorkingSet::parse(["absent"]).expect("working set");
    assert!(matches!(
        apply_roster(&paths, Some(&bind(&tree_id, true)), &invalid).await,
        Err(RosterApplyError::BeforePublication(_))
    ));
    assert!(!paths.config_file().is_file());
    assert_eq!(
        runtime
            .tree_handle_for_test()
            .lock()
            .unwrap()
            .current_epoch()
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
    let runtime = insert_root(tree_id.clone(), tree, None).await.unwrap();

    let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
    // The plain save is fenced while the child is live.
    assert_eq!(
        apply_roster(&paths, Some(&bind(&tree_id, true)), &next).await,
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
            tree.current_epoch()
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
    let runtime = insert_root(tree_id.clone(), tree, None).await.unwrap();

    let next = roster_config("review", "scout", "codex", "gpt-5.6-luna");
    // The plain save is refused, naming the pause and not the descendants.
    assert_eq!(
        apply_roster(&paths, Some(&bind(&tree_id, true)), &next).await,
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
            tree.current_epoch()
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
        apply_roster(&paths, Some(&bind(&tree_id, true)), &after).await,
        Ok(RosterApplyOutcome::Applied)
    );
    assert_eq!(
        runtime
            .tree_handle_for_test()
            .lock()
            .unwrap()
            .current_epoch()
            .binding(&plakous_hosted::agent_tree_protocol::TemplateName::parse("critic").unwrap())
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
    let runtime = insert_root(tree_id.clone(), tree, None).await.unwrap();

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
        apply_roster(&paths, None, &next).block_here(),
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
        apply_roster(&paths, None, &other).block_here(),
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
        apply_roster(&paths, None, &third).block_here(),
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
        apply_roster(&paths, None, &next).block_here()
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
        Some(EpochRebindRefusal::RestoredPaused)
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
        Some(EpochRebindRefusal::RestoredPaused)
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
    install_successor_epoch(&paths, &tree_id, "grok", "grok-4.6", Some("low"), None)
        .block_here()
        .unwrap();
    let loaded = crate::store::tree::load_tree(&paths, &tree_id)
        .unwrap()
        .unwrap();
    assert_eq!(loaded.selected_tuple, "grok-4.6");
    assert_eq!(loaded.current_epoch().root_tuple, "grok-4.6");
    assert_eq!(loaded.current_epoch().root_account, "grok");
    assert!(loaded.current_epoch().digest_matches());
}
