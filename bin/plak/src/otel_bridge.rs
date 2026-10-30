//! Mirrors typed [`TelemetryObservation`] values into finished OTel spans.
//!
//! # Export model
//!
//! Only **finished** observations become spans. Started kinds (`TurnStarted`,
//! `ModelStarted`, `ToolStarted`, `CompactionStarted`) are dropped here; their
//! timing is carried on the corresponding finished span (`start_time =
//! end_time - elapsed`). `TurnStarted::queue_time` stays in the schema-versioned telemetry database
//! only.
//!
//! `ModelFirstEvent` is not a span: TTFT is stored in a bounded in-memory map
//! keyed by `attempt_id` and attached to the model-attempt span as a
//! `gen_ai.first_token` event and `plak.ttft_ns` when `ModelFinished` arrives.
//!
//! # Identity and parenting
//!
//! | Span | `trace_id` | `span_id` | `parent_span_id` |
//! | --- | --- | --- | --- |
//! | `turn` | session uuid (16 B) | turn uuid first 8 B | invalid (trace root) |
//! | `turn_usage` | session uuid | hash of turn uuid and `turn_usage` | turn span id |
//! | `model_attempt` | session uuid | attempt uuid first 8 B | turn span id |
//! | `tool_call` | session uuid | call uuid first 8 B | turn span id |
//! | `compaction` | session uuid | synthetic hash | turn span id when `turn_id` set, else invalid |
//! | `durability` | session uuid | synthetic hash | turn span id when set, else invalid |
//! | `durability_attempt` | session uuid | synthetic hash | turn span id when set, else invalid |
//! | `child_process_exited` | session uuid | synthetic hash | turn span id when set, else invalid |
//! | `credential_refresh` | dedicated credential hash | synthetic hash | invalid (no parent) |
//!
//! Synthetic span ids: SHA-256 over `(kind tag, turn_id or session_id uuid bytes,
//! timestamp secs+nanos)` truncated to 8 bytes (stable across processes).
//!
//! # Span names and attributes
//!
//! | Kind (finished only) | Span name | `span_kind` | Attributes |
//! | --- | --- | --- | --- |
//! | `TurnFinished` | `turn` | Internal | wall times, diagnostic counts, `plak.status` |
//! | Product `TurnUsage` | `turn_usage` | Internal | authoritative usage tokens, cost, operation counts |
//! | `ModelFinished` | `model_attempt` | Client | `plak.elapsed_ns`, `plak.status`, optional usage, optional `plak.ttft_ns` + `gen_ai.first_token` event |
//! | `ToolFinished` | `tool_call` | Internal | `plak.elapsed_ns`, `plak.status`, `plak.output_bytes` |
//! | `CompactionFinished` | `compaction` | Internal | compaction fields, optional usage |
//! | `DurabilityFinished` | `durability` | Internal | `plak.elapsed_ns`, `plak.status` |
//! | `DurabilityAttemptFinished` | `durability_attempt` | Internal | attempt/outcome, `plak.elapsed_ns` |
//! | `ChildProcessExited` | `child_process_exited` | Internal | child CPU/RSS fields |
//! | `CredentialRefresh` | `credential_refresh` | Internal | provider label, phase, optional failure class / HTTP status, generation |
//!
//! Finished session-scoped spans also carry correlation ids (`plak.session_id`,
//! …) and `gen_ai.operation.name` matching the span name. `CredentialRefresh`
//! spans are a dedicated parentless scope: they omit session/run/agent
//! identities so placeholder credential-lifecycle UUIDs are never exported.
//! No prompt or tool content.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
#[cfg(test)]
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::observation_projection::{
    AttributeValue, Output, SpanIdentity, SpanTiming, SyntheticKind,
};
use crate::telemetry_store::TelemetryItem;
use opentelemetry::trace::{SpanKind, Status, TraceFlags, TraceState};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, SpanData, SpanEvents, SpanLinks, SpanProcessor,
};
#[cfg(test)]
use plakous::agent::{
    ChildProcessKind, CompactionEventOutcome, CompactionEventReason, DurabilityAttemptKind,
    DurabilityAttemptOutcome, TelemetryObservationKind, TelemetryTerminalStatus,
};
use plakous::agent::{TelemetryContext, TelemetryObservation, TelemetrySink, TelemetrySinkError};
use plakous::model::{AttemptId, CallId, SessionId, Timestamp, TurnId, sha256_bytes};
use plakous_hosted::product_observation::ProductObservation;

const TTFT_CACHE_CAP: usize = 1024;
const BRIDGE_QUEUE_CAP: usize = 4096;

/// Failure to construct the OTel bridge.
#[derive(Debug)]
pub enum OtelBridgeError {
    /// The OTLP exporter could not be built.
    Exporter(String),
    /// The bridge worker thread could not be spawned.
    Thread(std::io::Error),
}

impl std::fmt::Display for OtelBridgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exporter(message) => write!(f, "{message}"),
            Self::Thread(error) => write!(f, "otel bridge thread: {error}"),
        }
    }
}

/// Nonblocking bridge from the typed telemetry port into OTel span export.
pub struct OtelBridge {
    sender: Mutex<Option<SyncSender<TelemetryItem>>>,
    processor: Arc<dyn SpanProcessor>,
    #[cfg(test)]
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct BridgeWorker {
    processor: Arc<dyn SpanProcessor>,
    ttft: TtftCache,
}

struct TtftRecord {
    at: SystemTime,
    elapsed: Duration,
}

struct TtftCache {
    order: VecDeque<AttemptId>,
    entries: HashMap<AttemptId, TtftRecord>,
}

impl TtftCache {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
        }
    }

    fn insert(&mut self, attempt_id: AttemptId, at: SystemTime, elapsed: Duration) {
        if let Some(record) = self.entries.get_mut(&attempt_id) {
            *record = TtftRecord { at, elapsed };
            return;
        }
        while self.order.len() >= TTFT_CACHE_CAP {
            if let Some(evicted) = self.order.pop_front() {
                self.entries.remove(&evicted);
            } else {
                break;
            }
        }
        self.order.push_back(attempt_id);
        self.entries.insert(attempt_id, TtftRecord { at, elapsed });
    }

    fn take(&mut self, attempt_id: AttemptId) -> Option<TtftRecord> {
        self.entries.remove(&attempt_id).inspect(|_| {
            self.order.retain(|id| *id != attempt_id);
        })
    }
}

impl OtelBridge {
    /// Builds a bridge with its own HTTP/protobuf OTLP batch processor.
    ///
    /// # Errors
    ///
    /// Returns [`OtelBridgeError`] when the exporter cannot be constructed.
    pub fn from_endpoint(endpoint: &str) -> Result<Self, OtelBridgeError> {
        use opentelemetry_otlp::WithExportConfig;

        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .with_timeout(Duration::from_millis(250))
            .build()
            .map_err(|error| OtelBridgeError::Exporter(error.to_string()))?;
        let resource = Resource::builder()
            .with_service_name("plak")
            .with_attribute(KeyValue::new("service.version", plakous::VERSION))
            .build();
        let mut processor = BatchSpanProcessor::builder(exporter).build();
        processor.set_resource(&resource);
        Self::with_processor(Arc::new(processor))
    }

    /// Creates a bridge backed by an injected span processor.
    ///
    /// # Errors
    ///
    /// Returns [`OtelBridgeError::Thread`] when the worker thread cannot start.
    pub fn with_processor(processor: Arc<dyn SpanProcessor>) -> Result<Self, OtelBridgeError> {
        spawn_bridge(processor)
    }

    /// Test-only: close the queue and join the bridge thread after queued work drains.
    #[cfg(test)]
    pub fn drain_for_test(self: &Arc<Self>) {
        *self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            handle
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        }
    }
}

fn spawn_bridge(processor: Arc<dyn SpanProcessor>) -> Result<OtelBridge, OtelBridgeError> {
    let (sender, receiver) = mpsc::sync_channel(BRIDGE_QUEUE_CAP);
    let worker = BridgeWorker {
        processor: Arc::clone(&processor),
        ttft: TtftCache::new(),
    };
    let handle = thread::Builder::new()
        .name("plak-otel-bridge".into())
        .spawn(move || bridge_loop(receiver, worker))
        .map_err(OtelBridgeError::Thread)?;
    #[cfg(not(test))]
    drop(handle);
    Ok(OtelBridge {
        sender: Mutex::new(Some(sender)),
        processor,
        #[cfg(test)]
        thread: Mutex::new(Some(handle)),
    })
}

fn bridge_loop(receiver: Receiver<TelemetryItem>, mut worker: BridgeWorker) {
    while let Ok(observation) = receiver.recv() {
        match observation {
            TelemetryItem::Agent(observation) => emit_observation(&mut worker, observation),
            TelemetryItem::Product(observation) => emit_product_observation(&worker, observation),
        }
    }
}

impl Drop for OtelBridge {
    fn drop(&mut self) {
        *self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        let processor = Arc::clone(&self.processor);
        thread::spawn(move || {
            let _ = processor.shutdown_with_timeout(Duration::from_millis(500));
        });
    }
}

impl TelemetrySink for OtelBridge {
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = sender.as_ref() else {
            return Err(TelemetrySinkError::Closed);
        };
        sender
            .try_send(TelemetryItem::Agent(observation))
            .map_err(|error| match error {
                TrySendError::Full(_) => TelemetrySinkError::Full,
                TrySendError::Disconnected(_) => TelemetrySinkError::Closed,
            })
    }
}

impl plakous_hosted::agent::ExtraTelemetrySink for OtelBridge {
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation"
    )]
    fn try_emit_product(&self, observation: ProductObservation) -> Result<(), TelemetrySinkError> {
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = sender.as_ref() else {
            return Err(TelemetrySinkError::Closed);
        };
        sender
            .try_send(TelemetryItem::Product(observation))
            .map_err(|error| match error {
                TrySendError::Full(_) => TelemetrySinkError::Full,
                TrySendError::Disconnected(_) => TelemetrySinkError::Closed,
            })
    }
}

#[allow(
    clippy::large_types_passed_by_value,
    reason = "Copy product observation"
)]
fn emit_product_observation(worker: &BridgeWorker, observation: ProductObservation) {
    let ProductObservation::TurnUsage {
        at,
        session_id,
        branch_id,
        turn_id,
        usage,
    } = observation
    else {
        return;
    };
    let model = usage.model();
    let mut attributes = vec![
        KeyValue::new("gen_ai.operation.name", "turn_usage"),
        KeyValue::new("plak.session_id", session_id.to_string()),
        KeyValue::new("plak.branch_id", branch_id.to_string()),
        KeyValue::new("plak.turn_id", turn_id.to_string()),
        KeyValue::new(
            "plak.timestamp_secs",
            i64::try_from(at.seconds()).unwrap_or(i64::MAX),
        ),
        KeyValue::new("plak.timestamp_nanos", i64::from(at.nanoseconds())),
    ];
    for (key, value) in [
        ("gen_ai.usage.input_tokens", model.input_tokens()),
        ("gen_ai.usage.output_tokens", model.output_tokens()),
        ("gen_ai.usage.reasoning_tokens", model.reasoning_tokens()),
        (
            "gen_ai.usage.cached_input_tokens",
            model.cached_input_tokens(),
        ),
        (
            "gen_ai.usage.cache_write_tokens",
            model.cache_write_tokens(),
        ),
        ("plak.turn.model_operations", usage.model_operations()),
        (
            "plak.turn.model_operations_reporting_usage",
            usage.model_operations_reporting_usage(),
        ),
        ("plak.turn.local_tool_attempts", usage.local_tool_attempts()),
    ] {
        push_u64(&mut attributes, key, value);
    }
    if let Some(cost) = model.cost_micros() {
        push_u64(&mut attributes, "plak.cost_micros", cost);
    }
    let digest = sha256_bytes(&[b"turn_usage", turn_id.as_uuid().as_bytes()]);
    let mut id = [0; 8];
    id.copy_from_slice(&digest[..8]);
    let span = SpanData {
        span_context: opentelemetry::trace::SpanContext::new(
            trace_id_for_session(session_id),
            opentelemetry::trace::SpanId::from_bytes(id),
            TraceFlags::SAMPLED,
            false,
            TraceState::NONE,
        ),
        parent_span_id: span_id_for_turn(turn_id),
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: "turn_usage".into(),
        start_time: timestamp_to_system_time(at),
        end_time: timestamp_to_system_time(at),
        attributes,
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status: Status::Unset,
        instrumentation_scope: InstrumentationScope::builder("plak.telemetry").build(),
    };
    worker.processor.on_end(span);
}

/// Every attribute name this bridge may attach to an exported span.
#[allow(dead_code, reason = "contract allowlist asserted by in-module tests")]
pub const ALLOWED_ATTRIBUTE_NAMES: &[&str] = &[
    "gen_ai.operation.name",
    "gen_ai.usage.input_tokens",
    "gen_ai.usage.output_tokens",
    "gen_ai.usage.reasoning_tokens",
    "gen_ai.usage.cached_input_tokens",
    "gen_ai.usage.cache_write_tokens",
    "plak.agent_id",
    "plak.attempt_id",
    "plak.branch_id",
    "plak.call_id",
    "plak.child_kind",
    "plak.compaction.reason",
    "plak.compaction.outcome",
    "plak.compaction.tokens_after",
    "plak.compaction.tokens_before",
    "plak.compactions",
    "plak.compaction_wall_ns",
    "plak.cost_micros",
    "plak.credential.failure_class",
    "plak.credential.generation",
    "plak.credential.http_status",
    "plak.credential.phase",
    "plak.credential.provider",
    "plak.durability.attempt",
    "plak.durability.outcome",
    "plak.retention.pruned_checkpoints",
    "plak.elapsed_ns",
    "plak.model_attempts",
    "plak.model_wall_ns",
    "plak.output_bytes",
    "plak.peak_rss_bytes",
    "plak.run_id",
    "plak.served_endpoint",
    "plak.session_id",
    "plak.status",
    "plak.system_cpu_ns",
    "plak.timestamp_nanos",
    "plak.timestamp_secs",
    "plak.tool_calls",
    "plak.tool_wall_ns",
    "plak.ttft_ns",
    "plak.turn_id",
    "plak.turn.model_operations",
    "plak.turn.model_operations_reporting_usage",
    "plak.turn.local_tool_attempts",
    "plak.user_cpu_ns",
];

#[allow(
    clippy::large_types_passed_by_value,
    reason = "TelemetrySink::try_emit passes observation by value"
)]
fn emit_observation(worker: &mut BridgeWorker, observation: TelemetryObservation) {
    use crate::observation_projection as projection;
    let context = observation.context();
    let timestamp = observation.timestamp();
    let projected = projection::project(&observation, Output::Span);
    if let (Some(attempt), Some(elapsed)) = (context.attempt_id(), projected.first_event) {
        worker
            .ttft
            .insert(attempt, timestamp_to_system_time(timestamp), elapsed);
    }
    let Some(span) = projected.span else {
        return;
    };
    let (projected_spec, timing, values, take_ttft) = match span {
        projection::Span::Observed {
            spec,
            timing,
            attributes,
            take_ttft,
        } => (spec, timing, attributes, take_ttft),
        projection::Span::Credential {
            provider,
            generation,
            elapsed,
            status,
            attributes,
        } => {
            let end_time = timestamp_to_system_time(timestamp);
            let start_time = elapsed
                .and_then(|elapsed| end_time.checked_sub(elapsed))
                .unwrap_or(end_time);
            emit_credential_span(
                worker,
                credential_span_id(&provider, generation, timestamp),
                credential_trace_id(&provider, generation, timestamp),
                start_time,
                end_time,
                span_attributes(attributes),
                span_status(status),
            );
            return;
        }
    };
    let span_id = match projected_spec.span_id {
        SpanIdentity::Turn(id) => span_id_for_turn(id),
        SpanIdentity::Attempt(id) => span_id_for_attempt(id),
        SpanIdentity::Call(id) => span_id_for_call(id),
        SpanIdentity::Synthetic(kind) => synthetic_span_id(kind, context, timestamp),
    };
    let parent = match projected_spec.parent {
        projection::ParentSpec::Root => ParentSpec::Root,
        projection::ParentSpec::Turn => ParentSpec::Turn,
        projection::ParentSpec::TurnOrRoot => ParentSpec::TurnOrRoot,
    };
    let kind = match projected_spec.kind {
        projection::SpanKind::Internal => SpanKind::Internal,
        projection::SpanKind::Client => SpanKind::Client,
    };
    let spec = SpanSpec {
        name: projected_spec.name,
        span_id,
        parent,
        kind,
        status: span_status(projected_spec.status),
    };
    let mut attributes = base_attributes(context, timestamp);
    attributes.extend(span_attributes(values));
    let mut events = SpanEvents::default();
    if take_ttft && let Some(record) = context.attempt_id().and_then(|id| worker.ttft.take(id)) {
        push_u128(&mut attributes, "plak.ttft_ns", duration_ns(record.elapsed));
        events.events.push(opentelemetry::trace::Event::new(
            "gen_ai.first_token",
            record.at,
            Vec::new(),
            0,
        ));
    }
    match timing {
        SpanTiming::Elapsed(elapsed) => emit_finished(
            worker, context, timestamp, elapsed, spec, attributes, events,
        ),
        SpanTiming::Instant => {
            let at = timestamp_to_system_time(timestamp);
            emit_span(worker, context, spec, at, at, attributes, events);
        }
    }
}

fn span_attributes(attributes: Vec<crate::observation_projection::Attribute>) -> Vec<KeyValue> {
    attributes
        .into_iter()
        .map(|attribute| match attribute.value {
            AttributeValue::Integer(value) => KeyValue::new(attribute.key, value),
            AttributeValue::Text(value) => KeyValue::new(attribute.key, value),
            AttributeValue::StaticText(value) => KeyValue::new(attribute.key, value),
        })
        .collect()
}

fn span_status(status: crate::observation_projection::Status) -> Status {
    match status {
        crate::observation_projection::Status::Ok => Status::Ok,
        crate::observation_projection::Status::Error(reason) => Status::error(reason),
    }
}

struct SpanSpec {
    name: &'static str,
    span_id: opentelemetry::trace::SpanId,
    parent: ParentSpec,
    kind: SpanKind,
    status: Status,
}

enum ParentSpec {
    Root,
    Turn,
    TurnOrRoot,
}

fn emit_finished(
    worker: &BridgeWorker,
    ctx: TelemetryContext,
    timestamp: Timestamp,
    elapsed: Duration,
    spec: SpanSpec,
    attributes: Vec<KeyValue>,
    events: SpanEvents,
) {
    let end_time = timestamp_to_system_time(timestamp);
    let start_time = end_time.checked_sub(elapsed).unwrap_or(UNIX_EPOCH);
    emit_span(worker, ctx, spec, start_time, end_time, attributes, events);
}

fn emit_span(
    worker: &BridgeWorker,
    ctx: TelemetryContext,
    spec: SpanSpec,
    start_time: SystemTime,
    end_time: SystemTime,
    attributes: Vec<KeyValue>,
    events: SpanEvents,
) {
    let trace_id = trace_id_for_session(ctx.session_id());
    let (parent_span_id, parent_span_is_remote) = match spec.parent {
        ParentSpec::Root => (opentelemetry::trace::SpanId::INVALID, false),
        ParentSpec::Turn => ctx
            .turn_id()
            .map_or((opentelemetry::trace::SpanId::INVALID, false), |turn_id| {
                (span_id_for_turn(turn_id), false)
            }),
        ParentSpec::TurnOrRoot => ctx
            .turn_id()
            .map_or((opentelemetry::trace::SpanId::INVALID, false), |turn_id| {
                (span_id_for_turn(turn_id), false)
            }),
    };
    let span_context = opentelemetry::trace::SpanContext::new(
        trace_id,
        spec.span_id,
        TraceFlags::SAMPLED,
        false,
        TraceState::NONE,
    );
    let span = SpanData {
        span_context,
        parent_span_id,
        parent_span_is_remote,
        span_kind: spec.kind,
        name: spec.name.into(),
        start_time,
        end_time,
        attributes,
        dropped_attributes_count: 0,
        events,
        links: SpanLinks::default(),
        status: spec.status,
        instrumentation_scope: InstrumentationScope::builder("plak.telemetry").build(),
    };
    worker.processor.on_end(span);
}

fn emit_credential_span(
    worker: &BridgeWorker,
    span_id: opentelemetry::trace::SpanId,
    trace_id: opentelemetry::trace::TraceId,
    start_time: SystemTime,
    end_time: SystemTime,
    attributes: Vec<KeyValue>,
    status: Status,
) {
    let span = SpanData {
        span_context: opentelemetry::trace::SpanContext::new(
            trace_id,
            span_id,
            TraceFlags::SAMPLED,
            false,
            TraceState::NONE,
        ),
        parent_span_id: opentelemetry::trace::SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Internal,
        name: "credential_refresh".into(),
        start_time,
        end_time,
        attributes,
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links: SpanLinks::default(),
        status,
        instrumentation_scope: InstrumentationScope::builder("plak.credential").build(),
    };
    worker.processor.on_end(span);
}

fn base_attributes(ctx: TelemetryContext, timestamp: Timestamp) -> Vec<KeyValue> {
    let mut attributes = vec![
        KeyValue::new("plak.session_id", ctx.session_id().to_string()),
        KeyValue::new("plak.branch_id", ctx.branch_id().to_string()),
        KeyValue::new("plak.run_id", ctx.run_id().to_string()),
        KeyValue::new("plak.agent_id", ctx.agent_id().to_string()),
        KeyValue::new(
            "plak.timestamp_secs",
            i64::try_from(timestamp.seconds()).unwrap_or(i64::MAX),
        ),
        KeyValue::new("plak.timestamp_nanos", i64::from(timestamp.nanoseconds())),
    ];
    if let Some(turn_id) = ctx.turn_id() {
        attributes.push(KeyValue::new("plak.turn_id", turn_id.to_string()));
    }
    if let Some(attempt_id) = ctx.attempt_id() {
        attributes.push(KeyValue::new("plak.attempt_id", attempt_id.to_string()));
    }
    if let Some(call_id) = ctx.call_id() {
        attributes.push(KeyValue::new("plak.call_id", call_id.to_string()));
    }
    attributes
}

fn push_u64(attributes: &mut Vec<KeyValue>, key: &'static str, value: u64) {
    attributes.push(KeyValue::new(key, i64::try_from(value).unwrap_or(i64::MAX)));
}

fn push_u128(attributes: &mut Vec<KeyValue>, key: &'static str, value: u128) {
    attributes.push(KeyValue::new(key, value.to_string()));
}

fn trace_id_for_session(session_id: SessionId) -> opentelemetry::trace::TraceId {
    opentelemetry::trace::TraceId::from_bytes(*session_id.as_uuid().as_bytes())
}

fn span_id_from_uuid(uuid: uuid::Uuid) -> opentelemetry::trace::SpanId {
    let bytes = uuid.as_bytes();
    let mut span_bytes = [0_u8; 8];
    span_bytes.copy_from_slice(&bytes[..8]);
    opentelemetry::trace::SpanId::from_bytes(span_bytes)
}

fn span_id_for_turn(turn_id: TurnId) -> opentelemetry::trace::SpanId {
    span_id_from_uuid(turn_id.as_uuid())
}

fn span_id_for_attempt(attempt_id: AttemptId) -> opentelemetry::trace::SpanId {
    span_id_from_uuid(attempt_id.as_uuid())
}

fn span_id_for_call(call_id: CallId) -> opentelemetry::trace::SpanId {
    span_id_from_uuid(call_id.as_uuid())
}

fn credential_trace_id(
    provider: &str,
    generation: u64,
    timestamp: Timestamp,
) -> opentelemetry::trace::TraceId {
    let digest = credential_identity_digest(provider, generation, timestamp);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    opentelemetry::trace::TraceId::from_bytes(bytes)
}

fn credential_span_id(
    provider: &str,
    generation: u64,
    timestamp: Timestamp,
) -> opentelemetry::trace::SpanId {
    let digest = credential_identity_digest(provider, generation, timestamp);
    let mut bytes = [0_u8; 8];
    bytes.copy_from_slice(&digest[16..24]);
    opentelemetry::trace::SpanId::from_bytes(bytes)
}

fn credential_identity_digest(provider: &str, generation: u64, timestamp: Timestamp) -> [u8; 32] {
    sha256_bytes(&[
        b"plak.credential",
        provider.as_bytes(),
        &generation.to_be_bytes(),
        &timestamp.seconds().to_be_bytes(),
        &timestamp.nanoseconds().to_be_bytes(),
    ])
}

fn synthetic_span_id(
    kind: SyntheticKind,
    ctx: TelemetryContext,
    timestamp: Timestamp,
) -> opentelemetry::trace::SpanId {
    let scope = ctx.turn_id().map_or_else(
        || *ctx.session_id().as_uuid().as_bytes(),
        |turn_id| *turn_id.as_uuid().as_bytes(),
    );
    let digest = sha256_bytes(&[
        kind.tag().as_bytes(),
        &scope,
        &timestamp.seconds().to_be_bytes(),
        &timestamp.nanoseconds().to_be_bytes(),
    ]);
    let mut span_bytes = [0_u8; 8];
    span_bytes.copy_from_slice(&digest[..8]);
    opentelemetry::trace::SpanId::from_bytes(span_bytes)
}

fn timestamp_to_system_time(timestamp: Timestamp) -> SystemTime {
    UNIX_EPOCH + Duration::new(timestamp.seconds(), timestamp.nanoseconds())
}

fn duration_ns(duration: Duration) -> u128 {
    duration.as_nanos()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "otel bridge contract tests"
)]
mod tests;
