//! Deterministic concurrency, supervision, and cleanup conformance.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use plakous_agent::internal::{
    AuthorizedToolInvocation, EffectAuthorization, ToolExecutionControl, ToolRuntime,
    ToolRuntimeConfig, ToolScheduler, ToolTask, ToolTaskDrain, ToolTaskFuture, ToolTaskHandle,
    ToolTaskJoin, ToolTaskOwner, ToolTaskOwnerStatus, ToolTaskStatus, ToolTimer, ToolTimerFuture,
};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, FixedClock, RunId, Timestamp, ToolConcurrency,
    ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    ToolResourceKey, TurnId,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope,
    FrozenToolRegistry, ProgressSink, ReadOnlyToolPolicy, RegisteredToolId, ToolContext,
    ToolExecutor, ToolExecutorId, ToolFuture, ToolInvocation, ToolInvocationRequest, ToolOutcome,
    ToolRegistration, ToolRegistryBuilder, ToolRuntimeError, ToolRuntimeErrorKind,
};
use serde_json::json;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const THREAD_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn test_block_on<F: Future>(future: F) -> F::Output {
    match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime.block_on(future),
        Err(error) => panic!("test Tokio runtime failed: {error}"),
    }
}

#[derive(Default)]
struct Gate {
    open: AtomicBool,
    wakers: Mutex<Vec<Waker>>,
}

impl Gate {
    fn open(&self) {
        self.open.store(true, Ordering::Release);
        for waker in take_lock(&self.wakers).drain(..) {
            waker.wake();
        }
    }

    async fn wait(&self) {
        std::future::poll_fn(|context| {
            if self.open.load(Ordering::Acquire) {
                Poll::Ready(())
            } else {
                let mut wakers = take_lock(&self.wakers);
                if self.open.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    wakers.push(context.waker().clone());
                    Poll::Pending
                }
            }
        })
        .await;
    }
}

struct ProbeTool {
    definition: ToolDefinition,
    gate: Arc<Gate>,
    started: mpsc::Sender<CallId>,
    finished: mpsc::Sender<CallId>,
    cancellation_aware: bool,
}

impl ToolExecutor for ProbeTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let call = invocation.call_id();
            let _ = self.started.send(call);
            let result = if self.cancellation_aware {
                match futures::future::select(
                    Box::pin(self.gate.wait()),
                    Box::pin(context.cancellation().cancelled()),
                )
                .await
                {
                    futures::future::Either::Left(((), _)) => ToolOutcome::success_text("done"),
                    futures::future::Either::Right(((), _)) => Err(ToolRuntimeError::new(
                        ToolRuntimeErrorKind::Cancelled,
                        "probe cancelled",
                    )),
                }
            } else {
                self.gate.wait().await;
                ToolOutcome::success_text("done")
            };
            let _ = self.finished.send(call);
            result
        })
    }
}

#[derive(Clone, Copy)]
enum TerminalAction {
    ReturnFailure,
    Panic,
    Succeed,
}

struct TerminalProbeTool {
    definition: ToolDefinition,
    gate: Arc<Gate>,
    started: mpsc::Sender<CallId>,
    actions: Mutex<VecDeque<TerminalAction>>,
    active: Arc<AtomicUsize>,
    overlap_detected: AtomicBool,
}

impl ToolExecutor for TerminalProbeTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        Box::pin(async move {
            let _active = ActiveExecutorCall::enter(&self.active, &self.overlap_detected);
            let action = take_lock(&self.actions).pop_front().ok_or_else(|| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "terminal probe action was exhausted",
                )
            })?;
            let _ = self.started.send(invocation.call_id());
            self.gate.wait().await;
            match action {
                TerminalAction::ReturnFailure => Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "scripted executor failure",
                )),
                TerminalAction::Panic => panic!("scripted executor panic"),
                TerminalAction::Succeed => ToolOutcome::success_text("later work"),
            }
        })
    }
}

struct ActiveExecutorCall {
    active: Arc<AtomicUsize>,
}

impl ActiveExecutorCall {
    fn enter(active: &Arc<AtomicUsize>, overlap_detected: &AtomicBool) -> Self {
        if active.fetch_add(1, Ordering::AcqRel) != 0 {
            overlap_detected.store(true, Ordering::Release);
        }
        Self {
            active: Arc::clone(active),
        }
    }
}

impl Drop for ActiveExecutorCall {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Debug, Default)]
struct ThreadTaskOwner {
    inner: Arc<ThreadTaskOwnerInner>,
}

#[derive(Debug, Default)]
struct ThreadTaskOwnerInner {
    tasks: Mutex<Vec<std::sync::Weak<ThreadTask>>>,
    drain_waker: Mutex<Option<Waker>>,
    closed: AtomicBool,
}

#[derive(Debug)]
struct ThreadTask {
    result: Mutex<Option<Result<ToolOutcome, ToolRuntimeError>>>,
    join_waker: Mutex<Option<Waker>>,
    abort_requested: AtomicBool,
    completed: AtomicBool,
    owner: std::sync::Weak<ThreadTaskOwnerInner>,
}

impl ThreadTask {
    fn complete(&self, result: Result<ToolOutcome, ToolRuntimeError>) {
        *take_lock(&self.result) = Some(result);
        self.completed.store(true, Ordering::Release);
        if let Some(waker) = take_lock(&self.join_waker).take() {
            waker.wake();
        }
        if let Some(owner) = self.owner.upgrade()
            && let Some(waker) = take_lock(&owner.drain_waker).take()
        {
            waker.wake();
        }
    }
}

impl ToolTask for ThreadTask {
    fn join(&self) -> ToolTaskJoin {
        let owner = self.owner.upgrade();
        let task = owner.and_then(|owner| {
            take_lock(&owner.tasks)
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .find(|candidate| std::ptr::eq(Arc::as_ptr(candidate), self))
        });
        let Some(task) = task else {
            return Box::pin(async {
                Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "test task handle lost ownership",
                ))
            });
        };
        Box::pin(ThreadTaskJoin { task })
    }

    fn request_abort(&self) {
        self.abort_requested.store(true, Ordering::Release);
    }

    fn status(&self) -> ToolTaskStatus {
        if self.completed.load(Ordering::Acquire) {
            ToolTaskStatus::Completed
        } else if self.abort_requested.load(Ordering::Acquire) {
            ToolTaskStatus::AbortRequested
        } else {
            ToolTaskStatus::Running
        }
    }
}

struct ThreadTaskJoin {
    task: Arc<ThreadTask>,
}

impl Future for ThreadTaskJoin {
    type Output = Result<ToolOutcome, ToolRuntimeError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = take_lock(&self.task.result).clone() {
            Poll::Ready(result)
        } else {
            *take_lock(&self.task.join_waker) = Some(context.waker().clone());
            take_lock(&self.task.result)
                .clone()
                .map_or(Poll::Pending, Poll::Ready)
        }
    }
}

struct ThreadTaskDrain {
    owner: Arc<ThreadTaskOwnerInner>,
}

impl Future for ThreadTaskDrain {
    type Output = ToolTaskOwnerStatus;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let status = thread_owner_status(&self.owner);
        if status.is_drained() {
            Poll::Ready(status)
        } else {
            *take_lock(&self.owner.drain_waker) = Some(context.waker().clone());
            Poll::Pending
        }
    }
}

fn thread_owner_status(owner: &ThreadTaskOwnerInner) -> ToolTaskOwnerStatus {
    let mut active = 0;
    let mut abort_requested = 0;
    take_lock(&owner.tasks).retain(|task| {
        let Some(task) = task.upgrade() else {
            return false;
        };
        match task.status() {
            ToolTaskStatus::Running => active += 1,
            ToolTaskStatus::AbortRequested => {
                active += 1;
                abort_requested += 1;
            }
            _ => {}
        }
        !task.completed.load(Ordering::Acquire)
    });
    ToolTaskOwnerStatus::new(active, abort_requested)
        .with_closed(owner.closed.load(Ordering::Acquire))
}

impl ToolTaskOwner for ThreadTaskOwner {
    fn spawn(&self, future: ToolTaskFuture) -> Result<ToolTaskHandle, ToolRuntimeError> {
        let task = Arc::new(ThreadTask {
            result: Mutex::new(None),
            join_waker: Mutex::new(None),
            abort_requested: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            owner: Arc::downgrade(&self.inner),
        });
        {
            let mut tasks = take_lock(&self.inner.tasks);
            if self.inner.closed.load(Ordering::Acquire) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "test task owner is closed",
                ));
            }
            tasks.push(Arc::downgrade(&task));
        }
        let worker_task = Arc::clone(&task);
        thread::Builder::new()
            .name("plakous-tool-test".to_owned())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| futures::executor::block_on(future)))
                    .unwrap_or_else(|_| {
                        Err(ToolRuntimeError::new(
                            ToolRuntimeErrorKind::Execution,
                            "test task panicked",
                        ))
                    });
                worker_task.complete(result);
            })
            .map_err(|_| {
                ToolRuntimeError::new(
                    ToolRuntimeErrorKind::Execution,
                    "test task owner could not start its worker",
                )
            })?;
        Ok(task)
    }

    fn status(&self) -> ToolTaskOwnerStatus {
        thread_owner_status(&self.inner)
    }

    fn request_abort_all(&self) -> usize {
        let tasks = take_lock(&self.inner.tasks)
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .filter(|task| task.status() != ToolTaskStatus::Completed)
            .collect::<Vec<_>>();
        for task in &tasks {
            task.request_abort();
        }
        tasks.len()
    }

    fn drain(&self) -> ToolTaskDrain {
        Box::pin(ThreadTaskDrain {
            owner: Arc::clone(&self.inner),
        })
    }

    fn shutdown(&self) -> ToolTaskDrain {
        let tasks = {
            let tasks = take_lock(&self.inner.tasks);
            self.inner.closed.store(true, Ordering::Release);
            tasks
                .iter()
                .filter_map(std::sync::Weak::upgrade)
                .filter(|task| task.status() != ToolTaskStatus::Completed)
                .collect::<Vec<_>>()
        };
        for task in tasks {
            task.request_abort();
        }
        self.drain()
    }
}

#[derive(Clone, Copy, Debug)]
struct PendingTimer;

impl ToolTimer for PendingTimer {
    fn sleep_until(&self, _deadline: Instant) -> ToolTimerFuture {
        Box::pin(std::future::pending())
    }
}

fn definition(name: &str, concurrency: ToolConcurrency) -> TestResult<ToolDefinition> {
    Ok(ToolDefinition::new(
        ToolName::new(name)?,
        "1.0.0",
        "scheduler probe",
        ToolInputContract::structured(
            json!({"$schema": "https://json-schema.org/draft/2020-12/schema", "type": "object", "properties": {}, "required": [], "additionalProperties": false}),
        )?,
        ToolOutputContract::portable_model_content(),
        concurrency,
        ToolEffect::ReadOnly,
    )?)
}

trait TestToolDefinition {
    fn registered_definition(&self) -> ToolDefinition;
}

impl TestToolDefinition for ProbeTool {
    fn registered_definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
}

impl TestToolDefinition for TerminalProbeTool {
    fn registered_definition(&self) -> ToolDefinition {
        self.definition.clone()
    }
}

fn registry<T: TestToolDefinition + ToolExecutor>(
    tools: Vec<Arc<T>>,
) -> TestResult<FrozenToolRegistry> {
    let mut builder = ToolRegistryBuilder::default();
    for tool in tools {
        let definition = tool.registered_definition();
        let name = definition.name().as_str().to_owned();
        let executor: Arc<dyn ToolExecutor> = tool;
        builder.register(ToolRegistration::new(
            RegisteredToolId::new(format!("test.{name}"))?,
            ToolExecutorId::new(format!("test.{name}.v1"))?,
            plakous_tools::ToolExecutorBindingIdentity::new(
                1,
                &[format!("test.{name}.v1").as_bytes()],
            )?,
            definition,
            executor,
        ))?;
    }
    Ok(builder.freeze())
}

struct Harness {
    runtime: ToolRuntime,
    run: RunId,
    turn: TurnId,
    attempt: AttemptId,
    clock: Arc<dyn Clock>,
    _workspace: tempfile::TempDir,
}

fn harness(
    registry: FrozenToolRegistry,
    scheduler: ToolScheduler,
    config: ToolRuntimeConfig,
    timer: Option<Arc<dyn ToolTimer>>,
) -> TestResult<Harness> {
    let run = RunId::from_str("00000000-0000-0000-0000-000000000101")?;
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000102")?;
    let attempt = AttemptId::from_str("00000000-0000-0000-0000-000000000103")?;
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let policy = Arc::new(ReadOnlyToolPolicy);
    let timer = timer.unwrap_or_else(|| Arc::new(PendingTimer));
    let runtime = ToolRuntime::with_scheduler_timer_and_tasks(
        registry,
        environment,
        policy,
        scheduler,
        config,
        timer,
        Arc::new(ThreadTaskOwner::default()),
    );
    Ok(Harness {
        runtime,
        run,
        turn,
        attempt,
        clock: Arc::new(FixedClock::new(Timestamp::new(1_000, 0)?)),
        _workspace: workspace,
    })
}

impl Harness {
    fn prepare(&self, name: &str, call_suffix: u8) -> TestResult<ToolInvocation> {
        let call = CallId::from_str(&format!(
            "00000000-0000-0000-0000-0000000001{call_suffix:02}"
        ))?;
        Ok(self.runtime.prepare(ToolInvocationRequest::new(
            self.run,
            self.turn,
            self.attempt,
            call,
            ToolName::new(name)?,
            ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
            "{}",
        ))?)
    }

    fn control(
        &self,
        call: CallId,
        cancellation: CancellationToken,
    ) -> TestResult<ToolExecutionControl> {
        let (progress, _receiver) = ProgressSink::channel(1, call, Arc::clone(&self.clock))?;
        Ok(ToolExecutionControl::new(
            cancellation,
            None,
            Arc::clone(&self.clock),
            progress,
        ))
    }

    fn authorize(&self, invocation: ToolInvocation) -> TestResult<AuthorizedToolInvocation> {
        match test_block_on(self.runtime.authorize(invocation))? {
            EffectAuthorization::Allowed(authorized) => Ok(authorized),
            _ => Err("probe was not authorized".into()),
        }
    }
}

fn spawn_execute(
    runtime: ToolRuntime,
    authorized: AuthorizedToolInvocation,
    control: ToolExecutionControl,
) -> ExecutionThread {
    let (completion, completed) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let result = catch_unwind(AssertUnwindSafe(|| {
            test_block_on(runtime.execute(authorized, control))
        }));
        let _ = completion.send(result);
    });
    ExecutionThread { completed }
}

struct ExecutionThread {
    completed: mpsc::Receiver<thread::Result<Result<ToolOutcome, ToolRuntimeError>>>,
}

impl ExecutionThread {
    fn wait(self, description: &str) -> TestResult<Result<ToolOutcome, ToolRuntimeError>> {
        match self.completed.recv_timeout(THREAD_EVENT_TIMEOUT) {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(format!("{description} thread panicked").into()),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
                "{description} did not complete within {THREAD_EVENT_TIMEOUT:?}"
            )
            .into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(format!("{description} completion channel disconnected").into())
            }
        }
    }
}

fn assert_constrained_reacquisition_after(
    first_action: TerminalAction,
    expected_error: &str,
) -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let tool = Arc::new(TerminalProbeTool {
        definition: definition(
            "terminal",
            ToolConcurrency::ResourceExclusive(vec![ToolResourceKey::new("resource:terminal")?]),
        )?,
        gate: Arc::clone(&gate),
        started: started_tx,
        actions: Mutex::new(VecDeque::from([first_action, TerminalAction::Succeed])),
        active: Arc::new(AtomicUsize::new(0)),
        overlap_detected: AtomicBool::new(false),
    });
    let scheduler = ToolScheduler::with_waiter_limit(1, 4)?;
    let harness = harness(
        registry(vec![Arc::clone(&tool)])?,
        scheduler.clone(),
        ToolRuntimeConfig::with_waiter_limit(1, 4)?,
        None,
    )?;

    let first = harness.prepare("terminal", 41)?;
    let first_call = first.call_id();
    let first_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(first)?,
        harness.control(first_call, CancellationToken::new())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, first_call);
    assert_eq!(tool.active.load(Ordering::Acquire), 1);
    assert_eq!(scheduler.status().active(), 1);
    assert_eq!(scheduler.status().active_resource_keys(), 1);

    let later = harness.prepare("terminal", 42)?;
    let later_call = later.call_id();
    let mut waiting = Box::pin(harness.runtime.execute(
        harness.authorize(later)?,
        harness.control(later_call, CancellationToken::new())?,
    ));
    let waker = futures::task::noop_waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(started_rx.try_recv().is_err());
    assert_eq!(tool.active.load(Ordering::Acquire), 1);
    assert!(!tool.overlap_detected.load(Ordering::Acquire));
    let constrained = scheduler.status();
    assert_eq!(constrained.active(), 1);
    assert_eq!(constrained.waiting(), 1);
    assert_eq!(constrained.active_resource_keys(), 1);
    assert_eq!(constrained.active_runtime_lanes(), 1);

    gate.open();
    let Err(error) = first_task.wait("first execution")? else {
        return Err("terminal probe unexpectedly produced a successful outcome".into());
    };
    assert_eq!(error.kind(), ToolRuntimeErrorKind::Execution);
    assert_eq!(error.to_string(), expected_error);

    let later_outcome = test_block_on(waiting)?;
    assert_eq!(later_outcome, ToolOutcome::success_text("later work")?);
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, later_call);
    assert_eq!(tool.active.load(Ordering::Acquire), 0);
    assert!(!tool.overlap_detected.load(Ordering::Acquire));
    let released = scheduler.status();
    assert_eq!(released.active(), 0);
    assert_eq!(released.waiting(), 0);
    assert_eq!(released.active_resource_keys(), 0);
    assert_eq!(released.active_runtime_lanes(), 0);
    Ok(())
}

#[test]
fn executor_returned_failure_releases_identical_constrained_permit_and_resource() -> TestResult {
    assert_constrained_reacquisition_after(
        TerminalAction::ReturnFailure,
        "scripted executor failure",
    )
}

#[test]
fn contained_executor_panic_releases_identical_constrained_permit_and_resource() -> TestResult {
    assert_constrained_reacquisition_after(TerminalAction::Panic, "test task panicked")
}

#[test]
fn parallel_calls_overlap_and_serial_excludes_parallel() -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, _finished_rx) = mpsc::channel();
    let parallel = Arc::new(ProbeTool {
        definition: definition("parallel", ToolConcurrency::Parallel)?,
        gate: Arc::clone(&gate),
        started: started_tx.clone(),
        finished: finished_tx.clone(),
        cancellation_aware: true,
    });
    let serial = Arc::new(ProbeTool {
        definition: definition("serial", ToolConcurrency::Serial)?,
        gate: Arc::clone(&gate),
        started: started_tx,
        finished: finished_tx,
        cancellation_aware: true,
    });
    let harness = harness(
        registry(vec![parallel, serial])?,
        ToolScheduler::with_waiter_limit(3, 8)?,
        ToolRuntimeConfig::with_waiter_limit(2, 8)?,
        None,
    )?;

    let first = harness.prepare("parallel", 1)?;
    let first_call = first.call_id();
    let first_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(first)?,
        harness.control(first_call, CancellationToken::new())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, first_call);

    let serial = harness.prepare("serial", 2)?;
    let serial_call = serial.call_id();
    let authorized = harness.authorize(serial)?;
    let control = harness.control(serial_call, CancellationToken::new())?;
    let mut waiting = Box::pin(harness.runtime.execute(authorized, control));
    let waker = futures::task::noop_waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(started_rx.try_recv().is_err());
    drop(waiting);

    let second = harness.prepare("parallel", 3)?;
    let second_call = second.call_id();
    let second_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(second)?,
        harness.control(second_call, CancellationToken::new())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, second_call);
    gate.open();
    first_task.wait("first execution")??;
    second_task.wait("second execution")??;
    Ok(())
}

#[test]
fn resource_keys_exclude_overlap_but_disjoint_keys_overlap() -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, _finished_rx) = mpsc::channel();
    let tool = |name: &str, keys: &[&str]| -> TestResult<Arc<ProbeTool>> {
        Ok(Arc::new(ProbeTool {
            definition: definition(
                name,
                ToolConcurrency::ResourceExclusive(
                    keys.iter()
                        .map(|key| ToolResourceKey::new(*key))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
            )?,
            gate: Arc::clone(&gate),
            started: started_tx.clone(),
            finished: finished_tx.clone(),
            cancellation_aware: true,
        }))
    };
    let harness = harness(
        registry(vec![
            tool("ab", &["resource:b", "resource:a"])?,
            tool("a", &["resource:a"])?,
            tool("c", &["resource:c"])?,
        ])?,
        ToolScheduler::with_waiter_limit(4, 8)?,
        ToolRuntimeConfig::with_waiter_limit(4, 8)?,
        None,
    )?;
    let first = harness.prepare("ab", 11)?;
    let first_call = first.call_id();
    let first_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(first)?,
        harness.control(first_call, CancellationToken::new())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, first_call);

    let overlapping = harness.prepare("a", 12)?;
    let overlap_call = overlapping.call_id();
    let mut overlap = Box::pin(harness.runtime.execute(
        harness.authorize(overlapping)?,
        harness.control(overlap_call, CancellationToken::new())?,
    ));
    let waker = futures::task::noop_waker();
    assert!(
        overlap
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(started_rx.try_recv().is_err());
    drop(overlap);

    let disjoint = harness.prepare("c", 13)?;
    let disjoint_call = disjoint.call_id();
    let disjoint_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(disjoint)?,
        harness.control(disjoint_call, CancellationToken::new())?,
    );
    assert_eq!(
        started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?,
        disjoint_call
    );
    gate.open();
    first_task.wait("first execution")??;
    disjoint_task.wait("disjoint execution")??;
    Ok(())
}

#[test]
fn dropping_outer_execution_cancels_but_supervisor_retains_work_and_permit() -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let tool = Arc::new(ProbeTool {
        definition: definition("probe", ToolConcurrency::Parallel)?,
        gate: Arc::clone(&gate),
        started: started_tx,
        finished: finished_tx,
        cancellation_aware: true,
    });
    let harness = harness(
        registry(vec![tool])?,
        ToolScheduler::with_waiter_limit(1, 4)?,
        ToolRuntimeConfig::with_waiter_limit(1, 4)?,
        None,
    )?;
    let first = harness.prepare("probe", 21)?;
    let first_call = first.call_id();
    let mut execution = Box::pin(harness.runtime.execute(
        harness.authorize(first)?,
        harness.control(first_call, CancellationToken::new())?,
    ));
    let waker = futures::task::noop_waker();
    assert!(
        execution
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, first_call);
    drop(execution);
    assert_eq!(finished_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, first_call);

    let second = harness.prepare("probe", 22)?;
    let second_call = second.call_id();
    let second_task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(second)?,
        harness.control(second_call, CancellationToken::new())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, second_call);
    gate.open();
    second_task.wait("second execution")??;
    Ok(())
}

#[derive(Default)]
struct ManualTimer {
    state: Mutex<ManualTimerState>,
    changed: Condvar,
}

#[derive(Default)]
struct ManualTimerState {
    sleeps: VecDeque<Arc<ManualSleepState>>,
    fire_new_sleeps: bool,
}

#[derive(Default)]
struct ManualSleepState {
    inner: Mutex<ManualSleepInner>,
}

#[derive(Default)]
struct ManualSleepInner {
    fired: bool,
    waker: Option<Waker>,
}

impl ManualSleepState {
    fn fire(&self) -> Option<Waker> {
        let mut inner = take_lock(&self.inner);
        inner.fired = true;
        inner.waker.take()
    }
}

impl ManualTimer {
    fn wait_for_sleeps(&self, count: usize) -> TestResult {
        let state = take_lock(&self.state);
        let (state, timeout) = self
            .changed
            .wait_timeout_while(state, THREAD_EVENT_TIMEOUT, |state| {
                state.sleeps.len() < count
            })
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if timeout.timed_out() && state.sleeps.len() < count {
            return Err(format!(
                "manual timer observed {} of {count} expected sleeps within {THREAD_EVENT_TIMEOUT:?}",
                state.sleeps.len()
            )
            .into());
        }
        Ok(())
    }

    fn fire_all(&self) {
        // A selected-away deadline future may be queued immediately before the
        // dispatch deadline is registered. Keep the timer fired so the later
        // registration cannot miss this deterministic deadline edge.
        let wakers = {
            let mut state = take_lock(&self.state);
            state.fire_new_sleeps = true;
            state
                .sleeps
                .drain(..)
                .filter_map(|sleep| sleep.fire())
                .collect::<Vec<_>>()
        };
        for waker in wakers {
            waker.wake();
        }
    }

    fn reset(&self) {
        take_lock(&self.state).fire_new_sleeps = false;
    }
}

impl ToolTimer for ManualTimer {
    fn sleep_until(&self, _deadline: Instant) -> ToolTimerFuture {
        let state = Arc::new(ManualSleepState::default());
        {
            let mut timer = take_lock(&self.state);
            if timer.fire_new_sleeps {
                drop(state.fire());
            }
            timer.sleeps.push_back(Arc::clone(&state));
        }
        self.changed.notify_all();
        Box::pin(std::future::poll_fn(move |context| {
            let mut inner = take_lock(&state.inner);
            if inner.fired {
                Poll::Ready(())
            } else {
                inner.waker = Some(context.waker().clone());
                Poll::Pending
            }
        }))
    }
}

#[test]
fn execution_deadline_cancels_only_the_child_tool_context() -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, _finished_rx) = mpsc::channel();
    let tool = Arc::new(ProbeTool {
        definition: definition("deadline", ToolConcurrency::Parallel)?,
        gate,
        started: started_tx,
        finished: finished_tx,
        cancellation_aware: true,
    });
    let timer = Arc::new(ManualTimer::default());
    let timer_api: Arc<dyn ToolTimer> = timer.clone();
    let harness = harness(
        registry(vec![tool])?,
        ToolScheduler::with_waiter_limit(1, 4)?,
        ToolRuntimeConfig::with_waiter_limit(1, 4)?,
        Some(timer_api),
    )?;
    let invocation = harness.prepare("deadline", 30)?;
    let call = invocation.call_id();
    let parent = CancellationToken::new();
    let task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(invocation)?,
        harness.control(call, parent.clone())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, call);
    timer.wait_for_sleeps(1)?;
    timer.fire_all();
    let Err(error) = task.wait("deadline execution")? else {
        return Err("deadline unexpectedly produced a successful outcome".into());
    };
    assert_eq!(error.kind(), ToolRuntimeErrorKind::ExecutionTimeout);
    assert!(!parent.is_cancelled());
    Ok(())
}

#[test]
fn cleanup_timeout_is_typed_and_releases_only_after_executor_future_ends() -> TestResult {
    let gate = Arc::new(Gate::default());
    let (started_tx, started_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let tool = Arc::new(ProbeTool {
        definition: definition("cleanup", ToolConcurrency::Parallel)?,
        gate: Arc::clone(&gate),
        started: started_tx,
        finished: finished_tx,
        cancellation_aware: false,
    });
    let timer = Arc::new(ManualTimer::default());
    let timer_api: Arc<dyn ToolTimer> = timer.clone();
    let harness = harness(
        registry(vec![tool])?,
        ToolScheduler::with_waiter_limit(1, 4)?,
        ToolRuntimeConfig::with_waiter_limit(1, 4)?,
        Some(timer_api),
    )?;
    let invocation = harness.prepare("cleanup", 31)?;
    let call = invocation.call_id();
    let cancellation = CancellationToken::new();
    let task = spawn_execute(
        harness.runtime.clone(),
        harness.authorize(invocation)?,
        harness.control(call, cancellation.clone())?,
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, call);
    timer.wait_for_sleeps(1)?;
    cancellation.cancel();
    timer.wait_for_sleeps(2)?;
    timer.fire_all();
    let Err(error) = task.wait("cleanup execution")? else {
        return Err("cleanup unexpectedly completed before its deadline".into());
    };
    assert_eq!(error.kind(), ToolRuntimeErrorKind::CleanupTimeout);
    assert!(finished_rx.try_recv().is_err());

    // The first forced deadline must not leak into the independent follow-up
    // dispatch. That dispatch is used to prove the released permit can be
    // acquired and that dropping its caller still leaves cleanup supervised.
    timer.reset();

    let blocked = harness.prepare("cleanup", 32)?;
    let blocked_call = blocked.call_id();
    let mut blocked_execution = Box::pin(harness.runtime.execute(
        harness.authorize(blocked)?,
        harness.control(blocked_call, CancellationToken::new())?,
    ));
    let waker = futures::task::noop_waker();
    assert!(
        blocked_execution
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(started_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?, blocked_call);
    drop(blocked_execution);

    gate.open();
    assert_eq!(
        finished_rx.recv_timeout(THREAD_EVENT_TIMEOUT)?,
        blocked_call
    );
    Ok(())
}

fn take_lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
