//! Engine-neutral session over one [`CellClient`]: the cell table, output
//! buffers, observation by window, nested-call correlation and the retained
//! `store` map. A session runs one cell at a time.
//!
//! Two observation modes exist. An engine-parked cell (`QuickJS`) stops at
//! each yield until the host resumes it. A host-window cell (Monty) keeps
//! running; an observation that ends before its terminal returns the output
//! buffered so far, and a request bound kills a worker that computes without
//! reaching a host-visible boundary.

use std::{
    collections::{BTreeSet, HashMap},
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::client::{CellClient, CellEvent, ChildProcessUsage, NestedDeliveryOutcome, WorkerStop};
use crate::frame::encode_frame;
use crate::protocol::{
    CellOutcome, CellPrelude, CellValue, MAX_CELL_VALUE_DEPTH, OutputItem, json_depth,
};

mod parked;
use parked::{ParkedCell, observe_parked};

/// Output a cell may buffer before its next observation.
pub const MAX_COLLECTED_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Output items a cell may buffer before its next observation.
pub const MAX_COLLECTED_OUTPUT_ITEMS: usize = 4096;

/// How a session observes its running cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationMode {
    /// The worker parks the cell at each yield until the host resumes it.
    EngineParked,
    /// The cell keeps running between observations. `request_bound` limits
    /// how long the worker computes without a nested call, a wait on nested
    /// results or a terminal before it is killed.
    HostWindow { request_bound: Duration },
    /// One foreground terminal, bounded across compute and external waits.
    Foreground {
        request_bound: Duration,
        budget: Duration,
    },
}

/// What an observation of a cell returns.
#[derive(Clone, Debug, PartialEq)]
pub enum CellResponse {
    /// The cell parked; it continues on the next observation.
    Yielded { cell: u64, items: Vec<OutputItem> },
    /// The cell ended, with its error text when it failed.
    Result {
        cell: u64,
        items: Vec<OutputItem>,
        error: Option<String>,
    },
}

/// An observation's answer, or the loss that ended the cell.
pub type CellCompletion = Result<CellResponse, String>;

/// A started cell and its first observation.
pub struct StartedCell {
    pub cell: u64,
    pub initial: Pin<Box<dyn Future<Output = CellCompletion> + Send>>,
}

/// The answer to a later observation of a cell.
#[derive(Clone, Debug, PartialEq)]
pub enum WaitOutcome {
    Live(CellResponse),
    /// No such cell is running; `error` explains an id that is not a cell id.
    Missing {
        error: Option<String>,
    },
    Lost(String),
}

/// A request from the running cell that the owner must answer.
pub enum CellSessionEvent {
    NestedCall {
        cell: u64,
        call: u64,
        name: String,
        input: Value,
        responder: NestedResponder,
    },
    Notification {
        cell: u64,
        text: String,
        responder: NotificationResponder,
    },
}

type ParkedCells = Arc<Mutex<HashMap<u64, Arc<ParkedCell>>>>;
type Windows = Arc<Mutex<HashMap<u64, Arc<WindowCell>>>>;

/// The session's state; cheap to share with its event receiver.
pub struct CellSession {
    client: Arc<CellClient>,
    mode: ObservationMode,
    next_cell: AtomicU64,
    stored: Arc<Mutex<HashMap<String, Value>>>,
    events_tx: mpsc::Sender<CellSessionEvent>,
    completions: ParkedCells,
    streams: ParkedCells,
    windows: Windows,
    generation_lost: Arc<AtomicBool>,
    pump: Mutex<Option<PumpTask>>,
}

/// The session's requests, in the order the cell made them.
pub struct CellSessionEvents {
    rx: mpsc::Receiver<CellSessionEvent>,
}

impl CellSessionEvents {
    /// The next request, or `None` once the session is gone.
    pub async fn receive(&mut self) -> Option<CellSessionEvent> {
        self.rx.recv().await
    }
}

struct PumpTask(Option<tokio::task::JoinHandle<()>>);

impl Drop for PumpTask {
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.abort();
        }
    }
}

/// The cell, first observation window and request bound of a host-window start.
struct WindowStart {
    cell: u64,
    window: u64,
    request_bound: Duration,
    foreground: Option<Duration>,
}

/// A host-window cell: output not yet observed, and its terminal once known.
struct WindowCell {
    output: std::sync::Mutex<CollectedOutput>,
    terminal: watch::Sender<Option<Result<WindowTerminal, String>>>,
    submitted: watch::Sender<BTreeSet<u64>>,
}

#[derive(Clone)]
struct WindowTerminal {
    value: Option<String>,
    error: Option<String>,
}

impl WindowTerminal {
    fn from_outcome(outcome: CellOutcome, generation_lost: &AtomicBool) -> Self {
        match outcome {
            CellOutcome::Stopped { reason } => {
                generation_lost.store(true, Ordering::Release);
                Self {
                    value: None,
                    error: Some(format!("cell stopped: {reason:?}")),
                }
            }
            CellOutcome::Completed { value, .. } => Self { value, error: None },
            CellOutcome::Failed {
                message,
                generation_lost: lost,
                ..
            } => {
                if lost {
                    generation_lost.store(true, Ordering::Release);
                }
                Self {
                    value: None,
                    error: Some(message),
                }
            }
        }
    }
}

impl WindowCell {
    fn take_submitted(&self, pending: &BTreeSet<u64>) -> bool {
        if !pending.is_subset(&self.submitted.borrow()) {
            return false;
        }
        self.submitted
            .send_modify(|submitted| submitted.retain(|id| !pending.contains(id)));
        true
    }

    fn drain(&self) -> Vec<OutputItem> {
        std::mem::take(
            &mut self
                .output
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .items,
        )
    }
}

impl CellSession {
    /// A session over `client` and the receiver of its requests.
    #[must_use]
    pub fn new(client: Arc<CellClient>, mode: ObservationMode) -> (Self, CellSessionEvents) {
        let (events_tx, rx) = mpsc::channel(64);
        (
            Self {
                client,
                mode,
                next_cell: AtomicU64::new(1),
                stored: Arc::new(Mutex::new(HashMap::new())),
                events_tx,
                completions: Arc::new(Mutex::new(HashMap::new())),
                streams: Arc::new(Mutex::new(HashMap::new())),
                windows: Arc::new(Mutex::new(HashMap::new())),
                generation_lost: Arc::new(AtomicBool::new(false)),
                pump: Mutex::new(None),
            },
            CellSessionEvents { rx },
        )
    }

    /// Whether a definite cell outcome cost this session its interpreter, so
    /// the owner must start a fresh one instead of reusing it.
    #[must_use]
    pub fn generation_lost(&self) -> bool {
        self.generation_lost.load(Ordering::Acquire)
    }

    /// Marks the interpreter as unusable for further cells.
    pub fn lose_generation(&self) {
        self.generation_lost.store(true, Ordering::Release);
    }

    /// The host-window cell whose terminal has not been observed yet.
    pub async fn active_cell(&self) -> Option<u64> {
        self.windows.lock().await.keys().copied().min()
    }

    /// Whether `cell` is a host-window cell whose terminal is buffered.
    pub async fn terminal_ready(&self, cell: u64) -> bool {
        self.windows
            .lock()
            .await
            .get(&cell)
            .is_some_and(|entry| entry.terminal.borrow().is_some())
    }

    /// Consumes a cell id without starting a cell, for a refused start.
    pub fn skip_cell(&self) -> u64 {
        self.next_cell.fetch_add(1, Ordering::Relaxed)
    }

    /// Starts one cell after `admit` grants the start. A refused admission
    /// starts nothing. For `QuickJS` the session supplies the retained
    /// `stored` map in place of the prelude's.
    ///
    /// # Errors
    /// The admission's refusal, or a loss when another cell is active or the
    /// worker refuses the start.
    pub async fn execute<E>(
        &self,
        source: &str,
        prelude: CellPrelude,
        window: u64,
        admit: impl Future<Output = Result<(), E>>,
    ) -> Result<StartedCell, CellStartError<E>> {
        let mut pump = self.pump.lock().await;
        if !self.completions.lock().await.is_empty() || !self.windows.lock().await.is_empty() {
            return Err(CellStartError::Lost("cell is already active".into()));
        }
        if let Some(mut previous) = pump.take()
            && let Some(task) = previous.0.take()
        {
            let _ = task.await;
        }
        let cell = self.next_cell.fetch_add(1, Ordering::Relaxed);
        let prelude = match prelude {
            CellPrelude::Quickjs { tools, .. } => CellPrelude::Quickjs {
                tools,
                stored: self.stored.lock().await.clone(),
            },
            monty @ (CellPrelude::Monty { .. } | CellPrelude::MontySdk { .. }) => monty,
        };
        match self.mode {
            ObservationMode::EngineParked => {
                self.start_parked(cell, source, prelude, window, admit, &mut pump)
                    .await
            }
            ObservationMode::HostWindow { request_bound } => {
                let start = WindowStart {
                    cell,
                    window,
                    request_bound,
                    foreground: None,
                };
                self.start_windowed(start, source, prelude, admit, &mut pump)
                    .await
            }
            ObservationMode::Foreground {
                request_bound,
                budget,
            } => {
                let start = WindowStart {
                    cell,
                    window: u64::MAX,
                    request_bound,
                    foreground: Some(budget),
                };
                self.start_windowed(start, source, prelude, admit, &mut pump)
                    .await
            }
        }
    }

    async fn start_parked<E>(
        &self,
        cell: u64,
        source: &str,
        prelude: CellPrelude,
        window: u64,
        admit: impl Future<Output = Result<(), E>>,
        pump: &mut Option<PumpTask>,
    ) -> Result<StartedCell, CellStartError<E>> {
        let rx = Arc::new(ParkedCell::default());
        self.completions.lock().await.insert(cell, Arc::clone(&rx));
        self.streams.lock().await.insert(cell, Arc::clone(&rx));
        let started = match admit.await {
            Err(error) => Err(CellStartError::Refused(error)),
            Ok(()) => self
                .client
                .start_cell(cell, source, prelude, Some(window))
                .map_err(CellStartError::Lost),
        };
        if let Err(error) = started {
            self.completions.lock().await.remove(&cell);
            self.streams.lock().await.remove(&cell);
            return Err(error);
        }
        *pump = Some(PumpTask(Some(tokio::spawn(pump_parked(
            self.pump_state(cell),
        )))));
        let streams = Arc::clone(&self.streams);
        Ok(StartedCell {
            cell,
            initial: Box::pin(observe_parked(streams, cell, rx)),
        })
    }

    async fn start_windowed<E>(
        &self,
        WindowStart {
            cell,
            window,
            request_bound,
            foreground,
        }: WindowStart,
        source: &str,
        prelude: CellPrelude,
        admit: impl Future<Output = Result<(), E>>,
        pump: &mut Option<PumpTask>,
    ) -> Result<StartedCell, CellStartError<E>> {
        let entry = Arc::new(WindowCell {
            output: std::sync::Mutex::new(CollectedOutput::default()),
            terminal: watch::channel(None).0,
            submitted: watch::channel(BTreeSet::new()).0,
        });
        self.windows.lock().await.insert(cell, Arc::clone(&entry));
        let started = match admit.await {
            Err(error) => Err(CellStartError::Refused(error)),
            Ok(()) => self
                .client
                .start_cell(cell, source, prelude, None)
                .map_err(CellStartError::Lost),
        };
        if let Err(error) = started {
            self.windows.lock().await.remove(&cell);
            return Err(error);
        }
        *pump = Some(PumpTask(Some(tokio::spawn(pump_windowed(
            self.pump_state(cell),
            entry,
            request_bound,
            Arc::clone(&self.generation_lost),
            foreground,
        )))));
        let windows = Arc::clone(&self.windows);
        Ok(StartedCell {
            cell,
            initial: Box::pin(observe_window(windows, cell, window)),
        })
    }

    fn pump_state(&self, cell: u64) -> Pump {
        Pump {
            client: Arc::clone(&self.client),
            cell,
            events_tx: self.events_tx.clone(),
            completions: Arc::clone(&self.completions),
            stored: Arc::clone(&self.stored),
        }
    }

    /// Observes `cell` for another `window`: a buffered response first,
    /// otherwise a resume of its parked yield or another host window.
    pub async fn wait(&self, cell: &str, window: u64) -> WaitOutcome {
        if let Some(buffered) = self.try_take(cell).await
            && !matches!(&buffered, WaitOutcome::Live(CellResponse::Yielded { items, .. }) if items.is_empty())
        {
            return buffered;
        }
        let Ok(cell) = cell.parse::<u64>() else {
            return WaitOutcome::Missing {
                error: Some("invalid cell id".into()),
            };
        };
        if matches!(
            self.mode,
            ObservationMode::HostWindow { .. } | ObservationMode::Foreground { .. }
        ) {
            if !self.windows.lock().await.contains_key(&cell) {
                return WaitOutcome::Missing { error: None };
            }
            return match observe_window(Arc::clone(&self.windows), cell, window).await {
                Ok(response) => WaitOutcome::Live(response),
                Err(loss) => WaitOutcome::Lost(loss),
            };
        }
        let Some(rx) = self.streams.lock().await.get(&cell).cloned() else {
            return WaitOutcome::Missing { error: None };
        };
        if rx.begin_resume() {
            // A lost worker cannot resume, but the pump still answers with
            // the terminal the worker sent before it ended, or the loss.
            if let Err(error) = self.client.resume(cell, window)
                && !self.client.is_lost()
            {
                self.streams.lock().await.remove(&cell);
                return WaitOutcome::Lost(error);
            }
        }
        match observe_parked(Arc::clone(&self.streams), cell, rx).await {
            Ok(response) => WaitOutcome::Live(response),
            Err(loss) => WaitOutcome::Lost(loss),
        }
    }

    /// Takes a buffered observation without issuing Resume or waiting for output.
    pub async fn try_take(&self, cell: &str) -> Option<WaitOutcome> {
        let cell = cell.parse::<u64>().ok()?;
        if !matches!(self.mode, ObservationMode::EngineParked) {
            if !self.terminal_ready(cell).await {
                return None;
            }
            return Some(
                match observe_window(Arc::clone(&self.windows), cell, 0).await {
                    Ok(response) => WaitOutcome::Live(response),
                    Err(loss) => WaitOutcome::Lost(loss),
                },
            );
        }
        let mut streams = self.streams.lock().await;
        let response = streams.get(&cell)?.take(cell)?;
        if !matches!(response, Ok(CellResponse::Yielded { .. })) {
            streams.remove(&cell);
        }
        Some(match response {
            Ok(response) => WaitOutcome::Live(response),
            Err(loss) => WaitOutcome::Lost(loss),
        })
    }

    /// Rearms an in-flight parked observation for the next worker idle boundary.
    ///
    /// # Errors
    /// Invalid cell ids or resume-admission failures. A disconnected worker
    /// is observed through its outstanding wait. Window-only observers need no rearm.
    pub async fn expedite(&self, cell: &str) -> Result<(), String> {
        if !matches!(self.mode, ObservationMode::EngineParked) {
            return Ok(());
        }
        let cell = cell
            .parse::<u64>()
            .map_err(|_| "invalid cell id".to_owned())?;
        // The pump still drains a terminal sent before transport loss.
        if self
            .streams
            .lock()
            .await
            .get(&cell)
            .is_some_and(|entry| entry.resuming())
            && let Err(loss) = self.client.resume(cell, 0)
            && !self.client.is_lost()
        {
            return Err(loss);
        }
        Ok(())
    }

    /// Stops the session's cell and shuts its worker down.
    ///
    /// # Errors
    /// The worker's loss when it could not be shut down and reaped cleanly.
    pub async fn shutdown(&self) -> Result<Option<ChildProcessUsage>, String> {
        if let Some(mut owner) = self.pump.lock().await.take()
            && let Some(task) = owner.0.take()
        {
            task.abort();
            let _ = task.await;
        }
        self.end_parked().await;
        end_windows(&self.windows).await;
        self.client.terminate().await
    }

    /// Stops the session's cell and its worker like [`Self::shutdown`], but
    /// reports a worker killed after the voluntary bound as a forced stop.
    ///
    /// # Errors
    /// The worker's loss when it stayed unreaped, or an earlier sticky loss.
    pub async fn stop(&self) -> Result<WorkerStop, String> {
        if let Some(mut owner) = self.pump.lock().await.take()
            && let Some(task) = owner.0.take()
        {
            task.abort();
            let _ = task.await;
        }
        self.end_parked().await;
        end_windows(&self.windows).await;
        self.client.stop().await
    }

    async fn end_parked(&self) {
        for (_, entry) in self.completions.lock().await.drain() {
            entry.publish(Err("cell worker stopped".into()));
        }
        self.streams.lock().await.clear();
    }
}

/// Ends every host-window cell with a stop, so an observation in progress
/// returns instead of waiting out its window.
async fn end_windows(windows: &Windows) {
    for (_, entry) in windows.lock().await.drain() {
        entry.terminal.send_if_modified(|terminal| {
            let unset = terminal.is_none();
            if unset {
                *terminal = Some(Err("cell worker stopped".into()));
            }
            unset
        });
    }
}

/// Waits up to `window` for a host-window cell's terminal. Without one the
/// cell yields the output buffered so far and keeps running.
async fn observe_window(windows: Windows, cell: u64, window: u64) -> CellCompletion {
    let entry = windows
        .lock()
        .await
        .get(&cell)
        .cloned()
        .ok_or_else(|| "cell observation is gone".to_owned())?;
    let mut terminal = entry.terminal.subscribe();
    let terminal = match tokio::time::timeout(Duration::from_millis(window), async {
        terminal
            .wait_for(Option::is_some)
            .await
            .map(|value| value.clone())
    })
    .await
    {
        Err(_) => {
            return Ok(CellResponse::Yielded {
                cell,
                items: entry.drain(),
            });
        }
        Ok(Ok(terminal)) => terminal,
        Ok(Err(_)) => None,
    };
    windows.lock().await.remove(&cell);
    match terminal {
        Some(Ok(WindowTerminal { value, error })) => {
            let mut items = entry.drain();
            items.extend(value.map(|text| OutputItem::Text { text }));
            Ok(CellResponse::Result { cell, items, error })
        }
        Some(Err(loss)) => Err(loss),
        None => Err("cell ended".into()),
    }
}

/// Why a cell did not start.
#[derive(Debug)]
pub enum CellStartError<E> {
    /// The owner's admission refused the start.
    Refused(E),
    /// The session or worker could not start it.
    Lost(String),
}

struct Pump {
    client: Arc<CellClient>,
    cell: u64,
    events_tx: mpsc::Sender<CellSessionEvent>,
    completions: ParkedCells,
    stored: Arc<Mutex<HashMap<String, Value>>>,
}

async fn pump_parked(pump: Pump) {
    let mut collected = CollectedOutput::default();
    loop {
        let event = match pump.client.read_event().await {
            Ok(event) => event,
            Err(error) => {
                pump.lose(error).await;
                break;
            }
        };
        if event.cell() != pump.cell {
            pump.lose("cell worker named a cell that is not running".into())
                .await;
            break;
        }
        match event {
            CellEvent::NestedCall {
                call, name, input, ..
            } => {
                let Some((event, replied)) = pump.nested_call(call, name, input, None).await else {
                    break;
                };
                if !pump.forward(event, replied).await {
                    break;
                }
            }
            CellEvent::Output {
                item: OutputItem::Notify { text },
                ..
            } => {
                let (ack, acknowledged) = oneshot::channel();
                let event = CellSessionEvent::Notification {
                    cell: pump.cell,
                    text,
                    responder: NotificationResponder { ack: Some(ack) },
                };
                if !pump.forward(event, acknowledged).await {
                    break;
                }
            }
            CellEvent::Output { item, .. } => {
                collected.append_truncated(item);
            }
            CellEvent::Waiting { .. } => {
                pump.lose("cell worker waited inside an engine-parked cell".into())
                    .await;
                break;
            }
            CellEvent::Yielded { .. } => {
                if let Some(entry) = pump.completions.lock().await.get(&pump.cell) {
                    let output = std::mem::take(&mut collected);
                    entry.publish_collected(
                        Ok(CellResponse::Yielded {
                            cell: pump.cell,
                            items: output.items,
                        }),
                        output.truncated,
                    );
                }
            }
            CellEvent::Done { outcome, .. } => {
                let (stored, error) = match outcome {
                    CellOutcome::Stopped { reason } => {
                        (HashMap::new(), Some(format!("cell stopped: {reason:?}")))
                    }
                    CellOutcome::Completed { stored, .. } => (stored, None),
                    CellOutcome::Failed {
                        message, stored, ..
                    } => (stored, Some(message)),
                };
                pump.finish(stored, collected, error).await;
                break;
            }
        }
    }
}

/// Pumps a host-window cell: output is buffered for the next observation,
/// nested calls are published without waiting for their answers, and the
/// request bound runs while the worker computes.
async fn pump_windowed(
    pump: Pump,
    entry: Arc<WindowCell>,
    request_bound: Duration,
    generation_lost: Arc<AtomicBool>,
    foreground: Option<Duration>,
) {
    let end = |result: Result<WindowTerminal, String>| {
        entry.terminal.send_replace(Some(result));
    };
    let mut deadline = Some(tokio::time::Instant::now() + request_bound);
    let foreground = foreground.map(|budget| tokio::time::Instant::now() + budget);
    let mut submitted = entry.submitted.subscribe();
    let mut waiting = BTreeSet::new();
    loop {
        let event = tokio::select! {
            ()=sleep_until(foreground) => {
                match pump.client.stop().await {
                    Ok(_) => end(Ok(WindowTerminal::from_outcome(CellOutcome::Stopped{reason:crate::CellStopReason::ForegroundBudget},&generation_lost))),
                    Err(loss) => end(Err(loss)),
                }
                break;
            }
            event = pump.client.read_event() => event,
            changed = submitted.changed() => {
                if changed.is_ok() && !waiting.is_empty() && entry.take_submitted(&waiting) {
                    deadline = Some(tokio::time::Instant::now() + request_bound);
                    waiting.clear();
                }
                continue;
            }
            () = sleep_until(deadline) => {
                let loss = format!(
                    "cell worker request timed out after {}s; killed",
                    request_bound.as_secs()
                );
                pump.client.kill(&loss).await;
                end(Err(loss));
                break;
            }
        };
        let event = match event {
            Ok(event) if event.cell() == pump.cell => event,
            Ok(_) => {
                let loss = "cell worker named a cell that is not running".to_owned();
                let _ = pump.client.terminate().await;
                end(Err(loss));
                break;
            }
            Err(loss) => {
                let _ = pump.client.terminate().await;
                end(Err(loss));
                break;
            }
        };
        match event {
            CellEvent::NestedCall {
                call, name, input, ..
            } => {
                deadline = Some(tokio::time::Instant::now() + request_bound);
                if let Err(loss) = pump.forward_windowed(&entry, call, name, input).await {
                    end(Err(loss));
                    break;
                }
            }
            CellEvent::Output { item, .. } => {
                let appended = entry
                    .output
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .append(item, true);
                if let Err(loss) = appended {
                    let _ = pump.client.terminate().await;
                    end(Err(loss));
                    break;
                }
            }
            CellEvent::Waiting { calls, .. } => {
                waiting = calls.into_iter().collect();
                deadline = if entry.take_submitted(&waiting) {
                    waiting.clear();
                    Some(tokio::time::Instant::now() + request_bound)
                } else {
                    None
                };
            }
            CellEvent::Yielded { .. } => {
                let _ = pump.client.terminate().await;
                end(Err("cell worker parked a host-window cell".into()));
                break;
            }
            CellEvent::Done { outcome, .. } => {
                end(Ok(WindowTerminal::from_outcome(outcome, &generation_lost)));
                break;
            }
        }
    }
}

async fn sleep_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[derive(Default)]
struct CollectedOutput {
    items: Vec<OutputItem>,
    bytes: usize,
    truncated: bool,
}

impl CollectedOutput {
    fn append_truncated(&mut self, item: OutputItem) {
        if self.truncated {
            return;
        }
        if self.append(item, false).is_ok() {
            return;
        }
        // The same bounded prefix and warning apply within and across yields.
        let warning = OutputItem::Text {
            text: "Warning: cell output omitted: collected output limit".into(),
        };
        while self.append(warning.clone(), false).is_err() {
            if let Some(item) = self.items.pop() {
                self.bytes -= output_bytes(&item);
            }
        }
        self.truncated = true;
    }

    /// Buffers one item; with `merge_text`, consecutive text joins one item.
    fn append(&mut self, item: OutputItem, merge_text: bool) -> Result<(), String> {
        let bytes = output_bytes(&item);
        if bytes > MAX_COLLECTED_OUTPUT_BYTES.saturating_sub(self.bytes)
            || self.items.len() >= MAX_COLLECTED_OUTPUT_ITEMS
        {
            return Err("resource limit: cell collected output".into());
        }
        self.bytes += bytes;
        if merge_text
            && let OutputItem::Text { text } = &item
            && let Some(OutputItem::Text { text: last }) = self.items.last_mut()
        {
            last.push_str(text);
            return Ok(());
        }
        self.items.push(item);
        Ok(())
    }
}

fn output_bytes(item: &OutputItem) -> usize {
    match item {
        OutputItem::Text { text } | OutputItem::Notify { text } => text.len(),
        OutputItem::Image { url, .. } | OutputItem::Audio { url } => url.len(),
    }
}

impl Pump {
    async fn forward_windowed(
        &self,
        entry: &Arc<WindowCell>,
        call: u64,
        name: String,
        input: CellValue,
    ) -> Result<(), String> {
        let Some((event, _)) = self
            .nested_call(call, name, input, Some(Arc::clone(entry)))
            .await
        else {
            return Err("cell worker sent a nested input without a JSON form".into());
        };
        if self.events_tx.send(event).await.is_err() {
            let _ = self.client.terminate().await;
            return Err("cell event delivery unavailable".into());
        }
        Ok(())
    }

    async fn lose(&self, reason: String) {
        if let Some(entry) = self.completions.lock().await.remove(&self.cell) {
            entry.publish(Err(reason));
        }
        let _ = self.client.terminate().await;
    }

    /// The session event for one nested call, or `None` after a loss.
    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    async fn nested_call(
        &self,
        call: u64,
        name: String,
        input: CellValue,
        window: Option<Arc<WindowCell>>,
    ) -> Option<(CellSessionEvent, oneshot::Receiver<()>)> {
        let input = match Value::try_from(input) {
            Ok(input) => input,
            Err(error) => {
                self.lose(format!("cell worker sent a nested input: {error}"))
                    .await;
                return None;
            }
        };
        let (reply, replied) = oneshot::channel();
        Some((
            CellSessionEvent::NestedCall {
                cell: self.cell,
                call,
                name,
                input,
                responder: NestedResponder {
                    client: Arc::clone(&self.client),
                    cell: self.cell,
                    call,
                    reply: Some(reply),
                    window,
                },
            },
            replied,
        ))
    }

    async fn forward(&self, event: CellSessionEvent, answered: oneshot::Receiver<()>) -> bool {
        if self.events_tx.try_send(event).is_err() {
            self.lose("cell event delivery unavailable".into()).await;
            return false;
        }
        let loss = tokio::select! {
            result = answered => result.err().map(|_| "cell request answer closed".into()),
            // Loss can arrive before the responder wakes us for a delivered
            // result. Let read_event drain any queued terminal before loss.
            _ = self.client.wait_loss() => None,
        };
        if let Some(loss) = loss {
            self.lose(loss).await;
            return false;
        }
        true
    }

    async fn finish(
        &self,
        stored: HashMap<String, Value>,
        output: CollectedOutput,
        error: Option<String>,
    ) {
        let mut retained = self.stored.lock().await;
        retained.extend(stored);
        if encode_frame(&*retained).is_err() {
            retained.clear();
            drop(retained);
            self.lose("resource limit: cell retained storage".into())
                .await;
            return;
        }
        drop(retained);
        if let Some(entry) = self.completions.lock().await.remove(&self.cell) {
            entry.publish_collected(
                Ok(CellResponse::Result {
                    cell: self.cell,
                    items: output.items,
                    error,
                }),
                output.truncated,
            );
        }
    }
}

/// Answers one nested call of the running cell, exactly once.
pub struct NestedResponder {
    client: Arc<CellClient>,
    cell: u64,
    call: u64,
    reply: Option<oneshot::Sender<()>>,
    /// The host-window cell the call belongs to.
    window: Option<Arc<WindowCell>>,
}

impl NestedResponder {
    /// Delivers the call's value. A value nested deeper than
    /// [`MAX_CELL_VALUE_DEPTH`] reaches the cell as a failure instead.
    pub async fn resolve(self, value: Value) -> NestedDeliveryOutcome {
        if json_depth(&value) > MAX_CELL_VALUE_DEPTH {
            let refusal = format!("nested result nests deeper than {MAX_CELL_VALUE_DEPTH} levels");
            return self.deliver(Err(refusal)).await;
        }
        self.deliver(Ok(CellValue::from(value))).await
    }

    /// Delivers a typed value, including plain-data records and bytes.
    pub async fn resolve_cell(self, value: CellValue) -> NestedDeliveryOutcome {
        self.deliver(Ok(value)).await
    }

    /// Delivers the call's failure, which the cell observes as an error.
    pub async fn reject(self, reason: String) -> NestedDeliveryOutcome {
        self.deliver(Err(reason)).await
    }

    async fn deliver(mut self, outcome: Result<CellValue, String>) -> NestedDeliveryOutcome {
        // A host-window cell that already ended, or whose worker was stopped,
        // provably never observes the result.
        if self
            .window
            .as_ref()
            .is_some_and(|window| window.terminal.borrow().is_some() || self.client.is_stopped())
        {
            if let Some(reply) = self.reply.take() {
                let _ = reply.send(());
            }
            return NestedDeliveryOutcome::NotObserved(
                "the cell had already ended when its result was delivered".into(),
            );
        }
        // Start the compute watchdog when the complete pending batch has
        // been submitted, not when acceptance arrives: the interpreter may
        // compute forever after consuming the values without emitting output.
        if let Some(window) = &self.window {
            window.submitted.send_modify(|submitted| {
                submitted.insert(self.call);
            });
        }
        let delivered = self.client.deliver(self.cell, self.call, outcome).await;
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(());
        }
        delivered
    }
}

/// Acknowledges one notification of the running cell.
pub struct NotificationResponder {
    ack: Option<oneshot::Sender<()>>,
}

impl NotificationResponder {
    /// Lets the cell continue; false when nobody waits for the answer.
    #[must_use]
    pub fn acknowledge(mut self) -> bool {
        self.ack.take().is_some_and(|ack| ack.send(()).is_ok())
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::expect_used, reason = "scripted worker regressions")]
mod tests;
