use std::sync::Arc;

use super::ProductStateRoots;

#[derive(Default)]
struct PausedTreeStore {
    inner: crate::agent_tree::MemoryTreeStore,
    full: std::sync::atomic::AtomicBool,
    waiting: std::sync::atomic::AtomicBool,
    recovered: std::sync::atomic::AtomicUsize,
}

impl crate::agent_tree::TreeStore for PausedTreeStore {
    fn exists(&self, id: &str) -> Result<bool, crate::agent_tree::TreeStoreError> {
        self.inner.exists(id)
    }
    fn ids(&self) -> Result<Vec<String>, crate::agent_tree::TreeStoreError> {
        self.inner.ids()
    }
    fn load(
        &self,
        id: &str,
    ) -> Result<Option<crate::agent_tree::PersistedTree>, crate::agent_tree::TreeStoreError> {
        self.inner.load(id)
    }
    fn save(
        &self,
        tree: &crate::agent_tree::ProductAgentTree,
    ) -> Result<crate::agent_tree::Revision, crate::agent_tree::TreeStoreError> {
        if self.full.load(std::sync::atomic::Ordering::Acquire) {
            self.waiting
                .store(true, std::sync::atomic::Ordering::Release);
            return Err(crate::agent_tree::TreeStoreError::storage_full());
        }
        self.inner.save(tree)
    }
    fn storage_unavailable(&self) -> bool {
        self.waiting.load(std::sync::atomic::Ordering::Acquire)
    }
    fn recover_storage(&self) -> Result<(), crate::agent_tree::TreeStoreError> {
        if self.full.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::agent_tree::TreeStoreError::storage_full());
        }
        self.recovered
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.waiting
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[tokio::test]
async fn next_root_prompt_retries_the_paused_tree_store_without_an_incidental_tree_mutation() {
    use crate::agent_tree::TreeStore as _;
    use std::sync::atomic::Ordering;
    let home = tempfile::tempdir().unwrap();
    let roots =
        ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
    let store = Arc::new(PausedTreeStore::default());
    *roots.tree_store.lock().unwrap() = Some(store.clone());
    let sessions = roots.runtime_sessions().unwrap();
    let controller = plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        home.path().join("evidence.jsonl"),
    )
    .unwrap();
    let binding = plakous_offline_crash_harness::runtime_binding(home.path(), controller).unwrap();
    let plakous::sessions::CreateSessionOutcome::Created { session, .. } = sessions
        .create(plakous::sessions::CreateSessionRequest::new(
            plakous::sessions::SessionOperationId::new("create:paused-tree-admission").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("created root")
    };
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test_with(
        Arc::new(crate::occupancy_vcs::fake::FakeVcs::new()),
        "paused-tree",
        "root",
        "owner",
        home.path().to_path_buf(),
        "tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    store.full.store(true, Ordering::Release);
    assert!(store.save(&tree).is_err());
    store.full.store(false, Ordering::Release);
    assert!(session.agent().storage_unavailable());
    let submission = session
        .agent()
        .start(
            plakous::agent::CommandId::new("root:after-space-freed").unwrap(),
            "complete after space is freed",
        )
        .await
        .unwrap();
    let result = submission.result().unwrap().clone().result().await.unwrap();
    assert!(matches!(
        result.outcome(),
        plakous::agent::TurnResultOutcome::Completed(_)
    ));
    assert_eq!(store.recovered.load(Ordering::Acquire), 1);
    assert!(
        store.ids().unwrap().is_empty(),
        "no incidental tree save cleared the debt"
    );
    session.agent().shutdown().await.unwrap();
}

struct LowStorage;

impl crate::runtime::fault_store::StoreFaults for LowStorage {
    fn storage_admission(&self) -> Option<crate::session_store::StorageAdmission> {
        Some(crate::session_store::StorageAdmission::Refused(
            "product volume: 0 MiB free".into(),
        ))
    }

    fn before(
        &self,
        _: crate::runtime::fault_store::StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        Err("capacity refusal must precede store access".into())
    }
}

#[test]
fn low_storage_refuses_spawn_before_template_or_store_access() {
    let mut parent = crate::agent::tests::openrouter_test_parent();
    parent.state_roots.session_records = Arc::new(crate::runtime::fault_store::FaultStore::new(
        parent.state_roots.session_records(),
        LowStorage,
    ));
    let binder = crate::agent::children::ProductChildSessionBinder {
        parent: crate::agent::SpawnParent::Api(parent),
    };
    let refusal = binder
        .admit_spawn(&crate::agent_tree_protocol::SpawnAgentRequest::test_review())
        .unwrap_err();
    assert_eq!(
        refusal.code,
        crate::agent_tree_protocol::TopologyErrorCode::StorageLow
    );
    assert!(refusal.message.contains("do not delete files or retry"));
    assert!(!refusal.message.contains("0 MiB"));
}

struct UnreadableRoster;

impl super::RosterSource for UnreadableRoster {
    fn roster(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
    ) -> crate::CliResult<crate::orchestration::RosterInput> {
        Err(crate::CliFailure::new("named config error"))
    }

    fn account_name(&self, _: plakous::subscriptions::NativeProvider) -> &'static str {
        "codex"
    }

    fn topology_admitted(&self) -> crate::CliResult<bool> {
        Err(crate::CliFailure::new("named config error"))
    }
}

#[test]
fn unreadable_roster_refuses_fresh_genesis_without_publishing_a_tree() {
    let home = tempfile::tempdir().unwrap();
    let mut roots =
        ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
    roots.roster_source = Arc::new(UnreadableRoster);
    let tree_id = uuid::Uuid::new_v4().to_string();
    let parent = crate::agent::SpawnParent::Api(crate::agent::ApiTopologyParent {
        prompt_host_facts: plakous_host_protocol::PromptHostFacts::default(),
        created_output_bound: None,
        workspace_root: home.path().to_path_buf(),
        cwd: home.path().to_path_buf(),
        plan_state_authority: tree_id.clone(),
        composition: crate::binding::CompositionSelection::Direct,
        topology_tree_id: None,
        topology_caller_id: None,
        state_roots: roots.clone(),
        model: "offline-model".into(),
        effort: None,
        target: crate::binding::ApiTarget::OpenRouter {
            wire: crate::binding::ResolvedProtocol::Chat,
        },
        scheduled_compaction: true,
        max_model_rounds: 8,
        topology_enabled: true,
        product_home: Some(home.path().to_path_buf()),
        tool_selection: crate::tool_plan::NodeToolSelection::default(),
    });
    let result = crate::agent::build::topology_extensions_if_enabled(
        &parent,
        "offline-model",
        crate::topology_runtime::TopologyHubAttach::AttachRoot,
        false,
        &mut None,
    );
    assert!(
        result
            .err()
            .is_some_and(|error| error.message() == "named config error")
    );
    assert!(!roots.tree_store().unwrap().exists(&tree_id).unwrap());
}

#[test]
fn reopen_pin_is_exact_engine_scoped_and_cleared_for_new_children() {
    use plakous::tools::SidecarLocator;
    struct Current(std::path::PathBuf);
    impl SidecarLocator for Current {
        fn javascript_host(&self) -> Result<std::path::PathBuf, String> {
            Ok(self.0.clone())
        }
        fn python_host(&self) -> Result<std::path::PathBuf, String> {
            Ok(self.0.clone())
        }
    }
    let home = tempfile::tempdir().unwrap();
    let historical = home.path().join("historical");
    let current = home.path().join("current");
    std::fs::write(&historical, b"identical bytes").unwrap();
    std::fs::copy(&historical, &current).unwrap();
    let historical = historical.canonicalize().unwrap();
    let current = current.canonicalize().unwrap();
    let mut roots =
        ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
    roots.sidecars = Arc::new(Current(current.clone()));
    roots.reopen_sidecars = Some(Arc::new(super::RecordedSidecar {
        artifact: Some(
            plakous::agent::LocalCompositionEngineArtifact::quickjs_worker_v1(&historical).unwrap(),
        ),
    }));
    assert_eq!(roots.sidecars().javascript_host().unwrap(), historical);
    assert!(roots.sidecars().python_host().is_err());
    assert_eq!(
        roots
            .clone()
            .without_reopen_artifact()
            .sidecars()
            .javascript_host()
            .unwrap(),
        current
    );
    std::fs::write(&historical, b"changed").unwrap();
    assert!(roots.sidecars().javascript_host().is_err());
    std::fs::remove_file(&historical).unwrap();
    assert!(roots.sidecars().javascript_host().is_err());
}

/// The roots open the topology store on the
/// first ask and hand that same handle to every attach after it — on
/// every clone of the roots, since the cache is shared.
#[test]
fn the_tree_store_is_opened_once_and_shared() {
    let home = tempfile::tempdir().unwrap();
    let roots =
        ProductStateRoots::with_doubles(home.path().to_path_buf(), home.path().join("state"));
    let first = roots.tree_store().unwrap();
    let second = roots.tree_store().unwrap();
    let third = roots.clone().tree_store().unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(Arc::ptr_eq(&first, &third));
}

/// The other half of fix 6, and the tree crate's own contract rather than
/// a test name cited from here: an attach the hub can already answer asks
/// the roots for no store at all.
#[tokio::test]
async fn a_hub_hit_asks_the_roots_for_no_store() {
    let home = tempfile::tempdir().unwrap();
    crate::topology_runtime::test_support::hub_hit_opens_no_store(home.path().to_path_buf()).await;
}
