//! Engine-neutral session over one [`CellClient`]: the cell table, output
//! buffers, observation by window, nested-call correlation and the retained
//! `store` map. A session runs one cell at a time.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::client::{CellClient, CellEvent, ChildProcessUsage, NestedDeliveryOutcome};
use crate::frame::encode_frame;
use crate::protocol::{
    CellOutcome, CellPrelude, CellValue, MAX_CELL_VALUE_DEPTH, OutputItem, json_depth,
};

/// Output a cell may buffer before its next observation.
pub const MAX_COLLECTED_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Output items a cell may buffer before its next observation.
pub const MAX_COLLECTED_OUTPUT_ITEMS: usize = 4096;

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

/// The session's state; cheap to share with its event receiver.
pub struct CellSession {
    client: Arc<CellClient>,
    next_cell: AtomicU64,
    stored: Arc<Mutex<HashMap<String, Value>>>,
    events_tx: mpsc::Sender<CellSessionEvent>,
    completions: CompletionSenders,
    streams: Arc<Mutex<HashMap<u64, CellResponseStream>>>,
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

impl CellSession {
    /// A session over `client` and the receiver of its requests.
    #[must_use]
    pub fn new(client: Arc<CellClient>) -> (Self, CellSessionEvents) {
        let (events_tx, rx) = mpsc::channel(64);
        (
            Self {
                client,
                next_cell: AtomicU64::new(1),
                stored: Arc::new(Mutex::new(HashMap::new())),
                events_tx,
                completions: Arc::new(Mutex::new(HashMap::new())),
                streams: Arc::new(Mutex::new(HashMap::new())),
                pump: Mutex::new(None),
            },
            CellSessionEvents { rx },
        )
    }

    /// Starts one cell with `tools` declared and the retained `store` map,
    /// after `admit` grants the start. A refused admission starts nothing.
    ///
    /// # Errors
    /// The admission's refusal, or a loss when another cell is active or the
    /// worker refuses the start.
    pub async fn execute<E>(
        &self,
        source: &str,
        tools: Vec<Value>,
        window: u64,
        admit: impl Future<Output = Result<(), E>>,
    ) -> Result<StartedCell, CellStartError<E>> {
        let mut pump = self.pump.lock().await;
        if !self.completions.lock().await.is_empty() {
            return Err(CellStartError::Lost("cell is already active".into()));
        }
        if let Some(mut previous) = pump.take()
            && let Some(task) = previous.0.take()
        {
            let _ = task.await;
        }
        let cell = self.next_cell.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(2);
        let rx = Arc::new(Mutex::new(rx));
        self.completions.lock().await.insert(cell, tx);
        self.streams.lock().await.insert(cell, Arc::clone(&rx));
        let stored = self.stored.lock().await.clone();
        if let Err(error) = admit.await {
            self.completions.lock().await.remove(&cell);
            self.streams.lock().await.remove(&cell);
            return Err(CellStartError::Refused(error));
        }
        if let Err(error) = self.client.start_cell(
            cell,
            source,
            CellPrelude::Quickjs { tools, stored },
            Some(window),
        ) {
            self.completions.lock().await.remove(&cell);
            self.streams.lock().await.remove(&cell);
            return Err(CellStartError::Lost(error));
        }
        *pump = Some(PumpTask(Some(tokio::spawn(pump_cell(Pump {
            client: Arc::clone(&self.client),
            cell,
            events_tx: self.events_tx.clone(),
            completions: Arc::clone(&self.completions),
            stored: Arc::clone(&self.stored),
        })))));
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

    /// Observes `cell` for another `window`: a buffered response first,
    /// otherwise a resume of its parked yield.
    pub async fn wait(&self, cell: &str, window: u64) -> WaitOutcome {
        let Ok(cell) = cell.parse::<u64>() else {
            return WaitOutcome::Missing {
                error: Some("invalid cell id".into()),
            };
        };
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
        self.client.terminate().await
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

async fn pump_cell(pump: Pump) {
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
                let input = match Value::try_from(input) {
                    Ok(input) => input,
                    Err(error) => {
                        pump.lose(format!("cell worker sent a nested input: {error}"))
                            .await;
                        break;
                    }
                };
                let (reply, replied) = oneshot::channel();
                let event = CellSessionEvent::NestedCall {
                    cell: pump.cell,
                    call,
                    name,
                    input,
                    responder: NestedResponder {
                        client: Arc::clone(&pump.client),
                        cell: pump.cell,
                        call,
                        reply: Some(reply),
                    },
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
                if let Err(error) = collected.append(item) {
                    pump.lose(error).await;
                    break;
                }
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

#[derive(Default)]
struct CollectedOutput {
    items: Vec<OutputItem>,
    bytes: usize,
}

impl CollectedOutput {
    fn append(&mut self, item: OutputItem) -> Result<(), String> {
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
        self.items.push(item);
        Ok(())
    }
}

impl Pump {
    async fn lose(&self, reason: String) {
        if let Some(tx) = self.completions.lock().await.remove(&self.cell) {
            let _ = tx.try_send(Err(reason));
        }
        let _ = self.client.terminate().await;
    }

    async fn forward(&self, event: CellSessionEvent, answered: oneshot::Receiver<()>) -> bool {
        if self.events_tx.try_send(event).is_err() {
            self.lose("cell event delivery unavailable".into()).await;
            return false;
        }
        let loss = tokio::select! {
            result = answered => result.err().map(|_| "cell request answer closed".into()),
            loss = self.client.wait_loss() => Some(loss),
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
        let hello = WorkerFrame::Hello {
            revision: CELL_PROTOCOL_REVISION.to_owned(),
            engine: QUICKJS_WORKER.engine.to_owned(),
            adapter: QUICKJS_WORKER.adapter.to_owned(),
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
        let (session, events) = CellSession::new(Arc::clone(&client));
        (client, session, events)
    }

    async fn execute(session: &CellSession) -> StartedCell {
        session
            .execute(SOURCE, Vec::new(), WINDOW, async { Ok::<(), ()>(()) })
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
            let terminal = tokio::time::timeout(std::time::Duration::from_secs(5), started.initial)
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
}
