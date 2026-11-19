use super::*;
use crate::telemetry_store::TelemetryStore;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering as AtomicOrdering};
use std::time::Duration;

use plakous::agent::{
    TelemetryContext, TelemetryObservation, TelemetryObservationKind, TelemetryTerminalStatus,
};
use plakous::model::{
    AttemptId, BranchId, CallId, DriverInstanceId, RunId, SessionId, Timestamp, TurnId, Usage,
};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[test]
fn a_full_writer_queue_persists_writer_gap_evidence() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let store = TelemetryStore::open(&paths)?;
    let (writer, receiver) = TelemetryWriter::detached(1);
    assert!(writer.try_emit(observation()?).is_ok());
    assert!(writer.try_emit(observation()?).is_err());
    assert_eq!(writer.status().dropped, 1);
    writer.sender.lock().unwrap().take();
    writer_loop(
        store,
        receiver,
        Arc::clone(&writer.write_errors),
        Arc::clone(&writer.dropped),
        Arc::clone(&writer.wal_checkpoint_busy),
    );
    let connection = rusqlite::Connection::open(paths.telemetry_database())?;
    assert_eq!(
        connection.query_row(
            "SELECT dropped FROM recording_evidence WHERE singleton=1",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

fn observation() -> TestResult<TelemetryObservation> {
    Ok(TelemetryObservation::new(
        TelemetryContext::new(
            DriverInstanceId::try_from(Uuid::from_u128(1))?,
            SessionId::try_from(Uuid::from_u128(2))?,
            BranchId::try_from(Uuid::from_u128(3))?,
            RunId::try_from(Uuid::from_u128(4))?,
        )
        .with_turn(TurnId::try_from(Uuid::from_u128(5))?)
        .with_attempt(AttemptId::try_from(Uuid::from_u128(6))?)
        .with_call(CallId::try_from(Uuid::from_u128(7))?),
        Timestamp::new(8, 9)?,
        TelemetryObservationKind::ModelFinished {
            elapsed: Duration::from_millis(10),
            status: TelemetryTerminalStatus::Completed,
            usage: Some(Usage::new(11, 12, 13, 14, 15)),
            served_by: None,
            output_deltas: 0,
            measurement: plakous::agent::ModelMeasurement::absent(),
        },
    ))
}

#[test]
fn every_runtime_retry_is_enqueued_once_with_its_own_context() -> TestResult {
    let (writer, receiver) = TelemetryWriter::detached(4);
    let sink = ModelDiagnosticsSink {
        inner: None,
        writer: Some(writer),
    };
    let retry = plakous::model::ModelRetry::new(
        2,
        4,
        Duration::ZERO,
        plakous::model::ModelRetryCause::RecordRejectedInTransit,
        false,
        "fixture",
    );
    for id in [1, 99] {
        let context = TelemetryContext::new(
            DriverInstanceId::try_from(Uuid::from_u128(id))?,
            SessionId::try_from(Uuid::from_u128(id + 100))?,
            BranchId::try_from(Uuid::from_u128(3))?,
            RunId::try_from(Uuid::from_u128(4))?,
        )
        .with_turn(TurnId::try_from(Uuid::from_u128(id + 200))?);
        sink.observe_model_retry(context, &retry);
        let TelemetryItem::Product(ProductObservation::ModelRetry {
            agent_id, turn_id, ..
        }) = receiver.try_recv()?
        else {
            panic!("expected a retry row");
        };
        assert_eq!(agent_id, context.agent_id());
        assert_eq!(Some(turn_id), context.turn_id());
    }
    assert!(receiver.try_recv().is_err());
    Ok(())
}

#[derive(Clone)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>, mpsc::Sender<()>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for LogBuffer {
    fn drop(&mut self) {
        let _ = self.1.send(());
    }
}

#[test]
fn child_retry_is_logged_with_host_route_and_size_even_without_analytics() -> TestResult {
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (done, finished) = mpsc::channel();
    let writer = LogBuffer(Arc::clone(&bytes), done);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let context = TelemetryContext::new(
        DriverInstanceId::try_from(Uuid::from_u128(99))?,
        SessionId::try_from(Uuid::from_u128(2))?,
        BranchId::try_from(Uuid::from_u128(3))?,
        RunId::try_from(Uuid::from_u128(4))?,
    )
    .with_turn(TurnId::try_from(Uuid::from_u128(5))?);
    let retry = plakous::model::ModelRetry::new(
        2,
        4,
        Duration::ZERO,
        plakous::model::ModelRetryCause::RecordRejectedInTransit,
        false,
        "peer_rejected_record",
    )
    .with_request(
        "fixture.invalid",
        "/v1/messages?secret=must-not-log",
        4_194_304,
    );
    let emitter = TelemetryEmitter::enabled(Arc::new(ModelDiagnosticsSink {
        inner: None,
        writer: None,
    }));
    // The child's ordinary telemetry port closes before its retry.
    emitter.emit(observation()?);
    tracing::subscriber::with_default(subscriber, || {
        emitter.observe_model_retry(context, &retry);
    });
    // A liveness bound only: generous, so load never trips it.
    finished.recv_timeout(Duration::from_secs(30))?;
    let log = String::from_utf8(bytes.lock().unwrap().clone())?;
    assert!(log.contains("plak.retry"), "{log}");
    assert!(log.contains(&Uuid::from_u128(99).to_string()), "{log}");
    assert!(log.contains("host=\"fixture.invalid\""), "{log}");
    assert!(log.contains("route=\"/v1/messages\""), "{log}");
    assert!(log.contains("request_bytes=4194304"), "{log}");
    assert!(
        log.contains("request corrupted in transit (TLS record rejected)"),
        "{log}"
    );
    assert!(!log.contains("secret") && !log.contains("must-not-log"));
    Ok(())
}

#[test]
fn a_terminal_provider_failure_logs_and_stores_only_a_bounded_redacted_excerpt() -> TestResult {
    use plakous::model::{
        ModelError, ModelErrorContext, ModelErrorKind, ModelErrorPhase, ModelHttpStatus, ModelId,
        ModelProviderCode, ModelRetryability, ProtocolId, RouteId,
    };

    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    let (writer, receiver) = TelemetryWriter::detached(4);
    let sink = ModelDiagnosticsSink {
        inner: None,
        writer: Some(writer),
    };
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let (done, finished) = mpsc::channel();
    let writer = LogBuffer(Arc::clone(&bytes), done);
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    let error = ModelError::contextual(
        ModelErrorKind::Authorization,
        format!(
            "Access de\u{202e}nied, token=FIXTURE_SECRET_NOT_FOR_LOGS for us\u{2066}er@example.invalid, account_id=42\n {}",
            "界".repeat(150)
        ),
        ModelErrorContext::new(
            ProtocolId::new("example.responses")?,
            RouteId::new("example.platform")?,
            ModelId::new("test-model")?,
            ModelErrorPhase::FirstEvent,
            ModelRetryability::Permanent,
        )?
        .with_http_status(ModelHttpStatus::new(403)?)
        .with_provider_code(ModelProviderCode::new("permission-denied")?),
    )?
    .with_source("unsent-to-logs source marker");
    let context = observation()?.context();
    tracing::subscriber::with_default(subscriber, || sink.observe_model_failure(context, &error));
    store.write_batch(&[receiver.try_recv()?])?;
    let detail = store
        .query_text("SELECT status FROM product_events WHERE kind = 'model_attempt_failed'")
        .ok_or("missing model failure row")?;
    let detail: serde_json::Value = serde_json::from_str(&detail)?;
    assert_eq!(detail["http_status"], 403);
    assert_eq!(detail["provider_code"], "permission-denied");
    assert_eq!(detail["attempt_id"], Uuid::from_u128(6).to_string());
    let excerpt = detail["excerpt"].as_str().ok_or("missing excerpt")?;
    assert!(
        excerpt.contains("Access denied, token=[redacted]"),
        "{excerpt}"
    );
    assert!(excerpt.contains("for [redacted], account_id=[redacted]"));
    assert!(!excerpt.contains('\n'), "{excerpt}");
    assert!((298..=300).contains(&excerpt.len()), "{excerpt}");

    finished.recv_timeout(Duration::from_secs(30))?;
    let log = String::from_utf8(bytes.lock().unwrap().clone())?;
    assert!(log.contains("http_status=403"), "{log}");
    assert!(log.contains("permission-denied"), "{log}");
    assert!(log.contains(excerpt), "{log}");
    for output in [log.as_str(), excerpt] {
        assert!(!output.contains("FIXTURE_SECRET_NOT_FOR_LOGS"), "{output}");
        assert!(!output.contains("unsent-to-logs"), "{output}");
        assert!(!output.contains(['\u{202e}', '\u{2066}']), "{output}");
        assert!(!output.contains("example.invalid"), "{output}");
        assert!(!output.contains("account_id=42"), "{output}");
    }
    Ok(())
}

#[test]
fn full_queue_drops_without_blocking() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let (writer, _receiver) = TelemetryWriter::detached(1);
    let sink: Arc<dyn TelemetrySink> = Arc::clone(&writer) as Arc<dyn TelemetrySink>;
    let emitter = Arc::new(TelemetryEmitter::enabled(sink));

    let first = observation()?;
    emitter.emit(first);
    emitter.emit(first);
    emitter.emit(first);

    let saw_full = Arc::new(AtomicU8::new(0));
    let probe = Arc::clone(&saw_full);
    let probe_writer = Arc::clone(&writer);
    let handle = thread::spawn(move || {
        if probe_writer.try_emit(first).is_err() {
            probe.store(1, AtomicOrdering::Release);
        }
    });
    handle.join().map_err(|_| "full probe thread panicked")?;
    assert_eq!(saw_full.load(AtomicOrdering::Acquire), 1);
    assert!(emitter.status().dropped() >= 1);
    Ok(())
}

#[derive(Debug)]
struct MockSink {
    behavior: AtomicU8,
    calls: AtomicU32,
}

impl MockSink {
    const OK: u8 = 0;
    const FULL: u8 = 1;
    const CLOSED: u8 = 2;
    const PANIC: u8 = 3;

    fn new(behavior: u8) -> Arc<Self> {
        Arc::new(Self {
            behavior: AtomicU8::new(behavior),
            calls: AtomicU32::new(0),
        })
    }
}

impl TelemetrySink for MockSink {
    fn try_emit(&self, _observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        self.calls.fetch_add(1, AtomicOrdering::AcqRel);
        match self.behavior.load(AtomicOrdering::Acquire) {
            Self::OK => Ok(()),
            Self::FULL => Err(TelemetrySinkError::Full),
            Self::CLOSED => Err(TelemetrySinkError::Closed),
            _ => panic!("mock sink panic"),
        }
    }
}

impl plakous_hosted::agent::ExtraTelemetrySink for MockSink {
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation"
    )]
    fn try_emit_product(&self, _observation: ProductObservation) -> Result<(), TelemetrySinkError> {
        self.try_emit(observation().unwrap())
    }
}

#[test]
fn failing_product_exporters_never_disable_the_telemetry_writer() {
    for behavior in [MockSink::FULL, MockSink::CLOSED, MockSink::PANIC] {
        let (writer, receiver) = TelemetryWriter::detached(4);
        let extra = MockSink::new(behavior);
        install_product_exporter(&writer, Some(extra.clone()));
        let context = observation().unwrap().context();
        let observation = ProductObservation::TurnUsage {
            account: None,
            at: observation().unwrap().timestamp(),
            session_id: context.session_id(),
            branch_id: context.branch_id(),
            turn_id: context.turn_id().unwrap(),
            usage: plakous::agent::TurnUsage::default(),
        };
        writer.emit_product(observation);
        writer.emit_product(observation);
        assert_eq!(receiver.try_iter().count(), 2);
        assert_eq!(
            extra.calls.load(AtomicOrdering::Acquire),
            if behavior == MockSink::FULL { 2 } else { 1 }
        );
    }
}

#[test]
fn merge_child_outcomes_matrix() {
    assert_eq!(
        merge_child_outcomes(&[Some(Ok(())), Some(Err(TelemetrySinkError::Closed))]),
        Ok(())
    );
    assert_eq!(
        merge_child_outcomes(&[Some(Err(TelemetrySinkError::Closed)), Some(Ok(()))]),
        Ok(())
    );
    assert_eq!(
        merge_child_outcomes(&[
            Some(Err(TelemetrySinkError::Full)),
            Some(Err(TelemetrySinkError::Closed))
        ]),
        Err(TelemetrySinkError::Full)
    );
    assert_eq!(
        merge_child_outcomes(&[
            Some(Err(TelemetrySinkError::Closed)),
            Some(Err(TelemetrySinkError::Closed))
        ]),
        Err(TelemetrySinkError::Closed)
    );
    assert_eq!(
        merge_child_outcomes(&[None, None]),
        Err(TelemetrySinkError::Closed)
    );
}

#[test]
fn closed_extra_sink_does_not_disable_emitter() -> TestResult {
    let closed = MockSink::new(MockSink::CLOSED);
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    let writer = TelemetryWriter::spawn(store, 4096)?;
    let fanout = FanOutSink::new(vec![
        Arc::clone(&writer) as Arc<dyn TelemetrySink>,
        closed.clone(),
    ]);
    let emitter = Arc::new(TelemetryEmitter::enabled(fanout));
    let obs = observation()?;
    emitter.emit(obs);
    assert!(emitter.status().enabled());
    assert_eq!(closed.calls.load(AtomicOrdering::Acquire), 1);
    emitter.emit(obs);
    assert!(emitter.status().enabled());
    assert_eq!(closed.calls.load(AtomicOrdering::Acquire), 1);
    writer.close_and_join()?;
    Ok(())
}

#[test]
fn panicking_extra_sink_does_not_disable_emitter() -> TestResult {
    let panicking = MockSink::new(MockSink::PANIC);
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    let writer = TelemetryWriter::spawn(store, 4096)?;
    let fanout = FanOutSink::new(vec![
        Arc::clone(&writer) as Arc<dyn TelemetrySink>,
        panicking.clone(),
    ]);
    let emitter = Arc::new(TelemetryEmitter::enabled(fanout));
    let obs = observation()?;
    emitter.emit(obs);
    assert!(emitter.status().enabled());
    assert_eq!(panicking.calls.load(AtomicOrdering::Acquire), 1);
    emitter.emit(obs);
    assert!(emitter.status().enabled());
    assert_eq!(panicking.calls.load(AtomicOrdering::Acquire), 1);
    writer.close_and_join()?;
    Ok(())
}
