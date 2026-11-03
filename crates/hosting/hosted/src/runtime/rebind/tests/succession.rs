//! Switch now: the root node continues on its own line as another profile
//! (spec 02 §5.5, spec 03 §1.2).

use super::*;

fn switch_to(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<TestExtension>,
    opened: &mut AttachedCodingSession,
    profile: &str,
    roster: &crate::orchestration::RosterInput,
) -> (CliResult<Rebound>, Vec<String>) {
    let (events, notes_rx) = host_event_sink();
    let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let state = RefCell::new(InterruptWatchState::default());
    let result = executor
        .block_on(handle_rebind(
            InterruptWatch::new(&receiver, &state),
            config,
            opened,
            &mut Vec::new(),
            &events,
            &roster.root.account,
            &roster.root.tuple_id,
            roster.root.effort.as_deref(),
            None,
            Some(ProductComposition::Direct),
            None,
            roster,
            Some(profile),
        ))
        .expect("the executor drives the call");
    let notes = notes_rx
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(row) => row.text().map(str::to_owned),
            _ => None,
        })
        .collect();
    (result, notes)
}

fn profile_roster(
    model: &str,
    effort: &str,
    transient_resumes: u8,
) -> crate::orchestration::RosterInput {
    let mut roster = crate::orchestration::RosterInput::unpinned(model);
    roster.root.account = "openai".to_owned();
    roster.root.effort = Some(effort.to_owned());
    roster.transient_resumes = transient_resumes;
    roster
}

/// A switch publishes the successor as the next head of the same line and
/// node, appends the profile's epoch to the tree from the bytes it recorded,
/// and the successor reopens exactly against that epoch. A second switch
/// whose roster differs only in the root binding writes no epoch: only the
/// tree's view of the root moves.
#[test]
fn switch_now_continues_the_root_node_and_appends_the_profile_epoch() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source_id = config.session.id.clone();
    let source_node = config.session.node.clone();
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let header = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .read(|tree| tree.current_epoch().clone())
        .unwrap();

    let lean = profile_roster(
        "gpt-5.6-sol",
        "medium",
        header.transient_resumes.wrapping_add(1),
    );
    let (switched, notes) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    assert_eq!(switched.unwrap(), Rebound::Succeeded);
    let successor_id = config.session.id.clone();
    assert_ne!(successor_id, source_id);
    assert_eq!(config.session.node, source_node, "the node continues");
    assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-sol"));
    assert_eq!(
        config.session.root_profile,
        Some(sessions::RootProfile::from_roster(&lean))
    );
    assert!(
        config
            .session
            .source_workflow_operation_id
            .as_deref()
            .is_some_and(|operation| operation.starts_with("plak:switch:")),
        "{:?}",
        config.session.source_workflow_operation_id
    );
    assert_eq!(
        durable_tree_id(&config.topology_rows(), &config.session),
        tree_id
    );
    let frozen = crate::orchestration::OrchestrationSnapshot::freeze(lean.clone());
    let (current, epoch, pending) = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .read(|tree| {
            (
                tree.epochs().current,
                tree.current_epoch().clone(),
                tree.epoch_pending().unwrap(),
            )
        })
        .unwrap();
    assert_eq!((current, &epoch), (1, &frozen));
    assert_eq!(pending, Vec::new(), "the settle removed the row");
    assert_eq!(
        retained_api_parent(&tree_id).model,
        "gpt-5.6-sol",
        "the hub rejoins children under the successor"
    );
    assert!(
        notes
            .iter()
            .any(|note| note == "switched to profile lean; 0 agents and 0 processes kept running"),
        "{notes:?}"
    );

    // The successor compiled against the epoch the tree now holds.
    let mut opened = reopen_exactly(&executor, &mut config, opened);

    // Only the root binding differs: no epoch, the root view moves.
    let mut rebound = lean.clone();
    rebound.root.tuple_id = "gpt-5.6-luna".to_owned();
    rebound.root.effort = Some("low".to_owned());
    let (switched, _) = switch_to(&executor, &mut config, &mut opened, "lean", &rebound);
    assert_eq!(switched.unwrap(), Rebound::Succeeded);
    let (current, root_tuple) = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .read(|tree| {
            (
                tree.epochs().current,
                tree.node(&tree.root_agent_id).unwrap().tuple_id.clone(),
            )
        })
        .unwrap();
    assert_eq!(current, 1);
    assert_eq!(root_tuple, "gpt-5.6-luna");
    assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-luna"));
    assert_eq!(retained_api_parent(&tree_id).model, "gpt-5.6-luna");
    // A crash before that view moved leaves the predecessor's: the
    // successor's open moves it.
    executor
        .block_on(
            crate::topology_runtime::lookup(&tree_id)
                .unwrap()
                .mutate_then_emit(false, |tree| {
                    tree.record_root_bind("openai", "gpt-5.6-sol", Some("medium"));
                    Ok::<_, String>(())
                }),
        )
        .expect("the executor drives the call")
        .unwrap();
    executor
        .block_on(settle_switch_epoch_on_open(
            &config.topology_rows(),
            &tree_id,
            &config.session,
        ))
        .unwrap()
        .unwrap();
    let (root_tuple, root_effort) = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .read(|tree| {
            let root = tree.node(&tree.root_agent_id).unwrap();
            (root.tuple_id.clone(), root.effort.clone())
        })
        .unwrap();
    assert_eq!(
        (root_tuple.as_str(), root_effort.as_deref()),
        ("gpt-5.6-luna", Some("low"))
    );
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A switch with queued prompts is refused before any intent: no request,
/// no pending epoch, and the source stays attached.
#[test]
fn a_switch_with_queued_prompts_records_nothing() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source_id = config.session.id.clone();
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    config
        .session
        .pending_queue
        .push(sessions::QueueItem::new("queued"));
    let roster = profile_roster("gpt-5.6-sol", "medium", 3);
    let (refused, _) = switch_to(&executor, &mut config, &mut opened, "lean", &roster);
    assert!(refused.is_err());
    config.session.pending_queue.clear();
    assert_eq!(config.session.id, source_id);
    let pending = crate::topology_runtime::lookup(&tree_id)
        .unwrap()
        .read(|tree| (tree.epochs().current, tree.epoch_pending().unwrap()))
        .unwrap();
    assert_eq!(pending, (0, Vec::new()));
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A running child keeps running across a switch: the predecessor's
/// hand-off release leaves the tree in custody with the same hub, the child
/// keeps the epoch and card it was spawned under, and the notice counts it.
#[test]
fn a_switch_keeps_a_running_child_on_its_own_epoch() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
    let child = executor
        .block_on(hub.mutate_then_emit(true, |tree| {
            tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(
                crate::orchestration::RosterInput::test_working_set("gpt-5.6-luna"),
            ));
            let root = tree.root_agent_id.clone();
            tree.spawn_agent(
                &root,
                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .map(|spawned| spawned.agent_id)
            .map_err(|error| error.message)
        }))
        .expect("the executor drives the call")
        .unwrap();
    let card = hub
        .read(|tree| tree.epoch_of(&child).cloned())
        .unwrap()
        .unwrap();

    let lean = profile_roster("gpt-5.6-sol", "medium", 2);
    let (switched, notes) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    assert_eq!(switched.unwrap(), Rebound::Succeeded);
    assert!(
        notes
            .iter()
            .any(|note| note == "switched to profile lean; 1 agent and 0 processes kept running"),
        "{notes:?}"
    );
    let still = crate::topology_runtime::lookup(&tree_id).unwrap();
    assert!(Arc::ptr_eq(&still, &hub), "the switch replaced the hub");
    let (current, node, epoch) = still
        .read(|tree| {
            (
                tree.epochs().current,
                tree.node(&child).unwrap().clone(),
                tree.epoch_of(&child).cloned(),
            )
        })
        .unwrap();
    assert_eq!(current, 1);
    assert_eq!(node.epoch_no, 0);
    assert!(!node.status.is_finished(), "{:?}", node.status);
    assert_eq!(epoch, Some(card), "the child keeps its card");
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// The production factory, except that the first `failures` successor
/// attaches of a rebind fail.
struct FlakyAttach {
    inner: Arc<dyn RuntimeFactory<TestExtension>>,
    failures: std::sync::atomic::AtomicUsize,
    attempts: std::sync::atomic::AtomicUsize,
}

impl factory::sealed::Sealed for FlakyAttach {}

impl RuntimeFactory<TestExtension> for FlakyAttach {
    fn runtime<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        self.inner.runtime(config, role, stderr)
    }
    fn open_head<'a>(
        &'a self,
        config: &'a mut HostConfig<TestExtension>,
        head: plakous::sessions::SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure>>
    {
        self.inner.open_head(config, head, stderr)
    }
    fn verify_head<'a>(
        &'a self,
        config: &'a mut HostConfig<TestExtension>,
        head: plakous::sessions::SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<(), crate::agent::CodingSessionOpenFailure>> {
        self.inner.verify_head(config, head, stderr)
    }
    fn create<'a>(
        &'a self,
        config: &'a mut HostConfig<TestExtension>,
        seed: Option<crate::agent::SeededHistory>,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<AttachedCodingSession>> {
        self.inner.create(config, seed, stderr)
    }
    fn open<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        record: &'a SessionRecord,
        head: plakous::sessions::SessionLocator,
        role: RuntimeRole,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure>>
    {
        self.inner.open(config, record, head, role, stderr)
    }
    fn open_rebind_target<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        record: &'a SessionRecord,
        spec: Option<&'a ProductAgentSpec>,
        locator: plakous::sessions::SessionLocator,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, Result<AttachedCodingSession, crate::agent::CodingSessionOpenFailure>>
    {
        use std::sync::atomic::Ordering;
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let failing = self
            .failures
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok();
        if failing {
            return Box::pin(std::future::ready(Err(
                crate::agent::CodingSessionOpenFailure::from(CliFailure::new(
                    "the successor's catalog is unavailable",
                )),
            )));
        }
        self.inner
            .open_rebind_target(config, record, spec, locator, stderr)
    }
    fn fork_runtime<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        tree_id: &'a str,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        self.inner.fork_runtime(config, tree_id, stderr)
    }
    fn rebind_runtime<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        spec: Option<&'a ProductAgentSpec>,
        attach: crate::topology_runtime::TopologyHubAttach,
        native_target: bool,
        stderr: &'a mut Vec<u8>,
    ) -> RuntimeFuture<'a, (CliResult<SessionRuntimeBinding>, Option<String>)> {
        self.inner
            .rebind_runtime(config, spec, attach, native_target, stderr)
    }
    fn input_window(
        &self,
        config: &HostConfig<TestExtension>,
    ) -> CliResult<plakous::sessions::SessionInputWindow> {
        self.inner.input_window(config)
    }
    fn clear<'a>(
        &'a self,
        config: &'a mut HostConfig<TestExtension>,
        opened: &'a AttachedCodingSession,
        target_product_session_id: &'a str,
        create_operation_id: &'a str,
        stderr_buf: &'a mut Vec<u8>,
        events: &'a Sender,
        started: Instant,
    ) -> RuntimeFuture<'a, CliResult<(AttachedCodingSession, Option<Arc<ResolvedProvider>>)>> {
        self.inner.clear(
            config,
            opened,
            target_product_session_id,
            create_operation_id,
            stderr_buf,
            events,
            started,
        )
    }
    fn api_runtime<'a>(
        &'a self,
        config: &'a HostConfig<TestExtension>,
        record: &'a SessionRecord,
        opening_durable_session: bool,
        topology_attach: crate::topology_runtime::TopologyHubAttach,
        tree_id: Option<&'a str>,
        attached_tree: &'a mut Option<String>,
    ) -> RuntimeFuture<'a, CliResult<SessionRuntimeBinding>> {
        self.inner.api_runtime(
            config,
            record,
            opening_durable_session,
            topology_attach,
            tree_id,
            attached_tree,
        )
    }
    fn configure(&self, config: &mut HostConfig<TestExtension>) {
        self.inner.configure(config);
    }
}

fn flaky_attach(config: &mut HostConfig<TestExtension>, failures: usize) -> Arc<FlakyAttach> {
    let flaky = Arc::new(FlakyAttach {
        inner: Arc::clone(&config.runtime_factory),
        failures: std::sync::atomic::AtomicUsize::new(failures),
        attempts: std::sync::atomic::AtomicUsize::new(0),
    });
    config.runtime_factory = flaky.clone();
    flaky
}

/// A switch's publication is final, so a successor attach that fails once
/// is tried again in process and the switch completes.
#[test]
fn a_switch_retries_a_failed_successor_attach_once() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let flaky = flaky_attach(&mut config, 1);
    let lean = profile_roster("gpt-5.6-sol", "medium", 2);
    let (switched, _) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    assert_eq!(switched.unwrap(), Rebound::Succeeded);
    assert_eq!(flaky.attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-sol"));
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A live child turn that ends only when the hub releases it.
struct ParkedChild(Arc<AtomicBool>);

impl crate::topology_runtime::LiveChildHandle for ParkedChild {
    fn agent(&self) -> Option<Agent> {
        None
    }
    fn turn_id(&self) -> plakous::model::TurnId {
        "00000000-0000-0000-0000-000000000001".parse().unwrap()
    }
    fn turn_control(&self) -> TurnControl {
        panic!("unused")
    }
    fn follow_up(
        &self,
        _: CommandId,
        _: String,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), String>> + Send>> {
        Box::pin(async { Ok(()) })
    }
    fn follow_up_barrier(&self) -> Option<Arc<tokio::sync::RwLock<()>>> {
        None
    }
    fn restart(
        &mut self,
        _: crate::agent_tree_tools::TreeDriver,
        _: CommandId,
        _: String,
        _delivery: Option<tokio::sync::OwnedRwLockWriteGuard<()>>,
    ) -> std::pin::Pin<
        Box<dyn Future<Output = Result<plakous::agent::TurnResultHandle, String>> + Send + '_>,
    > {
        Box::pin(async { Err("unused".into()) })
    }
    fn release(
        self: Box<Self>,
        driver: crate::agent_tree_tools::TreeDriver,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        self.cancel_and_release(driver, None)
    }
    fn cancel_and_release(
        self: Box<Self>,
        _driver: crate::agent_tree_tools::TreeDriver,
        dispatched: Option<tokio::sync::oneshot::Sender<()>>,
    ) -> std::pin::Pin<Box<dyn Future<Output = crate::topology_runtime::LiveRelease> + Send>> {
        if let Some(dispatched) = dispatched {
            let _ = dispatched.send(());
        }
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { crate::topology_runtime::LiveRelease::default() })
    }
}

/// A published switch whose successor does not attach on its retry either
/// hands its tree to custody: the running child keeps running, and the
/// host's quit drain ends the tree.
#[test]
fn a_switch_that_cannot_attach_hands_its_tree_to_the_quit_drain() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
    let child = executor
        .block_on(hub.mutate_then_emit(true, |tree| {
            tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(
                crate::orchestration::RosterInput::test_working_set("gpt-5.6-luna"),
            ));
            let root = tree.root_agent_id.clone();
            tree.spawn_agent(
                &root,
                &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                None,
            )
            .map(|spawned| spawned.agent_id)
            .map_err(|error| error.message)
        }))
        .expect("the executor drives the call")
        .unwrap();
    let released = Arc::new(AtomicBool::new(false));
    assert!(
        executor
            .block_on(hub.try_occupy_live(child.clone(), Box::new(ParkedChild(released.clone()))))
            .expect("the executor drives the call")
            .is_ok(),
        "the child's turn is live"
    );
    let flaky = flaky_attach(&mut config, 2);
    let lean = profile_roster("gpt-5.6-sol", "medium", 2);
    let (switched, _) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    let error = switched.unwrap_err();
    assert!(
        error
            .message()
            .contains("the profile switch was published but its session could not attach"),
        "{}",
        error.message()
    );
    assert_eq!(flaky.attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
    let still = crate::topology_runtime::lookup(&tree_id).expect("the tree stays in custody");
    assert!(Arc::ptr_eq(&still, &hub));
    assert!(
        !released.load(std::sync::atomic::Ordering::SeqCst),
        "the child's turn keeps running"
    );

    executor
        .block_on(crate::topology_runtime::shut_down_handed_off_trees())
        .unwrap()
        .unwrap();
    assert!(
        crate::topology_runtime::lookup(&tree_id).is_none(),
        "the quit drain ended the handed-off tree"
    );
    assert!(released.load(std::sync::atomic::Ordering::SeqCst));
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A switch is not refused for a running process: the head-advancing rebind
/// hands the process and its job row to the successor in the publishing
/// transaction, and the notice counts it.
#[test]
fn a_switch_hands_a_running_process_to_the_successor() {
    use plakous::tools::{
        JobStatus, JobStopReason, JoinedProcessCommand, JoinedProcessStartRequest,
        JoinedProcessTerminalMode, OperationContext,
    };
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source = opened.locator();
    let service = config
        .state_roots
        .job_registry()
        .service_of_head(source.session_id(), source.branch_id())
        .expect("the root's line has a process service");
    let local = service.as_local().unwrap();
    let id = local
        .start(
            JoinedProcessStartRequest::new(
                JoinedProcessCommand::literal(
                    "/bin/sh",
                    ["-c".to_owned(), "exec sleep 600".to_owned()],
                )
                .unwrap(),
                ".",
                Vec::new(),
                false,
                JoinedProcessTerminalMode::Pipes,
            )
            .unwrap(),
            OperationContext::new(plakous::model::CancellationToken::new()),
        )
        .unwrap();
    plakous_tools::internal::promote(local, id).unwrap();

    let lean = profile_roster("gpt-5.6-sol", "medium", 2);
    let (switched, notes) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    assert_eq!(switched.unwrap(), Rebound::Succeeded);
    assert!(
        notes
            .iter()
            .any(|note| note == "switched to profile lean; 0 agents and 1 process kept running"),
        "{notes:?}"
    );
    let successor = opened.locator();
    assert_ne!(successor, source);
    let moved = config
        .state_roots
        .job_registry()
        .service_of_head(successor.session_id(), successor.branch_id())
        .expect("the line's service moved to the successor");
    let jobs = moved.as_local().unwrap().jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, JobStatus::Running);
    assert_eq!(
        opened
            .agent()
            .unwrap()
            .jobs()
            .iter()
            .filter(|job| job.status == JobStatus::Running)
            .count(),
        1,
        "the successor's job row is the moved job"
    );
    moved
        .as_local()
        .unwrap()
        .stop_jobs(JobStopReason::User, Duration::from_millis(200))
        .unwrap();
    executor.block_on(opened.release()).unwrap().unwrap();
}

/// A switch past the tree's epoch bound (eight referenced epochs) is
/// refused before its publication: no pending row, no epoch, and the source
/// stays the attached head.
#[test]
fn a_switch_past_the_epoch_bound_publishes_nothing() {
    let root = tempfile::tempdir().unwrap();
    let executor = ApplicationExecutor::new().unwrap();
    let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
    let source_id = config.session.id.clone();
    let source = opened.locator();
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
    // A closed child on each of epochs 0..=7 keeps all eight referenced.
    executor
        .block_on(hub.mutate_then_emit(true, |tree| {
            tree.install_snapshot(crate::orchestration::OrchestrationSnapshot::freeze(
                crate::orchestration::RosterInput::test_working_set("gpt-5.6-luna"),
            ));
            let root = tree.root_agent_id.clone();
            for epoch in 0..8_u64 {
                let child = tree
                    .spawn_agent(
                        &root,
                        &crate::agent_tree_protocol::SpawnAgentRequest::test_review(),
                        None,
                    )
                    .map_err(|error| error.message)?
                    .agent_id;
                tree.set_node_status_for_test(
                    &child,
                    crate::agent_tree_protocol::NodeStatus::Closed,
                );
                if epoch < 7 {
                    let mut next = tree.current_epoch().clone();
                    next.root_tuple = format!("tuple-{epoch}");
                    next.digest = next.compute_digest();
                    tree.append_epoch(next).map_err(|error| error.message)?;
                }
            }
            Ok::<_, String>(())
        }))
        .expect("the executor drives the call")
        .unwrap();

    let lean = profile_roster("gpt-5.6-sol", "medium", 2);
    let (switched, _) = switch_to(&executor, &mut config, &mut opened, "lean", &lean);
    let error = switched.unwrap_err();
    assert!(
        error.message().contains("until retention removes it"),
        "{}",
        error.message()
    );
    let (current, pending) = hub
        .read(|tree| (tree.epochs().current, tree.epoch_pending().unwrap()))
        .unwrap();
    assert_eq!(current, 7);
    assert_eq!(pending, Vec::new());
    assert_eq!(config.session.id, source_id, "no successor was published");
    assert_eq!(opened.locator(), source);
    assert!(opened.agent().is_some(), "the source stays attached");
    executor.block_on(opened.release()).unwrap().unwrap();
}
