//! Reopen-owned reconciliation fences retries without failing unrelated effects.

use super::{test_support::*, *};
use crate::tools::TreeDriver;
use plakous_workspace::occupancy_vcs::fake::FakeOp;
use std::sync::Mutex;

struct Fixture {
    _root: tempfile::TempDir,
    vcs: Arc<FakeVcs>,
    tree: ProductAgentTree,
    child: String,
    workspace: String,
    request: AdoptCandidateRequest,
    observer: String,
}

impl Fixture {
    fn interrupted() -> Self {
        let (root, vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &mint_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let observer = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let request = adopt_req(&candidate_of(&tree, &child.agent_id));
        tree.begin_adopt_for_test("root", &request).unwrap();
        let _ = tree.take_effects();
        Self {
            _root: root,
            vcs,
            tree,
            child: child.agent_id,
            workspace: child.workspace.id,
            request,
            observer,
        }
    }

    fn reopen(&mut self) {
        self.tree = ProductAgentTree::reopen(self.tree.to_persisted());
        self.tree.set_vcs(self.vcs.clone());
        self.tree.resume_tree().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_pin_attachment_survives_dropped_first_mutation_effects() {
    for failed in [false, true] {
        let mut fixture = Fixture::interrupted();
        fixture.reopen();
        drop(fixture.tree.take_effects());
        if failed {
            fixture
                .vcs
                .restore_base_next_with(Err(VcsError::seal_failed("pin locked")));
        }
        let store = Arc::new(MemoryTreeStore::new());
        fixture.tree.acknowledge_revision(Revision::published(0));
        fixture
            .tree
            .acknowledge_revision(store.save(&fixture.tree).unwrap());
        let driver = TreeDriver::new(
            Arc::new(Mutex::new(fixture.tree)),
            store,
            fixture.vcs.clone(),
            None,
        );
        assert!(fixture.vcs.calls_of(FakeOp::RestoreBase).is_empty());
        driver.reconcile_attachment().await.unwrap();
        assert_eq!(fixture.vcs.calls_of(FakeOp::RestoreBase).len(), 1);
        // This also models a mutation whose caller fails before settlement.
        drop(driver.mutate(|_| ()).await.unwrap());
        let (admitted, settled) = driver
            .mutate_and_settle({
                let request = fixture.request.clone();
                move |tree| tree.begin_adopt_for_test("root", &request)
            })
            .await
            .unwrap();
        if failed {
            let error = admitted.unwrap_err();
            assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
            assert!(error.message.contains("pin locked"));
            assert!(error.message.contains("reopen to retry"));
            assert_eq!(fixture.vcs.calls_of(FakeOp::Adopt).len(), 0);
        } else {
            admitted.unwrap();
            assert!(settled.adopts[0].is_ok());
            assert_eq!(fixture.vcs.calls_of(FakeOp::Adopt).len(), 1);
        }
        driver.settle(Effects::default()).await.unwrap();
        assert_eq!(fixture.vcs.calls_of(FakeOp::RestoreBase).len(), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_pin_attachment_publication_failure_has_a_remedy_instead_of_pending() {
    let mut fixture = Fixture::interrupted();
    fixture.reopen();
    let store = Arc::new(MemoryTreeStore::new());
    fixture.tree.acknowledge_revision(Revision::published(0));
    fixture
        .tree
        .acknowledge_revision(store.save(&fixture.tree).unwrap());
    let mut newer = fixture.tree.clone();
    newer.bump();
    let _ = store.save(&newer).unwrap();
    let driver = TreeDriver::new(
        Arc::new(Mutex::new(fixture.tree)),
        store,
        fixture.vcs.clone(),
        None,
    );
    driver.reconcile_attachment().await.unwrap();
    let error = driver
        .read(|tree| {
            tree.check_base_pin_recovery(&fixture.workspace)
                .unwrap_err()
        })
        .unwrap();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(error.message.contains("attachment publication failed"));
    assert!(error.message.contains("reopen to retry"));
    assert_eq!(fixture.vcs.calls_of(FakeOp::RestoreBase).len(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_pin_failure_is_workspace_local_and_is_not_retried_by_settle() {
    let mut fixture = Fixture::interrupted();
    fixture.reopen();
    assert!(fixture.tree.take_effects().is_empty());
    fixture
        .vcs
        .restore_base_next_with(Err(VcsError::seal_failed("pin locked")));
    let store = Arc::new(MemoryTreeStore::new());
    fixture.tree.acknowledge_revision(Revision::published(0));
    fixture
        .tree
        .acknowledge_revision(store.save(&fixture.tree).unwrap());
    let driver = TreeDriver::new(
        Arc::new(Mutex::new(fixture.tree)),
        store,
        fixture.vcs.clone(),
        None,
    );
    driver.reconcile_attachment().await.unwrap();
    let settled = driver.settle(Effects::default()).await.unwrap();
    assert_eq!(settled.cleanup_failures, Vec::new());
    let (submitted, _) = driver
        .mutate_and_settle({
            let observer = fixture.observer.clone();
            move |tree| {
                tree.submit_result(
                    &observer,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: "{\"ok\":true}".into(),
                    },
                    Some("unrelated-submit"),
                )
            }
        })
        .await
        .unwrap();
    assert!(submitted.is_ok(), "{submitted:?}");
    driver.settle(Effects::default()).await.unwrap();
    let mut tree = driver.read(Clone::clone).unwrap();
    assert_eq!(
        tree.begin_adopt_for_test("root", &fixture.request)
            .unwrap_err()
            .code,
        TopologyErrorCode::WorkspaceRecoveryRequired
    );
    assert!(
        tree.fault_reason(&fixture.child)
            .unwrap()
            .contains("pin locked")
    );
    assert_eq!(fixture.vcs.calls_of(FakeOp::RestoreBase).len(), 1);
    tree.restore_paused();
    assert!(
        tree.take_effects().is_empty(),
        "same generation never queues again"
    );
    tree = ProductAgentTree::reopen(tree.to_persisted());
    assert!(
        !tree.take_attach_effects().is_empty(),
        "a new host may reconcile once"
    );
}

#[test]
fn base_pin_missing_path_base_or_producer_does_not_block_reopen_or_unrelated_settle() {
    for missing in ["path", "base", "producer"] {
        let mut fixture = Fixture::interrupted();
        match missing {
            "path" => {
                std::fs::remove_dir_all(&fixture.tree.workspaces[&fixture.workspace].path).unwrap();
            }
            "base" => {
                fixture
                    .tree
                    .workspaces
                    .get_mut(&fixture.workspace)
                    .unwrap()
                    .base_commit = None;
            }
            "producer" => {
                fixture.tree.nodes.remove(&fixture.child);
            }
            _ => unreachable!(),
        }
        fixture.reopen();
        assert!(fixture.tree.take_effects().is_empty(), "{missing}");
        assert_eq!(fixture.tree.settle_for_test().cleanup_failures, Vec::new());
        assert!(
            fixture.vcs.calls_of(FakeOp::RestoreBase).is_empty(),
            "{missing}"
        );
    }
}

#[test]
fn base_pin_pending_reconciliation_fences_adopt_until_completion_not_merely_vcs_return() {
    let mut fixture = Fixture::interrupted();
    fixture.reopen();
    let recovery = fixture.tree.take_attach_effects();
    assert_eq!(
        fixture
            .tree
            .begin_adopt_for_test("root", &fixture.request)
            .unwrap_err()
            .code,
        TopologyErrorCode::WorkspaceRecoveryRequired
    );
    let completed = block_on_for_test(recovery.run(fixture.vcs.as_ref()));
    assert_eq!(
        fixture
            .tree
            .begin_adopt_for_test("root", &fixture.request)
            .unwrap_err()
            .code,
        TopologyErrorCode::WorkspaceRecoveryRequired
    );
    fixture.tree.complete(completed);
    fixture
        .tree
        .begin_adopt_for_test("root", &fixture.request)
        .unwrap();
    fixture.tree.settle_for_test();
    fixture.tree.restore_paused();
    fixture.tree.settle_for_test();
    let calls = fixture.vcs.calls();
    assert_eq!(fixture.vcs.calls_of(FakeOp::RestoreBase).len(), 1);
    assert!(
        calls
            .iter()
            .rposition(|call| call.op == FakeOp::RestoreBase)
            < calls.iter().rposition(|call| call.op == FakeOp::Adopt)
    );
}

#[test]
fn base_pin_interrupted_adopt_retry_unavailable_retains_the_target_recovery_hold() {
    let mut fixture = Fixture::interrupted();
    fixture.reopen();
    fixture.tree.settle_for_test();
    fixture
        .tree
        .begin_adopt_for_test("root", &fixture.request)
        .unwrap();
    let ticket = fixture.tree.take_effects().adopts.remove(0);
    let error = fixture
        .tree
        .finish_adopt(
            &ticket,
            Err(AdoptFailure::Unavailable(
                "zero-file pin publication refused".into(),
            )),
        )
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(fixture.tree.workspace_has_vcs_fault(&fixture.workspace));
    assert_eq!(
        fixture.tree.workspace(&fixture.workspace).unwrap().status,
        WorkspaceStatus::Adopting
    );
    fixture.tree.restore_paused();
    assert!(fixture.tree.take_effects().is_empty());
}

/// Whether an interrupted copy is still there is asked of the port during
/// attachment. A port that cannot answer fails that workspace's
/// reconciliation with the reopen remedy; it is never skipped as absent.
#[test]
fn base_pin_presence_the_port_cannot_answer_fails_reconciliation_closed() {
    let mut fixture = Fixture::interrupted();
    fixture.reopen();
    fixture.vcs.refuse_workspace_questions();
    assert_eq!(fixture.tree.settle_for_test().cleanup_failures, Vec::new());
    let error = fixture
        .tree
        .begin_adopt_for_test("root", &fixture.request)
        .unwrap_err();
    assert_eq!(error.code, TopologyErrorCode::WorkspaceRecoveryRequired);
    assert!(error.message.contains("reopen to retry"), "{error:?}");
    assert_eq!(fixture.vcs.calls_of(FakeOp::Exists).len(), 1);
    assert!(fixture.vcs.calls_of(FakeOp::RestoreBase).is_empty());
}
