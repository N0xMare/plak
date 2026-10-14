use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use plakous_js_eval::NestedDeliveryOutcome;
use rquickjs::{
    CatchResultExt, Context, Ctx, Exception, Function, Persistent, Promise, Runtime,
    function::Func, promise::PromiseState,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch};

use crate::RuntimeEvent;

const BOOTSTRAP: &str = include_str!("bootstrap.js");

type SavedFunction = Persistent<Function<'static>>;

/// One evaluator thread with bounded commands, events, and joined termination.
pub struct EmbeddedHost {
    command_tx: std_mpsc::SyncSender<HostCommand>,
    events: tokio::sync::Mutex<mpsc::Receiver<RuntimeEvent>>,
    interrupted: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    loss: watch::Receiver<Option<String>>,
    worker: tokio::sync::Mutex<Option<thread::JoinHandle<()>>>,
}

enum HostCommand {
    Start(StartExecution),
    ToolResult {
        execution_id: u64,
        id: u64,
        value: Value,
        success: bool,
        /// A refusal or a proved non-delivery answers only this sender; a loss
        /// is terminal for the evaluator.
        accepted: oneshot::Sender<NestedDeliveryOutcome>,
    },
    Resume {
        execution_id: u64,
        yield_time_ms: u64,
    },
    Interrupt,
    Shutdown,
}

#[derive(serde::Serialize)]
struct StartExecution {
    execution_id: u64,
    source: String,
    tools: Vec<Value>,
    stored: HashMap<String, Value>,
    yield_time_ms: u64,
    #[cfg(test)]
    #[serde(skip)]
    before_start: Option<Arc<std::sync::Barrier>>,
}

enum ExecutionPhase {
    Running,
    /// Cell script finished but nested tool promises are still outstanding.
    Settling(ExecutionTerminal),
    Finished,
}

struct ExecutionState {
    execution_id: u64,
    event_tx: mpsc::Sender<RuntimeEvent>,
    phase: ExecutionPhase,
    pending_tools: HashMap<u64, (SavedFunction, SavedFunction)>,
    pending_yield: Option<(SavedFunction, SavedFunction)>,
    /// Cooperative yield-timeout park with no `yield_control()` Promise.
    auto_parked: bool,
    yield_deadline: Instant,
    pending_timeouts: HashMap<u32, PendingTimeout>,
    next_tool_id: u64,
    next_timeout_id: u32,
    generated_bytes: usize,
    generated_items: usize,
    limits: EvalLimits,
    control: WorkerControl,
}

#[derive(Clone, Copy)]
struct EvalLimits {
    bytes: usize,
    items: usize,
    pending: usize,
}
impl EvalLimits {
    const DEFAULT: Self = Self {
        bytes: 8 * 1024 * 1024,
        items: 4096,
        pending: 64,
    };
}

impl ExecutionState {
    fn charge(&mut self, bytes: usize) -> Result<(), String> {
        if self.control.loss.borrow().is_some()
            || bytes > self.limits.bytes.saturating_sub(self.generated_bytes)
            || self.generated_items >= self.limits.items
        {
            return Err(self.fail("generated output budget"));
        }
        self.generated_bytes += bytes;
        self.generated_items += 1;
        Ok(())
    }
    fn fail(&self, reason: &str) -> String {
        let message = format!("resource limit: QuickJS {reason}");
        self.control.loss.send_replace(Some(message.clone()));
        self.control.interrupted.store(true, Ordering::Release);
        message
    }
    fn emit(&self, event: RuntimeEvent) -> Result<(), String> {
        self.event_tx
            .try_send(event)
            .map_err(|_| self.fail("event queue"))
    }

    fn is_running(&self) -> bool {
        matches!(self.phase, ExecutionPhase::Running)
    }

    fn is_parked(&self) -> bool {
        self.pending_yield.is_some() || self.auto_parked
    }
}

struct PendingTimeout {
    callback: SavedFunction,
    deadline: Option<Instant>,
}

enum CommandPoll {
    Command(HostCommand),
    TimerReady,
    Disconnected,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ExecutionTerminal {
    Done {
        #[serde(default)]
        stored: HashMap<String, Value>,
    },
    Error {
        message: String,
        #[serde(default)]
        stored: HashMap<String, Value>,
    },
}

impl EmbeddedHost {
    /// Starts a fresh evaluator with the fixed host resource limits.
    ///
    /// # Errors
    /// Returns an error if the thread or `QuickJS` runtime cannot start.
    pub fn spawn() -> Result<Self, String> {
        Self::spawn_with_limits(EvalLimits::DEFAULT)
    }

    fn spawn_with_limits(limits: EvalLimits) -> Result<Self, String> {
        let (command_tx, command_rx) = std_mpsc::sync_channel(plakous_js_eval::COMMAND_QUEUE_ITEMS);
        let (event_tx, events) = mpsc::channel(256);
        let (loss_tx, loss) = watch::channel(None);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let interrupted = Arc::new(AtomicBool::new(false));
        let worker_interrupted = Arc::clone(&interrupted);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::Builder::new()
            .name("plakous-code-mode-quickjs".to_owned())
            .spawn(move || {
                run_worker(
                    &command_rx,
                    &event_tx,
                    &ready_tx,
                    &worker_interrupted,
                    limits,
                    &loss_tx,
                    &worker_stopped,
                );
                if loss_tx.borrow().is_none() {
                    loss_tx.send_replace(Some("embedded QuickJS host ended".into()));
                }
            })
            .map_err(|error| format!("failed to start embedded QuickJS code-mode host: {error}"))?;
        let ready = ready_rx
            .recv()
            .unwrap_or_else(|_| Err("embedded QuickJS code-mode host ended during startup".into()));
        if let Err(error) = ready {
            let _ = worker.join();
            return Err(error);
        }
        Ok(Self {
            command_tx,
            events: tokio::sync::Mutex::new(events),
            interrupted,
            stopped,
            loss,
            worker: tokio::sync::Mutex::new(Some(worker)),
        })
    }

    /// Queues a cell with its stored values and visible tool declarations.
    ///
    /// # Errors
    /// Refuses oversized input or an unavailable command queue.
    pub fn start_cell(
        &self,
        execution_id: u64,
        source: &str,
        stored: HashMap<String, Value>,
        tools: Vec<Value>,
        yield_time_ms: u64,
    ) -> Result<(), String> {
        if source.len() > EvalLimits::DEFAULT.bytes {
            return Err("resource limit: QuickJS source".into());
        }
        let start = StartExecution {
            execution_id,
            source: source.to_owned(),
            tools,
            stored,
            yield_time_ms,
            #[cfg(test)]
            before_start: None,
        };
        plakous_js_eval::encode_frame(&start)?;
        self.command_tx
            .try_send(HostCommand::Start(start))
            .map_err(|_| "resource limit: QuickJS command queue unavailable".into())
    }

    /// Waits for evaluator loss independently of the event queue.
    pub async fn wait_loss(&self) -> String {
        let mut loss = self.loss.clone();
        loop {
            if let Some(error) = loss.borrow().clone() {
                return error;
            }
            if loss.changed().await.is_err() {
                return "embedded QuickJS control closed".into();
            }
        }
    }

    /// Reads the next cell event, or the independently observed loss.
    ///
    /// # Errors
    /// Returns an error when the evaluator closes or loses its event stream.
    pub async fn read_event(&self) -> Result<RuntimeEvent, String> {
        let mut events = self.events.lock().await;
        tokio::select! {
            biased;
            event = events.recv() => event.ok_or_else(|| self.loss.borrow().clone().unwrap_or_else(|| "embedded QuickJS host ended".into())),
            loss = self.wait_loss() => Err(loss),
        }
    }

    /// Delivers a nested result and waits for evaluator acceptance, keeping an
    /// evaluator refusal distinct from evaluator loss.
    ///
    /// The outer `Err` is a genuine loss of the evaluator or of its
    /// acknowledgement. The inner `Err` is the evaluator refusing a result that
    /// names no pending request of the owning cell: it answers only this sender
    /// and leaves the cell undisturbed.
    ///
    /// # Errors
    /// Errors on an oversized result, an unavailable queue, or a lost
    /// evaluator acknowledgement.
    pub async fn deliver_tool_result(
        &self,
        execution_id: u64,
        id: u64,
        value: Value,
        success: bool,
    ) -> NestedDeliveryOutcome {
        // Both refusals below happen before anything is queued, so the
        // evaluator never learns of the result and cannot have acted on it.
        if let Err(error) = plakous_js_eval::encode_frame(&value) {
            return NestedDeliveryOutcome::NotDelivered(error);
        }
        let (accepted, acknowledgement) = oneshot::channel();
        if self
            .command_tx
            .try_send(HostCommand::ToolResult {
                execution_id,
                id,
                value,
                success,
                accepted,
            })
            // Above `COMMAND_QUEUE_ITEMS` outstanding commands this is a
            // genuine resource limit, not a burst the profile admits: the
            // queue holds the whole nested-call budget and the control
            // commands behind it.
            .is_err()
        {
            return NestedDeliveryOutcome::NotDelivered(
                "resource limit: embedded QuickJS command queue unavailable".to_owned(),
            );
        }
        tokio::select! {
            biased;
            result = acknowledgement => result.unwrap_or_else(|_| NestedDeliveryOutcome::Lost(
                "QuickJS result acknowledgement lost".to_owned(),
            )),
            loss = self.wait_loss() => NestedDeliveryOutcome::Lost(loss),
        }
    }

    /// Delivers a nested result where every unsettled answer is alike.
    ///
    /// # Errors
    /// Errors on a refused delivery, a proved non-delivery, or evaluator loss.
    pub async fn send_tool_result(
        &self,
        execution_id: u64,
        id: u64,
        value: Value,
        success: bool,
    ) -> Result<(), String> {
        let outcome = self
            .deliver_tool_result(execution_id, id, value, success)
            .await;
        match outcome.message() {
            None => Ok(()),
            Some(message) => Err(message.to_owned()),
        }
    }

    /// Resume a parked `yield_control()` or auto-yield.
    ///
    /// # Errors
    /// Returns an error if the command queue is unavailable.
    pub fn resume(&self, execution_id: u64, yield_time_ms: u64) -> Result<(), String> {
        self.command_tx
            .try_send(HostCommand::Resume {
                execution_id,
                yield_time_ms,
            })
            .map_err(|_| "embedded QuickJS code-mode host closed before resume".into())
    }

    /// Cooperative interrupt of the current cell (busy-loop / long JS).
    pub fn interrupt(&self) {
        self.interrupted.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(HostCommand::Interrupt);
    }

    /// Interrupts execution and joins the evaluator thread within two seconds.
    ///
    /// # Errors
    /// Returns an error if the worker panics or does not join before the deadline.
    pub async fn terminate(&self) -> Result<(), String> {
        self.stopped.store(true, Ordering::Release);
        self.interrupted.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(HostCommand::Shutdown);
        let mut owner = self.worker.lock().await;
        let deadline = Instant::now() + Duration::from_secs(2);
        while owner.as_ref().is_some_and(|worker| !worker.is_finished()) {
            if Instant::now() >= deadline {
                return Err("QuickJS worker did not join within 2s".into());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if let Some(worker) = owner.take() {
            worker
                .join()
                .map_err(|_| "QuickJS worker panicked".to_owned())?;
        }
        Ok(())
    }
}

impl Drop for EmbeddedHost {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.interrupted.store(true, Ordering::Release);
        let _ = self.command_tx.try_send(HostCommand::Shutdown);
    }
}

fn new_runtime(
    interrupted: &Arc<AtomicBool>,
    stopped: &Arc<AtomicBool>,
) -> Result<Runtime, String> {
    let runtime = Runtime::new()
        .map_err(|error| format!("failed to initialize embedded QuickJS runtime: {error}"))?;
    runtime.set_memory_limit(64 * 1024 * 1024);
    runtime.set_max_stack_size(512 * 1024);
    let interrupt_signal = Arc::clone(interrupted);
    let stop_signal = Arc::clone(stopped);
    runtime.set_interrupt_handler(Some(Box::new(move || {
        interrupt_signal.load(Ordering::Acquire) || stop_signal.load(Ordering::Acquire)
    })));
    Ok(runtime)
}

fn run_worker(
    command_rx: &std_mpsc::Receiver<HostCommand>,
    event_tx: &mpsc::Sender<RuntimeEvent>,
    ready_tx: &std_mpsc::SyncSender<Result<(), String>>,
    interrupted: &Arc<AtomicBool>,
    limits: EvalLimits,
    loss: &watch::Sender<Option<String>>,
    stopped: &Arc<AtomicBool>,
) {
    let runtime = match new_runtime(interrupted, stopped) {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    let prewarmed_context = match Context::full(&runtime) {
        Ok(context) => context,
        Err(error) => {
            let _ = ready_tx.send(Err(format!(
                "failed to create embedded QuickJS context: {error}"
            )));
            return;
        }
    };
    drop(prewarmed_context);
    drop(runtime);
    if ready_tx.send(Ok(())).is_err() {
        return;
    }

    loop {
        if stopped.load(Ordering::Acquire) {
            return;
        }
        match command_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(HostCommand::Start(start)) => {
                if stopped.load(Ordering::Acquire) {
                    return;
                }
                #[cfg(test)]
                if let Some(gate) = &start.before_start {
                    gate.wait();
                    gate.wait();
                }
                // A new cell resets its cooperative interrupt, but terminal
                // stop remains independently visible to the VM interrupt hook.
                interrupted.store(false, Ordering::Release);
                let runtime = match new_runtime(interrupted, stopped) {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        loss.send_replace(Some(error));
                        return;
                    }
                };
                let context = match Context::full(&runtime) {
                    Ok(context) => context,
                    Err(error) => {
                        loss.send_replace(Some(format!(
                            "failed to create embedded QuickJS context: {error}"
                        )));
                        return;
                    }
                };
                let control = WorkerControl {
                    loss: loss.clone(),
                    interrupted: Arc::clone(interrupted),
                    stopped: Arc::clone(stopped),
                };
                let result = context.with(|ctx| {
                    run_execution_in_context(
                        &ctx,
                        start,
                        command_rx,
                        event_tx.clone(),
                        limits,
                        control,
                    )
                });
                drop(context);
                drop(runtime);
                if let Err(error) = result {
                    loss.send_replace(Some(error));
                    return;
                }
            }
            Ok(HostCommand::ToolResult { accepted, .. }) => {
                let _ = accepted.send(NestedDeliveryOutcome::Refused(
                    "embedded QuickJS has no active execution for tool result".into(),
                ));
            }
            Ok(HostCommand::Resume { .. } | HostCommand::Interrupt)
            | Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Ok(HostCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
    }
}

struct WorkerControl {
    loss: watch::Sender<Option<String>>,
    interrupted: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
}

fn run_execution_in_context(
    ctx: &Ctx<'_>,
    start: StartExecution,
    command_rx: &std_mpsc::Receiver<HostCommand>,
    event_tx: mpsc::Sender<RuntimeEvent>,
    limits: EvalLimits,
    control: WorkerControl,
) -> Result<(), String> {
    let execution_id = start.execution_id;
    let state = Rc::new(RefCell::new(ExecutionState {
        execution_id,
        event_tx,
        phase: ExecutionPhase::Running,
        pending_tools: HashMap::new(),
        pending_yield: None,
        auto_parked: false,
        yield_deadline: arm_yield_deadline(start.yield_time_ms),
        pending_timeouts: HashMap::new(),
        next_tool_id: 1,
        next_timeout_id: 1,
        generated_bytes: 0,
        generated_items: 0,
        limits,
        control,
    }));
    install_native_functions(ctx, &state)?;
    let promise = start_promise(ctx, start)?;

    // Every fallible step after the script has started runs inside this
    // closure so that saved resolvers are always released before the
    // per-cell runtime is dropped; a `Persistent` outliving its runtime aborts.
    let result = (|| {
        loop {
            if state.borrow().control.stopped.load(Ordering::Acquire) {
                return Err("QuickJS stopped".into());
            }
            if let Some(loss) = state.borrow().control.loss.borrow().clone() {
                return Err(loss);
            }
            advance_jobs(ctx, &state, &promise)?;
            if let Some(terminal) = take_ready_terminal(&state) {
                return send_terminal(&state, terminal);
            }
            match receive_command(
                command_rx,
                Some(
                    next_event_wait(&state)
                        .unwrap_or(Duration::from_millis(20))
                        .min(Duration::from_millis(20)),
                ),
            ) {
                CommandPoll::Command(HostCommand::ToolResult {
                    execution_id: result_execution_id,
                    id,
                    value,
                    success,
                    accepted,
                }) if result_execution_id == execution_id => {
                    acknowledge_tool_result(ctx, &state, &promise, id, &value, success, accepted)?;
                }
                CommandPoll::Command(HostCommand::Resume {
                    execution_id: resume_id,
                    yield_time_ms,
                }) if resume_id == execution_id => {
                    // A settling cell has no parked yield and runs no jobs, but
                    // its consumer's `wait` is still bounded by the re-armed
                    // yield deadline: it yields again rather than blocking
                    // until the nested request settles.
                    {
                        let mut state = state.borrow_mut();
                        state.auto_parked = false;
                        state.yield_deadline = arm_yield_deadline(yield_time_ms);
                    }
                    resume_yield(ctx, &state, true)?;
                    advance_jobs(ctx, &state, &promise)?;
                }
                CommandPoll::TimerReady => {
                    if park_for_elapsed_yield(&state) {
                        continue;
                    }
                    if state.borrow().is_running()
                        && let Some(id) = next_due_timeout(&state)
                    {
                        invoke_timeout(ctx, &state, id)?;
                        advance_jobs(ctx, &state, &promise)?;
                    }
                }
                CommandPoll::Command(HostCommand::Interrupt) => {
                    if state.borrow().is_running() {
                        state.borrow_mut().auto_parked = false;
                        resume_yield(ctx, &state, false)?;
                        advance_jobs(ctx, &state, &promise)?;
                    }
                }
                CommandPoll::Command(HostCommand::Shutdown) | CommandPoll::Disconnected => {
                    return Err("embedded QuickJS host stopped".into());
                }
                CommandPoll::Command(HostCommand::ToolResult { accepted, .. }) => {
                    let _ = accepted.send(NestedDeliveryOutcome::Refused(
                        "embedded QuickJS received a tool result for a foreign execution".into(),
                    ));
                }
                CommandPoll::Command(HostCommand::Start(_) | HostCommand::Resume { .. }) => {}
            }
        }
    })();
    let mut state = state.borrow_mut();
    state.pending_tools.clear();
    state.pending_yield = None;
    state.pending_timeouts.clear();
    result
}

fn start_promise<'js>(ctx: &Ctx<'js>, start: StartExecution) -> Result<Promise<'js>, String> {
    let run_cell = ctx
        .eval::<Function<'js>, _>(BOOTSTRAP)
        .catch(ctx)
        .map_err(|error| format!("failed to evaluate embedded QuickJS bootstrap: {error}"))?;
    remove_native_globals(ctx)?;

    let tools = serde_json::to_string(&start.tools)
        .map_err(|error| format!("failed to encode QuickJS tool metadata: {error}"))?;
    let stored = serde_json::to_string(&start.stored)
        .map_err(|error| format!("failed to encode QuickJS stored values: {error}"))?;
    run_cell
        .call::<_, Promise<'js>>((start.source, tools, stored))
        .catch(ctx)
        .map_err(|error| format!("embedded QuickJS execution failed to start: {error}"))
}

#[allow(
    clippy::too_many_lines,
    reason = "bootstrap native helper bindings are installed together"
)]
fn install_native_functions<'js>(
    ctx: &Ctx<'js>,
    state: &Rc<RefCell<ExecutionState>>,
) -> Result<(), String> {
    let globals = ctx.globals();

    let tool_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexTool",
            Func::from(
                move |ctx: Ctx<'js>,
                      name: rquickjs::String<'js>,
                      input_json: rquickjs::String<'js>|
                      -> rquickjs::Result<Promise<'js>> {
                    let name = name.to_cstring()?;
                    let input_json = input_json.to_cstring()?;
                    {
                        let mut state = tool_state.borrow_mut();
                        if state.pending_tools.len() >= state.limits.pending {
                            return Err(Exception::throw_type(
                                &ctx,
                                &state.fail("pending tool calls"),
                            ));
                        }
                        state
                            .charge(name.len().saturating_add(input_json.len()))
                            .map_err(|message| Exception::throw_type(&ctx, &message))?;
                    }
                    let input = serde_json::from_str(input_json.as_str()).map_err(|error| {
                        Exception::throw_type(&ctx, &format!("invalid tool input: {error}"))
                    })?;
                    let (promise, resolve, reject) = Promise::new(&ctx)?;
                    let mut state = tool_state.borrow_mut();
                    let id = state.next_tool_id;
                    state.next_tool_id = state.next_tool_id.saturating_add(1);
                    state.pending_tools.insert(
                        id,
                        (
                            Persistent::save(&ctx, resolve),
                            Persistent::save(&ctx, reject),
                        ),
                    );
                    state
                        .emit(RuntimeEvent::ToolCall {
                            cell_id: state.execution_id,
                            id,
                            name: name.as_str().to_owned(),
                            input,
                        })
                        .map_err(|message| Exception::throw_type(&ctx, &message))?;
                    Ok(promise)
                },
            ),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS tool callback: {error}"))?;

    let content_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexContent",
            Func::from(
                move |ctx: Ctx<'js>, content_json: rquickjs::String<'js>| -> rquickjs::Result<()> {
                    let content_json = content_json.to_cstring()?;
                    content_state
                        .borrow_mut()
                        .charge(content_json.len())
                        .map_err(|message| Exception::throw_type(&ctx, &message))?;
                    let content = serde_json::from_str(content_json.as_str()).map_err(|error| {
                        Exception::throw_type(&ctx, &format!("invalid output content: {error}"))
                    })?;
                    let state = content_state.borrow();
                    state
                        .emit(RuntimeEvent::Content {
                            cell_id: state.execution_id,
                            content,
                        })
                        .map_err(|message| Exception::throw_type(&ctx, &message))?;
                    Ok(())
                },
            ),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS content callback: {error}"))?;

    let notify_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexNotify",
            Func::from(
                move |ctx: Ctx<'js>, text: rquickjs::String<'js>| -> rquickjs::Result<()> {
                    let text = text.to_cstring()?;
                    let mut state = notify_state.borrow_mut();
                    state
                        .charge(text.len())
                        .map_err(|message| Exception::throw_type(&ctx, &message))?;
                    state
                        .emit(RuntimeEvent::Notify {
                            cell_id: state.execution_id,
                            text: text.as_str().to_owned(),
                        })
                        .map_err(|message| Exception::throw_type(&ctx, &message))
                },
            ),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS notify callback: {error}"))?;

    let yield_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexYield",
            Func::from(move |ctx: Ctx<'js>| -> rquickjs::Result<Promise<'js>> {
                let (promise, resolve, reject) = Promise::new(&ctx)?;
                let mut state = yield_state.borrow_mut();
                if state.pending_yield.is_some() {
                    return Err(Exception::throw_type(
                        &ctx,
                        "yield_control is already waiting",
                    ));
                }
                let already_parked = state.auto_parked;
                state.pending_yield = Some((
                    Persistent::save(&ctx, resolve),
                    Persistent::save(&ctx, reject),
                ));
                if !already_parked {
                    let _ = state.emit(RuntimeEvent::Yielded {
                        cell_id: state.execution_id,
                    });
                }
                Ok(promise)
            }),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS yield callback: {error}"))?;

    let timeout_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexSetTimeout",
            Func::from(
                move |ctx: Ctx<'js>,
                      callback: Function<'js>,
                      delay_ms: i64|
                      -> rquickjs::Result<u32> {
                    let delay_ms = u64::try_from(delay_ms).unwrap_or_default();
                    let mut state = timeout_state.borrow_mut();
                    if state.pending_timeouts.len() >= state.limits.pending {
                        return Err(Exception::throw_type(&ctx, &state.fail("pending timers")));
                    }
                    let id = state.next_timeout_id;
                    state.next_timeout_id = state.next_timeout_id.saturating_add(1);
                    state.pending_timeouts.insert(
                        id,
                        PendingTimeout {
                            callback: Persistent::save(&ctx, callback),
                            deadline: Instant::now().checked_add(Duration::from_millis(delay_ms)),
                        },
                    );
                    Ok(id)
                },
            ),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS timer callback: {error}"))?;

    let clear_timeout_state = Rc::clone(state);
    globals
        .set(
            "__nanocodexClearTimeout",
            Func::from(move |id: u32| {
                clear_timeout_state
                    .borrow_mut()
                    .pending_timeouts
                    .remove(&id);
            }),
        )
        .catch(ctx)
        .map_err(|error| format!("failed to install QuickJS timer cleanup: {error}"))?;
    Ok(())
}

fn remove_native_globals(ctx: &Ctx<'_>) -> Result<(), String> {
    let globals = ctx.globals();
    for name in [
        "__nanocodexTool",
        "__nanocodexContent",
        "__nanocodexNotify",
        "__nanocodexYield",
        "__nanocodexSetTimeout",
        "__nanocodexClearTimeout",
        "console",
        "Atomics",
        "SharedArrayBuffer",
        "WebAssembly",
    ] {
        globals
            .remove(name)
            .catch(ctx)
            .map_err(|error| format!("failed to remove QuickJS global `{name}`: {error}"))?;
    }
    Ok(())
}

fn advance_jobs(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<ExecutionState>>,
    promise: &Promise<'_>,
) -> Result<(), String> {
    loop {
        if !state.borrow().is_running() {
            return Ok(());
        }
        if let Some(terminal) = completed_terminal(ctx, promise)? {
            let mut state = state.borrow_mut();
            state.phase = ExecutionPhase::Settling(terminal);
            state.pending_timeouts.clear();
            state.pending_yield = None;
            state.auto_parked = false;
            return Ok(());
        }
        if state.borrow().is_parked() || !ctx.execute_pending_job() {
            return Ok(());
        }
    }
}

fn take_ready_terminal(state: &Rc<RefCell<ExecutionState>>) -> Option<ExecutionTerminal> {
    let mut state = state.borrow_mut();
    if !state.pending_tools.is_empty() {
        return None;
    }
    match std::mem::replace(&mut state.phase, ExecutionPhase::Finished) {
        ExecutionPhase::Settling(terminal) => Some(terminal),
        phase => {
            state.phase = phase;
            None
        }
    }
}

fn completed_terminal(
    ctx: &Ctx<'_>,
    promise: &Promise<'_>,
) -> Result<Option<ExecutionTerminal>, String> {
    match promise.state() {
        PromiseState::Pending => Ok(None),
        PromiseState::Resolved => {
            let encoded = promise
                .result::<rquickjs::String<'_>>()
                .ok_or_else(|| "embedded QuickJS promise lost its result".to_owned())?
                .catch(ctx)
                .map_err(|error| format!("failed to read embedded QuickJS result: {error}"))?;
            let encoded = encoded.to_cstring().map_err(|e| e.to_string())?;
            if encoded.len() > EvalLimits::DEFAULT.bytes {
                return Err("resource limit: QuickJS terminal bytes".into());
            }
            serde_json::from_str(encoded.as_str())
                .map(Some)
                .map_err(|error| format!("embedded QuickJS returned an invalid result: {error}"))
        }
        PromiseState::Rejected => {
            let result = promise
                .result::<String>()
                .ok_or_else(|| "embedded QuickJS promise lost its rejection".to_owned())?
                .catch(ctx);
            match result {
                Err(error) => Err(format!("embedded QuickJS execution rejected: {error}")),
                Ok(_) => {
                    Err("rejected embedded QuickJS promise returned a successful value".to_owned())
                }
            }
        }
    }
}

fn send_terminal(
    state: &Rc<RefCell<ExecutionState>>,
    terminal: ExecutionTerminal,
) -> Result<(), String> {
    let state = state.borrow();
    let event = match terminal {
        ExecutionTerminal::Done { stored } => RuntimeEvent::Done {
            cell_id: state.execution_id,
            stored,
        },
        ExecutionTerminal::Error { message, stored } => RuntimeEvent::Error {
            cell_id: state.execution_id,
            message,
            stored,
        },
    };
    state.emit(event)
}

fn resume_yield(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<ExecutionState>>,
    success: bool,
) -> Result<(), String> {
    let Some((resolve, reject)) = state.borrow_mut().pending_yield.take() else {
        return Ok(());
    };
    let function = if success { resolve } else { reject };
    let function = function
        .restore(ctx)
        .map_err(|error| format!("failed to restore QuickJS yield promise: {error}"))?;
    if success {
        function
            .call::<_, ()>(())
            .catch(ctx)
            .map_err(|error| format!("failed to resume QuickJS yield: {error}"))
    } else {
        function
            .call::<_, ()>(("yield interrupted",))
            .catch(ctx)
            .map_err(|error| format!("failed to reject QuickJS yield: {error}"))
    }
}

/// Answer one tool result: a refusal goes only to its sender, a settled
/// promise advances the cell, and an engine failure loses the cell after
/// the sender has been told.
fn acknowledge_tool_result(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<ExecutionState>>,
    promise: &Promise<'_>,
    id: u64,
    value: &Value,
    success: bool,
    accepted: oneshot::Sender<NestedDeliveryOutcome>,
) -> Result<(), String> {
    match resolve_tool(ctx, state, id, value, success) {
        Ok(Ok(())) => {
            let _ = accepted.send(NestedDeliveryOutcome::Accepted);
            advance_jobs(ctx, state, promise)
        }
        Ok(Err(refusal)) => {
            let _ = accepted.send(NestedDeliveryOutcome::Refused(refusal));
            Ok(())
        }
        // The result reached the interpreter and settling it failed there, so
        // acceptance is undetermined: this is a loss, not a non-delivery.
        Err(loss) => {
            let _ = accepted.send(NestedDeliveryOutcome::Lost(loss.clone()));
            Err(loss)
        }
    }
}

/// Settle one pending tool promise.
///
/// The outer `Err` is a genuine engine failure that loses the cell. The inner
/// `Err` is a caller protocol refusal (no pending request with that id): it
/// is reported through the acknowledgement and must not disturb the cell.
fn resolve_tool(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<ExecutionState>>,
    id: u64,
    value: &Value,
    success: bool,
) -> Result<Result<(), String>, String> {
    let Some((resolve, reject)) = state.borrow_mut().pending_tools.remove(&id) else {
        return Ok(Err(format!(
            "embedded QuickJS received a result for unknown tool call {id}"
        )));
    };
    let function = if success { resolve } else { reject };
    let function = function
        .restore(ctx)
        .map_err(|error| format!("failed to restore QuickJS tool promise: {error}"))?;
    let encoded = serde_json::to_string(value)
        .map_err(|error| format!("failed to encode QuickJS tool result: {error}"))?;
    function
        .call::<_, ()>((encoded,))
        .catch(ctx)
        .map_err(|error| format!("failed to settle QuickJS tool promise: {error}"))?;
    Ok(Ok(()))
}

fn invoke_timeout(
    ctx: &Ctx<'_>,
    state: &Rc<RefCell<ExecutionState>>,
    id: u32,
) -> Result<(), String> {
    let timeout = state.borrow_mut().pending_timeouts.remove(&id);
    let Some(timeout) = timeout else {
        return Ok(());
    };
    let callback = timeout
        .callback
        .restore(ctx)
        .map_err(|error| format!("failed to restore QuickJS timeout callback: {error}"))?;
    callback
        .call::<_, ()>(())
        .catch(ctx)
        .map_err(|error| format!("embedded QuickJS timeout callback failed: {error}"))
}

fn arm_yield_deadline(yield_time_ms: u64) -> Instant {
    Instant::now()
        .checked_add(Duration::from_millis(yield_time_ms))
        .unwrap_or_else(Instant::now)
}

/// Auto-yield only runs at event-loop idle. A busy `while (true) {}` never
/// reaches this; interrupt aborts the job and is not a resumeable yield.
fn park_for_elapsed_yield(state: &Rc<RefCell<ExecutionState>>) -> bool {
    let mut state = state.borrow_mut();
    if state.is_parked() {
        return true;
    }
    if Instant::now() < state.yield_deadline {
        return false;
    }
    state.auto_parked = true;
    let _ = state.emit(RuntimeEvent::Yielded {
        cell_id: state.execution_id,
    });
    true
}

fn next_event_wait(state: &Rc<RefCell<ExecutionState>>) -> Option<Duration> {
    let state = state.borrow();
    if state.is_parked() {
        return None;
    }
    let now = Instant::now();
    let mut wait = state.yield_deadline.saturating_duration_since(now);
    if let Some(deadline) = state
        .pending_timeouts
        .values()
        .filter_map(|timeout| timeout.deadline)
        .min()
    {
        wait = wait.min(deadline.saturating_duration_since(now));
    }
    Some(wait)
}

fn next_due_timeout(state: &Rc<RefCell<ExecutionState>>) -> Option<u32> {
    let now = Instant::now();
    state
        .borrow()
        .pending_timeouts
        .iter()
        .filter_map(|(id, timeout)| {
            timeout
                .deadline
                .filter(|deadline| *deadline <= now)
                .map(|deadline| (*id, deadline))
        })
        .min_by_key(|(id, deadline)| (*deadline, *id))
        .map(|(id, _)| id)
}

fn receive_command(
    receiver: &std_mpsc::Receiver<HostCommand>,
    timeout: Option<Duration>,
) -> CommandPoll {
    let Some(timeout) = timeout else {
        return receiver
            .recv()
            .map_or(CommandPoll::Disconnected, CommandPoll::Command);
    };
    match receiver.recv_timeout(timeout) {
        Ok(command) => CommandPoll::Command(command),
        Err(std_mpsc::RecvTimeoutError::Timeout) => CommandPoll::TimerReady,
        Err(std_mpsc::RecvTimeoutError::Disconnected) => CommandPoll::Disconnected,
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "small injected resource limits"
)]
mod resource_tests {
    use super::*;

    #[tokio::test]
    async fn terminate_remains_sticky_across_dequeued_start() {
        let host = EmbeddedHost::spawn().expect("host");
        let gate = Arc::new(std::sync::Barrier::new(2));
        host.command_tx
            .send(HostCommand::Start(StartExecution {
                execution_id: 1,
                source: "while (true) {}".into(),
                tools: vec![],
                stored: HashMap::new(),
                yield_time_ms: 10_000,
                before_start: Some(Arc::clone(&gate)),
            }))
            .expect("queued start");
        gate.wait();
        let stopped = Arc::clone(&host.stopped);
        let release = thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                thread::yield_now();
            }
            gate.wait();
        });
        let result = host.terminate().await;
        release.join().expect("start gate released");
        if result.is_err() {
            // Even a failed regression must not leave a busy worker behind.
            host.interrupt();
            host.terminate().await.expect("failure cleanup joined");
        }
        assert!(
            result.is_ok(),
            "sticky stop was erased by Start: {result:?}"
        );
        assert!(host.worker.lock().await.is_none());
    }

    #[tokio::test]
    async fn native_callback_checks_exact_bytes_before_emitting() {
        let one = serde_json::json!({"type": "input_text", "text": "é"})
            .to_string()
            .len();
        let host = EmbeddedHost::spawn_with_limits(EvalLimits {
            bytes: one,
            items: 4,
            pending: 2,
        })
        .expect("host");
        host.start_cell(1, "text('é'); text('é');", HashMap::new(), vec![], 10_000)
            .expect("start");
        assert!(matches!(
            host.read_event().await,
            Ok(RuntimeEvent::Content { .. })
        ));
        assert!(
            host.read_event()
                .await
                .expect_err("second output exceeds exact limit")
                .contains("resource limit")
        );
        host.terminate().await.expect("join");
        assert!(host.worker.lock().await.is_none());
    }

    #[tokio::test]
    async fn blocked_consumer_keeps_independent_loss_and_cancel_control() {
        let host = EmbeddedHost::spawn().expect("host");
        host.start_cell(
            1,
            "for (let i=0; i<300; i++) notify('x');",
            HashMap::new(),
            vec![],
            10_000,
        )
        .expect("start");
        let loss = tokio::time::timeout(Duration::from_secs(1), host.wait_loss())
            .await
            .expect("control available");
        assert!(loss.contains("resource limit"));
        assert!(host.events.lock().await.len() <= 256);
        tokio::time::timeout(Duration::from_secs(1), host.terminate())
            .await
            .expect("cancel available")
            .expect("joined");
        assert!(host.worker.lock().await.is_none());
    }
}
