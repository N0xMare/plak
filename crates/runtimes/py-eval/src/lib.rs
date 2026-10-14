//! monty-pool py-ptc session.

mod bindings;
mod codec;
mod prompts;

pub use bindings::{PythonToolBinding, PythonToolInput};

#[cfg(test)]
mod tests;

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::prompts::{
    PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_PREFIX, PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_SUFFIX,
    PLAKOUS_PY_PTC_ERROR_INVALID_CELL_ID, PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX,
    PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX,
    PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX,
};
use monty_pool::{
    Checkout, Pool, PoolConfig, PoolError, ReplConfig, ResumeValue, TurnEvent, on_print_sync,
};
use monty_types::{
    CallArgs, DateTimeSource, ExcType, MontyException, MontyObject, NameLookupResult, NamedValues,
    OsPolicy, RandomStart, ResourceLimits, SleepMode,
};
use plakous_agent::internal::{
    ChildExitEscalation, ChildProcessExit, ChildProcessKind, LocalCompositionContentItem,
    LocalCompositionEvent, LocalCompositionEventReceiver, LocalCompositionExecuteRequest,
    LocalCompositionFuture, LocalCompositionNestedCall, LocalCompositionNestedDelivery,
    LocalCompositionNestedResponder, LocalCompositionResponse, LocalCompositionSession,
    LocalCompositionSessionBundle, LocalCompositionSessionFuture, LocalCompositionSessionLoss,
    LocalCompositionShutdownResult, LocalCompositionStartedCell, LocalCompositionTerminateResult,
    LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use plakous_js_eval::{ChildReapBounds, ChildReapEscalation};
use serde_json::Value;
use tokio::{
    sync::{Mutex, mpsc, oneshot, watch},
    task::JoinHandle,
};

use self::codec::{call_to_json, format_monty, nested_reply_to_resume};

// Leave room for the longest status/header (including a u64 cell id), so a
// cumulative-cap notice survives the facade's status-inclusive 1 MiB bound.
const MAX_PRINT_BYTES: usize = 1_048_576 - 64;
const MAX_OUTSTANDING_NESTED: usize = 16;

/// Start of the monty-pool protocol context for a raised exception whose
/// payload would not convert.
///
/// monty-pool 1.1.0 builds this context at exactly one site (`checkout.rs`,
/// the `Error` child-event arm) when `MontyException::try_from` rejects the
/// worker's exception — monty-proto refusing a single-line traceback
/// `StackFrame` whose columns are inverted or run past its preview line, which
/// only a misbehaving worker sends. The event itself is the proof
/// that the cell raised, so the cell's outcome is known even though its
/// diagnostics are not; every other `Protocol` context leaves the outcome
/// unknown. The dependency is pinned, so this exact context is the only
/// available discriminator between the two.
const MONTY_UNDECODABLE_EXCEPTION_CONTEXT: &str = "invalid exception payload";

/// Identity of the Python tool bindings and interpreter delivery contract.
pub const ADAPTER_REVISION: &str = "plakous.monty-python-adapter.v6";

type CellCompletion = Result<LocalCompositionResponse, LocalCompositionSessionLoss>;
type MontyFeedTask = (u64, AbortOnDropJoinHandle);
type CellObservations = Arc<Mutex<BTreeMap<u64, CellObservation>>>;

#[derive(Clone)]
struct CellObservation {
    completion: watch::Receiver<Option<CellCompletion>>,
    output: Arc<std::sync::Mutex<PrintObservation>>,
}

#[derive(Default)]
struct PrintObservation {
    text: String,
    consumed: usize,
}

struct Printed {
    text: String,
    truncated: bool,
    output: Arc<std::sync::Mutex<PrintObservation>>,
}

impl std::ops::Deref for Printed {
    type Target = str;
    fn deref(&self) -> &str {
        &self.text
    }
}

type NestedAcceptance = oneshot::Sender<LocalCompositionNestedDelivery>;
struct PendingNestedReply {
    reply: NestedReply,
    accepted: NestedAcceptance,
}
struct FuturesBatch {
    results: Vec<(u32, ResumeValue)>,
    acceptances: Vec<NestedAcceptance>,
}

/// Wall bound on one parent request to the interpreter worker.
///
/// This is not a per-cell or interpreter-lifetime budget: it bounds a single
/// unanswered request, after which monty-pool kills the worker. Operation and
/// root-turn deadlines live in the driver.
const WORKER_REQUEST_TIMEOUT: Duration = Duration::from_secs(90);

fn pool_config(worker: &std::path::Path, request_timeout: Duration) -> PoolConfig {
    PoolConfig {
        min_processes: 0,
        max_processes: 1,
        request_timeout: Some(request_timeout),
        ..PoolConfig::subprocess(worker)
    }
}

fn repl_config() -> ReplConfig {
    ReplConfig {
        script_name: "plak-py-ptc.py".into(),
        limits: Some(ResourceLimits {
            max_memory: Some(256 * 1024 * 1024),
            // monty-pool counts suspensions per checkout, across every cell of
            // the interpreter's life, so any finite cap would be a cumulative
            // lifetime expiry. The nested-call cap bounds outstanding calls
            // (16); total work is bounded by the operation and root-turn
            // deadlines instead.
            max_suspensions: usize::MAX,
            ..ResourceLimits::default()
        }),
        // Clock reads (UTC) and `random` seeding are answered inside the
        // worker. Sleeps suspend to the host, which refuses them like every
        // other OS call.
        os_policy: OsPolicy {
            datetime: DateTimeSource::System,
            random_start: RandomStart::System,
            sleep: SleepMode::CallHost,
            ..OsPolicy::default()
        },
        ..ReplConfig::default()
    }
}

/// Build a py-ptc local-composition session bundle backed by a monty worker.
#[must_use]
pub fn session_bundle(
    worker: &std::path::Path,
    bindings: BTreeMap<String, PythonToolBinding>,
) -> LocalCompositionFuture<'static, LocalCompositionSessionBundle> {
    session_bundle_within(worker, bindings, WORKER_REQUEST_TIMEOUT)
}

/// Same bundle as [`session_bundle`] with an explicit worker request bound.
///
/// Only the production bound is ever built outside tests; the parameter exists
/// so a test can observe the kill-and-reap path without waiting out 90 seconds.
fn session_bundle_within(
    worker: &std::path::Path,
    bindings: BTreeMap<String, PythonToolBinding>,
    request_timeout: Duration,
) -> LocalCompositionFuture<'static, LocalCompositionSessionBundle> {
    let worker = worker.to_path_buf();
    Box::pin(async move {
        let bindings = bindings::PythonBindings::compile(bindings)?;
        let pool = Pool::new(pool_config(&worker, request_timeout))
            .await
            .map_err(|error| error.to_string())?;
        let mut checkout = pool
            .checkout(&repl_config())
            .await
            .map_err(|error| error.to_string())?;
        if !bindings.source.is_empty() {
            let pid = checkout.pid();
            let bootstrap = {
                let on_print = &mut on_print_sync(|_, _: &str| {});
                checkout
                    .feed(
                        bindings.source.as_str(),
                        NamedValues::new(),
                        Vec::new(),
                        false,
                        on_print,
                    )
                    .await
            };
            let failure = match bootstrap {
                Ok(TurnEvent::Complete(_)) => None,
                Ok(_) => Some("Python tool bootstrap unexpectedly suspended".to_owned()),
                Err(error) => Some(format!("Python tool bootstrap failed: {error}")),
            };
            if let Some(failure) = failure {
                let before = plakous_js_eval::sample_children_rusage();
                drop(checkout);
                let exit = reap_checkout_pid(pid, before).await;
                return Err(if exit.escalation == ChildExitEscalation::Unreaped {
                    format!("{failure}; Python worker cleanup did not establish reaping")
                } else {
                    failure
                });
            }
        }
        let (events_tx, events_rx) = mpsc::channel(MAX_OUTSTANDING_NESTED.saturating_mul(4));
        Ok(LocalCompositionSessionBundle {
            session: Box::new(MontySession {
                checkout: Arc::new(Mutex::new(Some(checkout))),
                feed_task: Arc::new(Mutex::new(None)),
                cells: Arc::new(Mutex::new(BTreeMap::new())),
                events_tx,
                next_cell: AtomicU64::new(1),
                bindings: Arc::new(bindings.host),
                last_child_exit: Arc::new(Mutex::new(None)),
                generation_lost: Arc::new(AtomicBool::new(false)),
            }),
            events: Box::new(MontyEvents {
                rx: Mutex::new(events_rx),
            }),
        })
    })
}

struct AbortOnDropJoinHandle {
    handle: Option<JoinHandle<()>>,
}

impl AbortOnDropJoinHandle {
    fn new(handle: JoinHandle<()>) -> Self {
        Self {
            handle: Some(handle),
        }
    }
}

impl Drop for AbortOnDropJoinHandle {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

struct MontySession {
    checkout: Arc<Mutex<Option<Checkout>>>,
    feed_task: Arc<Mutex<Option<MontyFeedTask>>>,
    cells: CellObservations,
    events_tx: mpsc::Sender<LocalCompositionEvent>,
    next_cell: AtomicU64,
    bindings: Arc<BTreeMap<String, PythonToolBinding>>,
    last_child_exit: Arc<Mutex<Option<ChildProcessExit>>>,
    /// Set when a definite cell outcome cost this interpreter its worker, so
    /// the driver resets the generation instead of reusing a dead checkout.
    generation_lost: Arc<AtomicBool>,
}

impl Drop for MontySession {
    fn drop(&mut self) {
        if let Ok(mut guard) = self.feed_task.try_lock() {
            let _ = guard.take();
        }
    }
}

async fn abort_active_feed(feed_task: &Mutex<Option<MontyFeedTask>>) {
    let active = feed_task.lock().await.take();
    if let Some((_, mut owner)) = active
        && let Some(handle) = owner.handle.take()
    {
        handle.abort();
        let _ = handle.await;
    }
}

fn append_print(printed: &mut Printed, text: &str) {
    const NOTICE: &str = "\n[Python output truncated at 1 MiB]\n";
    if printed.truncated {
        return;
    }
    let before = printed.text.len();
    // Reserve the notice before output can be observed; never retract bytes.
    let room = (MAX_PRINT_BYTES - NOTICE.len()).saturating_sub(before);
    let mut end = text.len().min(room);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    printed.text.push_str(&text[..end]);
    if end < text.len() {
        printed.text.push_str(NOTICE);
        printed.truncated = true;
    }
    let mut output = printed
        .output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    output.text.push_str(&printed.text[before..]);
}

struct MontyEvents {
    rx: Mutex<mpsc::Receiver<LocalCompositionEvent>>,
}

fn drain_print(output: &std::sync::Mutex<PrintObservation>) -> Vec<LocalCompositionContentItem> {
    let mut output = output
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let unread = output.text[output.consumed..].to_owned();
    output.consumed = output.text.len();
    if unread.is_empty() {
        Vec::new()
    } else {
        vec![LocalCompositionContentItem::Text(unread)]
    }
}

async fn observe_cell(
    cells: CellObservations,
    feed_task: Arc<Mutex<Option<MontyFeedTask>>>,
    cell_id: u64,
    yield_time_ms: u64,
) -> CellCompletion {
    let observation =
        cells.lock().await.get(&cell_id).cloned().ok_or_else(|| {
            LocalCompositionSessionLoss::HostLost("cell observation is gone".into())
        })?;
    let mut rx = observation.completion;
    // The feed owns execution and retains its terminal result. Dropping an
    // observation (including steer preemption) cannot cancel or consume it.
    let terminal = tokio::time::timeout(Duration::from_millis(yield_time_ms), async {
        rx.wait_for(Option::is_some)
            .await
            .map(|value| value.clone())
    })
    .await;
    let mut result = match terminal {
        Err(_) => {
            return Ok(LocalCompositionResponse::Yielded {
                cell_id: cell_id.to_string(),
                items: drain_print(&observation.output),
            });
        }
        Ok(Ok(Some(result))) => result,
        _ => Err(LocalCompositionSessionLoss::HostLost("cell ended".into())),
    };
    let mut active = feed_task.lock().await;
    if let Some((id, owner)) = active.as_mut()
        && *id == cell_id
    {
        if let Some(handle) = owner.handle.as_mut() {
            handle.await.map_err(|_| {
                LocalCompositionSessionLoss::HostLost("monty feed task did not join".into())
            })?;
        }
        owner.handle.take();
        active.take();
    }
    let mut cells = cells.lock().await;
    if let Ok(LocalCompositionResponse::Result { items, .. }) = &mut result {
        let mut output = observation
            .output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !output.text.is_empty()
            && let Some(LocalCompositionContentItem::Text(text)) = items.first_mut()
            && *text == output.text
        {
            *text = text[output.consumed..].to_owned();
            if text.is_empty() {
                items.remove(0);
            }
        }
        output.consumed = output.text.len();
    }
    cells.remove(&cell_id);
    result
}

impl LocalCompositionEventReceiver for MontyEvents {
    fn receive(&mut self) -> LocalCompositionSessionFuture<'_, Option<LocalCompositionEvent>> {
        Box::pin(async move { Ok(self.rx.lock().await.recv().await) })
    }
}

impl LocalCompositionSession for MontySession {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        Box::pin(async move {
            let mut active = self.feed_task.lock().await;
            if let Some((active_cell_id, _)) = active.as_ref() {
                let refused_cell_id = self.next_cell.fetch_add(1, Ordering::Relaxed);
                let response =
                    concurrent_execute_refused_response(refused_cell_id, *active_cell_id);
                return Ok(LocalCompositionStartedCell {
                    cell_id: refused_cell_id.to_string(),
                    initial_response: Box::pin(async move { Ok(response) }),
                });
            }
            let cell_id = self.next_cell.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = watch::channel(None);
            let output = Arc::new(std::sync::Mutex::new(PrintObservation::default()));
            let feed_output = Arc::clone(&output);
            // The diagnostic pid is read while the worker is still attached:
            // a violating reply discards it inside monty-pool, and the dead
            // checkout can no longer name the child it left behind.
            let worker_pid = match self.checkout.lock().await.as_ref() {
                Some(checkout) => checkout.pid(),
                None => {
                    return Err(LocalCompositionSessionLoss::HostLost(
                        "monty checkout is gone".into(),
                    ));
                }
            };
            let checkout = Arc::clone(&self.checkout);
            let bindings = Arc::clone(&self.bindings);
            let events_tx = self.events_tx.clone();
            let feed_task = Arc::clone(&self.feed_task);
            let cells = Arc::clone(&self.cells);
            let generation_lost = Arc::clone(&self.generation_lost);
            let last_child_exit = Arc::clone(&self.last_child_exit);
            let source = request.source;
            let handle = AbortOnDropJoinHandle::new(tokio::spawn(async move {
                let result: CellCompletion = {
                    let mut guard = checkout.lock().await;
                    match guard.as_mut() {
                        Some(checkout) => match request.start_admission.admit().await {
                            Ok(()) => {
                                drive_feed(
                                    checkout,
                                    &source,
                                    cell_id,
                                    &bindings,
                                    &events_tx,
                                    &generation_lost,
                                    feed_output,
                                )
                                .await
                            }
                            Err(error) => Err(error),
                        },
                        None => Err(LocalCompositionSessionLoss::HostLost(
                            "monty checkout is gone".into(),
                        )),
                    }
                };
                if generation_lost.load(Ordering::Acquire)
                    && let Some(exit) = reap_lost_generation(&checkout, worker_pid).await
                {
                    *last_child_exit.lock().await = Some(exit);
                }
                let _ = tx.send(Some(result));
            }));
            active.replace((cell_id, handle));
            cells.lock().await.insert(
                cell_id,
                CellObservation {
                    completion: rx,
                    output,
                },
            );
            Ok(LocalCompositionStartedCell {
                cell_id: cell_id.to_string(),
                initial_response: Box::pin(observe_cell(
                    cells,
                    feed_task,
                    cell_id,
                    request.yield_time_ms,
                )),
            })
        })
    }

    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async move {
            let Ok(cell_id) = request.cell_id.parse::<u64>() else {
                return Ok(LocalCompositionWaitOutcome::Missing(
                    LocalCompositionResponse::Result {
                        cell_id: request.cell_id,
                        items: Vec::new(),
                        error: Some(PLAKOUS_PY_PTC_ERROR_INVALID_CELL_ID.into()),
                    },
                ));
            };
            if self.cells.lock().await.contains_key(&cell_id) {
                return observe_cell(
                    Arc::clone(&self.cells),
                    Arc::clone(&self.feed_task),
                    cell_id,
                    request.yield_time_ms.min(600_000),
                )
                .await
                .map(LocalCompositionWaitOutcome::Live)
                .map_err(|error| error.to_string());
            }
            Ok(LocalCompositionWaitOutcome::Missing(
                LocalCompositionResponse::Result {
                    cell_id: request.cell_id,
                    items: Vec::new(),
                    error: None,
                },
            ))
        })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        Box::pin(async move {
            if self
                .last_child_exit
                .lock()
                .await
                .as_ref()
                .is_some_and(|exit| exit.escalation == ChildExitEscalation::Unreaped)
            {
                return LocalCompositionTerminateResult::Lost(unreaped_loss());
            }
            let completed_id = if let Ok(id) = cell_id.parse::<u64>() {
                self.cells
                    .lock()
                    .await
                    .get(&id)
                    .is_some_and(|cell| cell.completion.borrow().is_some())
                    .then_some(id)
            } else {
                None
            };
            if let Some(id) = completed_id {
                return match observe_cell(
                    Arc::clone(&self.cells),
                    Arc::clone(&self.feed_task),
                    id,
                    0,
                )
                .await
                {
                    Ok(response) => LocalCompositionTerminateResult::Joined(
                        LocalCompositionWaitOutcome::Live(response),
                    ),
                    Err(loss) => LocalCompositionTerminateResult::Lost(loss),
                };
            }
            let matches_active = self
                .feed_task
                .lock()
                .await
                .as_ref()
                .is_some_and(|(id, _)| cell_id.parse::<u64>().ok() == Some(*id));
            if !matches_active {
                return LocalCompositionTerminateResult::Joined(
                    LocalCompositionWaitOutcome::Missing(LocalCompositionResponse::Result {
                        cell_id,
                        items: Vec::new(),
                        error: None,
                    }),
                );
            }
            abort_active_feed(&self.feed_task).await;
            self.cells.lock().await.clear();
            self.generation_lost.store(true, Ordering::Release);
            if self
                .last_child_exit
                .lock()
                .await
                .as_ref()
                .is_some_and(|exit| exit.escalation == ChildExitEscalation::Unreaped)
            {
                return LocalCompositionTerminateResult::Lost(unreaped_loss());
            }
            if let Some(exit) = sample_and_drop_checkout(&self.checkout).await {
                *self.last_child_exit.lock().await = Some(exit);
                if exit.escalation == ChildExitEscalation::Unreaped {
                    return LocalCompositionTerminateResult::Lost(unreaped_loss());
                }
            }
            LocalCompositionTerminateResult::Joined(LocalCompositionWaitOutcome::Live(
                LocalCompositionResponse::Terminated {
                    cell_id,
                    items: Vec::new(),
                },
            ))
        })
    }

    fn shutdown(
        &self,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionShutdownResult> + Send + '_>> {
        Box::pin(async move {
            abort_active_feed(&self.feed_task).await;
            let mut last_exit = self.last_child_exit.lock().await;
            if let Some(exit) = *last_exit {
                if exit.escalation == ChildExitEscalation::Unreaped {
                    return LocalCompositionShutdownResult::Lost(unreaped_loss());
                }
                *last_exit = None;
                return LocalCompositionShutdownResult::Joined {
                    child_exit: Some(exit),
                };
            }
            let child_exit = sample_and_drop_checkout(&self.checkout).await;
            if child_exit.is_some_and(|exit| exit.escalation == ChildExitEscalation::Unreaped) {
                *last_exit = child_exit;
                return LocalCompositionShutdownResult::Lost(unreaped_loss());
            }
            LocalCompositionShutdownResult::Joined { child_exit }
        })
    }

    fn generation_lost(&self) -> bool {
        self.generation_lost.load(Ordering::Acquire)
    }
}

/// Drops and reaps the worker behind a generation that a definite cell outcome
/// destroyed, so cleanup still certifies the child rather than losing it.
async fn reap_lost_generation(
    checkout: &Mutex<Option<Checkout>>,
    pid: Option<u32>,
) -> Option<ChildProcessExit> {
    let checkout = checkout.lock().await.take()?;
    let before = plakous_js_eval::sample_children_rusage();
    drop(checkout);
    Some(reap_checkout_pid(pid, before).await)
}

fn unreaped_loss() -> LocalCompositionSessionLoss {
    LocalCompositionSessionLoss::HostLost("monty worker cleanup did not establish reaping".into())
}

async fn sample_and_drop_checkout(checkout: &Mutex<Option<Checkout>>) -> Option<ChildProcessExit> {
    let mut guard = checkout.lock().await;
    let checkout = guard.take()?;
    let pid = checkout.pid();
    let before = plakous_js_eval::sample_children_rusage();
    drop(checkout);
    Some(reap_checkout_pid(pid, before).await)
}

async fn reap_checkout_pid(
    pid: Option<u32>,
    before: plakous_js_eval::ProcessResourceUsage,
) -> ChildProcessExit {
    match pid {
        Some(pid) => reap_child_exit(pid, before).await,
        // An absent diagnostic pid is not a receipt that the worker was reaped.
        None => ChildProcessExit {
            child: ChildProcessKind::PythonWorker,
            user_cpu: Duration::ZERO,
            system_cpu: Duration::ZERO,
            peak_rss_bytes: None,
            escalation: ChildExitEscalation::Unreaped,
        },
    }
}

async fn reap_child_exit(
    pid: u32,
    before: plakous_js_eval::ProcessResourceUsage,
) -> ChildProcessExit {
    reap_child_exit_within(pid, before, ChildReapBounds::DEFAULT).await
}

/// Reaps the monty worker inside `bounds` instead of waiting on it forever.
///
/// Dropping the checkout asks the worker to stop, but a wedged interpreter can
/// ignore that: the reap waits out the voluntary bound, then escalates
/// `SIGTERM` and `SIGKILL`, and reports how far it had to go so shutdown never
/// silently blocks on a worker that will not exit.
async fn reap_child_exit_within(
    pid: u32,
    before: plakous_js_eval::ProcessResourceUsage,
    bounds: ChildReapBounds,
) -> ChildProcessExit {
    let reap =
        tokio::task::spawn_blocking(move || plakous_js_eval::reap_child_pid_bounded(pid, bounds));
    finish_reap(reap, before).await
}

async fn finish_reap(
    reap: JoinHandle<ChildReapEscalation>,
    before: plakous_js_eval::ProcessResourceUsage,
) -> ChildProcessExit {
    let escalation = reap.await.unwrap_or(ChildReapEscalation::Unreaped);
    let after = plakous_js_eval::sample_children_rusage();
    let delta = plakous_js_eval::children_rusage_delta(before, after);
    ChildProcessExit {
        child: ChildProcessKind::PythonWorker,
        user_cpu: Duration::from_nanos(delta.user_cpu_ns),
        system_cpu: Duration::from_nanos(delta.system_cpu_ns),
        peak_rss_bytes: delta.peak_rss_bytes,
        escalation: match escalation {
            ChildReapEscalation::Voluntary => ChildExitEscalation::Voluntary,
            ChildReapEscalation::Terminated => ChildExitEscalation::Terminated,
            ChildReapEscalation::Killed => ChildExitEscalation::Killed,
            ChildReapEscalation::Unreaped => ChildExitEscalation::Unreaped,
        },
    }
}

fn runtime_exception_response(
    cell_id: u64,
    printed: &str,
    exc: &MontyException,
) -> LocalCompositionResponse {
    let mut items = Vec::new();
    if !printed.is_empty() {
        items.push(LocalCompositionContentItem::Text(printed.to_owned()));
    }
    LocalCompositionResponse::Result {
        cell_id: cell_id.to_string(),
        items,
        error: Some(exc.to_string()),
    }
}

fn undecodable_exception_response(
    cell_id: u64,
    printed: &str,
    context: &str,
) -> LocalCompositionResponse {
    let mut items = Vec::new();
    if !printed.is_empty() {
        items.push(LocalCompositionContentItem::Text(printed.to_owned()));
    }
    LocalCompositionResponse::Result {
        cell_id: cell_id.to_string(),
        items,
        error: Some(format!(
            "{PLAKOUS_PY_PTC_ERROR_UNDECODABLE_TRACEBACK_PREFIX}{context}"
        )),
    }
}

fn concurrent_execute_refused_response(
    refused_cell_id: u64,
    active_cell_id: u64,
) -> LocalCompositionResponse {
    LocalCompositionResponse::Result {
        cell_id: refused_cell_id.to_string(),
        items: Vec::new(),
        error: Some(format!(
            "{PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_PREFIX}{active_cell_id}{PLAKOUS_PY_PTC_ERROR_CONCURRENT_CELL_SUFFIX}"
        )),
    }
}

fn map_turn_error(
    cell_id: u64,
    printed: &str,
    error: PoolError,
    worker_alive: bool,
    generation_lost: &AtomicBool,
) -> Result<LocalCompositionResponse, LocalCompositionSessionLoss> {
    match error {
        // A Python exception leaves the worker alive, including the
        // interpreter's own `MemoryError` at its soft limit. The one `Runtime`
        // that costs the worker is the allocator's hard-ceiling exit, which
        // monty-pool reports as a `MemoryError` after discarding it.
        PoolError::Runtime(exc) if !worker_alive => {
            Err(LocalCompositionSessionLoss::HostLost(exc.to_string()))
        }
        PoolError::Runtime(exc) => Ok(runtime_exception_response(cell_id, printed, &exc)),
        // The cell raised and monty-pool discarded the worker over the
        // traceback it could not convert. The outcome is a definite cell
        // failure, not a host loss: report it with whatever the cell printed
        // and let the driver reset the interpreter generation, which never
        // re-evaluates source.
        PoolError::Protocol(context)
            if context.starts_with(MONTY_UNDECODABLE_EXCEPTION_CONTEXT) =>
        {
            generation_lost.store(true, Ordering::Release);
            Ok(undecodable_exception_response(cell_id, printed, &context))
        }
        other => Err(LocalCompositionSessionLoss::HostLost(other.to_string())),
    }
}

async fn resume_call(
    checkout: &mut Checkout,
    printed: &mut Printed,
    value: ResumeValue,
) -> Result<TurnEvent, PoolError> {
    let on_print = &mut on_print_sync(|_, text: &str| append_print(printed, text));
    checkout.resume(value, on_print).await
}

async fn resume_name_denied(
    checkout: &mut Checkout,
    printed: &mut Printed,
) -> Result<TurnEvent, PoolError> {
    let on_print = &mut on_print_sync(|_, text: &str| append_print(printed, text));
    checkout
        .resume_name_lookup(NameLookupResult::Undefined, on_print)
        .await
}

async fn resume_futures_batch(
    checkout: &mut Checkout,
    printed: &mut Printed,
    batch: FuturesBatch,
) -> Result<TurnEvent, PoolError> {
    let on_print = &mut on_print_sync(|_, text: &str| append_print(printed, text));
    let resumed = checkout.resume_futures(batch.results, on_print).await;
    // With monty-pool/proto pinned at 1.1.0, batch validation excludes the
    // pre-send frame-size refusal and the worker's full-frame decoding and
    // conversion errors; this adapter sends no mounts or working directory,
    // the other pre-send Runtime sources. The sole remaining non-wire Runtime
    // path is OOM poison, which removes the worker. A surviving checkout's Runtime is the exact response to
    // this valid ResumeFutures request. The trusted Child has consumed
    // it even when resumed Python raises. A PID alone proves nothing.
    //
    // An undecodable-traceback `Protocol` context is acceptance too, and for a
    // stronger reason than a surviving pid: the worker could only build that
    // exception by running the resumed cell, so it consumed this exact
    // `ResumeFutures` and the cell observed the result before raising over it.
    // `map_turn_error` reads the same context as a definite cell outcome; were
    // this acknowledgement a loss instead, the driver would escalate the
    // delivery to exact recovery and that definite outcome would never be
    // reported.
    let delivery = batch_acceptance(&resumed, checkout.pid());
    for acknowledgement in batch.acceptances {
        let _ = acknowledgement.send(delivery.clone());
    }
    resumed
}

/// Reads the worker's answer to one `ResumeFutures` as an acceptance verdict.
///
/// See [`resume_futures_batch`] for why a surviving `Runtime` and an
/// undecodable traceback each prove the worker consumed the request.
fn batch_acceptance(
    resumed: &Result<TurnEvent, PoolError>,
    pid: Option<u32>,
) -> LocalCompositionNestedDelivery {
    let accepted = match resumed {
        Ok(_) => true,
        Err(PoolError::Runtime(_)) => pid.is_some(),
        Err(PoolError::Protocol(context)) => {
            context.starts_with(MONTY_UNDECODABLE_EXCEPTION_CONTEXT)
        }
        Err(_) => false,
    };
    if accepted {
        LocalCompositionNestedDelivery::Accepted
    } else {
        LocalCompositionNestedDelivery::HostLost
    }
}

fn complete_cell(cell_id: u64, printed: String, value: &MontyObject) -> LocalCompositionResponse {
    let mut items = Vec::new();
    if !printed.is_empty() {
        items.push(LocalCompositionContentItem::Text(printed));
    }
    items.push(LocalCompositionContentItem::Text(format_monty(
        value,
        MAX_PRINT_BYTES,
    )));
    LocalCompositionResponse::Result {
        cell_id: cell_id.to_string(),
        items,
        error: None,
    }
}

async fn collect_futures_batch(
    pending_call_ids: &[u32],
    stashed_calls: &mut BTreeSet<u32>,
    nested_rx: &mut mpsc::Receiver<(u32, PendingNestedReply)>,
    nested_replies: &mut BTreeMap<u32, PendingNestedReply>,
) -> Result<FuturesBatch, LocalCompositionSessionLoss> {
    if pending_call_ids.is_empty() {
        return Ok(FuturesBatch {
            results: Vec::new(),
            acceptances: Vec::new(),
        });
    }
    for id in pending_call_ids {
        if !stashed_calls.contains(id) {
            return Err(LocalCompositionSessionLoss::ProtocolViolation(format!(
                "monty requested unknown nested call id {id}"
            )));
        }
    }
    while !pending_call_ids
        .iter()
        .all(|id| nested_replies.contains_key(id))
    {
        let Some((reply_id, reply)) = nested_rx.recv().await else {
            return Err(LocalCompositionSessionLoss::HostLost(
                "nested reply channel closed".into(),
            ));
        };
        if stashed_calls.contains(&reply_id) {
            nested_replies.insert(reply_id, reply);
        }
    }
    let mut results = Vec::with_capacity(pending_call_ids.len());
    let mut acceptances = Vec::with_capacity(pending_call_ids.len());
    for id in pending_call_ids {
        stashed_calls.remove(id);
        let Some(reply) = nested_replies.remove(id) else {
            decline_acceptances(acceptances);
            return Err(LocalCompositionSessionLoss::ProtocolViolation(format!(
                "monty nested reply missing for call {id}"
            )));
        };
        let value = match nested_reply_to_resume(reply.reply) {
            Ok(value) => value,
            Err(reason) => {
                acceptances.push(reply.accepted);
                decline_acceptances(acceptances);
                return Err(LocalCompositionSessionLoss::ProtocolViolation(format!(
                    "monty nested reply for call {id}: {reason}"
                )));
            }
        };
        results.push((*id, value));
        acceptances.push(reply.accepted);
    }
    if let Err(loss) = validate_futures_batch(&results) {
        decline_acceptances(acceptances);
        return Err(loss);
    }
    Ok(FuturesBatch {
        results,
        acceptances,
    })
}

/// Answers every acknowledgement of a batch this host abandoned before it
/// reached the worker.
///
/// Nothing in the batch was written to the interpreter, so the cell provably
/// never observed any of these results. Saying so is what keeps an abandoned
/// batch an ordinary definite failure instead of the unknown acceptance a
/// dropped acknowledgement would become.
fn decline_acceptances(acceptances: Vec<NestedAcceptance>) {
    for acknowledgement in acceptances {
        let _ = acknowledgement.send(LocalCompositionNestedDelivery::NotObserved(
            "the batch carrying the result never reached the interpreter".into(),
        ));
    }
}

fn validate_futures_batch(
    results: &[(u32, ResumeValue)],
) -> Result<(), LocalCompositionSessionLoss> {
    use monty_proto::pb;
    use monty_types::ExtFunctionResult;
    let invalid = |reason: &str| LocalCompositionSessionLoss::ProtocolViolation(reason.to_owned());
    let mut terminal = Vec::with_capacity(results.len());
    for (call_id, value) in results {
        let result = match value {
            ResumeValue::Return(value) => ExtFunctionResult::Return(value.clone()),
            ResumeValue::Error(error) => ExtFunctionResult::Error(error.clone()),
            _ => return Err(invalid("monty nested result is not terminal")),
        };
        terminal.push((*call_id, result));
    }
    let encoded = monty_proto::future_results_to_proto(terminal);
    monty_proto::future_results_from_proto(encoded.results.clone(), encoded.values.clone())
        .map_err(|_| invalid("monty nested result failed wire validation"))?;
    let request = pb::ParentRequest {
        kind: Some(pb::parent_request::Kind::ResumeFutures(encoded)),
        ..pb::ParentRequest::default()
    };
    if monty_proto::exceeds_max_frame_len(&request).is_some() {
        return Err(invalid("monty nested result batch exceeds wire frame size"));
    }
    // Decode the complete frame with the worker's own decoder, so a batch the
    // worker would refuse never reaches it.
    let mut frame = Vec::new();
    monty_proto::write_frame(&mut frame, &request)
        .map_err(|_| invalid("monty nested result batch failed frame encoding"))?;
    let decoded = monty_proto::FrameReader::new(frame.as_slice())
        .read::<pb::ParentRequest>()
        .map_err(|_| invalid("monty nested result batch failed frame decoding"))?;
    if decoded.as_ref() != Some(&request) {
        return Err(invalid("monty nested result batch failed frame round trip"));
    }
    Ok(())
}

/// Answers one `FunctionCall`, publishing a nested call when the cell invoked a
/// bound tool with a valid argument. The inner result is the worker's next turn,
/// mapped by the caller; the outer error is a host loss before any answer.
#[allow(
    clippy::too_many_arguments,
    reason = "monty FunctionCall turn bundles feed-local mutable state"
)]
async fn handle_function_call(
    checkout: &mut Checkout,
    printed: &mut Printed,
    cell_id: u64,
    bindings: &BTreeMap<String, PythonToolBinding>,
    events_tx: &mpsc::Sender<LocalCompositionEvent>,
    reply_tx: &mpsc::Sender<(u32, PendingNestedReply)>,
    stashed_calls: &mut BTreeSet<u32>,
    outstanding_futures: &mut usize,
    function_name: String,
    args: CallArgs,
    monty_call_id: u32,
) -> Result<Result<TurnEvent, PoolError>, LocalCompositionSessionLoss> {
    let Some(tool) = bindings.get(&function_name) else {
        return Ok(resume_call(checkout, printed, ResumeValue::NotFound).await);
    };
    let argument = args.arg(0).filter(|argument| {
        args.args().len() == 1
            && args.kwargs().len() == 0
            && match tool.input_kind {
                PythonToolInput::Dictionary => matches!(
                    monty_types::unstable::node(*argument),
                    monty_types::unstable::MontyNode::Dict(_)
                ),
                PythonToolInput::String => argument.as_str().is_some(),
            }
    });
    let refusal = |exc_type, message: String| {
        ResumeValue::Error(MontyException::new(exc_type, Some(message)))
    };
    let Some(argument) = argument else {
        let expected = match tool.input_kind {
            PythonToolInput::Dictionary => "dictionary",
            PythonToolInput::String => "string",
        };
        let message = format!("Python tool requires exactly one {expected} argument");
        return Ok(resume_call(checkout, printed, refusal(ExcType::TypeError, message)).await);
    };
    if *outstanding_futures >= MAX_OUTSTANDING_NESTED {
        let message = format!(
            "{PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_PREFIX}{MAX_OUTSTANDING_NESTED}{PLAKOUS_PY_PTC_ERROR_OUTSTANDING_NESTED_SUFFIX}"
        );
        return Ok(resume_call(checkout, printed, refusal(ExcType::RuntimeError, message)).await);
    }
    let input = match call_to_json(argument) {
        Ok(input) => input,
        Err(reason) => {
            return Ok(resume_call(checkout, printed, refusal(ExcType::ValueError, reason)).await);
        }
    };
    let published = events_tx
        .send(LocalCompositionEvent::NestedCall {
            call: LocalCompositionNestedCall {
                cell_id: cell_id.to_string(),
                runtime_call_id: monty_call_id.to_string(),
                tool_name: tool.tool_name.clone(),
                input: Some(input),
            },
            responder: Box::new(MontyNestedResponder {
                call_id: monty_call_id,
                nested_tx: reply_tx.clone(),
            }),
        })
        .await;
    if published.is_err() {
        return Err(LocalCompositionSessionLoss::HostLost(
            "composition event channel closed".into(),
        ));
    }
    stashed_calls.insert(monty_call_id);
    *outstanding_futures += 1;
    Ok(resume_call(checkout, printed, ResumeValue::Future).await)
}

async fn drive_feed(
    checkout: &mut Checkout,
    source: &str,
    cell_id: u64,
    bindings: &BTreeMap<String, PythonToolBinding>,
    events_tx: &mpsc::Sender<LocalCompositionEvent>,
    generation_lost: &AtomicBool,
    output: Arc<std::sync::Mutex<PrintObservation>>,
) -> CellCompletion {
    let mut printed = Printed {
        text: String::new(),
        truncated: false,
        output,
    };
    let (nested_tx, mut nested_rx) = mpsc::channel(MAX_OUTSTANDING_NESTED);
    let mut nested_replies = BTreeMap::<u32, PendingNestedReply>::new();
    let mut stashed_calls = BTreeSet::<u32>::new();
    let mut outstanding_futures = 0usize;
    let mut turn = {
        let on_print = &mut on_print_sync(|_, text: &str| append_print(&mut printed, text));
        checkout
            .feed(source, NamedValues::new(), Vec::new(), false, on_print)
            .await
    };
    loop {
        let event = match turn {
            Ok(event) => event,
            Err(error) => {
                let alive = checkout.pid().is_some();
                return map_turn_error(cell_id, &printed, error, alive, generation_lost);
            }
        };
        turn = match event {
            TurnEvent::Complete(value) => return Ok(complete_cell(cell_id, printed.text, &value)),
            TurnEvent::FunctionCall {
                function_name,
                args,
                call_id,
                ..
            } => {
                handle_function_call(
                    checkout,
                    &mut printed,
                    cell_id,
                    bindings,
                    events_tx,
                    &nested_tx,
                    &mut stashed_calls,
                    &mut outstanding_futures,
                    function_name,
                    args,
                    call_id,
                )
                .await?
            }
            TurnEvent::OsCall { .. } => {
                resume_call(checkout, &mut printed, ResumeValue::NotHandled).await
            }
            TurnEvent::NameLookup { .. } => resume_name_denied(checkout, &mut printed).await,
            TurnEvent::ResolveFutures {
                pending_call_ids, ..
            } => {
                let batch = collect_futures_batch(
                    &pending_call_ids,
                    &mut stashed_calls,
                    &mut nested_rx,
                    &mut nested_replies,
                )
                .await?;
                outstanding_futures = outstanding_futures.saturating_sub(pending_call_ids.len());
                resume_futures_batch(checkout, &mut printed, batch).await
            }
        };
    }
}

pub(crate) enum NestedReply {
    Ok(Value),
    Err(String),
}

pub(crate) fn sanitize_nested_error(message: &str) -> String {
    if message.len() <= 16 * 1024 {
        return message.to_owned();
    }
    let mut end = 16 * 1024;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[diagnostic truncated]", &message[..end])
}

struct MontyNestedResponder {
    call_id: u32,
    nested_tx: mpsc::Sender<(u32, PendingNestedReply)>,
}

impl MontyNestedResponder {
    fn deliver(
        self,
        reply: NestedReply,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move {
            let (accepted, receipt) = oneshot::channel();
            if self
                .nested_tx
                .send((self.call_id, PendingNestedReply { reply, accepted }))
                .await
                .is_err()
            {
                // The feed loop owns the receiving half for exactly as long as
                // the cell runs. A closed channel is proof that the cell
                // already reached its own outcome, so this result never entered
                // a batch, never reached the worker, and cannot have been acted
                // on. That is a definite non-delivery, not an unknown one.
                return Ok(LocalCompositionNestedDelivery::NotObserved(
                    "the cell had already ended when its result was delivered".into(),
                ));
            }
            Ok(receipt
                .await
                .unwrap_or(LocalCompositionNestedDelivery::HostLost))
        })
    }
}

impl LocalCompositionNestedResponder for MontyNestedResponder {
    fn resolve(
        self: Box<Self>,
        value: Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        self.deliver(NestedReply::Ok(value))
    }
    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        self.deliver(NestedReply::Err(reason))
    }
}

#[cfg(test)]
mod missing_pid_tests {
    #[tokio::test]
    async fn checkout_without_pid_cannot_certify_reaping() {
        let exit = super::reap_checkout_pid(None, plakous_js_eval::sample_children_rusage()).await;
        assert_eq!(exit.escalation, super::ChildExitEscalation::Unreaped);
    }
}
