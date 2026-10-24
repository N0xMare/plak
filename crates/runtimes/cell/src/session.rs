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

type CompletionSenders = Arc<Mutex<HashMap<u64, mpsc::Sender<CellCompletion>>>>;
type CellResponseStream = Arc<Mutex<mpsc::Receiver<CellCompletion>>>;
type Windows = Arc<Mutex<HashMap<u64, Arc<WindowCell>>>>;

/// The session's state; cheap to share with its event receiver.
pub struct CellSession {
    client: Arc<CellClient>,
    mode: ObservationMode,
    next_cell: AtomicU64,
    stored: Arc<Mutex<HashMap<String, Value>>>,
    events_tx: mpsc::Sender<CellSessionEvent>,
    completions: CompletionSenders,
    streams: Arc<Mutex<HashMap<u64, CellResponseStream>>>,
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
            monty @ CellPrelude::Monty { .. } => monty,
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
        let (tx, rx) = mpsc::channel(2);
        let rx = Arc::new(Mutex::new(rx));
        self.completions.lock().await.insert(cell, tx);
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
            initial: Box::pin(async move {
                let response = rx
                    .lock()
                    .await
                    .recv()
                    .await
                    .unwrap_or_else(|| Err("cell ended".into()));
                if !matches!(response, Ok(CellResponse::Yielded { .. })) {
                    streams.lock().await.remove(&cell);
                }
                response
            }),
        })
    }

    async fn start_windowed<E>(
        &self,
        WindowStart {
            cell,
            window,
            request_bound,
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
        let Ok(cell) = cell.parse::<u64>() else {
            return WaitOutcome::Missing {
                error: Some("invalid cell id".into()),
            };
        };
        if matches!(self.mode, ObservationMode::HostWindow { .. }) {
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
        let mut rx = rx.lock().await;
        let response = match rx.try_recv() {
            Ok(response) => Some(response),
            Err(mpsc::error::TryRecvError::Disconnected) => None,
            Err(mpsc::error::TryRecvError::Empty) => {
                // A lost worker cannot resume, but the pump still answers with
                // the terminal the worker sent before it ended, or the loss.
                if let Err(error) = self.client.resume(cell, window)
                    && !self.client.is_lost()
                {
                    self.streams.lock().await.remove(&cell);
                    return WaitOutcome::Lost(error);
                }
                rx.recv().await
            }
        };
        match response {
            Some(Ok(response)) => {
                if !matches!(response, CellResponse::Yielded { .. }) {
                    self.streams.lock().await.remove(&cell);
                }
                WaitOutcome::Live(response)
            }
            Some(Err(loss)) => {
                self.streams.lock().await.remove(&cell);
                WaitOutcome::Lost(loss)
            }
            None => {
                self.streams.lock().await.remove(&cell);
                WaitOutcome::Lost("cell completion closed".into())
            }
        }
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
        self.completions.lock().await.clear();
        self.streams.lock().await.clear();
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
        self.completions.lock().await.clear();
        self.streams.lock().await.clear();
        end_windows(&self.windows).await;
        self.client.stop().await
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
    completions: CompletionSenders,
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
                if let Err(error) = collected.append(item, false) {
                    pump.lose(error).await;
                    break;
                }
            }
            CellEvent::Waiting { .. } => {
                pump.lose("cell worker waited inside an engine-parked cell".into())
                    .await;
                break;
            }
            CellEvent::Yielded { .. } => {
                let sent = pump
                    .completions
                    .lock()
                    .await
                    .get(&pump.cell)
                    .is_some_and(|tx| {
                        tx.try_send(Ok(CellResponse::Yielded {
                            cell: pump.cell,
                            items: std::mem::take(&mut collected.items),
                        }))
                        .is_ok()
                    });
                if !sent {
                    pump.lose("resource limit: cell completion queue".into())
                        .await;
                    break;
                }
            }
            CellEvent::Done { outcome, .. } => {
                let (stored, error) = match outcome {
                    CellOutcome::Completed { stored, .. } => (stored, None),
                    CellOutcome::Failed {
                        message, stored, ..
                    } => (stored, Some(message)),
                };
                pump.finish(stored, collected.items, error).await;
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
) {
    let end = |result: Result<WindowTerminal, String>| {
        entry.terminal.send_replace(Some(result));
    };
    let mut deadline = Some(tokio::time::Instant::now() + request_bound);
    let mut submitted = entry.submitted.subscribe();
    let mut waiting = BTreeSet::new();
    loop {
        let event = tokio::select! {
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
}

impl CollectedOutput {
    /// Buffers one item; with `merge_text`, consecutive text joins one item.
    fn append(&mut self, item: OutputItem, merge_text: bool) -> Result<(), String> {
        let bytes = match &item {
            OutputItem::Text { text } | OutputItem::Notify { text } => text.len(),
            OutputItem::Image { url, .. } | OutputItem::Audio { url } => url.len(),
        };
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
        if let Some(tx) = self.completions.lock().await.remove(&self.cell) {
            let _ = tx.try_send(Err(reason));
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
        items: Vec<OutputItem>,
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
        if let Some(tx) = self.completions.lock().await.remove(&self.cell) {
            let _ = tx.try_send(Ok(CellResponse::Result {
                cell: self.cell,
                items,
                error,
            }));
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
mod tests {
    use super::*;
    use crate::protocol::{
        CELL_PROTOCOL_REVISION, DeliveryVerdict, EngineConfig, HostFrame, QUICKJS_WORKER,
        WorkerFrame,
    };
    use std::{fmt::Write as _, os::unix::fs::PermissionsExt as _, path::PathBuf};

    const SOURCE: &str = "text('scripted')";
    const WINDOW: u64 = 10_000;

    /// One step of a scripted worker after its hello.
    enum Step {
        /// Consume exactly these host frames.
        Read(Vec<HostFrame>),
        /// Write these worker frames.
        Write(Vec<WorkerFrame>),
    }

    fn configure() -> HostFrame {
        HostFrame::Configure {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: EngineConfig::QUICKJS,
        }
    }

    fn start(cell: u64) -> HostFrame {
        HostFrame::StartCell {
            cell,
            source: SOURCE.to_owned(),
            prelude: CellPrelude::Quickjs {
                tools: Vec::new(),
                stored: HashMap::new(),
            },
            window: Some(WINDOW),
        }
    }

    fn nested_result(call: u64) -> HostFrame {
        HostFrame::NestedResult {
            cell: 1,
            call,
            outcome: Ok(CellValue::from(serde_json::json!(7))),
        }
    }

    #[allow(
        clippy::similar_names,
        reason = "cell and call are the protocol's own field names"
    )]
    fn nested_call(cell: u64, call: u64) -> WorkerFrame {
        WorkerFrame::NestedCall {
            cell,
            call,
            name: "lookup".into(),
            input: CellValue::from(serde_json::json!({})),
        }
    }

    fn done(cell: u64) -> WorkerFrame {
        WorkerFrame::CellDone {
            cell,
            outcome: CellOutcome::Completed {
                value: None,
                stored: HashMap::new(),
            },
        }
    }

    /// A worker that says hello as the `QuickJS` worker, runs `steps` and
    /// exits; host frames are consumed by exact encoded length.
    fn scripted_worker(directory: &std::path::Path, steps: &[Step]) -> PathBuf {
        scripted_worker_as(directory, QUICKJS_WORKER, steps, "")
    }

    /// A scripted worker naming `identity`, which runs `tail` after `steps`.
    fn scripted_worker_as(
        directory: &std::path::Path,
        identity: crate::protocol::WorkerIdentity,
        steps: &[Step],
        tail: &str,
    ) -> PathBuf {
        let hello = WorkerFrame::Hello {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: identity.engine.to_owned(),
            adapter: identity.adapter.to_owned(),
        };
        let mut writes = vec![encode_frame(&hello).expect("hello")];
        let mut body = String::from("#!/bin/sh\ncat \"$0.0\"\n");
        for step in steps {
            match step {
                Step::Read(frames) => {
                    let bytes: usize = frames
                        .iter()
                        .map(|frame| encode_frame(frame).expect("host frame").len())
                        .sum();
                    let _ = writeln!(body, "head -c {bytes} >/dev/null");
                }
                Step::Write(frames) => {
                    let _ = writeln!(body, "cat \"$0.{}\"", writes.len());
                    writes.push(
                        frames
                            .iter()
                            .flat_map(|frame| encode_frame(frame).expect("worker frame"))
                            .collect(),
                    );
                }
            }
        }
        body.push_str(tail);
        let program = directory.join("worker");
        for (index, bytes) in writes.iter().enumerate() {
            std::fs::write(directory.join(format!("worker.{index}")), bytes).expect("frames");
        }
        let staging = directory.join("worker.pending");
        std::fs::write(&staging, body).expect("program");
        std::fs::rename(staging, &program).expect("publish program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700))
            .expect("permissions");
        program
    }

    async fn session(
        program: &std::path::Path,
    ) -> (Arc<CellClient>, CellSession, CellSessionEvents) {
        let client = Arc::new(
            CellClient::spawn(program, QUICKJS_WORKER, EngineConfig::QUICKJS)
                .await
                .expect("worker hello"),
        );
        let (session, events) =
            CellSession::new(Arc::clone(&client), ObservationMode::EngineParked);
        (client, session, events)
    }

    async fn execute(session: &CellSession) -> StartedCell {
        session
            .execute(
                SOURCE,
                CellPrelude::Quickjs {
                    tools: Vec::new(),
                    stored: HashMap::new(),
                },
                WINDOW,
                async { Ok::<(), ()>(()) },
            )
            .await
            .map_err(|_| "start")
            .expect("start admitted")
    }

    #[tokio::test]
    async fn wait_delivers_a_buffered_terminal_after_the_worker_ends() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker(
            directory.path(),
            &[
                Step::Read(vec![configure(), start(1)]),
                Step::Write(vec![WorkerFrame::Yielded { cell: 1 }, done(1)]),
            ],
        );
        let (client, session, _events) = session(&program).await;
        let started = execute(&session).await;
        assert!(matches!(
            started.initial.await,
            Ok(CellResponse::Yielded { cell: 1, .. })
        ));
        // The worker ended after its terminal: no resume can reach it.
        let _ = client.wait_loss().await;
        assert!(matches!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Live(CellResponse::Result {
                cell: 1,
                error: None,
                ..
            })
        ));
        assert_eq!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Missing { error: None }
        );
        let _ = session.shutdown().await;
    }

    struct Case {
        name: &'static str,
        steps: Vec<Step>,
        accepted: bool,
        completed: bool,
    }

    #[tokio::test]
    async fn worker_eof_drains_a_terminal_while_the_nested_responder_is_pending() {
        for completed in [true, false] {
            let directory = tempfile::tempdir().expect("directory");
            let mut final_frames = vec![WorkerFrame::Delivery {
                cell: 1,
                call: 1,
                verdict: DeliveryVerdict::Accepted,
            }];
            if completed {
                final_frames.push(done(1));
            }
            let program = scripted_worker(
                directory.path(),
                &[
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![nested_call(1, 1)]),
                    Step::Read(vec![nested_result(1)]),
                    Step::Write(final_frames),
                ],
            );
            let (client, session, mut events) = session(&program).await;
            let started = execute(&session).await;
            let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await
            else {
                panic!("nested call");
            };
            // Model the responder being descheduled after the worker accepts
            // its result, before it wakes the pump with its local reply.
            assert_eq!(
                client
                    .deliver(1, 1, Ok(CellValue::from(serde_json::json!(7))))
                    .await,
                NestedDeliveryOutcome::Accepted
            );
            let _ = client.wait_loss().await;
            let terminal = tokio::time::timeout(Duration::from_secs(5), started.initial)
                .await
                .expect("terminal despite the pending responder");
            assert_eq!(
                matches!(terminal, Ok(CellResponse::Result { error: None, .. })),
                completed,
                "completed={completed}: {terminal:?}"
            );
            if !completed {
                assert!(matches!(terminal, Err(ref loss) if loss == "cell worker closed"));
            }
            assert_eq!(
                session.wait("1", WINDOW).await,
                WaitOutcome::Missing { error: None }
            );
            drop(responder);
            let _ = session.shutdown().await;
        }
    }

    /// Interleavings of nested calls, verdicts, output and terminals.
    fn interleavings() -> Vec<Case> {
        vec![
            Case {
                name: "accepted then done",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![nested_call(1, 1)]),
                    Step::Read(vec![nested_result(1)]),
                    Step::Write(vec![
                        WorkerFrame::Delivery {
                            cell: 1,
                            call: 1,
                            verdict: DeliveryVerdict::Accepted,
                        },
                        WorkerFrame::Output {
                            cell: 1,
                            item: OutputItem::Text { text: "7".into() },
                        },
                        done(1),
                    ]),
                ],
                accepted: true,
                completed: true,
            },
            Case {
                name: "result read, worker ends before its verdict",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![nested_call(1, 1)]),
                    Step::Read(vec![nested_result(1)]),
                ],
                accepted: false,
                completed: false,
            },
            Case {
                name: "not delivered then done",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![nested_call(1, 1)]),
                    Step::Read(vec![nested_result(1)]),
                    Step::Write(vec![
                        WorkerFrame::Delivery {
                            cell: 1,
                            call: 1,
                            verdict: DeliveryVerdict::NotDelivered("cancelled".into()),
                        },
                        done(1),
                    ]),
                ],
                accepted: false,
                completed: true,
            },
            Case {
                name: "verdict for a result never sent",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![
                        WorkerFrame::Delivery {
                            cell: 1,
                            call: 9,
                            verdict: DeliveryVerdict::Accepted,
                        },
                        done(1),
                    ]),
                ],
                accepted: false,
                completed: false,
            },
            Case {
                name: "a frame naming another cell",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![done(2), done(1)]),
                ],
                accepted: false,
                completed: false,
            },
            Case {
                name: "two terminals",
                steps: vec![
                    Step::Read(vec![configure(), start(1)]),
                    Step::Write(vec![done(1), done(1)]),
                ],
                accepted: false,
                completed: true,
            },
        ]
    }

    /// Every scripted interleaving ends the cell with exactly one terminal,
    /// and a nested result is `Accepted` only when the worker said so.
    #[tokio::test]
    async fn every_interleaving_has_one_terminal_and_no_unproven_acceptance() {
        for case in interleavings() {
            let directory = tempfile::tempdir().expect("directory");
            let program = scripted_worker(directory.path(), &case.steps);
            let (_client, session, mut events) = session(&program).await;
            let started = execute(&session).await;
            let answer = tokio::spawn(async move {
                let mut verdicts = Vec::new();
                while let Some(event) = events.receive().await {
                    match event {
                        CellSessionEvent::NestedCall { responder, .. } => {
                            verdicts.push(responder.resolve(serde_json::json!(7)).await);
                        }
                        CellSessionEvent::Notification { responder, .. } => {
                            let _ = responder.acknowledge();
                        }
                    }
                }
                verdicts
            });
            let terminal = tokio::time::timeout(Duration::from_secs(5), started.initial)
                .await
                .expect("one terminal without the turn's timeout");
            assert_eq!(
                matches!(terminal, Ok(CellResponse::Result { error: None, .. })),
                case.completed,
                "{}: {terminal:?}",
                case.name
            );
            assert_eq!(
                session.wait("1", WINDOW).await,
                WaitOutcome::Missing { error: None },
                "{}: a second terminal",
                case.name
            );
            let _ = session.shutdown().await;
            drop(session);
            let verdicts = answer.await.expect("answers");
            assert_eq!(
                verdicts.contains(&NestedDeliveryOutcome::Accepted),
                case.accepted,
                "{}: {verdicts:?}",
                case.name
            );
        }
    }

    fn monty_configure() -> HostFrame {
        HostFrame::Configure {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: EngineConfig::MONTY,
        }
    }

    fn monty_start(cell: u64) -> HostFrame {
        HostFrame::StartCell {
            cell,
            source: SOURCE.to_owned(),
            prelude: CellPrelude::Monty { bootstrap: None },
            window: None,
        }
    }

    async fn monty_session(
        program: &std::path::Path,
        request_bound: Duration,
    ) -> (CellSession, CellSessionEvents) {
        let client = Arc::new(
            CellClient::spawn(program, crate::protocol::MONTY_WORKER, EngineConfig::MONTY)
                .await
                .expect("worker hello"),
        );
        CellSession::new(client, ObservationMode::HostWindow { request_bound })
    }

    async fn monty_execute(session: &CellSession, window: u64) -> StartedCell {
        session
            .execute(
                SOURCE,
                CellPrelude::Monty { bootstrap: None },
                window,
                async { Ok::<(), ()>(()) },
            )
            .await
            .map_err(|_| "start")
            .expect("start admitted")
    }

    fn text(text: &str) -> WorkerFrame {
        WorkerFrame::Output {
            cell: 1,
            item: OutputItem::Text { text: text.into() },
        }
    }

    /// A host-window cell keeps running: an observation that ends first
    /// returns its buffered output as one text, and a later wait returns the
    /// rest with the terminal's value after it.
    #[tokio::test]
    async fn a_host_window_cell_yields_its_buffered_output_and_keeps_running() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker_as(
            directory.path(),
            crate::protocol::MONTY_WORKER,
            &[
                Step::Read(vec![monty_configure(), monty_start(1)]),
                Step::Write(vec![
                    text("a"),
                    text("b"),
                    nested_call(1, 7),
                    WorkerFrame::Waiting {
                        cell: 1,
                        calls: vec![7],
                    },
                ]),
                Step::Read(vec![nested_result(7)]),
                Step::Write(vec![
                    WorkerFrame::Delivery {
                        cell: 1,
                        call: 7,
                        verdict: DeliveryVerdict::Accepted,
                    },
                    text("c"),
                    WorkerFrame::CellDone {
                        cell: 1,
                        outcome: CellOutcome::Completed {
                            value: Some("v".into()),
                            stored: HashMap::new(),
                        },
                    },
                ]),
            ],
            "exec sleep 30\n",
        );
        let (session, mut events) = monty_session(&program, Duration::from_secs(30)).await;
        let started = monty_execute(&session, 0).await;
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("the nested call arrives after the output before it");
        };
        assert_eq!(session.active_cell().await, Some(1));
        assert_eq!(
            started.initial.await,
            Ok(CellResponse::Yielded {
                cell: 1,
                items: vec![OutputItem::Text { text: "ab".into() }],
            })
        );
        assert_eq!(
            responder.resolve(serde_json::json!(7)).await,
            NestedDeliveryOutcome::Accepted
        );
        assert_eq!(
            session.wait("1", WINDOW).await,
            WaitOutcome::Live(CellResponse::Result {
                cell: 1,
                items: vec![
                    OutputItem::Text { text: "c".into() },
                    OutputItem::Text { text: "v".into() },
                ],
                error: None,
            })
        );
        assert_eq!(session.active_cell().await, None);
        assert!(matches!(
            session.stop().await,
            Ok(WorkerStop { forced: true, .. })
        ));
    }

    /// Computation that reaches no host-visible boundary within the request
    /// bound kills the worker; that loss is sticky.
    #[tokio::test]
    async fn the_request_bound_kills_a_worker_that_never_reaches_a_boundary() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker_as(
            directory.path(),
            crate::protocol::MONTY_WORKER,
            &[Step::Read(vec![monty_configure(), monty_start(1)])],
            "exec sleep 30\n",
        );
        let (session, _events) = monty_session(&program, Duration::from_millis(300)).await;
        let started = monty_execute(&session, 20_000).await;
        let loss = tokio::time::timeout(Duration::from_secs(10), started.initial)
            .await
            .expect("the bound ends the observation")
            .expect_err("a killed worker is a loss");
        assert!(loss.contains("timed out"), "{loss}");
        assert!(session.shutdown().await.is_err(), "the kill stays a loss");
    }

    /// A buffered terminal is visible without consuming it, and results for
    /// a cell that already ended are definitely unobserved.
    #[tokio::test]
    async fn a_buffered_terminal_is_ready_and_late_results_are_unobserved() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker_as(
            directory.path(),
            crate::protocol::MONTY_WORKER,
            &[
                Step::Read(vec![monty_configure(), monty_start(1)]),
                Step::Write(vec![nested_call(1, 3), done(1)]),
            ],
            "exec sleep 30\n",
        );
        let (session, mut events) = monty_session(&program, Duration::from_secs(30)).await;
        let started = monty_execute(&session, 0).await;
        drop(started);
        let Some(CellSessionEvent::NestedCall { responder, .. }) = events.receive().await else {
            panic!("nested call");
        };
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            while !session.terminal_ready(1).await {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(ready.is_ok(), "the terminal is buffered");
        assert!(matches!(
            responder.resolve(serde_json::json!(1)).await,
            NestedDeliveryOutcome::NotObserved(_)
        ));
        assert!(matches!(
            session.wait("1", 0).await,
            WaitOutcome::Live(CellResponse::Result { error: None, .. })
        ));
        assert_eq!(
            session.wait("1", 0).await,
            WaitOutcome::Missing { error: None }
        );
        let _ = session.stop().await;
    }

    #[tokio::test]
    async fn a_lost_generation_still_certifies_its_reaped_worker() {
        let directory = tempfile::tempdir().expect("directory");
        let program = scripted_worker_as(
            directory.path(),
            crate::protocol::MONTY_WORKER,
            &[
                Step::Read(vec![monty_configure(), monty_start(1)]),
                Step::Write(vec![
                    text("partial stdout"),
                    WorkerFrame::CellDone {
                        cell: 1,
                        outcome: CellOutcome::Failed {
                            message: "undecodable traceback".into(),
                            stored: HashMap::new(),
                            generation_lost: true,
                        },
                    },
                ]),
            ],
            "",
        );
        let (session, _events) = monty_session(&program, Duration::from_secs(30)).await;
        let started = monty_execute(&session, WINDOW).await;
        assert_eq!(
            started.initial.await,
            Ok(CellResponse::Result {
                cell: 1,
                items: vec![OutputItem::Text {
                    text: "partial stdout".into()
                }],
                error: Some("undecodable traceback".into()),
            })
        );
        assert!(session.generation_lost());
        assert!(
            session.shutdown().await.is_ok(),
            "a definite failed cell does not erase its worker's reaped receipt"
        );
    }
}
