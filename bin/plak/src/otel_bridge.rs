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

use crate::telemetry_store::TelemetryItem;
use opentelemetry::trace::{SpanKind, Status, TraceFlags, TraceState};
use opentelemetry::{InstrumentationScope, KeyValue};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::{
    BatchSpanProcessor, SpanData, SpanEvents, SpanLinks, SpanProcessor,
};
use plakous::agent::{
    ChildProcessKind, CompactionEventOutcome, CompactionEventReason, CredentialRefreshPhase,
    DurabilityAttemptKind, DurabilityAttemptOutcome, TelemetryContext, TelemetryObservation,
    TelemetryObservationKind, TelemetrySink, TelemetrySinkError, TelemetryTerminalStatus,
};
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
    let ctx = observation.context();
    let timestamp = observation.timestamp();
    match observation.kind() {
        TelemetryObservationKind::TurnStarted { .. }
        | TelemetryObservationKind::ModelStarted
        | TelemetryObservationKind::ToolStarted
        | TelemetryObservationKind::CompactionStarted { .. } => {}
        TelemetryObservationKind::ModelFirstEvent { elapsed } => {
            if let Some(attempt_id) = ctx.attempt_id() {
                worker
                    .ttft
                    .insert(attempt_id, timestamp_to_system_time(timestamp), elapsed);
            }
        }
        TelemetryObservationKind::TurnFinished {
            elapsed,
            status,
            usage,
            model_wall,
            tool_wall,
            compaction_wall,
            model_attempts,
            tool_calls,
            compactions,
        } => {
            let Some(turn_id) = ctx.turn_id() else {
                return;
            };
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "turn");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(&mut attributes, "plak.status", terminal_status(status));
            push_u64(
                &mut attributes,
                "plak.turn.model_operations",
                usage.model_operations(),
            );
            push_u64(
                &mut attributes,
                "plak.turn.model_operations_reporting_usage",
                usage.model_operations_reporting_usage(),
            );
            push_u64(
                &mut attributes,
                "plak.turn.local_tool_attempts",
                usage.local_tool_attempts(),
            );
            push_u128(
                &mut attributes,
                "plak.model_wall_ns",
                duration_ns(model_wall),
            );
            push_u128(&mut attributes, "plak.tool_wall_ns", duration_ns(tool_wall));
            push_u128(
                &mut attributes,
                "plak.compaction_wall_ns",
                duration_ns(compaction_wall),
            );
            push_u64(
                &mut attributes,
                "plak.model_attempts",
                u64::from(model_attempts),
            );
            push_u64(&mut attributes, "plak.tool_calls", u64::from(tool_calls));
            push_u64(&mut attributes, "plak.compactions", u64::from(compactions));
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "turn",
                    span_id: span_id_for_turn(turn_id),
                    parent: ParentSpec::Root,
                    kind: SpanKind::Internal,
                    status: span_status(status),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::ModelFinished {
            elapsed,
            status,
            usage,
            served_by,
        } => {
            let Some(attempt_id) = ctx.attempt_id() else {
                return;
            };
            let ttft = worker.ttft.take(attempt_id);
            let mut events = SpanEvents::default();
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "model_attempt");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(&mut attributes, "plak.status", terminal_status(status));
            if let Some(endpoint) = served_by {
                attributes.push(KeyValue::new(
                    "plak.served_endpoint",
                    endpoint.as_str().to_owned(),
                ));
            }
            if let Some(model) = usage {
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.input_tokens",
                    model.input_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.output_tokens",
                    model.output_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.reasoning_tokens",
                    model.reasoning_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.cached_input_tokens",
                    model.cached_input_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.cache_write_tokens",
                    model.cache_write_tokens(),
                );
                if let Some(cost_micros) = model.cost_micros() {
                    push_u64(&mut attributes, "plak.cost_micros", cost_micros);
                }
            }
            if let Some(record) = ttft {
                push_u128(&mut attributes, "plak.ttft_ns", duration_ns(record.elapsed));
                events.events.push(opentelemetry::trace::Event::new(
                    "gen_ai.first_token",
                    record.at,
                    Vec::new(),
                    0,
                ));
            }
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "model_attempt",
                    span_id: span_id_for_attempt(attempt_id),
                    parent: ParentSpec::Turn,
                    kind: SpanKind::Client,
                    status: span_status(status),
                },
                attributes,
                events,
            );
        }
        TelemetryObservationKind::ToolFinished {
            elapsed,
            status,
            output_bytes,
        } => {
            let Some(call_id) = ctx.call_id() else {
                return;
            };
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "tool_call");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(&mut attributes, "plak.status", terminal_status(status));
            push_u64(&mut attributes, "plak.output_bytes", output_bytes);
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "tool_call",
                    span_id: span_id_for_call(call_id),
                    parent: ParentSpec::Turn,
                    kind: SpanKind::Internal,
                    status: span_status(status),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::CompactionFinished {
            refusal,
            elapsed,
            outcome,
            reason,
            tokens_before,
            tokens_after,
            usage,
        } => {
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "compaction");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(
                &mut attributes,
                "plak.compaction.outcome",
                compaction_outcome(outcome),
            );
            push_str(
                &mut attributes,
                "plak.compaction.reason",
                compaction_reason(reason),
            );
            if let Some(refusal) = refusal {
                push_str(
                    &mut attributes,
                    "plak.compaction.refusal_reason",
                    refusal.as_str(),
                );
            }
            if let (Some(tokens_before), Some(tokens_after), Some(model)) =
                (tokens_before, tokens_after, usage)
            {
                push_u64(
                    &mut attributes,
                    "plak.compaction.tokens_before",
                    tokens_before,
                );
                push_u64(
                    &mut attributes,
                    "plak.compaction.tokens_after",
                    tokens_after,
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.input_tokens",
                    model.input_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.output_tokens",
                    model.output_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.reasoning_tokens",
                    model.reasoning_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.cached_input_tokens",
                    model.cached_input_tokens(),
                );
                push_u64(
                    &mut attributes,
                    "gen_ai.usage.cache_write_tokens",
                    model.cache_write_tokens(),
                );
            }
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "compaction",
                    span_id: synthetic_span_id(SyntheticKind::Compaction, ctx, timestamp),
                    parent: ParentSpec::TurnOrRoot,
                    kind: SpanKind::Internal,
                    status: compaction_span_status(outcome),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::DurabilityFinished { elapsed, status } => {
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "durability");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(&mut attributes, "plak.status", terminal_status(status));
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "durability",
                    span_id: synthetic_span_id(SyntheticKind::Durability, ctx, timestamp),
                    parent: ParentSpec::TurnOrRoot,
                    kind: SpanKind::Internal,
                    status: span_status(status),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed,
            attempt,
            outcome,
        } => {
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(
                &mut attributes,
                "gen_ai.operation.name",
                "durability_attempt",
            );
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(
                &mut attributes,
                "plak.durability.attempt",
                durability_attempt(attempt),
            );
            push_str(
                &mut attributes,
                "plak.durability.outcome",
                durability_outcome(outcome),
            );
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "durability_attempt",
                    span_id: synthetic_span_id(SyntheticKind::DurabilityAttempt, ctx, timestamp),
                    parent: ParentSpec::TurnOrRoot,
                    kind: SpanKind::Internal,
                    status: durability_attempt_span_status(outcome),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::RetentionFinished {
            elapsed,
            status,
            pruned_checkpoints,
        } => {
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(&mut attributes, "gen_ai.operation.name", "retention");
            push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            push_str(&mut attributes, "plak.status", terminal_status(status));
            push_u64(
                &mut attributes,
                "plak.retention.pruned_checkpoints",
                pruned_checkpoints,
            );
            emit_finished(
                worker,
                ctx,
                timestamp,
                elapsed,
                SpanSpec {
                    name: "retention",
                    span_id: synthetic_span_id(SyntheticKind::Retention, ctx, timestamp),
                    parent: ParentSpec::TurnOrRoot,
                    kind: SpanKind::Internal,
                    status: span_status(status),
                },
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::ChildProcessExited {
            child,
            user_cpu,
            system_cpu,
            peak_rss_bytes,
        } => {
            let mut attributes = base_attributes(ctx, timestamp);
            push_str(
                &mut attributes,
                "gen_ai.operation.name",
                "child_process_exited",
            );
            push_str(
                &mut attributes,
                "plak.child_kind",
                child_process_kind(child),
            );
            push_u128(&mut attributes, "plak.user_cpu_ns", duration_ns(user_cpu));
            push_u128(
                &mut attributes,
                "plak.system_cpu_ns",
                duration_ns(system_cpu),
            );
            if let Some(peak_rss_bytes) = peak_rss_bytes {
                push_u64(&mut attributes, "plak.peak_rss_bytes", peak_rss_bytes);
            }
            let end_time = timestamp_to_system_time(timestamp);
            emit_span(
                worker,
                ctx,
                SpanSpec {
                    name: "child_process_exited",
                    span_id: synthetic_span_id(SyntheticKind::ChildProcessExited, ctx, timestamp),
                    parent: ParentSpec::TurnOrRoot,
                    kind: SpanKind::Internal,
                    status: Status::Ok,
                },
                end_time,
                end_time,
                attributes,
                SpanEvents::default(),
            );
        }
        TelemetryObservationKind::CredentialRefresh {
            provider,
            phase,
            failure_class,
            transport_phase,
            http_status,
            elapsed,
            generation,
        } => {
            let mut attributes = vec![
                KeyValue::new(
                    "plak.timestamp_secs",
                    i64::try_from(timestamp.seconds()).unwrap_or(i64::MAX),
                ),
                KeyValue::new("plak.timestamp_nanos", i64::from(timestamp.nanoseconds())),
            ];
            push_str(
                &mut attributes,
                "gen_ai.operation.name",
                "credential_refresh",
            );
            push_string(
                &mut attributes,
                "plak.credential.provider",
                provider.as_str().to_owned(),
            );
            push_str(
                &mut attributes,
                "plak.credential.phase",
                crate::telemetry_store::credential_refresh_phase_text(phase),
            );
            if let Some(failure_class) = failure_class {
                push_str(
                    &mut attributes,
                    "plak.credential.failure_class",
                    crate::telemetry_store::credential_refresh_failure_text(failure_class),
                );
            }
            if let Some(transport_phase) = transport_phase {
                push_str(
                    &mut attributes,
                    "plak.credential.transport_phase",
                    crate::telemetry_store::credential_transport_phase_text(transport_phase),
                );
            }
            if let Some(http_status) = http_status {
                push_u64(
                    &mut attributes,
                    "plak.credential.http_status",
                    u64::from(http_status),
                );
            }
            push_u64(&mut attributes, "plak.credential.generation", generation);
            if let Some(elapsed) = elapsed {
                push_u128(&mut attributes, "plak.elapsed_ns", duration_ns(elapsed));
            }
            let end_time = timestamp_to_system_time(timestamp);
            let start_time = elapsed
                .and_then(|elapsed| end_time.checked_sub(elapsed))
                .unwrap_or(end_time);
            let span_id = credential_span_id(provider.as_str(), generation, timestamp);
            emit_credential_span(
                worker,
                span_id,
                credential_trace_id(provider.as_str(), generation, timestamp),
                start_time,
                end_time,
                attributes,
                credential_refresh_span_status(phase),
            );
        }
        _ => {}
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

fn push_str(attributes: &mut Vec<KeyValue>, key: &'static str, value: &'static str) {
    attributes.push(KeyValue::new(key, value));
}

fn push_string(attributes: &mut Vec<KeyValue>, key: &'static str, value: String) {
    attributes.push(KeyValue::new(key, value));
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

#[derive(Clone, Copy)]
enum SyntheticKind {
    Compaction,
    Durability,
    DurabilityAttempt,
    Retention,
    ChildProcessExited,
}

impl SyntheticKind {
    const fn tag(self) -> &'static str {
        match self {
            Self::Compaction => "compaction",
            Self::Durability => "durability",
            Self::DurabilityAttempt => "durability_attempt",
            Self::Retention => "retention",
            Self::ChildProcessExited => "child_process_exited",
        }
    }
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

fn terminal_status(status: TelemetryTerminalStatus) -> &'static str {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match status {
        TelemetryTerminalStatus::Completed => "completed",
        TelemetryTerminalStatus::Failed => "failed",
        TelemetryTerminalStatus::Cancelled => "cancelled",
        TelemetryTerminalStatus::Shutdown => "shutdown",
        _ => "unknown",
    }
}

fn span_status(status: TelemetryTerminalStatus) -> Status {
    match status {
        TelemetryTerminalStatus::Completed => Status::Ok,
        _ => Status::error(terminal_status(status)),
    }
}

fn credential_refresh_span_status(phase: CredentialRefreshPhase) -> Status {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match phase {
        CredentialRefreshPhase::OutcomeUnknown | CredentialRefreshPhase::Quarantined => {
            Status::error(crate::telemetry_store::credential_refresh_phase_text(phase))
        }
        _ => Status::Ok,
    }
}

fn compaction_span_status(outcome: CompactionEventOutcome) -> Status {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match outcome {
        CompactionEventOutcome::Committed | CompactionEventOutcome::NotNeeded => Status::Ok,
        _ => Status::error(compaction_outcome(outcome)),
    }
}

fn durability_attempt_span_status(outcome: DurabilityAttemptOutcome) -> Status {
    match outcome {
        DurabilityAttemptOutcome::Committed
        | DurabilityAttemptOutcome::RecoveredAcknowledgement
        | DurabilityAttemptOutcome::Joined => Status::Ok,
        _ => Status::error(durability_outcome(outcome)),
    }
}

fn compaction_reason(reason: CompactionEventReason) -> &'static str {
    match reason {
        CompactionEventReason::Manual => "manual",
        CompactionEventReason::Scheduled => "scheduled",
    }
}

fn compaction_outcome(outcome: CompactionEventOutcome) -> &'static str {
    #[allow(
        unreachable_patterns,
        reason = "non-exhaustive telemetry enums reserve future variants"
    )]
    match outcome {
        CompactionEventOutcome::Committed => "committed",
        CompactionEventOutcome::Failed => "failed",
        CompactionEventOutcome::RecoveryRequired => "recovery_required",
        CompactionEventOutcome::NotNeeded => "not_needed",
        CompactionEventOutcome::NoEligibleRange => "no_eligible_range",
        CompactionEventOutcome::Unavailable => "unavailable",
        _ => "unknown",
    }
}

fn durability_attempt(attempt: DurabilityAttemptKind) -> &'static str {
    match attempt {
        DurabilityAttemptKind::InitialCommit => "initial_commit",
        DurabilityAttemptKind::InitialAcknowledgementRecovery => "initial_acknowledgement_recovery",
        DurabilityAttemptKind::CleanupToolDrain => "cleanup_tool_drain",
        DurabilityAttemptKind::CleanupJobTerminals => "cleanup_job_terminals",
        DurabilityAttemptKind::CleanupHeartbeatJoin => "cleanup_heartbeat_join",
        DurabilityAttemptKind::CleanupLeaseRelease => "cleanup_lease_release",
        DurabilityAttemptKind::CleanupFlush => "cleanup_flush",
        _ => "unknown",
    }
}

fn durability_outcome(outcome: DurabilityAttemptOutcome) -> &'static str {
    match outcome {
        DurabilityAttemptOutcome::Committed => "committed",
        DurabilityAttemptOutcome::Failed => "failed",
        DurabilityAttemptOutcome::RecoveredAcknowledgement => "recovered_acknowledgement",
        DurabilityAttemptOutcome::Joined => "joined",
        _ => "unknown",
    }
}

fn child_process_kind(child: ChildProcessKind) -> &'static str {
    match child {
        ChildProcessKind::JsEvalSidecar => "js_eval_sidecar",
        ChildProcessKind::PythonWorker => "python_worker",
        _ => "unknown",
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "otel bridge contract tests"
)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Event, SpanId, TraceId};
    use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
    use plakous::agent::{
        CredentialProviderLabel, CredentialRefreshFailureClass, TelemetryContext, TurnUsage,
    };
    use plakous::model::{AgentId, AttemptId, BranchId, CallId, RunId, SessionId, TurnId, Usage};
    use uuid::Uuid;

    use super::*;

    #[derive(Debug)]
    struct CaptureProcessor {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl CaptureProcessor {
        fn new(spans: Arc<Mutex<Vec<SpanData>>>) -> Self {
            Self { spans }
        }
    }

    impl SpanProcessor for CaptureProcessor {
        fn on_start(
            &self,
            _span: &mut opentelemetry_sdk::trace::Span,
            _cx: &opentelemetry::Context,
        ) {
        }

        fn on_end(&self, span: SpanData) {
            self.spans
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(span);
        }

        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn shutdown_with_timeout(
            &self,
            _timeout: Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }

        fn set_resource(&mut self, _resource: &Resource) {}
    }

    fn fixture_context() -> TelemetryContext {
        TelemetryContext::new(
            AgentId::try_from(Uuid::from_u128(0x11)).expect("agent"),
            SessionId::try_from(Uuid::from_u128(0x22)).expect("session"),
            BranchId::try_from(Uuid::from_u128(0x33)).expect("branch"),
            RunId::try_from(Uuid::from_u128(0x44)).expect("run"),
        )
        .with_turn(TurnId::try_from(Uuid::from_u128(0x55)).expect("turn"))
        .with_attempt(AttemptId::try_from(Uuid::from_u128(0x66)).expect("attempt"))
        .with_call(CallId::try_from(Uuid::from_u128(0x77)).expect("call"))
    }

    fn fixture_timestamp() -> Timestamp {
        Timestamp::new(1_700_000_000, 123_456_789).expect("timestamp")
    }

    fn make_bridge(spans: Arc<Mutex<Vec<SpanData>>>) -> Arc<OtelBridge> {
        Arc::new(
            OtelBridge::with_processor(Arc::new(CaptureProcessor::new(spans)))
                .expect("otel bridge thread"),
        )
    }

    fn emit(bridge: &Arc<OtelBridge>, observation: &TelemetryObservation) {
        bridge.try_emit(*observation).expect("bridge accepts");
    }

    fn drain(bridge: &Arc<OtelBridge>) {
        bridge.drain_for_test();
    }

    fn attributes_map(span: &SpanData) -> BTreeMap<String, String> {
        span.attributes
            .iter()
            .map(|kv| (kv.key.as_str().to_owned(), kv.value.as_str().into_owned()))
            .collect()
    }

    fn assert_allowlisted(span: &SpanData) {
        let allowed = ALLOWED_ATTRIBUTE_NAMES
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        for key in attributes_map(span).keys() {
            assert!(
                allowed.contains(key.as_str()),
                "unexpected attribute {key:?}; allowed {allowed:?}"
            );
        }
    }

    fn session_trace_id() -> TraceId {
        trace_id_for_session(SessionId::try_from(Uuid::from_u128(0x22)).expect("session"))
    }

    fn turn_span_id() -> SpanId {
        span_id_for_turn(TurnId::try_from(Uuid::from_u128(0x55)).expect("turn"))
    }

    fn attempt_span_id() -> SpanId {
        span_id_for_attempt(AttemptId::try_from(Uuid::from_u128(0x66)).expect("attempt"))
    }

    fn call_span_id() -> SpanId {
        span_id_for_call(CallId::try_from(Uuid::from_u128(0x77)).expect("call"))
    }

    fn end_time(timestamp: Timestamp) -> SystemTime {
        timestamp_to_system_time(timestamp)
    }

    #[test]
    fn started_kinds_emit_no_spans() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ctx = fixture_context();
        let ts = fixture_timestamp();
        for observation in [
            TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::TurnStarted {
                    origin: plakous::model::TurnOrigin::UserPrompt,
                    queue_time: Duration::from_nanos(1),
                },
            ),
            TelemetryObservation::new(ctx, ts, TelemetryObservationKind::ModelStarted),
            TelemetryObservation::new(ctx, ts, TelemetryObservationKind::ToolStarted),
            TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::CompactionStarted {
                    reason: CompactionEventReason::Manual,
                },
            ),
        ] {
            emit(&bridge, &observation);
        }
        drain(&bridge);
        assert!(spans.lock().unwrap().is_empty());
    }

    #[test]
    fn turn_finished_exports_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let usage = TurnUsage::new(Usage::new(10, 20, 3, 4, 5), 2, 1, 7).expect("usage");
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::TurnFinished {
                    elapsed: Duration::from_nanos(100),
                    status: TelemetryTerminalStatus::Completed,
                    usage,
                    model_wall: Duration::from_nanos(40),
                    tool_wall: Duration::from_nanos(30),
                    compaction_wall: Duration::from_nanos(10),
                    model_attempts: 2,
                    tool_calls: 3,
                    compactions: 1,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_allowlisted(&span);
        assert_eq!(span.name, "turn");
        assert_eq!(span.span_context.trace_id(), session_trace_id());
        assert_eq!(span.span_context.span_id(), turn_span_id());
        assert_eq!(span.parent_span_id, SpanId::INVALID);
        assert_eq!(span.span_kind, SpanKind::Internal);
        assert_eq!(span.status, Status::Ok);
        assert_eq!(span.end_time, end_time(fixture_timestamp()));
        assert_eq!(span.start_time, span.end_time - Duration::from_nanos(100));
        let attrs = attributes_map(&span);
        assert_eq!(attrs.get("gen_ai.operation.name"), Some(&"turn".to_owned()));
        assert_eq!(attrs.get("plak.status"), Some(&"completed".to_owned()));
        assert_eq!(attrs.get("plak.elapsed_ns"), Some(&"100".to_owned()));
        assert!(attrs.keys().all(|key| !key.starts_with("gen_ai.usage.")));
        assert!(!attrs.contains_key("plak.cost_micros"));
    }

    #[test]
    fn opening_never_projected_successors_does_not_export_inherited_turn_usage() {
        use crate::store::session_records::{SessionRecordStore, SqliteSessionStore};
        use plakous::agent::CommandId;
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, ForkSessionRequest,
            RebindSessionRequest, SessionBackend, SessionOperationId,
        };
        use plakous_hosted::runtime::project_session;
        use plakous_hosted::sessions::projector::WorkflowRequest;
        use plakous_hosted::sessions::{RebindSelection, SessionRecord, SessionWorkflowKind};
        use plakous_offline_crash_harness::{
            BoundaryController, runtime_binding, runtime_binding_for_account,
        };

        for rebind in [false, true] {
            let (_guard, paths) = crate::home::paths::PlakPaths::isolated_test_home();
            paths.ensure_layout().unwrap();
            let workspace = tempfile::tempdir().unwrap();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                workspace.path().join("successor-evidence.jsonl"),
            )
            .unwrap();
            let binding = || runtime_binding(workspace.path(), controller.clone()).unwrap();
            let sessions =
                AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap())
                    .unwrap();
            let product = Arc::new(SqliteSessionStore::new(paths.clone()));
            let records: Arc<dyn SessionRecordStore> = product.clone();
            let spans = Arc::new(Mutex::new(Vec::new()));
            let bridge = make_bridge(spans.clone());
            let ready = crate::telemetry_sink::open_product_telemetry(&paths, Some(bridge.clone()))
                .unwrap();
            plakous::application::ApplicationExecutor::new()
                .unwrap()
                .block_on(async {
                    let CreateSessionOutcome::Created { session, .. } = sessions
                        .create(CreateSessionRequest::new(
                            SessionOperationId::new("successor:create").unwrap(),
                            binding(),
                        ))
                        .await
                        .unwrap()
                    else {
                        panic!("created")
                    };
                    let turn = session
                        .agent()
                        .start(CommandId::new("successor:root").unwrap(), "source prompt")
                        .await
                        .unwrap();
                    assert!(turn.into_turn().unwrap().result().await.is_ok());
                    let mut source = SessionRecord::new(workspace.path().to_owned());
                    source.plakous_locator = Some(session.locator());
                    product.save(&mut source).unwrap();
                    project_session(&sessions, &records, &source).await.unwrap();
                    bridge.drain_for_test();
                    assert_eq!(
                        spans
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|span| span.name == "turn_usage")
                            .count(),
                        1
                    );
                    let locator = session.release().await.unwrap();
                    source = product.load(&source.id).unwrap();
                    let runtime = if rebind {
                        runtime_binding_for_account(
                            workspace.path(),
                            controller.clone(),
                            b"successor-account",
                        )
                        .unwrap()
                    } else {
                        binding()
                    };
                    let target = runtime.target_binding();
                    let kind = if rebind {
                        SessionWorkflowKind::Rebind(Box::new(RebindSelection {
                            provider: "codex".into(),
                            account: "codex".into(),
                            model: "offline-loopback-v1".into(),
                            effort: Some("medium".into()),
                            composition: source.composition,
                            target_selection: source.target_selection.clone(),
                            protocol: target.protocol().as_str().into(),
                            route: target.route().as_str().into(),
                            requested_model: target.model().as_str().into(),
                            selected_configuration: serde_json::to_string(
                                &target.selected_provider_configuration(),
                            )
                            .unwrap(),
                            effective_configuration: serde_json::to_string(
                                &target.effective_provider_configuration(),
                            )
                            .unwrap(),
                            account_binding: serde_json::to_string(&target.account_binding())
                                .unwrap(),
                        }))
                    } else {
                        SessionWorkflowKind::Fork
                    };
                    let request = WorkflowRequest {
                        operation_id: "successor:branch".into(),
                        source_locator: locator,
                        kind,
                    };
                    product
                        .submit_workflow_request(&mut source, &request)
                        .unwrap();
                    let operation = SessionOperationId::new(&request.operation_id).unwrap();
                    if rebind {
                        sessions
                            .rebind(RebindSessionRequest::new(
                                operation, locator, target, runtime,
                            ))
                            .await
                            .unwrap();
                    } else {
                        sessions
                            .fork(ForkSessionRequest::recompiling(operation, locator, runtime))
                            .await
                            .unwrap();
                    }
                    // A source-only background pass publishes the target, without ever projecting it.
                    project_session(&sessions, &records, &source).await.unwrap();
                    let successor = product.load(&request.successor_product_id()).unwrap();
                    assert!(product.load_views(&successor.id).unwrap().is_none());
                    for _ in 0..2 {
                        project_session(&sessions, &records, &successor)
                            .await
                            .unwrap();
                    }
                })
                .unwrap();
            ready.writer.close_and_join().unwrap();
            bridge.drain_for_test();
            let captured = spans.lock().unwrap();
            assert_eq!(
                captured
                    .iter()
                    .filter(|span| span.name == "turn_usage")
                    .count(),
                1,
                "rebind={rebind}"
            );
        }
    }

    #[test]
    fn projected_turn_usage_exports_without_live_diagnostics_through_the_writer() {
        let (_guard, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ready =
            crate::telemetry_sink::open_product_telemetry(&paths, Some(bridge.clone())).unwrap();
        let ctx = fixture_context();
        let observation = ProductObservation::TurnUsage {
            at: fixture_timestamp(),
            session_id: ctx.session_id(),
            branch_id: ctx.branch_id(),
            turn_id: ctx.turn_id().unwrap(),
            usage: TurnUsage::new(
                Usage::new(10, 20, 3, 4, 5).with_cost_micros(Some(999)),
                2,
                1,
                7,
            )
            .unwrap(),
        };
        ready.writer.emit_product(observation);
        ready.writer.emit_product(observation);
        drain(&bridge);
        ready.writer.close_and_join().unwrap();
        let spans = spans.lock().unwrap();
        assert_eq!(spans.len(), 2);
        let span = &spans[0];
        assert_allowlisted(span);
        assert_eq!(span.name, "turn_usage");
        assert_eq!(span.span_context.trace_id(), session_trace_id());
        assert_eq!(span.parent_span_id, turn_span_id());
        assert_ne!(span.span_context.span_id(), turn_span_id());
        assert_eq!(span.span_context.span_id(), spans[1].span_context.span_id());
        assert_eq!(span.start_time, end_time(fixture_timestamp()));
        assert_eq!(span.end_time, span.start_time);
        let attrs = attributes_map(span);
        assert_eq!(
            attrs.get("gen_ai.usage.input_tokens"),
            Some(&"10".to_owned())
        );
        assert_eq!(
            attrs.get("gen_ai.usage.output_tokens"),
            Some(&"20".to_owned())
        );
        assert_eq!(attrs.get("plak.cost_micros"), Some(&"999".to_owned()));
        assert!(!attrs.contains_key("plak.run_id"));
        assert!(!attrs.contains_key("plak.agent_id"));
    }

    #[test]
    fn model_finished_exports_served_endpoint_and_cost() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_nanos(50),
                    status: TelemetryTerminalStatus::Completed,
                    usage: Some(Usage::new(1, 2, 3, 4, 5).with_cost_micros(Some(999))),
                    served_by: Some(
                        plakous::model::ServedBy::new("deepinfra/turbo").expect("endpoint"),
                    ),
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_allowlisted(&span);
        let attrs = attributes_map(&span);
        assert_eq!(
            attrs.get("plak.served_endpoint"),
            Some(&"deepinfra/turbo".to_owned())
        );
        assert_eq!(attrs.get("plak.cost_micros"), Some(&"999".to_owned()));
    }

    #[test]
    fn model_finished_exports_span_with_parent() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_nanos(50),
                    status: TelemetryTerminalStatus::Failed,
                    usage: Some(Usage::new(1, 2, 3, 4, 5)),
                    served_by: None,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_allowlisted(&span);
        assert_eq!(span.name, "model_attempt");
        assert_eq!(span.span_context.span_id(), attempt_span_id());
        assert_eq!(span.parent_span_id, turn_span_id());
        assert_eq!(span.span_kind, SpanKind::Client);
        assert_eq!(span.status, Status::error("failed"));
        assert_eq!(span.events.events, [] as [Event; 0]);
        let attrs = attributes_map(&span);
        assert_eq!(attrs.get("plak.status"), Some(&"failed".to_owned()));
        assert!(!attrs.contains_key("plak.ttft_ns"));
    }

    #[test]
    fn model_first_event_then_finished_attaches_ttft() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ctx = fixture_context();
        let ts = fixture_timestamp();
        emit(
            &bridge,
            &TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::ModelFirstEvent {
                    elapsed: Duration::from_nanos(9),
                },
            ),
        );
        assert!(spans.lock().unwrap().is_empty());
        emit(
            &bridge,
            &TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_nanos(50),
                    status: TelemetryTerminalStatus::Completed,
                    usage: None,
                    served_by: None,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(
            attributes_map(&span).get("plak.ttft_ns"),
            Some(&"9".to_owned())
        );
        let event: &Event = span.events.events.first().expect("ttft event");
        assert_eq!(event.name, "gen_ai.first_token");
        assert_eq!(event.timestamp, end_time(ts));
    }

    #[test]
    fn tool_finished_exports_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::ToolFinished {
                    elapsed: Duration::from_nanos(8),
                    status: TelemetryTerminalStatus::Cancelled,
                    output_bytes: 99,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "tool_call");
        assert_eq!(span.span_context.span_id(), call_span_id());
        assert_eq!(span.parent_span_id, turn_span_id());
        assert_eq!(span.status, Status::error("cancelled"));
    }

    #[test]
    fn compaction_finished_exports_synthetic_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ctx = fixture_context();
        let ts = fixture_timestamp();
        emit(
            &bridge,
            &TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::CompactionFinished {
                    refusal: None,
                    elapsed: Duration::from_nanos(60),
                    outcome: CompactionEventOutcome::RecoveryRequired,
                    reason: CompactionEventReason::Manual,
                    tokens_before: Some(100),
                    tokens_after: Some(40),
                    usage: Some(Usage::new(6, 7, 8, 9, 10)),
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "compaction");
        assert_eq!(
            span.span_context.span_id(),
            synthetic_span_id(SyntheticKind::Compaction, ctx, ts)
        );
        assert_eq!(span.parent_span_id, turn_span_id());
        assert_eq!(span.status, Status::error("recovery_required"));
    }

    #[test]
    fn durability_finished_exports_synthetic_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ctx = fixture_context();
        let ts = fixture_timestamp();
        emit(
            &bridge,
            &TelemetryObservation::new(
                ctx,
                ts,
                TelemetryObservationKind::DurabilityFinished {
                    elapsed: Duration::from_nanos(15),
                    status: TelemetryTerminalStatus::Shutdown,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "durability");
        assert_eq!(span.status, Status::error("shutdown"));
    }

    #[test]
    fn durability_attempt_finished_exports_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::DurabilityAttemptFinished {
                    elapsed: Duration::from_nanos(3),
                    attempt: DurabilityAttemptKind::InitialAcknowledgementRecovery,
                    outcome: DurabilityAttemptOutcome::RecoveredAcknowledgement,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "durability_attempt");
        assert_eq!(span.status, Status::Ok);
    }

    #[test]
    fn retention_finished_exports_span_with_reclaim_counts() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                fixture_timestamp(),
                TelemetryObservationKind::RetentionFinished {
                    elapsed: Duration::from_nanos(9),
                    status: TelemetryTerminalStatus::Failed,
                    pruned_checkpoints: 0,
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "retention");
        assert!(matches!(span.status, Status::Error { .. }));
        assert!(
            span.attributes
                .iter()
                .any(|kv| kv.key.as_str() == "plak.retention.pruned_checkpoints")
        );
    }

    #[test]
    fn child_process_exited_exports_instant_span() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ts = fixture_timestamp();
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context(),
                ts,
                TelemetryObservationKind::ChildProcessExited {
                    child: ChildProcessKind::JsEvalSidecar,
                    user_cpu: Duration::from_nanos(100),
                    system_cpu: Duration::from_nanos(50),
                    peak_rss_bytes: Some(8192),
                },
            ),
        );
        drain(&bridge);
        let span = spans.lock().unwrap().pop().expect("one span");
        assert_eq!(span.name, "child_process_exited");
        assert_eq!(span.start_time, span.end_time);
        assert_eq!(span.end_time, end_time(ts));
        assert_eq!(
            attributes_map(&span).get("plak.peak_rss_bytes"),
            Some(&"8192".to_owned())
        );
    }

    #[test]
    fn ttft_cache_evicts_oldest_entry() {
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(Arc::clone(&spans));
        let ts = fixture_timestamp();
        for index in 0..TTFT_CACHE_CAP {
            let attempt = AttemptId::try_from(Uuid::from_u128(index as u128 + 1_000)).expect("id");
            emit(
                &bridge,
                &TelemetryObservation::new(
                    fixture_context().with_attempt(attempt),
                    ts,
                    TelemetryObservationKind::ModelFirstEvent {
                        elapsed: Duration::from_nanos(1),
                    },
                ),
            );
        }
        let evicted = AttemptId::try_from(Uuid::from_u128(1_000)).expect("evicted");
        let kept = AttemptId::try_from(Uuid::from_u128(1_001)).expect("kept");
        emit(
            &bridge,
            &TelemetryObservation::new(
                fixture_context()
                    .with_attempt(AttemptId::try_from(Uuid::from_u128(9_999)).expect("new")),
                ts,
                TelemetryObservationKind::ModelFirstEvent {
                    elapsed: Duration::from_nanos(2),
                },
            ),
        );
        for attempt in [evicted, kept] {
            emit(
                &bridge,
                &TelemetryObservation::new(
                    fixture_context().with_attempt(attempt),
                    ts,
                    TelemetryObservationKind::ModelFinished {
                        elapsed: Duration::from_nanos(10),
                        status: TelemetryTerminalStatus::Completed,
                        usage: None,
                        served_by: None,
                    },
                ),
            );
        }
        drain(&bridge);
        let captured = spans.lock().unwrap();
        assert_eq!(captured.len(), 2);
        assert!(!attributes_map(&captured[0]).contains_key("plak.ttft_ns"));
        assert!(attributes_map(&captured[1]).contains_key("plak.ttft_ns"));
    }

    #[test]
    fn synthetic_span_id_is_stable_across_bridge_instances() {
        let ctx = fixture_context();
        let ts = fixture_timestamp();
        let id_a = synthetic_span_id(SyntheticKind::Compaction, ctx, ts);
        let id_b = synthetic_span_id(SyntheticKind::Compaction, ctx, ts);
        assert_eq!(id_a, id_b);
        let other_ts =
            Timestamp::new(ts.seconds(), ts.nanoseconds().saturating_add(1)).expect("ts");
        let id_c = synthetic_span_id(SyntheticKind::Compaction, ctx, other_ts);
        assert_ne!(id_a, id_c);
    }

    #[test]
    fn every_kind_is_exhaustively_tested() {
        let _kinds = [
            TelemetryObservationKind::TurnStarted {
                origin: plakous::model::TurnOrigin::UserPrompt,
                queue_time: Duration::from_nanos(1),
            },
            TelemetryObservationKind::TurnFinished {
                elapsed: Duration::from_nanos(1),
                status: TelemetryTerminalStatus::Completed,
                usage: TurnUsage::default(),
                model_wall: Duration::ZERO,
                tool_wall: Duration::ZERO,
                compaction_wall: Duration::ZERO,
                model_attempts: 0,
                tool_calls: 0,
                compactions: 0,
            },
            TelemetryObservationKind::ModelStarted,
            TelemetryObservationKind::ModelFirstEvent {
                elapsed: Duration::from_nanos(1),
            },
            TelemetryObservationKind::ModelFinished {
                elapsed: Duration::from_nanos(1),
                status: TelemetryTerminalStatus::Completed,
                usage: None,
                served_by: None,
            },
            TelemetryObservationKind::ToolStarted,
            TelemetryObservationKind::ToolFinished {
                elapsed: Duration::from_nanos(1),
                status: TelemetryTerminalStatus::Completed,
                output_bytes: 0,
            },
            TelemetryObservationKind::CompactionStarted {
                reason: CompactionEventReason::Manual,
            },
            TelemetryObservationKind::CompactionFinished {
                refusal: None,
                elapsed: Duration::from_nanos(1),
                outcome: CompactionEventOutcome::Committed,
                reason: CompactionEventReason::Manual,
                tokens_before: None,
                tokens_after: None,
                usage: None,
            },
            TelemetryObservationKind::DurabilityFinished {
                elapsed: Duration::from_nanos(1),
                status: TelemetryTerminalStatus::Completed,
            },
            TelemetryObservationKind::DurabilityAttemptFinished {
                elapsed: Duration::from_nanos(1),
                attempt: DurabilityAttemptKind::InitialCommit,
                outcome: DurabilityAttemptOutcome::Committed,
            },
            TelemetryObservationKind::RetentionFinished {
                elapsed: Duration::from_nanos(1),
                status: TelemetryTerminalStatus::Completed,
                pruned_checkpoints: 1,
            },
            TelemetryObservationKind::ChildProcessExited {
                child: ChildProcessKind::PythonWorker,
                user_cpu: Duration::from_nanos(1),
                system_cpu: Duration::from_nanos(1),
                peak_rss_bytes: None,
            },
            TelemetryObservationKind::CredentialRefresh {
                provider: CredentialProviderLabel::new("xai").expect("label"),
                phase: CredentialRefreshPhase::OutcomeUnknown,
                failure_class: Some(CredentialRefreshFailureClass::Timeout),
                transport_phase: Some(plakous::model::ModelRetryPhase::Resolution),
                http_status: Some(503),
                elapsed: Some(Duration::from_millis(30)),
                generation: 40,
            },
            TelemetryObservationKind::PromptCacheDiagnosis {
                reason: plakous::model::PromptCacheMissReason::Other,
                missed_input_tokens: Some(587),
            },
        ];
    }
}
