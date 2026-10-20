//! The SDK session: one warm worker per generation, one fresh checkout per
//! cell, and a cell that always runs to a terminal inside `execute`.

use std::{
    collections::BTreeSet,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use monty_pool::{Checkout, Pool, PoolConfig, ReplConfig};
use monty_types::{
    DateTimeSource, OsPolicy, RandomStart, ResourceLimits, SleepMode, TypeCheckingConfig,
    TypeCheckingFormat,
};
use plakous_agent::internal::{
    ChildExitEscalation, ChildProcessExit, LocalCompositionContentItem, LocalCompositionEvent,
    LocalCompositionExecuteRequest, LocalCompositionFuture, LocalCompositionResponse,
    LocalCompositionSession, LocalCompositionSessionBundle, LocalCompositionSessionFuture,
    LocalCompositionSessionLoss, LocalCompositionShutdownResult, LocalCompositionStartedCell,
    LocalCompositionTerminateResult, LocalCompositionWaitOutcome, LocalCompositionWaitRequest,
};
use tokio::sync::{Mutex, mpsc, watch};

use super::{
    HarnessSdkProgram,
    cell::{CellDrive, Drive, Printed, bootstrap},
    evidence::EvidenceTable,
    render::{Observation, Terminal, render},
    scope::{CellScope, StopReason},
    state::NamedStatePort,
};
use crate::AbortOnDropJoinHandle;

type CellCompletion = Result<LocalCompositionResponse, LocalCompositionSessionLoss>;

/// The bootstrap's one import costs one suspension of the checkout's budget.
const BOOTSTRAP_SUSPENSIONS: usize = 1;
/// Slack on one worker request beyond the compute limit.
const REQUEST_SLACK: Duration = Duration::from_secs(30);
const CONCURRENT_CELL_REFUSAL: &str = "cell failed\nanother run_py cell is still running";

/// What every cell of one session shares.
struct Shared {
    pool: Pool,
    program: Arc<HarnessSdkProgram>,
    host_functions: BTreeSet<String>,
    repl: ReplConfig,
    events: mpsc::Sender<LocalCompositionEvent>,
    evidence: Arc<EvidenceTable>,
    state: Arc<dyn NamedStatePort>,
    generation_lost: AtomicBool,
    worker_pid: std::sync::Mutex<Option<u32>>,
    last_child_exit: Mutex<Option<ChildProcessExit>>,
}

impl Shared {
    fn set_pid(&self, pid: Option<u32>) {
        *self
            .worker_pid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = pid;
    }

    fn take_pid(&self) -> Option<u32> {
        self.worker_pid
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// Kills the cell's worker generation and records its reaping.
    async fn reap(&self, checkout: Option<Checkout>) {
        self.generation_lost.store(true, Ordering::Release);
        let pid = self.take_pid();
        let before = plakous_js_eval::sample_children_rusage();
        drop(checkout);
        if pid.is_some() {
            let exit = crate::reap_checkout_pid(pid, before).await;
            *self.last_child_exit.lock().await = Some(exit);
        }
    }
}

struct ActiveCell {
    id: u64,
    task: AbortOnDropJoinHandle,
    completion: watch::Receiver<Option<CellCompletion>>,
}

/// One generation of the SDK evaluator.
struct SdkSession {
    shared: Arc<Shared>,
    active: Arc<Mutex<Option<ActiveCell>>>,
    next_cell: AtomicU64,
}

fn repl_config(program: &HarnessSdkProgram) -> ReplConfig {
    let limits = &program.limits;
    ReplConfig {
        script_name: "cell.py".into(),
        limits: Some(ResourceLimits {
            max_memory: Some(usize::try_from(limits.engine_memory_bytes).unwrap_or(usize::MAX)),
            max_feed_duration: Some(Duration::from_millis(limits.compute_ms)),
            max_recursion_depth: limits.recursion_frames as usize,
            max_suspensions: (limits.suspensions as usize).saturating_add(BOOTSTRAP_SUSPENSIONS),
            ..ResourceLimits::default()
        }),
        type_check: true,
        type_check_stubs: Some(program.stubs.clone()),
        type_check_config: TypeCheckingConfig {
            format: TypeCheckingFormat::Concise,
            color: false,
        },
        os_policy: OsPolicy {
            datetime: DateTimeSource::System,
            random_start: RandomStart::System,
            sleep: SleepMode::CallHost,
            ..OsPolicy::default()
        },
        ..ReplConfig::default()
    }
}

/// Builds one generation: a pool holding one warm worker.
pub(super) fn bundle(
    worker: &std::path::Path,
    program: Arc<HarnessSdkProgram>,
    evidence: Arc<EvidenceTable>,
    state: Arc<dyn NamedStatePort>,
) -> LocalCompositionFuture<'static, LocalCompositionSessionBundle> {
    let worker = worker.to_path_buf();
    Box::pin(async move {
        let request_timeout = Duration::from_millis(program.limits.compute_ms) + REQUEST_SLACK;
        let pool = Pool::new(PoolConfig {
            min_processes: 1,
            max_processes: 1,
            request_timeout: Some(request_timeout),
            ..PoolConfig::subprocess(&worker)
        })
        .await
        .map_err(|error| error.to_string())?;
        let capacity = (program.limits.concurrent_nested_calls as usize).saturating_mul(4);
        let (events, rx) = mpsc::channel(capacity.max(1));
        let shared = Shared {
            pool,
            host_functions: program.host_functions.iter().cloned().collect(),
            repl: repl_config(&program),
            program,
            events,
            evidence,
            state,
            generation_lost: AtomicBool::new(false),
            worker_pid: std::sync::Mutex::new(None),
            last_child_exit: Mutex::new(None),
        };
        Ok(LocalCompositionSessionBundle {
            session: Box::new(SdkSession {
                shared: Arc::new(shared),
                active: Arc::new(Mutex::new(None)),
                next_cell: AtomicU64::new(1),
            }),
            events: Box::new(crate::MontyEvents { rx: Mutex::new(rx) }),
        })
    })
}

fn result(
    cell_id: u64,
    items: Vec<LocalCompositionContentItem>,
    error: Option<String>,
) -> LocalCompositionResponse {
    LocalCompositionResponse::Result {
        cell_id: cell_id.to_string(),
        items,
        error,
    }
}

/// Renders a terminal with the cell's output and scope.
fn finish(
    shared: &Shared,
    cell_id: u64,
    terminal: &Terminal,
    printed: &str,
    scope: Option<&CellScope>,
) -> LocalCompositionResponse {
    let untrusted = scope.map(CellScope::untrusted_sources).unwrap_or_default();
    let images = scope.map(CellScope::images).unwrap_or_default();
    let observation = Observation {
        terminal,
        printed,
        untrusted: &untrusted,
        images: &images,
    };
    let (items, error) = render(&observation, &shared.program.limits, &shared.evidence);
    result(cell_id, items, error)
}

/// Runs one cell from source bound to terminal.
async fn run_cell(
    shared: Arc<Shared>,
    cell_id: u64,
    request: LocalCompositionExecuteRequest,
) -> CellCompletion {
    let admitted = tokio::time::Instant::now();
    let limits = shared.program.limits;
    if request.source.len() as u64 > limits.source_bytes {
        return Ok(finish(
            &shared,
            cell_id,
            &Terminal::Stopped(StopReason::Source),
            "",
            None,
        ));
    }
    request.start_admission.admit().await?;
    let mut checkout = shared
        .pool
        .checkout(&shared.repl)
        .await
        .map_err(|error| LocalCompositionSessionLoss::HostLost(error.to_string()))?;
    shared.set_pid(checkout.pid());
    if let Err(failure) = bootstrap(
        &mut checkout,
        &shared.program.bootstrap,
        &shared.program.host_functions,
    )
    .await
    {
        shared.reap(Some(checkout)).await;
        return Err(LocalCompositionSessionLoss::HostLost(failure));
    }
    let scope = Arc::new(CellScope::new(
        cell_id.to_string(),
        shared.events.clone(),
        limits,
        Arc::clone(&shared.evidence),
        Arc::clone(&shared.state),
    ));
    let mut printed = Printed::default();
    let deadline = admitted + Duration::from_millis(limits.foreground_budget_ms);
    let drive = {
        let mut drive = CellDrive::new(
            &mut checkout,
            &mut printed,
            Arc::clone(&scope),
            &shared.host_functions,
        );
        tokio::select! {
            drive = drive.run(&request.source) => drive,
            () = tokio::time::sleep_until(deadline) => Drive::Terminal(Terminal::Stopped(StopReason::Time)),
        }
    };
    let healthy = matches!(
        drive,
        Drive::Terminal(Terminal::Completed(_) | Terminal::Failed(_) | Terminal::TypeErrors(_))
    ) && checkout.pid().is_some();
    if healthy {
        // A worker that cannot be reset is discarded by the pool, and the
        // next cell spawns a fresh one; no source is re-evaluated.
        let _ = checkout.finish().await;
        shared.set_pid(None);
    } else {
        shared.reap(Some(checkout)).await;
    }
    match drive {
        Drive::Lost(loss) => Err(loss),
        Drive::Terminal(terminal) => Ok(finish(
            &shared,
            cell_id,
            &terminal,
            &printed.text,
            Some(&scope),
        )),
    }
}

async fn observe(
    active: Arc<Mutex<Option<ActiveCell>>>,
    id: u64,
    mut completion: watch::Receiver<Option<CellCompletion>>,
) -> CellCompletion {
    let outcome = completion
        .wait_for(Option::is_some)
        .await
        .map(|value| value.clone())
        .ok()
        .flatten()
        .unwrap_or_else(|| {
            Err(LocalCompositionSessionLoss::HostLost(
                "cell ended without an outcome".into(),
            ))
        });
    let mut active = active.lock().await;
    if active.as_ref().is_some_and(|cell| cell.id == id) {
        active.take();
    }
    outcome
}

fn unreaped(exit: Option<&ChildProcessExit>) -> bool {
    exit.is_some_and(|exit| exit.escalation == ChildExitEscalation::Unreaped)
}

fn unreaped_loss() -> LocalCompositionSessionLoss {
    LocalCompositionSessionLoss::HostLost("monty worker cleanup did not establish reaping".into())
}

impl LocalCompositionSession for SdkSession {
    fn execute(
        &self,
        request: LocalCompositionExecuteRequest,
    ) -> LocalCompositionSessionFuture<'_, LocalCompositionStartedCell> {
        Box::pin(async move {
            let mut active = self.active.lock().await;
            let id = self.next_cell.fetch_add(1, Ordering::Relaxed);
            if active
                .as_ref()
                .is_some_and(|cell| cell.completion.borrow().is_none())
            {
                let response = result(
                    id,
                    vec![LocalCompositionContentItem::Text(
                        CONCURRENT_CELL_REFUSAL.into(),
                    )],
                    Some("cell failed".into()),
                );
                return Ok(LocalCompositionStartedCell {
                    cell_id: id.to_string(),
                    initial_response: Box::pin(async move { Ok(response) }),
                });
            }
            if self.shared.generation_lost.load(Ordering::Acquire) {
                return Err(LocalCompositionSessionLoss::HostLost(
                    "the SDK worker generation was reset".into(),
                ));
            }
            let (sender, completion) = watch::channel(None);
            let shared = Arc::clone(&self.shared);
            let task = AbortOnDropJoinHandle::new(tokio::spawn(async move {
                let outcome = run_cell(shared, id, request).await;
                let _ = sender.send(Some(outcome));
            }));
            active.replace(ActiveCell {
                id,
                task,
                completion: completion.clone(),
            });
            Ok(LocalCompositionStartedCell {
                cell_id: id.to_string(),
                initial_response: Box::pin(observe(Arc::clone(&self.active), id, completion)),
            })
        })
    }

    fn wait(
        &self,
        request: LocalCompositionWaitRequest,
    ) -> LocalCompositionFuture<'_, LocalCompositionWaitOutcome> {
        Box::pin(async move {
            let completion = self
                .active
                .lock()
                .await
                .as_ref()
                .filter(|cell| cell.id.to_string() == request.cell_id)
                .map(|cell| (cell.id, cell.completion.clone()));
            let Some((id, completion)) = completion else {
                return Ok(LocalCompositionWaitOutcome::Missing(
                    LocalCompositionResponse::Result {
                        cell_id: request.cell_id,
                        items: Vec::new(),
                        error: None,
                    },
                ));
            };
            Ok(
                match observe(Arc::clone(&self.active), id, completion).await {
                    Ok(response) => LocalCompositionWaitOutcome::Live(response),
                    Err(loss) => LocalCompositionWaitOutcome::Lost(loss),
                },
            )
        })
    }

    fn terminate(
        &self,
        cell_id: String,
    ) -> Pin<Box<dyn Future<Output = LocalCompositionTerminateResult> + Send + '_>> {
        Box::pin(async move {
            let cell = {
                let mut active = self.active.lock().await;
                if active
                    .as_ref()
                    .is_some_and(|cell| cell.id.to_string() == cell_id)
                {
                    active.take()
                } else {
                    None
                }
            };
            let Some(mut cell) = cell else {
                return LocalCompositionTerminateResult::Joined(
                    LocalCompositionWaitOutcome::Missing(LocalCompositionResponse::Result {
                        cell_id,
                        items: Vec::new(),
                        error: None,
                    }),
                );
            };
            let finished = cell.completion.borrow().clone();
            if let Some(outcome) = finished {
                return match outcome {
                    Ok(response) => LocalCompositionTerminateResult::Joined(
                        LocalCompositionWaitOutcome::Live(response),
                    ),
                    Err(loss) => LocalCompositionTerminateResult::Lost(loss),
                };
            }
            if let Some(handle) = cell.task.handle.take() {
                handle.abort();
                let _ = handle.await;
            }
            self.shared.reap(None).await;
            if unreaped(self.shared.last_child_exit.lock().await.as_ref()) {
                return LocalCompositionTerminateResult::Lost(unreaped_loss());
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
            if let Some(mut cell) = self.active.lock().await.take()
                && let Some(handle) = cell.task.handle.take()
            {
                handle.abort();
                let _ = handle.await;
                self.shared.reap(None).await;
            }
            let idle = self.shared.pool.idle_worker_pids();
            let before = plakous_js_eval::sample_children_rusage();
            self.shared.pool.close().await;
            let mut child_exit = self.shared.last_child_exit.lock().await.take();
            for pid in idle {
                child_exit = Some(crate::reap_checkout_pid(Some(pid), before).await);
            }
            if unreaped(child_exit.as_ref()) {
                return LocalCompositionShutdownResult::Lost(unreaped_loss());
            }
            LocalCompositionShutdownResult::Joined { child_exit }
        })
    }

    fn generation_lost(&self) -> bool {
        self.shared.generation_lost.load(Ordering::Acquire)
    }
}
