//! Parent-side client for `plakous-js-host`.
use crate::{
    RuntimeEvent,
    protocol::{ClientOp, HostOp},
};
use serde_json::Value;
use std::{collections::HashMap, path::Path, process::Stdio, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinHandle,
};

/// Content-free child-process usage returned when a sidecar exits.
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

/// Maximum encoded JSONL frame, excluding the newline.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
/// Byte budget for queued worker events.
pub const MAX_QUEUED_BYTES: usize = 16 * 1024 * 1024;
/// Shutdown includes cancelable I/O and a bounded child reap.
pub const SIDECAR_SHUTDOWN_SETTLE_TIMEOUT: Duration = Duration::from_secs(2);
const QUEUE_ITEMS: usize = 64;
/// Outstanding host commands a queue holds before it is a genuine resource limit.
///
/// The driver polls every pending nested delivery concurrently, so a profile
/// that admits sixteen outstanding nested calls can settle sixteen results in
/// one burst while the engine drains nothing. Sixteen deliveries plus the
/// control commands that may sit behind them (`Start`, `Resume`, `Interrupt`,
/// `Shutdown`) is the real floor; thirty-two leaves the same headroom again.
/// Below this a full queue is a queue-depth accident, not a resource limit.
pub const COMMAND_QUEUE_ITEMS: usize = 32;

struct QueuedEvent {
    event: RuntimeEvent,
    _bytes: OwnedSemaphorePermit,
}

type ResultAcks = Arc<Mutex<HashMap<(u64, u64), oneshot::Sender<NestedDeliveryOutcome>>>>;

/// What one nested result delivery proved about the evaluator.
///
/// The three unsettled answers are not interchangeable. A refusal is the
/// evaluator's own answer about ownership, a non-delivery is the host's proof
/// that the result never reached the evaluator at all, and only a loss leaves
/// acceptance undetermined. A host reports [`Self::NotDelivered`] only when the
/// result provably never left it; anything it cannot prove is [`Self::Lost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NestedDeliveryOutcome {
    /// The evaluator settled the exact correlated request.
    Accepted,
    /// The result named no pending request of the owning cell: an unissued id,
    /// a foreign execution, or a repeat of a settled id. The cell is
    /// undisturbed and this is not a loss.
    Refused(String),
    /// The host refused or failed before the result was handed over, so the
    /// evaluator never saw it and cannot have acted on it.
    NotDelivered(String),
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
            Self::Refused(message) | Self::NotDelivered(message) | Self::Lost(message) => {
                Some(message)
            }
        }
    }
}

/// One owned process with cancelable, bounded I/O tasks.
pub struct SidecarHost {
    child: Mutex<Child>,
    termination_loss: Mutex<Option<String>>,
    pid: u32,
    commands: mpsc::Sender<Vec<u8>>,
    events: Mutex<mpsc::Receiver<QueuedEvent>>,
    loss: watch::Receiver<Option<String>>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    exit_usage: Arc<Mutex<Option<ChildProcessUsage>>>,
    result_acks: ResultAcks,
}

impl SidecarHost {
    pub fn spawn(program: &Path) -> Result<Self, String> {
        // Check the task owner before creating a process; construction never
        // hands off a child without its I/O supervision.
        let runtime = tokio::runtime::Handle::try_current().map_err(|e| e.to_string())?;
        let mut child = Command::new(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn plakous-js-host: {e}"))?;
        let pid = child.id().ok_or("sidecar pid missing")?;
        let stdin = child.stdin.take().ok_or("sidecar stdin missing")?;
        let stdout = child.stdout.take().ok_or("sidecar stdout missing")?;
        let (commands, mut command_rx) = mpsc::channel::<Vec<u8>>(COMMAND_QUEUE_ITEMS);
        let (event_tx, events) = mpsc::channel(QUEUE_ITEMS);
        let (loss_tx, loss) = watch::channel(None);
        let (stop, mut writer_stop) = watch::channel(false);
        let reader_stop = stop.subscribe();
        let exit_usage = Arc::new(Mutex::new(None));
        let captured_usage = Arc::clone(&exit_usage);
        let writer_loss = loss_tx.clone();
        let writer = runtime.spawn(async move {
            let mut stdin = stdin;
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
                            writer_loss.send_replace(Some(format!("sidecar write failed: {error}")));
                            break;
                        }
                    }
                }
            }
        });
        let result_acks = Arc::new(Mutex::new(HashMap::new()));
        let reader = runtime.spawn(read_worker(
            stdout,
            reader_stop,
            loss_tx,
            event_tx,
            captured_usage,
            Arc::clone(&result_acks),
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
        })
    }

    pub fn child_id(&self) -> Option<u32> {
        Some(self.pid)
    }

    pub fn start_cell(
        &self,
        execution_id: u64,
        source: &str,
        stored: HashMap<String, Value>,
        tools: Vec<Value>,
        yield_time_ms: u64,
    ) -> Result<(), String> {
        if source.len() > MAX_FRAME_BYTES {
            return Err("resource limit: sidecar source".into());
        }
        self.send(&ClientOp::Start {
            execution_id,
            source: source.to_owned(),
            tools,
            stored,
            yield_time_ms,
        })
    }
    pub fn resume(&self, execution_id: u64, yield_time_ms: u64) -> Result<(), String> {
        self.send(&ClientOp::Resume {
            execution_id,
            yield_time_ms,
        })
    }
    /// Delivers a nested result, keeping the child's refusal, a proved
    /// non-delivery, and a genuine loss distinct from one another.
    ///
    /// A `tool_result_refused` frame is the evaluator's own answer that the
    /// result names no pending request of the owning cell. A failure on the way
    /// out is a non-delivery: the frame never entered the command queue, so the
    /// evaluator never saw the result. Only a transport or evaluator loss with
    /// the frame already written leaves acceptance undetermined.
    pub async fn deliver_tool_result(
        &self,
        execution_id: u64,
        id: u64,
        value: Value,
        success: bool,
    ) -> NestedDeliveryOutcome {
        let (tx, rx) = oneshot::channel();
        let key = (execution_id, id);
        {
            let mut pending = self.result_acks.lock().await;
            if pending.len() >= 64 || pending.contains_key(&key) {
                // Refused here, before `send`: nothing was written and the
                // evaluator never learned of this result.
                return NestedDeliveryOutcome::NotDelivered(
                    "resource limit: pending result acknowledgements".into(),
                );
            }
            pending.insert(key, tx);
        }
        let outcome = match self.send(&ClientOp::ToolResult {
            execution_id,
            id,
            value,
            success,
        }) {
            Ok(()) => tokio::select! {
                biased;
                result = rx => result.unwrap_or_else(|_| NestedDeliveryOutcome::Lost(
                    "sidecar result acknowledgement lost".to_owned(),
                )),
                loss = self.wait_loss() => NestedDeliveryOutcome::Lost(loss),
            },
            // The frame never entered the command queue.
            Err(error) => NestedDeliveryOutcome::NotDelivered(error),
        };
        self.result_acks.lock().await.remove(&key);
        outcome
    }

    /// Delivers a nested result where every unsettled answer is alike.
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
    pub fn interrupt(&self) {
        let _ = self.send(&ClientOp::Interrupt);
    }

    /// Independent terminal control: remains observable while a nested reply is pending.
    pub async fn wait_loss(&self) -> String {
        let mut loss = self.loss.clone();
        loop {
            if let Some(error) = loss.borrow().clone() {
                return error;
            }
            if loss.changed().await.is_err() {
                return "sidecar control closed".into();
            }
        }
    }
    pub async fn read_event(&self) -> Result<RuntimeEvent, String> {
        let mut events = self.events.lock().await;
        // Preserve already queued requests before publishing transport loss.
        tokio::select! {
            biased;
            event = events.recv() => event.map(|queued| queued.event).ok_or_else(|| self.loss.borrow().clone().unwrap_or_else(|| "sidecar closed".into())),
            loss = self.wait_loss() => Err(loss),
        }
    }

    pub async fn terminate(&self) -> Result<Option<ChildProcessUsage>, String> {
        self.result_acks.lock().await.clear();
        let before = crate::sample_children_rusage();
        let mut child = self.child.lock().await;
        let mut previous_loss = self.termination_loss.lock().await;
        if let Some(loss) = previous_loss.as_ref() {
            return Err(loss.clone());
        }
        let _ = self.send(&ClientOp::Shutdown);
        let voluntary = tokio::time::timeout(SIDECAR_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await;
        let mut loss = match voluntary {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => Some(format!("sidecar wait failed: {error}")),
            Err(_) => Some(
                "plakous-js-host sidecar did not acknowledge shutdown within 2s; killed".into(),
            ),
        };
        self.stop.send_replace(true);
        if loss.is_some() {
            let _ = child.start_kill();
            if !matches!(
                tokio::time::timeout(SIDECAR_SHUTDOWN_SETTLE_TIMEOUT, child.wait()).await,
                Ok(Ok(_))
            ) {
                loss = Some("sidecar remained unreaped after kill deadline".into());
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
        Ok(reported.or_else(|| {
            let usage = ChildProcessUsage {
                user_cpu: Duration::from_nanos(delta.user_cpu_ns),
                system_cpu: Duration::from_nanos(delta.system_cpu_ns),
                peak_rss_bytes: delta.peak_rss_bytes,
            };
            usage.is_meaningful().then_some(usage)
        }))
    }

    fn send(&self, op: &ClientOp) -> Result<(), String> {
        if let Some(loss) = self.loss.borrow().as_ref() {
            return Err(loss.clone());
        }
        let encoded = encode_frame(op)?;
        self.commands
            .try_send(encoded)
            .map_err(|_| "resource limit: sidecar command queue unavailable".into())
    }
}

impl Drop for SidecarHost {
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

async fn read_worker(
    stdout: tokio::process::ChildStdout,
    mut reader_stop: watch::Receiver<bool>,
    loss_tx: watch::Sender<Option<String>>,
    event_tx: mpsc::Sender<QueuedEvent>,
    captured_usage: Arc<Mutex<Option<ChildProcessUsage>>>,
    result_acks: ResultAcks,
) {
    let mut reader = BufReader::new(stdout);
    let bytes = Arc::new(Semaphore::new(MAX_QUEUED_BYTES));
    loop {
        let frame = tokio::select! {
            biased;
            _ = reader_stop.changed() => return,
            result = read_frame(&mut reader, MAX_FRAME_BYTES) => result,
        };
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                loss_tx.send_replace(Some("plakous-js-host closed".into()));
                return;
            }
            Err(error) => {
                loss_tx.send_replace(Some(error));
                return;
            }
        };
        if frame.is_empty() {
            continue;
        }
        let Ok(permit) = Arc::clone(&bytes)
            .try_acquire_many_owned(u32::try_from(frame.len()).unwrap_or(u32::MAX))
        else {
            loss_tx.send_replace(Some("resource limit: sidecar queued bytes".into()));
            return;
        };
        let Ok(op) = serde_json::from_slice::<HostOp>(&frame) else {
            loss_tx.send_replace(Some("sidecar malformed JSON frame".into()));
            return;
        };
        match op {
            HostOp::ToolResultAccepted { execution_id, id } => {
                if let Some(tx) = result_acks.lock().await.remove(&(execution_id, id)) {
                    let _ = tx.send(NestedDeliveryOutcome::Accepted);
                } else {
                    loss_tx.send_replace(Some("sidecar unexpected result acknowledgement".into()));
                    return;
                }
            }
            HostOp::ToolResultRefused {
                execution_id,
                id,
                message,
            } => {
                if let Some(tx) = result_acks.lock().await.remove(&(execution_id, id)) {
                    let _ = tx.send(NestedDeliveryOutcome::Refused(message));
                } else {
                    loss_tx.send_replace(Some("sidecar unexpected result refusal".into()));
                    return;
                }
            }
            HostOp::ToolResultNotDelivered {
                execution_id,
                id,
                message,
            } => {
                if let Some(tx) = result_acks.lock().await.remove(&(execution_id, id)) {
                    let _ = tx.send(NestedDeliveryOutcome::NotDelivered(message));
                } else {
                    loss_tx.send_replace(Some("sidecar unexpected result non-delivery".into()));
                    return;
                }
            }
            HostOp::HostLost { message } => {
                loss_tx.send_replace(Some(message));
                return;
            }
            HostOp::ResourceReport {
                user_cpu_ns,
                system_cpu_ns,
                peak_rss_bytes,
                ..
            } => {
                *captured_usage.lock().await = Some(ChildProcessUsage {
                    user_cpu: Duration::from_nanos(user_cpu_ns),
                    system_cpu: Duration::from_nanos(system_cpu_ns),
                    peak_rss_bytes,
                });
            }
            op => {
                if event_tx
                    .try_send(QueuedEvent {
                        event: op.into(),
                        _bytes: permit,
                    })
                    .is_err()
                {
                    loss_tx.send_replace(Some("resource limit: sidecar event queue".into()));
                    return;
                }
            }
        }
    }
}

/// Encode one bounded JSONL frame without an oversized intermediate buffer.
pub fn encode_frame(value: &impl serde::Serialize) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    serde_json::to_writer(
        BoundedWriter {
            bytes: &mut encoded,
            limit: MAX_FRAME_BYTES,
        },
        value,
    )
    .map_err(|_| "resource limit: sidecar frame bytes".to_owned())?;
    encoded.push(b'\n');
    Ok(encoded)
}

struct BoundedWriter<'a> {
    bytes: &'a mut Vec<u8>,
    limit: usize,
}
impl std::io::Write for BoundedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

async fn read_frame<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut frame = Vec::new();
    let n = reader
        .take((limit + 1) as u64)
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if frame.last() == Some(&b'\n') {
        frame.pop();
    } else if n > limit {
        return Err("resource limit: sidecar frame bytes".into());
    } else {
        return Err("sidecar incomplete JSON frame".into());
    }
    Ok(Some(frame))
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "bounded framing tests"
)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn frame_one_byte_over_limit_without_newline_fails_promptly() {
        let (mut writer, reader) = tokio::io::duplex(32);
        writer.write_all(b"123456789").await.expect("bytes");
        let result = tokio::time::timeout(
            Duration::from_millis(100),
            read_frame(&mut BufReader::new(reader), 8),
        )
        .await
        .expect("no newline needed");
        assert!(result.expect_err("over limit").contains("resource limit"));
    }
    #[tokio::test]
    async fn exact_frame_limit_is_accepted() {
        let mut reader = &b"12345678\n"[..];
        assert_eq!(
            read_frame(&mut reader, 8).await.expect("frame"),
            Some(b"12345678".to_vec())
        );
    }
}
