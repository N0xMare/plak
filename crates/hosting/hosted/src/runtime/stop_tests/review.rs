//! Adversarial recorded-stop retries: admission, ownership and failed settlement.

use super::*;
use crate::agent::ProductStateRoots;
use crate::sessions::projector::{Delivery, SessionViews, Submission, SubmissionKind, ViewWrites};
use plakous::runner::{ConfiguredRunner, RunnerRegistry, WorkspaceCatalog};
use plakous::sessions::{LeaseOnlyHead, SessionOperationId};

struct Recorded {
    root: tempfile::TempDir,
    lost: Lost,
    roots: ProductStateRoots,
    registry: RunnerRegistry,
}

impl Recorded {
    fn new() -> Self {
        let case = Self::unrecorded();
        case.stop().unwrap();
        case
    }

    fn unrecorded() -> Self {
        let root = tempfile::tempdir().unwrap();
        let lost = release_a_running_root_turn(root.path(), Release::Lost, Cancel::Deaf, |c| c);
        let registry = RunnerRegistry::default();
        registry.configure(ConfiguredRunner {
            runner_id: "local".into(),
            names: vec!["local".into()],
        });
        registry.begin_reconcile("local", 1);
        let roots = lost
            .state_roots
            .clone()
            .for_runner_stop(registry.clone(), "local");
        Self {
            root,
            lost,
            roots,
            registry,
        }
    }

    fn stop(&self) -> crate::error::CliResult<()> {
        let mut record = self.lost.store.load(&self.lost.id).unwrap();
        super::super::stop_dormant_session(&self.roots, self.lost.store.as_ref(), &mut record)
            .block()
    }

    fn ready(&self) {
        self.registry.ready_local(self.catalog(), 1).unwrap();
        assert!(!self.roots.runner_pending());
    }

    fn catalog(&self) -> Arc<WorkspaceCatalog> {
        let workspace = self.root.path().join("ready-workspace");
        let protected = self.root.path().join("protected");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&protected).unwrap();
        Arc::new(WorkspaceCatalog::local(Some(&workspace), &[], &protected).unwrap())
    }

    fn hold(&self) -> LeaseOnlyHead {
        let locator = self
            .lost
            .store
            .load(&self.lost.id)
            .unwrap()
            .plakous_locator
            .unwrap();
        let sessions = self.roots.runtime_sessions().unwrap();
        let line = sessions.line_head_of(locator).block().unwrap();
        sessions
            .open_lease_only(
                &SessionOperationId::new("review:held").unwrap(),
                line.line(),
            )
            .block()
            .unwrap()
    }

    fn accept_later_submission(&self) {
        let mut record = self.lost.store.load(&self.lost.id).unwrap();
        let submission = Submission {
            command_id: "later-command".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: "later-item".into(),
            queue_item_revision: 1,
            target_turn_id: None,
            canonical_command: "later accepted turn".into(),
            submitted_at_ms: 1,
        };
        self.lost
            .store
            .save_submission(&mut record, &submission)
            .unwrap();
        let from = self
            .lost
            .store
            .load_views(&self.lost.id)
            .unwrap()
            .unwrap_or_else(SessionViews::empty);
        let mut to = from.clone();
        to.state
            .deliveries
            .insert(submission.command_id, Delivery::Accepted);
        self.lost
            .store
            .commit_projection_views(&self.lost.id, &ViewWrites::between(&from, &to), &to.state)
            .unwrap();
    }
}

struct PausedProbe {
    runtime: tokio::runtime::Runtime,
    registry: RunnerRegistry,
    calls: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Semaphore>,
}

impl PausedProbe {
    fn new(case: &Recorded) -> Self {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let registry = case.registry.clone();
        registry.run_probes_on(runtime.handle().clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        registry.probe_with(Arc::new({
            let (calls, release, catalog) = (calls.clone(), release.clone(), case.catalog());
            move |registry, _| {
                let (calls, release, catalog) = (calls.clone(), release.clone(), catalog.clone());
                Box::pin(async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    release.acquire().await.unwrap().forget();
                    registry.ready_local(catalog, 1)
                })
            }
        }));
        registry.reconcile_when_available("local");
        runtime.block_on(probe_until(|| calls.load(Ordering::SeqCst) == 1));
        Self {
            runtime,
            registry,
            calls,
            release,
        }
    }

    fn require_fresh_pass(&self) {
        use std::sync::atomic::Ordering::SeqCst;
        self.runtime.block_on(async {
            self.release.add_permits(1);
            probe_until(|| {
                self.calls.load(SeqCst) == 2
                    || self.registry.rows()[0].status == plakous::runner::RunnerStatus::Ready
            })
            .await;
            assert_eq!(
                self.calls.load(SeqCst),
                2,
                "stale pass published Ready over repaired or failed stop debt"
            );
            assert!(self.registry.services("local").is_err());
        });
    }

    fn settle(&self) {
        self.runtime.block_on(async {
            self.release.add_permits(1);
            probe_until(|| self.registry.rows()[0].status == plakous::runner::RunnerStatus::Ready)
                .await;
        });
    }
}

async fn probe_until(predicate: impl Fn() -> bool) {
    for _ in 0..10000 {
        if predicate() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "probe did not reach its deterministic barrier");
}

struct CutAfterRootStop {
    written: std::sync::atomic::AtomicBool,
    cut: std::sync::atomic::AtomicBool,
}

impl crate::runtime::fault_store::StoreFaults for CutAfterRootStop {
    fn before(
        &self,
        call: crate::runtime::fault_store::StoreCall,
        _: Option<&crate::sessions::SessionRecord>,
    ) -> Result<(), String> {
        use crate::runtime::fault_store::StoreCall;
        use std::sync::atomic::Ordering::SeqCst;
        if call == StoreCall::WriteRootLoss && !self.cut.load(SeqCst) {
            self.written.store(true, SeqCst);
        }
        if call == StoreCall::LoadIntents
            && self.written.swap(false, SeqCst)
            && !self.cut.swap(true, SeqCst)
        {
            return Err("cut after committed root stop, before descendant save".into());
        }
        Ok(())
    }
}

#[test]
fn a_recorded_stop_cut_repairs_descendants_without_taking_the_head() {
    use crate::agent_tree::{
        AgentCommand, AgentCommandKind, ProductAgentTree, acquire_custody, release_custody_hold,
    };
    use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
    let case = Recorded::unrecorded();
    let mut record = case.lost.store.load(&case.lost.id).unwrap();
    let trees = case.roots.tree_store().unwrap();
    let custody = acquire_custody(trees.custody_port(), &record.node.tree_id).unwrap();
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        &record.node.tree_id,
        "root",
        "owner",
        case.root.path().to_path_buf(),
        "tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
        .unwrap()
        .agent_id;
    tree.bind_session_locator(&child, "child-locator").unwrap();
    tree.set_active_turn(&child, true).unwrap();
    tree.bind_attempt_turn(&child, "00000000-0000-4000-8000-000000000001")
        .unwrap();
    tree.record_command(AgentCommand::intent(
        "start-child",
        &child,
        tree.node(&child).unwrap().attempt_id.clone(),
        AgentCommandKind::Start,
        "assignment",
        Vec::new(),
    ))
    .unwrap();
    let revision = trees.save_in_custody(&tree).unwrap();
    tree.acknowledge_revision(revision);
    release_custody_hold(trees.custody_port(), custody).unwrap();
    let cut = crate::runtime::fault_store::FaultStore::new(
        case.lost.store.clone(),
        CutAfterRootStop {
            written: std::sync::atomic::AtomicBool::new(false),
            cut: std::sync::atomic::AtomicBool::new(false),
        },
    );
    assert!(
        super::super::stop_dormant_session(&case.roots, &cut, &mut record)
            .block()
            .is_err()
    );
    assert_eq!(
        root_causes(&case.lost).last(),
        Some(&RootInterruptCause::Stopped)
    );
    assert_eq!(
        ProductAgentTree::reopen(trees.load(&record.node.tree_id).unwrap().unwrap())
            .recorded_process_stops(),
        [] as [(String, String); 0]
    );
    let held = case.hold();
    let probe = PausedProbe::new(&case);
    super::super::stop_dormant_session(&case.roots, &cut, &mut record)
        .block()
        .unwrap();
    assert!(
        !ProductAgentTree::reopen(trees.load(&record.node.tree_id).unwrap().unwrap())
            .recorded_process_stops()
            .is_empty(),
        "pending acknowledgement lost descendant stop debt"
    );
    held.release().block().unwrap();
    probe.require_fresh_pass();
    probe.settle();
}

#[test]
fn a_failed_settlement_fences_a_skipping_pass_before_releasing_its_head() {
    use std::future::Future as _;
    let case = Recorded::new();
    case.ready();
    let head = case.hold();
    let probe = PausedProbe::new(&case);
    let db = rusqlite::Connection::open(case.roots.runtime_database()).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut releasing = Box::pin(super::super::dormant_stop::release_after_settlement(
        &case.roots,
        head,
        Err(crate::error::CliFailure::new("injected failed settlement")),
    ));
    probe.runtime.block_on(std::future::poll_fn(|cx| {
        assert!(
            releasing.as_mut().poll(cx).is_pending(),
            "release did not wait for the store lock"
        );
        std::task::Poll::Ready(())
    }));
    // Release is still blocked: a skipping pass must already require a successor.
    probe.require_fresh_pass();
    db.execute_batch("COMMIT").unwrap();
    probe.runtime.block_on(releasing).unwrap();
    probe.settle();
}

#[test]
fn a_recorded_stop_does_not_cover_a_later_accepted_submission() {
    let case = Recorded::new();
    case.accept_later_submission();
    let held = case.hold();
    assert!(case.roots.runner_pending());
    assert!(
        case.stop().is_err(),
        "an old stop acknowledged a later accepted turn"
    );
    held.release().block().unwrap();
}

#[test]
fn a_ready_recorded_stop_still_refuses_a_held_head() {
    let case = Recorded::new();
    case.ready();
    let held = case.hold();
    assert!(
        case.stop().is_err(),
        "Ready was treated as reconciliation ownership"
    );
    held.release().block().unwrap();
}

#[test]
fn a_recorded_stop_propagates_a_non_ownership_open_failure() {
    let case = Recorded::new();
    case.ready();
    let db = rusqlite::Connection::open(case.roots.runtime_database()).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_stop_lease BEFORE UPDATE OF active_owner ON branches BEGIN SELECT RAISE(ABORT, 'review storage refusal'); END;").unwrap();
    let sessions = case.roots.runtime_sessions().unwrap();
    let locator = case
        .lost
        .store
        .load(&case.lost.id)
        .unwrap()
        .plakous_locator
        .unwrap();
    let line = sessions.line_head_of(locator).block().unwrap();
    let error = sessions
        .open_lease_only(
            &SessionOperationId::new("review:storage").unwrap(),
            line.line(),
        )
        .block()
        .unwrap_err();
    assert!(!matches!(
        error.kind(),
        plakous::sessions::AgentSessionsErrorKind::Ownership
            | plakous::sessions::AgentSessionsErrorKind::Busy
    ));
    assert!(
        case.stop().is_err(),
        "storage failure acknowledged cancellation"
    );
}

struct RefuseFloors;
impl plakous::runner::OwnerFloors for RefuseFloors {
    fn load(&self) -> Vec<(String, plakous::runner::Floor)> {
        Vec::new()
    }
    fn record(&self, _: &str, _: &plakous::runner::Floor) -> std::io::Result<()> {
        Err(std::io::Error::other("review floor persistence refusal"))
    }
}

#[test]
fn a_failed_settlement_of_a_ready_recorded_stop_fences_again() {
    use plakous::runner::protocol::{Bind, VersionTriple};
    use plakous::runner::{
        ClientWindows, RunnerClient, RunnerIdentity, RunnerOptions, RunnerService,
    };
    use plakous::tools::{
        ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, RunnerId,
    };
    let mut case = Recorded::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let workspace = case.root.path().join("runner-workspace");
    std::fs::create_dir(&workspace).unwrap();
    let version = VersionTriple::current("review", "test");
    let service = RunnerService::with_options(
        ExecutionEnvironment::host(
            ExecutionEnvironmentScope::Run("00000000-0000-4000-8000-000000000001".parse().unwrap()),
            &workspace,
            ".",
            ExecutionEnvironmentConfig::default(),
        )
        .unwrap(),
        RunnerIdentity {
            runner_id: RunnerId::new("review").unwrap(),
            version: version.clone(),
            boot_id: "boot".into(),
            binary_id: None,
        },
        RunnerOptions {
            floors: Some(Arc::new(RefuseFloors)),
            ..RunnerOptions::default()
        },
    )
    .unwrap();
    let (host, runner) = tokio::io::duplex(65536);
    runtime.spawn(async move {
        service.serve_connection(runner).await;
    });
    let (client, _) = runtime
        .block_on(RunnerClient::connect_home_local(
            host,
            Bind {
                host_id: "review".into(),
                host_instance: "instance".into(),
                version,
                inspect: false,
            },
            ClientWindows::default(),
            None,
        ))
        .unwrap();
    case.roots = case.roots.with_runner_client(client.clone());
    for _ in 0..2 {
        case.ready();
        case.stop().unwrap();
        assert!(
            case.roots.runner_pending(),
            "failed settlement remained Ready"
        );
        assert!(case.registry.needs_connection("local"));
    }
    client.disconnect();
}
