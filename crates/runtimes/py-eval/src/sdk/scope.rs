//! One cell's host-side scope: the nested-call choke point (count, order and
//! concurrency), delivery acknowledgements, and the cell's untrusted sources
//! and issued images.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU32, AtomicU64, Ordering},
};

use plakous_agent::internal::{
    LocalCompositionEvent, LocalCompositionFuture, LocalCompositionNestedCall,
    LocalCompositionNestedDelivery, LocalCompositionNestedResponder, NamedStatePort,
};
use plakous_tools::{ExecutionValue, ImageValue, PythonHarnessSdkLimitsV1};
use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch};

use super::evidence::EvidenceTable;

/// A limit that stops a cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopReason {
    Source,
    Memory,
    Compute,
    Time,
    NestedCalls,
    Suspensions,
}

impl StopReason {
    /// The `<limit>` of `cell stopped: <limit>`.
    pub(super) fn render(self, limits: &PythonHarnessSdkLimitsV1) -> String {
        match self {
            Self::Source => format!("source limit ({})", bytes(limits.source_bytes)),
            Self::Memory => format!("memory limit ({})", bytes(limits.engine_memory_bytes)),
            Self::Compute => format!("compute limit ({})", seconds(limits.compute_ms)),
            Self::Time => format!("time limit ({})", seconds(limits.foreground_budget_ms)),
            Self::NestedCalls => format!("nested call limit ({})", limits.nested_calls),
            Self::Suspensions => format!("suspension limit ({})", limits.suspensions),
        }
    }
}

fn bytes(value: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    if value >= MIB && value.is_multiple_of(MIB) {
        format!("{} MiB", value / MIB)
    } else if value >= 1024 && value.is_multiple_of(1024) {
        format!("{} KiB", value / 1024)
    } else {
        format!("{value} bytes")
    }
}

fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        let fraction = format!("{:03}", ms % 1000);
        format!("{}.{} s", ms / 1000, fraction.trim_end_matches('0'))
    }
}

/// Acknowledges one nested result to the driver exactly once. Dropping an
/// unsent acknowledgement reports that the cell never observed the result.
/// It holds a concurrency permit, so a result awaiting delivery still counts
/// against the outstanding-call bound.
pub(super) struct Ack {
    sender: Option<oneshot::Sender<LocalCompositionNestedDelivery>>,
    _permit: OwnedSemaphorePermit,
}

impl Ack {
    pub(super) fn send(mut self, delivery: LocalCompositionNestedDelivery) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(delivery);
        }
    }

    /// The host consumed the result itself.
    pub(super) fn accept(self) {
        self.send(LocalCompositionNestedDelivery::Accepted);
    }
}

impl Drop for Ack {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(LocalCompositionNestedDelivery::NotObserved(
                "the cell ended before the result reached it".into(),
            ));
        }
    }
}

/// A committed nested result with its typed value.
pub(super) struct NestedResult {
    pub(super) value: Value,
    pub(super) execution: Option<Arc<ExecutionValue>>,
    pub(super) ack: Ack,
}

/// Why a nested call produced no result for the cell.
#[derive(Debug)]
pub(super) enum NestedError {
    /// Refused before any call; raised as `ValueError`.
    Invalid(String),
    /// A definite nested failure; raised as `RuntimeError` with its diagnostic.
    Failed(String),
    /// The cell is stopping; nothing more is delivered.
    Stopped,
}

enum Reply {
    Resolved(Value, Option<Arc<ExecutionValue>>),
    Rejected(String),
}

type ReplySender = oneshot::Sender<(Reply, oneshot::Sender<LocalCompositionNestedDelivery>)>;

struct Responder {
    reply: ReplySender,
}

impl Responder {
    fn deliver(
        self,
        reply: Reply,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        Box::pin(async move {
            let (sender, receipt) = oneshot::channel();
            if self.reply.send((reply, sender)).is_err() {
                return Ok(LocalCompositionNestedDelivery::NotObserved(
                    "the cell had already ended when its result was delivered".into(),
                ));
            }
            Ok(receipt.await.unwrap_or_else(|_| {
                LocalCompositionNestedDelivery::NotObserved(
                    "the cell ended before the result reached it".into(),
                )
            }))
        })
    }
}

impl LocalCompositionNestedResponder for Responder {
    fn resolve(
        self: Box<Self>,
        value: Value,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        self.deliver(Reply::Resolved(value, None))
    }
    fn resolve_with_execution(
        self: Box<Self>,
        value: Value,
        execution: Option<Arc<ExecutionValue>>,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        self.deliver(Reply::Resolved(value, execution))
    }
    fn reject(
        self: Box<Self>,
        reason: String,
    ) -> LocalCompositionFuture<'static, LocalCompositionNestedDelivery> {
        self.deliver(Reply::Rejected(reason))
    }
}

/// The host-side scope of one running cell.
pub(super) struct CellScope {
    cell_id: String,
    events: mpsc::Sender<LocalCompositionEvent>,
    limits: PythonHarnessSdkLimitsV1,
    next_call: AtomicU64,
    issued: AtomicU32,
    result_bytes: AtomicU64,
    permits: Arc<Semaphore>,
    stop: watch::Sender<Option<StopReason>>,
    pub(super) evidence: Arc<EvidenceTable>,
    pub(super) state: Arc<dyn NamedStatePort>,
    untrusted: Mutex<Vec<String>>,
    images: Mutex<Vec<(String, ImageValue)>>,
    attached: Mutex<Vec<String>>,
}

impl CellScope {
    pub(super) fn new(
        cell_id: String,
        events: mpsc::Sender<LocalCompositionEvent>,
        limits: PythonHarnessSdkLimitsV1,
        evidence: Arc<EvidenceTable>,
        state: Arc<dyn NamedStatePort>,
    ) -> Self {
        Self {
            cell_id,
            events,
            limits,
            next_call: AtomicU64::new(1),
            issued: AtomicU32::new(0),
            result_bytes: AtomicU64::new(0),
            permits: Arc::new(Semaphore::new(limits.concurrent_nested_calls as usize)),
            stop: watch::channel(None).0,
            evidence,
            state,
            untrusted: Mutex::new(Vec::new()),
            images: Mutex::new(Vec::new()),
            attached: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn stop_signal(&self) -> watch::Receiver<Option<StopReason>> {
        self.stop.subscribe()
    }

    pub(super) fn stop_reason(&self) -> Option<StopReason> {
        *self.stop.borrow()
    }

    /// Records that the cell read from an untrusted source.
    pub(super) fn mark_untrusted(&self, source: String) {
        let mut sources = lock(&self.untrusted);
        if !sources.contains(&source) {
            sources.push(source);
        }
    }

    pub(super) fn untrusted_sources(&self) -> Vec<String> {
        lock(&self.untrusted).clone()
    }

    /// Issues an image id valid for this cell's observation.
    pub(super) fn issue_image(&self, id: String, image: ImageValue) {
        lock(&self.images).push((id, image));
    }

    /// Issues an image the cell's output attaches without a print.
    pub(super) fn attach_image(&self, id: String, image: ImageValue) {
        lock(&self.attached).push(id.clone());
        self.issue_image(id, image);
    }

    pub(super) fn attached(&self) -> Vec<String> {
        lock(&self.attached).clone()
    }

    pub(super) fn images(&self) -> Vec<(String, ImageValue)> {
        lock(&self.images).clone()
    }

    /// Issues one nested child through the driver: counted, then admitted in
    /// order behind the concurrency bound.
    pub(super) async fn call(&self, tool: &str, input: Value) -> Result<NestedResult, NestedError> {
        let encoded = serde_json::to_vec(&input).map_or(usize::MAX, |bytes| bytes.len());
        if encoded as u64 > self.limits.nested_argument_bytes {
            return Err(NestedError::Invalid(format!(
                "{tool} argument is {encoded} bytes; the limit is {} bytes",
                self.limits.nested_argument_bytes
            )));
        }
        if self.issued.fetch_add(1, Ordering::AcqRel) >= self.limits.nested_calls {
            self.stop.send_replace(Some(StopReason::NestedCalls));
            return Err(NestedError::Stopped);
        }
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .map_err(|_| NestedError::Stopped)?;
        let (reply, receive) = oneshot::channel();
        let call = LocalCompositionNestedCall {
            cell_id: self.cell_id.clone(),
            runtime_call_id: self.next_call.fetch_add(1, Ordering::Relaxed).to_string(),
            tool_name: tool.to_owned(),
            input: Some(input),
        };
        let event = LocalCompositionEvent::NestedCall {
            call,
            responder: Box::new(Responder { reply }),
        };
        if self.events.send(event).await.is_err() {
            return Err(NestedError::Stopped);
        }
        let Ok((reply, sender)) = receive.await else {
            return Err(NestedError::Failed(format!(
                "{tool} ended without a result"
            )));
        };
        let ack = Ack {
            sender: Some(sender),
            _permit: permit,
        };
        match reply {
            Reply::Rejected(reason) => {
                ack.accept();
                Err(NestedError::Failed(crate::sanitize_nested_error(&reason)))
            }
            Reply::Resolved(value, execution) => self.admit_result(tool, value, execution, ack),
        }
    }

    fn admit_result(
        &self,
        tool: &str,
        value: Value,
        execution: Option<Arc<ExecutionValue>>,
        ack: Ack,
    ) -> Result<NestedResult, NestedError> {
        let bytes = serde_json::to_vec(&value).map_or(u64::MAX, |bytes| bytes.len() as u64);
        let total = self.result_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        if bytes > self.limits.nested_result_bytes {
            ack.accept();
            return Err(NestedError::Failed(format!(
                "{tool} result is {bytes} bytes; the limit is {} bytes",
                self.limits.nested_result_bytes
            )));
        }
        if total > self.limits.aggregate_nested_result_bytes {
            ack.accept();
            return Err(NestedError::Failed(format!(
                "nested results of this cell exceed {} bytes",
                self.limits.aggregate_nested_result_bytes
            )));
        }
        Ok(NestedResult {
            value,
            execution,
            ack,
        })
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
