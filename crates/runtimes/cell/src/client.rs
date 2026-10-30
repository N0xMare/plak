//! Parent-side client for one cell worker process.

use std::{collections::HashMap, path::Path, process::Stdio, sync::Arc, time::Duration};

use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::CellStartupError;
use crate::frame::{MAX_FRAME_BYTES, encode_frame, read_frame};
use crate::protocol::{
    CELL_PROTOCOL_REVISION, CellOutcome, CellPrelude, CellValue, DeliveryVerdict, EngineConfig,
    HostFrame, LossClass, OutputItem, WorkerFrame, WorkerIdentity,
};

/// Content-free child-process usage returned when a worker exits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ChildProcessUsage {
    pub user_cpu: Duration,
    pub system_cpu: Duration,
    pub peak_rss_bytes: Option<u64>,
}

impl ChildProcessUsage {
    /// True when the sample carries non-zero CPU or peak RSS.
    #[must_use]
    pub fn is_meaningful(self) -> bool {
        !self.user_cpu.is_zero() || !self.system_cpu.is_zero() || self.peak_rss_bytes.is_some()
    }
}

/// How a worker stopped: its usage, and whether it had to be killed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkerStop {
    pub usage: Option<ChildProcessUsage>,
    pub forced: bool,
}

/// Byte budget for queued worker events.
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Shutdown includes cancelable I/O and a bounded child reap.
pub const WORKER_SHUTDOWN_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a spawned worker has to send `Hello` and receive `Configure`.
pub const WORKER_HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const QUEUE_ITEMS: usize = 64;
/// Outstanding host commands a queue holds before it is a genuine resource limit.
///
/// The driver polls every pending nested delivery concurrently, so a profile
/// that admits sixteen outstanding nested calls can settle sixteen results in
/// one burst while the engine drains nothing. Sixteen deliveries plus the
/// control commands that may sit behind them (`StartCell`, `Resume`,
/// `Cancel`, `Shutdown`) is the real floor; thirty-two leaves the same
/// headroom again. Below this a full queue is a queue-depth accident, not a
/// resource limit.
pub const COMMAND_QUEUE_ITEMS: usize = 32;

/// One event of a running cell, in the order the worker produced it.
#[derive(Debug)]
pub enum CellEvent {
    NestedCall {
        cell: u64,
        call: u64,
        name: String,
        input: CellValue,
    },
    Output {
        cell: u64,
        item: OutputItem,
    },
    Waiting {
        cell: u64,
        calls: Vec<u64>,
    },
    Yielded {
        cell: u64,
    },
    Done {
        cell: u64,
        outcome: CellOutcome,
    },
}

impl CellEvent {
    /// The cell the event names.
    #[must_use]
    pub const fn cell(&self) -> u64 {
        match self {
            Self::NestedCall { cell, .. }
            | Self::Output { cell, .. }
            | Self::Waiting { cell, .. }
            | Self::Yielded { cell }
            | Self::Done { cell, .. } => *cell,
        }
    }
}

struct QueuedEvent {
    event: CellEvent,
    _bytes: OwnedSemaphorePermit,
}

type LossClasses = Arc<std::sync::Mutex<Option<LossClass>>>;
type ResultAcks = Arc<Mutex<HashMap<(u64, u64), oneshot::Sender<NestedDeliveryOutcome>>>>;

/// What one nested result delivery proved about the evaluator.
///
/// The three unsettled answers are not interchangeable. A refusal is the
/// evaluator's own answer about ownership, a non-delivery is the proof that
/// the result never reached the evaluator at all, and only a loss leaves
/// acceptance undetermined. [`Self::NotDelivered`] is reported only when the
/// result provably never left the host or the worker; anything else is
/// [`Self::Lost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NestedDeliveryOutcome {
    /// The evaluator settled the exact correlated request.
    Accepted,
    /// The result named no pending request of the owning cell: an unissued id,
    /// a foreign cell, or a repeat of a settled id. The cell is undisturbed
    /// and this is not a loss.
    Refused(String),
    /// The result was refused or failed before it was handed over, so the
    /// evaluator never saw it and cannot have acted on it.
    NotDelivered(String),
    /// The cell ended, or abandoned the batch carrying the result, before the
    /// evaluator observed it; nothing is unknown.
    NotObserved(String),
    /// The evaluator or its transport was lost with the result already handed
    /// over, so acceptance can be neither proved nor ruled out.
    Lost(String),
}

impl NestedDeliveryOutcome {
    /// The diagnostic an unsettled delivery carries.
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        match self {
            Self::Accepted => None,
            Self::Refused(message)
            | Self::NotDelivered(message)
            | Self::NotObserved(message)
            | Self::Lost(message) => Some(message),
        }
    }
}

impl From<DeliveryVerdict> for NestedDeliveryOutcome {
    fn from(verdict: DeliveryVerdict) -> Self {
        match verdict {
            DeliveryVerdict::Accepted => Self::Accepted,
            DeliveryVerdict::Refused(message) => Self::Refused(message),
            DeliveryVerdict::NotDelivered(message) => Self::NotDelivered(message),
            DeliveryVerdict::NotObserved(message) => Self::NotObserved(message),
        }
    }
}

/// One owned worker process with cancelable, bounded I/O tasks.
pub struct CellClient {
    child: Mutex<Child>,
    termination_loss: Mutex<Option<String>>,
    stopped_receipt: Mutex<Option<WorkerStop>>,
    pid: u32,
    commands: mpsc::Sender<Vec<u8>>,
    events: Mutex<mpsc::Receiver<QueuedEvent>>,
    loss: watch::Receiver<Option<String>>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    exit_usage: Arc<Mutex<Option<ChildProcessUsage>>>,
    result_acks: ResultAcks,
    loss_class: LossClasses,
}

impl CellClient {
    /// Spawns `program`, checks its `Hello` against `expected` and the
    /// protocol revision, and configures its engine.
    ///
    /// # Errors
    /// Refuses a worker that cannot start, says hello late, or names another
    /// revision, engine or adapter; it is killed before anything else is sent.
    pub async fn spawn(
        program: &Path,
        expected: WorkerIdentity,
        engine: EngineConfig,
    ) -> Result<Self, CellStartupError> {
        Self::spawn_within(program, expected, engine, WORKER_HELLO_TIMEOUT, None).await
    }

    /// Starts the Python worker with SDK type checking enabled.
    ///
    /// # Errors
    /// Returns the ordinary bounded worker-startup refusal.
    pub async fn spawn_python_sdk(program: &Path, stubs: String) -> Result<Self, CellStartupError> {
        Self::spawn_within(
            program,
            crate::MONTY_WORKER,
            EngineConfig::MONTY,
            WORKER_HELLO_TIMEOUT,
            Some(stubs),
        )
        .await
    }

    async fn spawn_within(
        program: &Path,
        expected: WorkerIdentity,
        engine: EngineConfig,
        hello_bound: Duration,
        sdk_stubs: Option<String>,
    ) -> Result<Self, CellStartupError> {
        let failure = |cause| CellStartupError::new(expected, cause);
        // Check the task owner before creating a process; construction never
        // hands off a child without its I/O supervision.
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| failure(e.to_string()))?;
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| failure(format!("failed to spawn cell worker: {e}")))?;
        let pid = child
            .id()
            .ok_or_else(|| failure("cell worker pid missing".into()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| failure("cell worker stdin missing".into()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| failure("cell worker stdout missing".into()))?;
        if let Err(error) = handshake(
            &mut stdin,
            &mut stdout,
            expected,
            engine,
            hello_bound,
            sdk_stubs,
        )
        .await
        {
            return Err(refuse(&mut child, error).await);
        }
        let (commands, mut command_rx) = mpsc::channel::<Vec<u8>>(COMMAND_QUEUE_ITEMS);
        let (event_tx, events) = mpsc::channel(QUEUE_ITEMS);
        let (loss_tx, loss) = watch::channel(None);
        let (stop, mut writer_stop) = watch::channel(false);
        let reader_stop = stop.subscribe();
        let exit_usage = Arc::new(Mutex::new(None));
        let captured_usage = Arc::clone(&exit_usage);
        let writer_loss = loss_tx.clone();
        let writer = runtime.spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = writer_stop.changed() => break,
                    command = command_rx.recv() => {
                        let Some(command) = command else { break };
                        let result = tokio::select! {
                            biased;
                            _ = writer_stop.changed() => break,
                            result = stdin.write_all(&command) => result,
                        };
                        if let Err(error) = result {
                            writer_loss.send_replace(Some(format!("cell worker write failed: {error}")));
                            break;
                        }
                    }
                }
            }
        });
        let result_acks = Arc::new(Mutex::new(HashMap::new()));
        let loss_class = LossClasses::default();
        let reader = runtime.spawn(read_worker(
            stdout,
            reader_stop,
            loss_tx,
            event_tx,
            captured_usage,
            Arc::clone(&result_acks),
            Arc::clone(&loss_class),
        ));
        Ok(Self {
            child: Mutex::new(child),
            pid,
            commands,
            events: Mutex::new(events),
            loss,
            stop,
            tasks: Mutex::new(vec![reader, writer]),
            exit_usage,
            result_acks,
            termination_loss: Mutex::new(None),
            stopped_receipt: Mutex::new(None),
            loss_class,
        })
    }

    #[must_use]
    pub fn child_id(&self) -> Option<u32> {
        Some(self.pid)
    }

    /// Starts one cell.
    ///
    /// # Errors
    /// Refuses an oversized start or an unavailable worker.
    pub fn start_cell(
        &self,
        cell: u64,
        source: &str,
        prelude: CellPrelude,
        window: Option<u64>,
    ) -> Result<(), String> {
        if source.len() > MAX_FRAME_BYTES {
            return Err("resource limit: cell source".into());
        }
        let prelude = if let CellPrelude::MontySdk { bootstrap } = prelude {
            self.send(&HostFrame::Reset)?;
            CellPrelude::Monty {
                bootstrap: Some(bootstrap),
            }
        } else {
            prelude
        };
        self.send(&HostFrame::StartCell {
            cell,
            source: source.to_owned(),
            prelude,
            window,
        })
    }

    /// Resumes an engine-parked yield.
    ///
    /// # Errors
    /// Refuses an unavailable worker.
    pub fn resume(&self, cell: u64, window: u64) -> Result<(), String> {
        self.send(&HostFrame::Resume { cell, window })
    }

    /// Delivers a nested outcome, keeping the worker's refusal, a proved
    /// non-delivery, and a genuine loss distinct from one another.
    ///
    /// A failure on the way out is a non-delivery: the frame never entered
    /// the command queue, so the worker never saw the result. Only a
    /// transport or evaluator loss with the frame already written leaves
    /// acceptance undetermined.
    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    pub async fn deliver(
        &self,
        cell: u64,
        call: u64,
        outcome: Result<CellValue, String>,
    ) -> NestedDeliveryOutcome {
        let (tx, rx) = oneshot::channel();
        let key = (cell, call);
        {
            let mut pending = self.result_acks.lock().await;
            if pending.len() >= 64 || pending.contains_key(&key) {
                // Refused here, before `send`: nothing was written and the
                // worker never learned of this result.
                return NestedDeliveryOutcome::NotDelivered(
                    "resource limit: pending result acknowledgements".into(),
                );
            }
            pending.insert(key, tx);
        }
        let outcome = match self.send(&HostFrame::NestedResult {
            cell,
            call,
            outcome,
        }) {
            Ok(()) => tokio::select! {
                biased;
                result = rx => result.unwrap_or_else(|_| NestedDeliveryOutcome::Lost(
                    "cell result acknowledgement lost".to_owned(),
                )),
                loss = self.wait_loss() => NestedDeliveryOutcome::Lost(loss),
            },
            // The frame never entered the command queue.
            Err(error) => NestedDeliveryOutcome::NotDelivered(error),
        };
        self.result_acks.lock().await.remove(&key);
        outcome
    }

    /// Cooperative interrupt of the running cell.
    pub fn cancel(&self, cell: u64) {
        let _ = self.send(&HostFrame::Cancel { cell });
    }

    /// The class the worker named in its `HostLost`, if it sent one.
    #[must_use]
    pub fn loss_class(&self) -> Option<LossClass> {
        *self
            .loss_class
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the worker was stopped or killed.
    #[must_use]
    pub fn is_stopped(&self) -> bool {
        *self.stop.borrow()
    }

    /// Whether the worker is already lost.
    #[must_use]
    pub fn is_lost(&self) -> bool {
        self.loss.borrow().is_some()
    }

    /// Independent terminal control: remains observable while a nested reply is pending.
    pub async fn wait_loss(&self) -> String {
        let mut loss = self.loss.clone();
        loop {
            if let Some(error) = loss.borrow().clone() {
                return error;
            }
            if loss.changed().await.is_err() {
                return "cell worker control closed".into();
            }
        }
    }

    /// The next cell event, or the worker's loss.
    ///
    /// # Errors
    /// Returns the loss once the worker is lost and its queued events drained.
    pub async fn read_event(&self) -> Result<CellEvent, String> {
        let mut events = self.events.lock().await;
        // Preserve already queued events before publishing transport loss.
        tokio::select! {
            biased;
            event = events.recv() => event.map(|queued| queued.event).ok_or_else(|| self.loss.borrow().clone().unwrap_or_else(|| "cell worker closed".into())),
            loss = self.wait_loss() => Err(loss),
        }
    }

    /// Shuts the worker down and reaps it within the settle bound.
    ///
    /// # Errors
    /// A worker that did not acknowledge shutdown, or stayed unreaped after
    /// its kill, is a sticky loss.
    pub async fn terminate(&self) -> Result<Option<ChildProcessUsage>, String> {
        let stopped = self.stop().await?;
        if stopped.forced {
            let loss = "cell worker did not acknowledge shutdown within 2s; killed".to_owned();
            *self.termination_loss.lock().await = Some(loss.clone());
            return Err(loss);
        }
        Ok(stopped.usage)
    }

    /// Shuts the worker down like [`Self::terminate`], but reports a worker
    /// killed after the voluntary bound as a forced stop instead of a loss.
    ///
    /// # Errors
    /// A worker that stayed unreaped after its kill, or an earlier sticky loss.
    pub async fn stop(&self) -> Result<WorkerStop, String> {
        self.result_acks.lock().await.clear();
        let before = crate::sample_children_rusage();
        let mut child = self.child.lock().await;
        let mut previous_loss = self.termination_loss.lock().await;
        if let Some(loss) = previous_loss.as_ref() {
            return Err(loss.clone());
        }
        if let Some(receipt) = *self.stopped_receipt.lock().await {
            return Ok(receipt);
        }
        let _ = self.send(&HostFrame::Shutdown);
        let voluntary = tokio::time::timeout(WORKER_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await;
        let (forced, mut loss) = match voluntary {
            Ok(Ok(_)) => (false, None),
            Ok(Err(error)) => (true, Some(format!("cell worker wait failed: {error}"))),
            Err(_) => (true, None),
        };
        self.stop.send_replace(true);
        if forced {
            let _ = child.start_kill();
            if !matches!(
                tokio::time::timeout(WORKER_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await,
                Ok(Ok(_))
            ) {
                loss = Some("cell worker remained unreaped after kill deadline".into());
            }
        }
        for task in self.tasks.lock().await.drain(..) {
            // Async pipe operations are cancellation safe; abort is followed by join.
            task.abort();
            let _ = task.await;
        }
        if let Some(loss) = loss {
            *previous_loss = Some(loss.clone());
            return Err(loss);
        }
        let reported = self.exit_usage.lock().await.take();
        let delta = crate::children_rusage_delta(before, crate::sample_children_rusage());
        let usage = reported.or_else(|| {
            let usage = ChildProcessUsage {
                user_cpu: Duration::from_nanos(delta.user_cpu_ns),
                system_cpu: Duration::from_nanos(delta.system_cpu_ns),
                peak_rss_bytes: delta.peak_rss_bytes,
            };
            usage.is_meaningful().then_some(usage)
        });
        let receipt = WorkerStop { usage, forced };
        *self.stopped_receipt.lock().await = Some(receipt);
        Ok(receipt)
    }

    /// Kills the worker without asking it to stop and records `reason` as
    /// its sticky loss; a later [`Self::terminate`] reports it.
    pub async fn kill(&self, reason: &str) {
        self.result_acks.lock().await.clear();
        let mut child = self.child.lock().await;
        let mut previous_loss = self.termination_loss.lock().await;
        if previous_loss.is_some() {
            return;
        }
        let _ = child.start_kill();
        let reaped = matches!(
            tokio::time::timeout(WORKER_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await,
            Ok(Ok(_))
        );
        self.stop.send_replace(true);
        for task in self.tasks.lock().await.drain(..) {
            task.abort();
            let _ = task.await;
        }
        *previous_loss = Some(if reaped {
            reason.to_owned()
        } else {
            format!("{reason}; cell worker remained unreaped after kill deadline")
        });
    }

    fn send(&self, frame: &HostFrame) -> Result<(), String> {
        if let Some(loss) = self.loss.borrow().as_ref() {
            return Err(loss.clone());
        }
        let encoded = encode_frame(frame)?;
        self.commands
            .try_send(encoded)
            .map_err(|_| "resource limit: cell worker command queue unavailable".into())
    }
}

impl Drop for CellClient {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Ok(mut tasks) = self.tasks.try_lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        // Child's kill_on_drop owns fallback reaping; no destructor writes to a pipe.
    }
}

async fn handshake(
    stdin: &mut ChildStdin,
    stdout: &mut ChildStdout,
    expected: WorkerIdentity,
    engine: EngineConfig,
    bound: Duration,
    sdk_stubs: Option<String>,
) -> Result<(), CellStartupError> {
    let failure = |cause| CellStartupError::new(expected, cause);
    let deadline = tokio::time::Instant::now() + bound;
    let hello = tokio::time::timeout_at(deadline, read_frame(stdout, MAX_FRAME_BYTES)).await;
    let frame = match hello {
        Ok(Ok(Some(frame))) => serde_json::from_slice::<WorkerFrame>(&frame)
            .map_err(|_| failure("cell worker sent a malformed hello".into())),
        Ok(Ok(None)) => Err(failure("cell worker closed before hello".into())),
        Ok(Err(error)) => Err(failure(error)),
        Err(_) => Err(CellStartupError::timeout(
            expected,
            bound,
            "cell worker did not say hello in time".into(),
        )),
    }?;
    check_hello(&frame, expected).map_err(failure)?;
    let configure = encode_frame(&HostFrame::Configure {
        sdk_stubs,
        revision: CELL_PROTOCOL_REVISION.to_owned(),
        engine,
    })
    .map_err(failure)?;
    match tokio::time::timeout_at(deadline, stdin.write_all(&configure)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(failure(format!("cell worker configure failed: {error}"))),
        Err(_) => Err(CellStartupError::timeout(
            expected,
            bound,
            "cell worker configure timed out".into(),
        )),
    }
}

/// Kills a worker refused before it was configured, and reaps it within the
/// settle bound.
async fn refuse(child: &mut Child, error: CellStartupError) -> CellStartupError {
    let _ = child.start_kill();
    match tokio::time::timeout(WORKER_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => error,
        _ => error.unreaped(),
    }
}

fn check_hello(frame: &WorkerFrame, expected: WorkerIdentity) -> Result<(), String> {
    match frame {
        WorkerFrame::Hello {
            revision,
            engine,
            adapter,
        } if revision == CELL_PROTOCOL_REVISION
            && engine == expected.engine
            && adapter == expected.adapter =>
        {
            Ok(())
        }
        WorkerFrame::Hello { revision, .. } if revision != CELL_PROTOCOL_REVISION => Err(format!(
            "cell worker speaks {}, not {CELL_PROTOCOL_REVISION}",
            hello_diagnostic(revision)
        )),
        WorkerFrame::Hello {
            engine, adapter, ..
        } => Err(format!(
            "cell worker is {}/{}, not {}/{}",
            hello_diagnostic(engine),
            hello_diagnostic(adapter),
            expected.engine,
            expected.adapter
        )),
        _ => Err("cell worker did not open with hello".into()),
    }
}

fn hello_diagnostic(value: &str) -> String {
    const LIMIT: usize = 64;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT - 3;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

async fn read_worker(
    mut stdout: ChildStdout,
    mut reader_stop: watch::Receiver<bool>,
    loss_tx: watch::Sender<Option<String>>,
    event_tx: mpsc::Sender<QueuedEvent>,
    captured_usage: Arc<Mutex<Option<ChildProcessUsage>>>,
    result_acks: ResultAcks,
    loss_class: LossClasses,
) {
    let bytes = Arc::new(Semaphore::new(MAX_QUEUED_BYTES));
    loop {
        let frame = tokio::select! {
            biased;
            _ = reader_stop.changed() => return,
            result = read_frame(&mut stdout, MAX_FRAME_BYTES) => result,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                loss_tx.send_replace(Some("cell worker closed".into()));
                return;
            }
            Err(error) => {
                loss_tx.send_replace(Some(error));
                return;
            }
        };
        let Ok(permit) = Arc::clone(&bytes)
            .try_acquire_many_owned(u32::try_from(frame.len()).unwrap_or(u32::MAX))
        else {
            loss_tx.send_replace(Some("resource limit: cell worker queued bytes".into()));
            return;
        };
        let Ok(frame) = serde_json::from_slice::<WorkerFrame>(&frame) else {
            loss_tx.send_replace(Some("cell worker malformed frame".into()));
            return;
        };
        let event = match frame {
            WorkerFrame::Delivery {
                cell,
                call,
                verdict,
            } => {
                if let Some(tx) = result_acks.lock().await.remove(&(cell, call)) {
                    let _ = tx.send(verdict.into());
                    continue;
                }
                loss_tx.send_replace(Some("cell worker unexpected delivery verdict".into()));
                return;
            }
            WorkerFrame::HostLost { class, message } => {
                *loss_class
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(class);
                loss_tx.send_replace(Some(message));
                return;
            }
            WorkerFrame::ResourceReport { usage } => {
                *captured_usage.lock().await = Some(ChildProcessUsage {
                    user_cpu: Duration::from_nanos(usage.user_cpu_ns),
                    system_cpu: Duration::from_nanos(usage.system_cpu_ns),
                    peak_rss_bytes: usage.peak_rss_bytes,
                });
                continue;
            }
            WorkerFrame::Hello { .. } => {
                loss_tx.send_replace(Some("cell worker repeated hello".into()));
                return;
            }
            WorkerFrame::NestedCall {
                cell,
                call,
                name,
                input,
            } => CellEvent::NestedCall {
                cell,
                call,
                name,
                input,
            },
            WorkerFrame::Output { cell, item } => CellEvent::Output { cell, item },
            WorkerFrame::Waiting { cell, calls } => CellEvent::Waiting { cell, calls },
            WorkerFrame::Yielded { cell } => CellEvent::Yielded { cell },
            WorkerFrame::CellDone { cell, outcome } => CellEvent::Done { cell, outcome },
        };
        if event_tx
            .try_send(QueuedEvent {
                event,
                _bytes: permit,
            })
            .is_err()
        {
            loss_tx.send_replace(Some("resource limit: cell worker event queue".into()));
            return;
        }
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "wire guard regressions")]
mod tests;
