use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use opentelemetry::trace::{Event, SpanId, TraceId};
use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use plakous::agent::{TelemetryContext, TurnUsage};
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
    fn on_start(&self, _span: &mut opentelemetry_sdk::trace::Span, _cx: &opentelemetry::Context) {}

    fn on_end(&self, span: SpanData) {
        self.spans
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(span);
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }

    fn shutdown_with_timeout(&self, _timeout: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
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
        TelemetryObservation::new(
            ctx,
            ts,
            TelemetryObservationKind::ToolStarted {
                tool: (&plakous::model::ToolName::new("test").unwrap()).into(),
            },
        ),
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
            AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
        let product = Arc::new(SqliteSessionStore::new(paths.clone()));
        let records: Arc<dyn SessionRecordStore> = product.clone();
        let spans = Arc::new(Mutex::new(Vec::new()));
        let bridge = make_bridge(spans.clone());
        let ready =
            crate::telemetry_sink::open_product_telemetry(&paths, Some(bridge.clone())).unwrap();
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
                        account_binding: serde_json::to_string(&target.account_binding()).unwrap(),
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
                // A source-only pass publishes the projection seed, but must not export inherited usage.
                project_session(&sessions, &records, &source).await.unwrap();
                let successor = product.load(&request.successor_product_id()).unwrap();
                assert!(product.load_views(&successor.id).unwrap().is_some());
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
                output_deltas: 0,
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
                output_deltas: 0,
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
                output_deltas: 0,
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
                    output_deltas: 0,
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
    let other_ts = Timestamp::new(ts.seconds(), ts.nanoseconds().saturating_add(1)).expect("ts");
    let id_c = synthetic_span_id(SyntheticKind::Compaction, ctx, other_ts);
    assert_ne!(id_a, id_c);
}
