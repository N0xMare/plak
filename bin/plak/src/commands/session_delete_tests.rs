#![allow(clippy::unwrap_used, reason = "offline deletion assertions")]

use super::*;
use std::sync::Arc;

use plakous::sessions::{
    CreateSessionOutcome, CreateSessionRequest, OpenedSession, SessionBackend, SessionOperationId,
};
use plakous_hosted::agent_tree::turn_outcome::FailedTurnDisposition;
use plakous_hosted::agent_tree::{ProductAgentTree, RecoveryNeed, TreeStore as _};
use plakous_hosted::agent_tree_protocol::{NodeStatus, SpawnAgentRequest};
use plakous_hosted::topology_runtime::test_support::bootstrap;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::{BoundaryController, runtime_binding};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

trait Checked<T> {
    fn checked(self) -> TestResult<T>;
}

impl<T, E: std::fmt::Debug> Checked<T> for Result<T, E> {
    fn checked(self) -> TestResult<T> {
        self.map_err(|error| format!("{error:?}").into())
    }
}

struct Fixture {
    _tmp: tempfile::TempDir,
    paths: crate::home::paths::PlakPaths,
    product: crate::store::ProductStore,
    runtime: RuntimeDeletion,
    record: product_sessions::SessionRecord,
    tree: ProductAgentTree,
    child_id: String,
    child_locator: SessionLocator,
    held_child: Option<OpenedSession>,
}

fn create(
    runtime: &RuntimeDeletion,
    paths: &crate::home::paths::PlakPaths,
    id: &str,
) -> TestResult<OpenedSession> {
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        paths.home().join("delete-physical.jsonl"),
    )
    .checked()?;
    let created = runtime
        .run(runtime.sessions.create(CreateSessionRequest::new(
            SessionOperationId::new(id).checked()?,
            runtime_binding(paths.home(), controller).checked()?,
        )))
        .checked()??;
    let CreateSessionOutcome::Created { session, .. } = created else {
        return Err("offline create did not return a session".into());
    };
    Ok(session)
}

fn fixture(leased: bool) -> TestResult<Fixture> {
    let (tmp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
    paths.ensure_layout().checked()?;
    let runtime = RuntimeDeletion {
        sessions: Arc::new(
            AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).checked()?)
                .checked()?,
        ),
        executor: plakous::application::ApplicationExecutor::new().checked()?,
    };
    let root = create(&runtime, &paths, "delete:create-root").checked()?;
    let root_locator = runtime.run(root.release()).checked()??;
    let child = create(&runtime, &paths, "delete:create-child").checked()?;
    let child_locator = child.locator();
    let held_child = if leased {
        Some(child)
    } else {
        runtime.run(child.release()).checked()??;
        None
    };
    let mut record = product_sessions::SessionRecord::new(paths.home().to_path_buf());
    record.bind_plakous_locator(root_locator).checked()?;
    let mut product = crate::store::ProductStore::open(&paths).checked()?;
    product.commit(&mut record).checked()?;
    let mut tree = bootstrap(&record.id, paths.home().to_path_buf());
    let child_id = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .checked()?
        .agent_id;
    tree.bind_session_locator(&child_id, child_locator.to_string())
        .checked()?;
    tree.record_turn_failure(
        &child_id,
        FailedTurnDisposition::RecoveryRequired,
        "offline unresolved child obligation".into(),
    )
    .checked()?;
    tree.settle_for_test();
    crate::store::tree::save_tree(&paths, &mut tree).checked()?;
    Ok(Fixture {
        _tmp: tmp,
        paths,
        product,
        runtime,
        record,
        tree,
        child_id,
        child_locator,
        held_child,
    })
}

#[test]
fn delete_without_abandon_flag_still_refuses_recovery_required_tree() -> TestResult {
    let mut f = fixture(false).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let refusal = deletion_plan(&f.paths, &mut f.product, &f.record, root, false)
        .err()
        .ok_or("unflagged delete accepted recovery debt")
        .checked()?;
    assert!(refusal.message().contains("need recovery"));
    assert!(refusal.message().contains("--abandon-children"));
    assert!(
        f.runtime
            .run(f.runtime.sessions.head_is_quiescent(root))
            .checked()??
    );
    let live = f
        .tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .checked()?;
    crate::store::tree::save_tree(&f.paths, &mut f.tree).checked()?;
    let refusal = deletion_plan(&f.paths, &mut f.product, &f.record, root, true)
        .err()
        .ok_or("flagged delete accepted a live child")
        .checked()?;
    assert!(refusal.message().contains(&live.agent_id));
    assert_eq!(
        f.tree.node(&f.child_id).checked()?.status,
        NodeStatus::RecoveryRequired
    );
    Ok(())
}

#[test]
fn delete_close_then_fault_refuses_or_explicitly_abandons_recovery() -> TestResult {
    let mut f = fixture(false).checked()?;
    let attempt = f
        .tree
        .attempt(&f.child_id)
        .ok_or("child attempt")
        .checked()?
        .attempt_id
        .clone();
    f.tree
        .recover_host_decision(
            &f.child_id,
            &attempt,
            plakous_hosted::agent_tree::RecoveryResolution::RecoveredWithoutTurn {
                message: "fixture is quiescent".into(),
            },
        )
        .checked()?;
    f.tree.settle_for_test();
    f.child_id = f
        .tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .checked()?
        .agent_id;
    f.tree
        .bind_session_locator(&f.child_id, f.child_locator.to_string())
        .checked()?;
    let attempt = f
        .tree
        .attempt(&f.child_id)
        .ok_or("new attempt")
        .checked()?
        .attempt_id
        .clone();
    f.tree.set_active_turn(&f.child_id, true).checked()?;
    f.tree
        .close_agent(
            "root",
            &plakous_hosted::agent_tree_protocol::AttemptScopedRequest {
                agent_id: f.child_id.clone(),
                expected_attempt_id: Some(attempt),
            },
            None,
        )
        .checked()?;
    f.tree
        .record_host_retirement(&f.child_id, "late release deadline".into())
        .and_then(|value| f.tree.fold_due(&f.child_id).map(|_| value))
        .checked()?;
    crate::store::tree::save_tree(&f.paths, &mut f.tree).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let refusal = deletion_plan(&f.paths, &mut f.product, &f.record, root, false)
        .err()
        .ok_or("delete accepted closed recovery debt")
        .checked()?;
    assert!(refusal.message().contains("need recovery"));
    assert!(refusal.message().contains("--abandon-children"));
    let mut plan = deletion_plan(&f.paths, &mut f.product, &f.record, root, true).checked()?;
    assert_eq!(plan.abandon, vec![f.child_id.clone()]);
    publish_abandoned_children(&f.paths, &mut plan).checked()?;
    let closed = crate::store::tree::load_tree(&f.paths, &f.record.id)
        .checked()?
        .ok_or("saved tree")
        .checked()?;
    assert_eq!(
        closed.node(&f.child_id).checked()?.status,
        NodeStatus::Closed
    );
    Ok(())
}

#[test]
fn delete_abandoned_obligation_survives_sqlite_restart_before_cleanup() -> TestResult {
    let mut f = fixture(false).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let attempt = f
        .tree
        .attempt(&f.child_id)
        .ok_or("attempt")
        .checked()?
        .attempt_id
        .clone();
    let mut plan = deletion_plan(&f.paths, &mut f.product, &f.record, root, true).checked()?;
    assert_eq!(plan.abandon, [f.child_id.clone()]);
    let before = SessionBackend::inspect_sqlite_usage(f.paths.runtime_database())
        .checked()?
        .ok_or("usage")
        .checked()?;
    f.runtime.archive(&f.record.id, root).checked()?;
    f.runtime.archive(&f.record.id, f.child_locator).checked()?;
    publish_abandoned_children(&f.paths, &mut plan).checked()?;
    let store = crate::store::tree::open_tree_store(&f.paths).checked()?;
    let persisted = store
        .load(&f.record.id)
        .checked()?
        .ok_or("persisted tree")
        .checked()?;
    let failure = persisted
        .rows()
        .attempt(&f.child_id)
        .ok_or("persisted attempt")
        .checked()?
        .failure
        .as_ref()
        .ok_or("persisted obligation")
        .checked()?;
    assert_eq!(failure.kind, "recovery_abandoned");
    assert!(failure.message.contains("operator"));
    assert!(failure.message.contains(attempt.as_str()));
    assert!(failure.message.contains("recovery will not run"));
    let reopened = ProductAgentTree::reopen(persisted);
    assert_eq!(
        reopened.recovery_need(&f.child_id).checked()?,
        RecoveryNeed::NotRequired
    );
    assert_eq!(
        reopened.node(&f.child_id).checked()?.status,
        NodeStatus::Closed
    );
    assert!(reopened.session_recovery_targets().is_empty());
    let after = SessionBackend::inspect_sqlite_usage(f.paths.runtime_database())
        .checked()?
        .ok_or("usage")
        .checked()?;
    for (before, after) in before.bounds().iter().zip(after.bounds()) {
        assert_eq!(before.limit(), after.limit());
    }
    drop(store);
    f.runtime = RuntimeDeletion {
        sessions: Arc::new(
            AgentSessions::new(SessionBackend::sqlite(f.paths.runtime_database()).checked()?)
                .checked()?,
        ),
        executor: plakous::application::ApplicationExecutor::new().checked()?,
    };
    // The durable abandonment is sufficient: an interrupted cleanup no longer
    // needs the flag, and never reopens or recovers a child.
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, false)
        .checked()?;
    assert!(product_sessions::load_session(&f.paths, &f.record.id).is_err());
    assert!(
        !crate::store::tree::open_tree_store(&f.paths)
            .checked()?
            .exists(&f.record.id)
            .checked()?
    );
    assert_eq!(
        f.runtime
            .run(f.runtime.sessions.head_is_quiescent(f.child_locator))
            .checked()?
            .unwrap_err()
            .kind(),
        AgentSessionsErrorKind::NotFound
    );
    Ok(())
}

#[test]
fn delete_abandon_flag_never_closes_a_leased_child_obligation() -> TestResult {
    let mut f = fixture(true).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let refusal =
        delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, true)
            .err()
            .ok_or("a leased child was abandoned")
            .checked()?;
    assert!(refusal.message().contains("open in a running plak"));
    let persisted = crate::store::tree::open_tree_store(&f.paths)
        .checked()?
        .load(&f.record.id)
        .checked()?
        .ok_or("tree")
        .checked()?;
    assert_eq!(
        persisted.rows().node(&f.child_id).checked()?.status,
        NodeStatus::RecoveryRequired
    );
    assert_ne!(
        persisted
            .rows()
            .attempt(&f.child_id)
            .ok_or("attempt")
            .checked()?
            .failure
            .as_ref()
            .ok_or("failure")
            .checked()?
            .kind,
        "recovery_abandoned"
    );
    let child = f.held_child.take().ok_or("held child").checked()?;
    f.runtime.run(child.release()).checked()??;
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, true)
        .checked()?;
    Ok(())
}

#[test]
fn delete_abandons_recovery_required_child_without_locator() -> TestResult {
    let mut f = fixture(false).checked()?;
    let child = f
        .tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .checked()?
        .agent_id;
    f.tree
        .record_turn_failure(
            &child,
            FailedTurnDisposition::RecoveryRequired,
            "unbound child".into(),
        )
        .checked()?;
    crate::store::tree::save_tree(&f.paths, &mut f.tree).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let mut plan = deletion_plan(&f.paths, &mut f.product, &f.record, root, true).checked()?;
    assert!(plan.abandon.contains(&child));
    assert!(!plan.children.iter().any(|(id, _)| id == &child));
    publish_abandoned_children(&f.paths, &mut plan).checked()?;
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, false)
        .checked()?;
    Ok(())
}

#[test]
fn delete_abandons_recovery_required_descendant() -> TestResult {
    let mut f = fixture(false).checked()?;
    f.tree.grants_mut_for_test("root").max_depth = 3;
    let parent = f
        .tree
        .spawn_agent("root", &SpawnAgentRequest::test_orchestrate(), None)
        .checked()?
        .agent_id;
    let child = f
        .tree
        .spawn_agent(&parent, &SpawnAgentRequest::test_review(), None)
        .checked()?
        .agent_id;
    for id in [&parent, &child] {
        f.tree
            .record_turn_failure(
                id,
                FailedTurnDisposition::RecoveryRequired,
                "descendant recovery".into(),
            )
            .checked()?;
    }
    crate::store::tree::save_tree(&f.paths, &mut f.tree).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let plan = deletion_plan(&f.paths, &mut f.product, &f.record, root, true).checked()?;
    assert!(plan.abandon.contains(&child));
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, true)
        .checked()?;
    Ok(())
}

#[test]
fn delete_abandon_flag_refuses_fork_source_recovery() -> TestResult {
    let mut f = fixture(false).checked()?;
    let fork = create(&f.runtime, &f.paths, "delete:create-fork").checked()?;
    let locator = f.runtime.run(fork.release()).checked()??;
    let mut record = product_sessions::SessionRecord::new(f.paths.home().to_path_buf());
    record.source_product_session_id = Some(f.record.id.clone());
    record.source_workflow_operation_id = Some("delete:test-fork-source".into());
    record.bind_plakous_locator(locator).checked()?;
    f.product.commit(&mut record).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let dependency = deletion_plan(&f.paths, &mut f.product, &f.record, root, true)
        .err()
        .ok_or("source with a dependent fork was accepted")
        .checked()?;
    assert!(dependency.message().contains("continue from it"));
    let refusal = deletion_plan(&f.paths, &mut f.product, &record, locator, true)
        .err()
        .ok_or("fork abandoned source tree")
        .checked()?;
    assert!(refusal.message().contains("need recovery"));
    let persisted = crate::store::tree::open_tree_store(&f.paths)
        .checked()?
        .load(&f.record.id)
        .checked()?
        .ok_or("tree")
        .checked()?;
    assert_eq!(
        persisted.rows().node(&f.child_id).checked()?.status,
        NodeStatus::RecoveryRequired
    );
    Ok(())
}

#[test]
fn publish_abandoned_children_refuses_stale_tree_revision() -> TestResult {
    let mut f = fixture(false).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let mut plan = deletion_plan(&f.paths, &mut f.product, &f.record, root, true).checked()?;
    f.tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .checked()?;
    crate::store::tree::save_tree(&f.paths, &mut f.tree).checked()?;
    let refusal = publish_abandoned_children(&f.paths, &mut plan)
        .err()
        .ok_or("stale abandonment accepted")
        .checked()?;
    assert!(
        refusal
            .message()
            .contains("changed before child abandonment")
    );
    let persisted = crate::store::tree::open_tree_store(&f.paths)
        .checked()?
        .load(&f.record.id)
        .checked()?
        .ok_or("tree")
        .checked()?;
    assert_eq!(
        persisted.rows().node(&f.child_id).checked()?.status,
        NodeStatus::RecoveryRequired
    );
    Ok(())
}

#[test]
fn delete_removes_that_trees_acknowledgements_and_is_idempotent() -> TestResult {
    let mut f = fixture(false).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let ack = crate::surface_protocol::AgentAcknowledgement {
        agent_id: f.child_id.clone(),
        attempt_id: "child-a1".into(),
        attention: "needs recovery".into(),
        ended_at: Some(10),
    };
    f.product
        .acknowledge_agent_attempt(&f.record.id, &ack, 11)
        .checked()?;
    f.product
        .acknowledge_agent_attempt("other-tree", &ack, 12)
        .checked()?;
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, true)
        .checked()?;
    assert!(
        f.product
            .agent_acknowledgements(&f.record.id)
            .checked()?
            .is_empty(),
        "the deleted session's tree acknowledgements go with it"
    );
    assert_eq!(
        f.product
            .agent_acknowledgements("other-tree")
            .checked()?
            .len(),
        1,
        "another tree's acknowledgements stay"
    );
    f.product
        .delete_agent_acknowledgements(&f.record.id)
        .checked()?;
    Ok(())
}

#[test]
fn delete_removes_every_deleted_runtime_sessions_job_output_files() -> TestResult {
    let mut f = fixture(false).checked()?;
    let root = f.record.plakous_locator.ok_or("root locator").checked()?;
    let jobs = f.paths.job_outputs();
    let owned = [root.session_id(), f.child_locator.session_id()]
        .map(|session| jobs.join(session.to_string()));
    let unrelated = jobs.join("00000000-0000-0000-0000-0000000000ff");
    for directory in owned.iter().chain([&unrelated]) {
        std::fs::create_dir_all(directory).checked()?;
        std::fs::write(directory.join("branch-p1.log"), b"output").checked()?;
    }
    delete_confirmed_session(&f.paths, &mut f.product, &f.runtime, &f.record, root, true)
        .checked()?;
    for directory in &owned {
        assert!(!directory.exists(), "{}", directory.display());
    }
    assert!(unrelated.join("branch-p1.log").exists());
    Ok(())
}
