//! Nonblocking typed telemetry sink backed by a dedicated writer thread.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::thread;
#[cfg(test)]
use std::thread::JoinHandle;

use plakous::agent::{
    CredentialRefreshPhase, TelemetryEmitter, TelemetryObservation, TelemetryObservationKind,
    TelemetrySink, TelemetrySinkError,
};

use crate::home::paths::PlakPaths;
use crate::telemetry_store::{
    TelemetryItem, TelemetryRetentionConfig, TelemetryStore, TelemetryStoreOpenError,
    WAL_CHECKPOINT_EVERY_WRITES, WRITER_BATCH_DRAIN, WalCheckpointOutcome,
};
use plakous_hosted::product_observation::{
    ProductObservation, ResourceSamplePhase, wall_timestamp,
};
use plakous_hosted::runtime::sample_process_resources;

/// Write-side health for the disposable telemetry store.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TelemetryWriterStatus {
    write_errors: u64,
    dropped: u64,
    wal_checkpoint_busy: u64,
}

impl TelemetryWriterStatus {
    #[must_use]
    pub const fn write_errors(self) -> u64 {
        self.write_errors
    }

    #[must_use]
    pub const fn dropped(self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub const fn wal_checkpoint_busy(self) -> u64 {
        self.wal_checkpoint_busy
    }
}

/// Bounded-channel sink feeding the dedicated telemetry writer thread.
pub struct TelemetryWriter {
    sender: Mutex<Option<SyncSender<TelemetryItem>>>,
    extra: Mutex<Option<Arc<ProductExporter>>>,
    write_errors: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    wal_checkpoint_busy: Arc<AtomicU64>,
    #[cfg(test)]
    #[allow(dead_code, reason = "kept alive so a detached writer can be joined")]
    thread: Mutex<Option<JoinHandle<()>>>,
}

struct ProductExporter {
    sink: Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>,
    disabled: AtomicBool,
}

impl TelemetryWriter {
    pub fn spawn(
        store: TelemetryStore,
        capacity: usize,
    ) -> Result<Arc<Self>, TelemetryStoreOpenError> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let write_errors = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicU64::new(0));
        let wal_checkpoint_busy = Arc::new(AtomicU64::new(0));
        let errors = Arc::clone(&write_errors);
        let checkpoint_busy = Arc::clone(&wal_checkpoint_busy);
        let thread = thread::Builder::new()
            .name("plak-telemetry-writer".into())
            .spawn(move || {
                writer_loop(store, receiver, errors, checkpoint_busy);
            })
            .map_err(|source| TelemetryStoreOpenError::WriterThread { source })?;
        #[cfg(not(test))]
        drop(thread);
        Ok(Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            extra: Mutex::new(None),
            write_errors,
            dropped,
            wal_checkpoint_busy,
            #[cfg(test)]
            thread: Mutex::new(Some(thread)),
        }))
    }

    /// Test-only constructor: bounded channel without a writer thread.
    #[cfg(test)]
    pub fn detached(capacity: usize) -> (Arc<Self>, Receiver<TelemetryItem>) {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let writer = Arc::new(Self {
            sender: Mutex::new(Some(sender)),
            extra: Mutex::new(None),
            write_errors: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
            wal_checkpoint_busy: Arc::new(AtomicU64::new(0)),
            thread: Mutex::new(None),
        });
        (writer, receiver)
    }

    #[must_use]
    pub fn status(&self) -> TelemetryWriterStatus {
        TelemetryWriterStatus {
            write_errors: self.write_errors.load(Ordering::Acquire),
            dropped: self.dropped.load(Ordering::Acquire),
            wal_checkpoint_busy: self.wal_checkpoint_busy.load(Ordering::Acquire),
        }
    }

    #[allow(
        clippy::large_types_passed_by_value,
        reason = "Copy product observation enum"
    )]
    pub(crate) fn emit_product(&self, observation: ProductObservation) {
        let extra = self
            .extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(extra) = extra
            && !extra.disabled.load(Ordering::Acquire)
            && !matches!(
                std::panic::catch_unwind(AssertUnwindSafe(|| extra
                    .sink
                    .try_emit_product(observation))),
                Ok(Ok(()) | Err(TelemetrySinkError::Full))
            )
        {
            extra.disabled.store(true, Ordering::Release);
        }
        let sender = self
            .sender
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(sender) = sender.as_ref() else {
            return;
        };
        if sender
            .try_send(TelemetryItem::Product(observation))
            .is_err()
        {
            let _ = self.dropped.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[cfg(test)]
    pub fn close_and_join(
        self: &Arc<Self>,
    ) -> Result<TelemetryWriterStatus, TelemetryStoreOpenError> {
        *self.sender.lock().map_err(|_| {
            TelemetryStoreOpenError::Unavailable("telemetry writer lock poisoned".into())
        })? = None;
        let handle = self
            .thread
            .lock()
            .map_err(|_| {
                TelemetryStoreOpenError::Unavailable("telemetry writer lock poisoned".into())
            })?
            .take()
            .ok_or_else(|| {
                TelemetryStoreOpenError::Unavailable("telemetry writer already joined".into())
            })?;
        handle.join().map_err(|_| {
            TelemetryStoreOpenError::Unavailable("telemetry writer panicked".into())
        })?;
        Ok(self.status())
    }
}

impl TelemetrySink for TelemetryWriter {
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "TelemetrySink trait passes Copy observations by value"
    )]
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        if let TelemetryObservationKind::CredentialRefresh {
            provider,
            phase,
            failure_class: failure_class @ Some(_),
            transport_phase,
            http_status,
            generation,
            detail,
            ..
        } = observation.kind()
        {
            let (phase_label, message) = match phase {
                CredentialRefreshPhase::OutcomeUnknown => (
                    "outcome_unknown",
                    "credential refresh left the durable record unknown",
                ),
                CredentialRefreshPhase::Quarantined => (
                    "quarantined",
                    "credential refresh left the durable record unknown",
                ),
                CredentialRefreshPhase::Restored => {
                    ("restored", "credential refresh failed; record restored")
                }
                _ => ("unknown", "credential refresh failed"),
            };
            tracing::warn!(
                provider = provider.as_str(),
                phase = phase_label,
                failure_class = ?failure_class,
                failure_reason = detail.reason.map(plakous::agent::CredentialRefreshFailureReason::as_str),
                expires_in_secs = detail.expires_in_secs,
                id_token_iat_age_secs = detail.id_token_iat_age_secs,
                id_token_exp_in_secs = detail.id_token_exp_in_secs,
                transport_phase = ?transport_phase,
                http_status,
                generation,
                "{message}"
            );
        }
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
                TrySendError::Full(_) => {
                    let _ = self.dropped.fetch_add(1, Ordering::AcqRel);
                    TelemetrySinkError::Full
                }
                TrySendError::Disconnected(_) => TelemetrySinkError::Closed,
            })
    }
}

fn writer_loop(
    mut store: TelemetryStore,
    receiver: Receiver<TelemetryItem>,
    write_errors: Arc<AtomicU64>,
    wal_checkpoint_busy: Arc<AtomicU64>,
) {
    let mut writes_since_checkpoint = 0_u32;
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        loop {
            match receiver.try_recv() {
                Ok(item) => {
                    batch.push(item);
                    if batch.len() > WRITER_BATCH_DRAIN {
                        break;
                    }
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    flush_batch(&mut store, &write_errors, &batch);
                    record_wal_checkpoint(&store, &wal_checkpoint_busy);
                    return;
                }
            }
        }
        flush_batch(&mut store, &write_errors, &batch);
        writes_since_checkpoint = writes_since_checkpoint.saturating_add(1);
        if writes_since_checkpoint >= WAL_CHECKPOINT_EVERY_WRITES {
            record_wal_checkpoint(&store, &wal_checkpoint_busy);
            writes_since_checkpoint = 0;
        }
    }
    record_wal_checkpoint(&store, &wal_checkpoint_busy);
}

fn record_wal_checkpoint(store: &TelemetryStore, wal_checkpoint_busy: &AtomicU64) {
    match store.checkpoint_wal() {
        Ok(WalCheckpointOutcome::Busy) => {
            let _ = wal_checkpoint_busy.fetch_add(1, Ordering::AcqRel);
        }
        Ok(WalCheckpointOutcome::Completed) => {}
        Err(error) => {
            tracing::warn!(error = %error, "telemetry WAL checkpoint failed");
        }
    }
}

fn flush_batch(store: &mut TelemetryStore, write_errors: &AtomicU64, batch: &[TelemetryItem]) {
    if batch.is_empty() {
        return;
    }
    let mut expanded = Vec::with_capacity(batch.len().saturating_mul(2));
    for &item in batch {
        expanded.push(item);
        if let TelemetryItem::Agent(observation) = item
            && let Some(sample) = resource_sample_for_agent_observation(&observation)
        {
            // Sampled on the writer thread after dequeue; typically ~ms after the agent path.
            expanded.push(TelemetryItem::Product(sample));
        }
    }
    if store.write_batch(&expanded).is_err() {
        let batch_len = u64::try_from(expanded.len()).unwrap_or(u64::MAX);
        let _ = write_errors.fetch_add(batch_len, Ordering::AcqRel);
    }
}

fn resource_sample_for_agent_observation(
    observation: &TelemetryObservation,
) -> Option<ProductObservation> {
    let phase = match observation.kind() {
        TelemetryObservationKind::TurnStarted { .. } => ResourceSamplePhase::TurnStarted,
        TelemetryObservationKind::TurnFinished { .. } => ResourceSamplePhase::TurnFinished,
        TelemetryObservationKind::CompactionFinished { .. } => {
            ResourceSamplePhase::CompactionFinished
        }
        _ => return None,
    };
    let snapshot = sample_process_resources();
    Some(ProductObservation::ResourceSample {
        at: wall_timestamp(),
        session_id: observation.context().session_id(),
        agent_id: Some(observation.context().agent_id()),
        phase,
        child_kind: None,
        rss_bytes: snapshot.rss_bytes,
        peak_rss_bytes: snapshot.peak_rss_bytes,
        user_cpu: snapshot.user_cpu,
        system_cpu: snapshot.system_cpu,
        threads: snapshot.threads,
        fds: snapshot.fds,
    })
}

fn canonical_home_key(home: &Path) -> PathBuf {
    home.canonicalize().unwrap_or_else(|_| home.to_path_buf())
}

pub(crate) fn register_telemetry_writer(home: &Path, writer: &Arc<TelemetryWriter>) {
    let home = canonical_home_key(home);
    let registry = WRITER_STATUS_BY_HOME.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut guard) = registry.lock() {
        guard.insert(home, Arc::downgrade(writer));
    }
    crate::tui_timing::writer_registered();
}

/// Status-registry lookup keyed by canonical home (CLI diagnostics only).
pub(crate) fn telemetry_writer_for_home(home: &Path) -> Option<Arc<TelemetryWriter>> {
    let registry = WRITER_STATUS_BY_HOME.get_or_init(|| Mutex::new(HashMap::new()));
    let guard = registry.lock().ok()?;
    guard.get(&canonical_home_key(home))?.upgrade()
}

static WRITER_STATUS_BY_HOME: OnceLock<Mutex<HashMap<PathBuf, Weak<TelemetryWriter>>>> =
    OnceLock::new();
#[derive(Clone)]
pub(crate) struct TelemetryReady {
    pub emitter: Arc<TelemetryEmitter>,
    pub writer: Arc<TelemetryWriter>,
}

/// Process-lifetime telemetry open outcome cached on [`plakous_hosted::agent::ProductStateRoots`].
#[derive(Clone)]
pub(crate) enum TelemetryCache {
    Ready(TelemetryReady),
    Unavailable(String),
}

/// Fans one observation to multiple sinks without blocking the agent path.
pub(crate) struct FanOutSink {
    children: Vec<FanOutChild>,
}

struct FanOutChild {
    sink: Arc<dyn TelemetrySink>,
    disabled: AtomicBool,
}

impl FanOutChild {
    fn new(sink: Arc<dyn TelemetrySink>) -> Self {
        Self {
            sink,
            disabled: AtomicBool::new(false),
        }
    }
}

impl FanOutSink {
    pub(crate) fn new(children: Vec<Arc<dyn TelemetrySink>>) -> Arc<Self> {
        Arc::new(Self {
            children: children.into_iter().map(FanOutChild::new).collect(),
        })
    }
}

impl TelemetrySink for FanOutSink {
    #[allow(
        clippy::large_types_passed_by_value,
        reason = "TelemetrySink trait passes Copy observations by value"
    )]
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        let mut outcomes = Vec::with_capacity(self.children.len());
        for child in &self.children {
            if child.disabled.load(Ordering::Acquire) {
                outcomes.push(None);
                continue;
            }
            let result =
                std::panic::catch_unwind(AssertUnwindSafe(|| child.sink.try_emit(observation)));
            outcomes.push(Some(match result {
                Ok(Ok(())) => Ok(()),
                Ok(Err(TelemetrySinkError::Full)) => Err(TelemetrySinkError::Full),
                // Any other admission failure or a panic retires the child for
                // the rest of the process so it cannot keep costing the turn.
                Ok(Err(_)) | Err(_) => {
                    child.disabled.store(true, Ordering::Release);
                    Err(TelemetrySinkError::Closed)
                }
            }));
        }
        merge_child_outcomes(&outcomes)
    }
}

/// Combines per-child results: any acceptance wins; else any `Full`; else `Closed`.
fn merge_child_outcomes(
    outcomes: &[Option<Result<(), TelemetrySinkError>>],
) -> Result<(), TelemetrySinkError> {
    let mut any_ok = false;
    let mut any_full = false;
    let mut any_active = false;
    for outcome in outcomes {
        match outcome {
            None => {}
            Some(Ok(())) => {
                any_ok = true;
                any_active = true;
            }
            Some(Err(TelemetrySinkError::Full)) => {
                any_full = true;
                any_active = true;
            }
            Some(Err(_)) => {
                any_active = true;
            }
        }
    }
    if any_ok {
        return Ok(());
    }
    if any_full {
        return Err(TelemetrySinkError::Full);
    }
    if any_active {
        return Err(TelemetrySinkError::Closed);
    }
    Err(TelemetrySinkError::Closed)
}

fn compose_telemetry_sink(
    writer: Arc<TelemetryWriter>,
    extra: Option<Arc<dyn TelemetrySink>>,
) -> Arc<dyn TelemetrySink> {
    let writer: Arc<dyn TelemetrySink> = writer;
    let inner = match extra {
        None => writer,
        Some(extra) => FanOutSink::new(vec![writer, extra]),
    };
    Arc::new(RetryLoggingSink { inner: Some(inner) })
}

struct RetryLog {
    context: plakous::agent::TelemetryContext,
    retry: plakous::model::ModelRetry,
    dispatcher: tracing::Dispatch,
}

// File rotation and writes must never run on the single-owner agent driver.
static RETRY_LOGS: OnceLock<Option<SyncSender<RetryLog>>> = OnceLock::new();

fn log_model_retry(context: plakous::agent::TelemetryContext, retry: &plakous::model::ModelRetry) {
    let request = retry.request();
    tracing::warn!(
        target: "plak.retry",
        session_id = %context.session_id().as_uuid(),
        branch_id = %context.branch_id().as_uuid(),
        agent_id = %context.agent_id().as_uuid(),
        turn_id = ?context.turn_id().map(plakous::model::TurnId::as_uuid),
        next_attempt = retry.next_physical(),
        max_attempts = retry.max_physical(),
        delay_ms = u64::try_from(retry.delay().as_millis()).unwrap_or(u64::MAX),
        cause = retry.cause().kind(),
        host = request.map(|(host, _, _)| host),
        route = request.map(|(_, route, _)| route),
        request_bytes = request.map(|(_, _, bytes)| bytes),
        output_discarded = retry.output_discarded(),
        detail = retry.detail().as_str(),
        "retrying model request: {}", retry.cause(),
    );
}

/// Logging is independent of SQLite availability and analytics admission.
struct RetryLoggingSink {
    inner: Option<Arc<dyn TelemetrySink>>,
}

impl TelemetrySink for RetryLoggingSink {
    fn observe_model_retry(
        &self,
        context: plakous::agent::TelemetryContext,
        retry: &plakous::model::ModelRetry,
    ) {
        if retry.loss().declined_reason.is_some() {
            return;
        }
        if let Some(sender) = RETRY_LOGS.get_or_init(|| {
            let (sender, receiver) = mpsc::sync_channel::<RetryLog>(64);
            thread::Builder::new()
                .name("plak-retry-log".into())
                .spawn(move || {
                    while let Ok(record) = receiver.recv() {
                        tracing::dispatcher::with_default(&record.dispatcher, || {
                            log_model_retry(record.context, &record.retry);
                        });
                    }
                })
                .ok()
                .map(|_| sender)
        }) {
            tracing::dispatcher::get_default(|dispatcher| {
                let _ = sender.try_send(RetryLog {
                    context,
                    retry: retry.clone(),
                    dispatcher: dispatcher.clone(),
                });
            });
        }
    }

    #[allow(
        clippy::large_types_passed_by_value,
        reason = "TelemetrySink passes Copy observations by value"
    )]
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        self.inner
            .as_ref()
            .ok_or(TelemetrySinkError::Closed)?
            .try_emit(observation)
    }
}

/// The product's telemetry: the SQLite writer under the product home, fanned
/// out with any extra sink, plus its health and unavailability log.
#[derive(Clone, Debug)]
pub(crate) struct ProductTelemetryFactory {
    paths: PlakPaths,
}

impl ProductTelemetryFactory {
    pub(crate) const fn new(paths: PlakPaths) -> Self {
        Self { paths }
    }
}

impl plakous_hosted::agent::TelemetrySinkFactory for ProductTelemetryFactory {
    fn open(
        &self,
        extra_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
    ) -> Arc<dyn plakous_hosted::agent::ProductTelemetry> {
        let cache = match open_product_telemetry(&self.paths, extra_sink) {
            Ok(ready) => TelemetryCache::Ready(ready),
            Err(error) => TelemetryCache::Unavailable(error.to_string()),
        };
        Arc::new(OpenedProductTelemetry {
            paths: self.paths.clone(),
            cache,
        })
    }

    fn log_unavailable(&self, session_id: &str, reason: &str) {
        log_telemetry_unavailable(&self.paths, session_id, reason);
    }
}

/// One opening of the product's telemetry, behind the hosted port.
struct OpenedProductTelemetry {
    paths: PlakPaths,
    cache: TelemetryCache,
}

impl OpenedProductTelemetry {
    fn writer(&self) -> Option<&Arc<TelemetryWriter>> {
        match &self.cache {
            TelemetryCache::Ready(ready) => Some(&ready.writer),
            TelemetryCache::Unavailable(_) => None,
        }
    }
}

impl plakous_hosted::agent::ProductTelemetry for OpenedProductTelemetry {
    fn agent_emitter(&self) -> Option<Arc<TelemetryEmitter>> {
        match &self.cache {
            TelemetryCache::Ready(ready) => Some(Arc::clone(&ready.emitter)),
            TelemetryCache::Unavailable(_) => Some(Arc::new(TelemetryEmitter::enabled(Arc::new(
                RetryLoggingSink { inner: None },
            )))),
        }
    }

    fn topology_observer(&self) -> Option<Arc<dyn plakous_hosted::agent_tree::TopologyObserver>> {
        crate::telemetry::topology::ProductTopologyObserver::port(self.writer().map(Arc::clone))
    }

    fn emit(&self, observation: ProductObservation) {
        try_emit_product(self.writer(), observation);
    }

    fn health(&self) -> TelemetryHealth {
        telemetry_health_from_cache(&self.paths, Some(&self.cache))
    }

    fn unavailable_reason(&self) -> Option<String> {
        match &self.cache {
            TelemetryCache::Ready(_) => None,
            TelemetryCache::Unavailable(reason) => Some(reason.clone()),
        }
    }
}

pub(crate) fn open_product_telemetry(
    paths: &PlakPaths,
    extra_sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
) -> Result<TelemetryReady, TelemetryStoreOpenError> {
    if let Some(writer) = telemetry_writer_for_home(paths.home()) {
        install_product_exporter(&writer, extra_sink.clone());
        let sink = compose_telemetry_sink(
            Arc::clone(&writer),
            extra_sink.map(|sink| sink as Arc<dyn TelemetrySink>),
        );
        let emitter = Arc::new(TelemetryEmitter::enabled(sink));
        plakous::subscriptions::install_credential_telemetry(Arc::clone(&emitter));
        return Ok(TelemetryReady { emitter, writer });
    }
    let retention =
        TelemetryRetentionConfig::from(crate::product_config::TelemetryConfig::load(paths));
    let mut store = TelemetryStore::open(paths)?;
    let prune = store
        .prune(retention, &paths.telemetry_database())
        .map_err(|error| TelemetryStoreOpenError::Unavailable(error.to_string()))?;
    if prune.wal_checkpoint.is_busy() {
        tracing::warn!(
            "telemetry startup prune could not truncate the WAL; a reader still holds the log"
        );
    }
    let writer = TelemetryWriter::spawn(store, 4096)?;
    register_telemetry_writer(paths.home(), &writer);
    install_product_exporter(&writer, extra_sink.clone());
    let sink = compose_telemetry_sink(
        Arc::clone(&writer),
        extra_sink.map(|sink| sink as Arc<dyn TelemetrySink>),
    );
    let emitter = Arc::new(TelemetryEmitter::enabled(sink));
    plakous::subscriptions::install_credential_telemetry(Arc::clone(&emitter));
    Ok(TelemetryReady { emitter, writer })
}

fn install_product_exporter(
    writer: &TelemetryWriter,
    sink: Option<Arc<dyn plakous_hosted::agent::ExtraTelemetrySink>>,
) {
    if let Some(sink) = sink {
        *writer
            .extra
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(ProductExporter {
            sink,
            disabled: AtomicBool::new(false),
        }));
    }
}

/// Keeps the credential-lifecycle writer alive for `plak login` / `plak logout`.
pub struct CredentialTelemetryGuard {
    _ready: TelemetryReady,
}

/// Opens (or reuses) product telemetry and installs credential-lifecycle emission.
#[must_use]
pub fn install_product_credential_telemetry(paths: &PlakPaths) -> Option<CredentialTelemetryGuard> {
    open_product_telemetry(paths, None)
        .ok()
        .map(|ready| CredentialTelemetryGuard { _ready: ready })
}

#[allow(
    clippy::large_types_passed_by_value,
    reason = "Copy product observation enum"
)]
pub(crate) fn try_emit_product(
    writer: Option<&Arc<TelemetryWriter>>,
    observation: ProductObservation,
) {
    if let Some(writer) = writer {
        writer.emit_product(observation);
    }
}

pub use plakous_hosted::agent::TelemetryHealth;

pub(crate) fn telemetry_health_from_cache(
    paths: &PlakPaths,
    cache: Option<&TelemetryCache>,
) -> TelemetryHealth {
    match cache {
        Some(TelemetryCache::Unavailable(reason)) => TelemetryHealth::Unavailable(reason.clone()),
        Some(TelemetryCache::Ready(ready)) => match TelemetryStore::count_all_rows(paths) {
            Ok(rows) => TelemetryHealth::Ok {
                rows,
                write_errors: ready.writer.status().write_errors(),
                dropped: ready.writer.status().dropped(),
            },
            Err(error) => TelemetryHealth::Unavailable(error.to_string()),
        },
        None => match paths.telemetry_database().try_exists() {
            Ok(true) => TelemetryHealth::Unavailable(
                "telemetry database exists but the writer has not been opened".into(),
            ),
            Ok(false) => TelemetryHealth::Missing,
            Err(error) => TelemetryHealth::Unavailable(error.to_string()),
        },
    }
}

pub(crate) fn inspect_product_telemetry(paths: &PlakPaths) -> TelemetryHealth {
    match paths.telemetry_database().try_exists() {
        Ok(false) => TelemetryHealth::Missing,
        Err(error) => TelemetryHealth::Unavailable(error.to_string()),
        Ok(true) => match TelemetryStore::count_all_rows(paths) {
            Ok(rows) => {
                let status = telemetry_writer_status(paths);
                TelemetryHealth::Ok {
                    rows,
                    write_errors: status.write_errors(),
                    dropped: status.dropped(),
                }
            }
            Err(error) => TelemetryHealth::Unavailable(error.to_string()),
        },
    }
}

pub(crate) fn log_telemetry_unavailable(paths: &PlakPaths, session_id: &str, reason: &str) {
    plakous_hosted::session_log::emit(
        &paths.session_home(),
        session_id,
        plakous_hosted::session_log::SessionEvent::new("telemetry.unavailable")
            .str("reason", reason)
            .str("database", paths.telemetry_database().display().to_string()),
    );
}

/// Writer drop/error counters when a session has registered the telemetry writer.
#[must_use]
pub fn telemetry_writer_status(paths: &PlakPaths) -> TelemetryWriterStatus {
    telemetry_writer_for_home(paths.home())
        .map(|writer| writer.status())
        .unwrap_or_default()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::implicit_clone, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::telemetry_store::TelemetryStore;
    use std::sync::atomic::{AtomicU8, AtomicU32, Ordering as AtomicOrdering};
    use std::time::Duration;

    use plakous::agent::{
        TelemetryContext, TelemetryObservation, TelemetryObservationKind, TelemetryTerminalStatus,
    };
    use plakous::model::{
        AgentId, AttemptId, BranchId, CallId, RunId, SessionId, Timestamp, TurnId, Usage,
    };
    use uuid::Uuid;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    fn observation() -> TestResult<TelemetryObservation> {
        Ok(TelemetryObservation::new(
            TelemetryContext::new(
                AgentId::try_from(Uuid::from_u128(1))?,
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
            },
        ))
    }

    #[test]
    fn child_retry_is_logged_with_host_route_and_size_even_without_analytics() -> TestResult {
        #[derive(Clone)]
        struct Buffer(Arc<Mutex<Vec<u8>>>, mpsc::Sender<()>);
        impl std::io::Write for Buffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Drop for Buffer {
            fn drop(&mut self) {
                let _ = self.1.send(());
            }
        }
        let bytes = Arc::new(Mutex::new(Vec::new()));
        let (done, finished) = mpsc::channel();
        let writer = Buffer(Arc::clone(&bytes), done);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(move || writer.clone())
            .finish();
        let context = TelemetryContext::new(
            AgentId::try_from(Uuid::from_u128(99))?,
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
        let emitter = TelemetryEmitter::enabled(Arc::new(RetryLoggingSink { inner: None }));
        // The child's ordinary telemetry port closes before its retry.
        emitter.emit(observation()?);
        tracing::subscriber::with_default(subscriber, || {
            emitter.observe_model_retry(context, &retry);
        });
        finished.recv_timeout(Duration::from_secs(5))?;
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
        fn try_emit_product(
            &self,
            _observation: ProductObservation,
        ) -> Result<(), TelemetrySinkError> {
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
}
