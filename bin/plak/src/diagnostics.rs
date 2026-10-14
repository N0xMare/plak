//! Optional file diagnostics (`PLAK_LOG`) and OTel export; never writes to the TUI terminal.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
#[cfg(feature = "otel")]
use std::time::Duration;

use crate::home::paths::PlakPaths;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[cfg(feature = "otel")]
use opentelemetry_sdk::trace::{BatchSpanProcessor, SdkTracerProvider};

/// Failure to install the optional diagnostics subscriber.
#[derive(Debug, Eq, PartialEq)]
pub enum DiagnosticsError {
    /// `PLAK_LOG` could not be parsed as an [`EnvFilter`] directive string.
    Filter(String),
    /// Plak home layout or the diagnostics log file could not be opened.
    Io(String),
    /// The optional OTel exporter could not be constructed.
    #[cfg(feature = "otel")]
    Otel(String),
}

impl fmt::Display for DiagnosticsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Filter(message) => write!(f, "invalid PLAK_LOG filter: {message}"),
            Self::Io(message) => write!(f, "{message}"),
            #[cfg(feature = "otel")]
            Self::Otel(message) => write!(f, "otel exporter unavailable: {message}"),
        }
    }
}

/// Keeps the global diagnostics subscriber alive and flushes the log on drop.
pub struct DiagnosticsGuard {
    log: Option<Arc<Mutex<OpenLog>>>,
    #[cfg(feature = "otel")]
    #[allow(dead_code, reason = "keeps the OTel provider alive until process exit")]
    otel: Option<OtelShutdown>,
}

#[cfg(feature = "otel")]
struct OtelShutdown {
    provider: SdkTracerProvider,
}

#[cfg(feature = "otel")]
impl Drop for OtelShutdown {
    fn drop(&mut self) {
        let provider = std::mem::replace(&mut self.provider, SdkTracerProvider::builder().build());
        std::thread::spawn(move || {
            let _ = provider.shutdown_with_timeout(Duration::from_millis(500));
        });
    }
}

impl Drop for DiagnosticsGuard {
    fn drop(&mut self) {
        if let Some(log) = self.log.as_ref()
            && let Ok(mut file) = log.lock()
        {
            let _ = file.file.flush();
        }
    }
}

/// Mutex-backed log writer for the diagnostics file layer.
struct LogFile(Arc<Mutex<OpenLog>>);

/// The open diagnostics log and what rotation needs to replace it.
struct OpenLog {
    path: PathBuf,
    file: File,
    written_since_check: u64,
}

impl OpenLog {
    /// Rotates once the file this process appends to passes the bound. Several
    /// processes append to one log, so the path is renamed only while it still
    /// names this process's file; if another process rotated first, this one
    /// just follows the path to the new file.
    fn rotate_if_large(&mut self) {
        use std::os::unix::fs::MetadataExt as _;
        let Ok(ours) = self.file.metadata() else {
            return;
        };
        if ours.len() <= ROTATE_AT_BYTES {
            return;
        }
        let current = std::fs::metadata(&self.path).ok();
        if current.is_some_and(|current| (current.dev(), current.ino()) == (ours.dev(), ours.ino()))
        {
            let _ = std::fs::rename(&self.path, rotated_path(&self.path));
        }
        if let Ok(file) = open_append_log(&self.path) {
            self.file = file;
        }
    }
}

/// Bytes a process writes between checks of its log's size.
const ROTATION_CHECK_BYTES: u64 = 1024 * 1024;

impl Write for LogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut log = self
            .0
            .lock()
            .map_err(|_| io::Error::other("diagnostics log lock poisoned"))?;
        let written = log.file.write(buf)?;
        log.written_since_check = log.written_since_check.saturating_add(written as u64);
        if log.written_since_check >= ROTATION_CHECK_BYTES {
            log.written_since_check = 0;
            log.rotate_if_large();
        }
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("diagnostics log lock poisoned"))?
            .file
            .flush()
    }
}

/// The `PLAK_LOG` directive when set and non-empty; the only reader of that variable.
pub fn requested_filter() -> Option<String> {
    let value = std::env::var_os("PLAK_LOG")?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_string_lossy().into_owned())
}

/// The OTLP endpoint when the `otel` feature is enabled; the only reader of that variable.
#[cfg(feature = "otel")]
pub fn otlp_endpoint() -> Option<String> {
    let value = std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT")?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_string_lossy().into_owned())
}

/// Install a composed `tracing` subscriber for optional file and OTel export layers.
pub fn install(
    paths: &PlakPaths,
    filter_text: Option<&str>,
    #[cfg(feature = "otel")] otlp_endpoint: Option<&str>,
) -> Result<DiagnosticsGuard, DiagnosticsError> {
    let filter = match filter_text {
        Some(text) => {
            EnvFilter::try_new(text).map_err(|error| DiagnosticsError::Filter(error.to_string()))?
        }
        None => EnvFilter::try_new("info")
            .map_err(|error| DiagnosticsError::Filter(error.to_string()))?,
    };

    let mut log = None;
    let fmt_layer = if filter_text.is_some() {
        // The log needs a private home and `logs/` only. `ensure_layout` also
        // creates `state/<key>/sessions`, and a maintainer build installs
        // diagnostics for every command, including `version --state-key`.
        ensure_log_directory(paths)?;
        let log_path = paths.diagnostics_log_file();
        let file = open_append_log(&log_path)?;
        let shared = Arc::new(Mutex::new(OpenLog {
            path: log_path,
            file,
            written_since_check: 0,
        }));
        let layer = tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_target(true)
            .with_thread_ids(false)
            .with_span_events(FmtSpan::CLOSE)
            .with_writer({
                let shared = Arc::clone(&shared);
                move || LogFile(Arc::clone(&shared))
            });
        log = Some(shared);
        Some(layer)
    } else {
        None
    };

    #[cfg(feature = "otel")]
    let otel_shutdown = otlp_endpoint.map(build_otel_provider).transpose()?;

    #[cfg(feature = "otel")]
    return Ok({
        use opentelemetry::trace::TracerProvider as _;
        match (fmt_layer, otel_shutdown) {
            (Some(fmt_layer), Some(otel)) => {
                let tracer = otel.provider.tracer("plak");
                tracing_subscriber::registry()
                    .with(filter)
                    .with(fmt_layer)
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
                DiagnosticsGuard {
                    log,
                    otel: Some(otel),
                }
            }
            (Some(fmt_layer), None) => {
                tracing_subscriber::registry()
                    .with(filter)
                    .with(fmt_layer)
                    .init();
                DiagnosticsGuard { log, otel: None }
            }
            (None, Some(otel)) => {
                let tracer = otel.provider.tracer("plak");
                tracing_subscriber::registry()
                    .with(filter)
                    .with(tracing_opentelemetry::layer().with_tracer(tracer))
                    .init();
                DiagnosticsGuard {
                    log,
                    otel: Some(otel),
                }
            }
            (None, None) => unreachable!(
                "install requires PLAK_LOG or OTEL_EXPORTER_OTLP_ENDPOINT when otel is enabled"
            ),
        }
    });

    #[cfg(not(feature = "otel"))]
    {
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
        Ok(DiagnosticsGuard { log })
    }
}

#[cfg(feature = "otel")]
fn build_otel_provider(endpoint: &str) -> Result<OtelShutdown, DiagnosticsError> {
    use opentelemetry::KeyValue;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::Resource;

    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_timeout(Duration::from_millis(250))
        .build()
        .map_err(|error| DiagnosticsError::Otel(error.to_string()))?;
    let resource = Resource::builder()
        .with_service_name("plak")
        .with_attribute(KeyValue::new("service.version", plakous::VERSION))
        .build();
    let processor = BatchSpanProcessor::builder(exporter).build();
    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_span_processor(processor)
        .build();
    Ok(OtelShutdown { provider })
}

/// A log past this size is moved to `<name>.1` (replacing the previous one),
/// so default maintainer logging stays bounded.
const ROTATE_AT_BYTES: u64 = 64 * 1024 * 1024;

fn rotated_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    PathBuf::from(rotated)
}

/// Private home plus `logs/`, and nothing under `state/`.
fn ensure_log_directory(paths: &PlakPaths) -> Result<(), DiagnosticsError> {
    paths
        .ensure_private_state_home()
        .map_err(DiagnosticsError::Io)?;
    let logs = paths.home().join("logs");
    match std::fs::symlink_metadata(&logs) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(DiagnosticsError::Io(
            "plak logs path is not a directory".to_owned(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir(&logs)
            .or_else(|error| {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|_| {
                DiagnosticsError::Io("could not create the diagnostics log directory".to_owned())
            }),
        Err(_) => Err(DiagnosticsError::Io(
            "could not inspect the diagnostics log directory".to_owned(),
        )),
    }
}

fn open_append_log(path: &Path) -> Result<File, DiagnosticsError> {
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > ROTATE_AT_BYTES) {
        let _ = std::fs::rename(path, rotated_path(path));
    }

    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| DiagnosticsError::Io(format!("could not open diagnostics log: {error}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests {
    use super::*;
    use crate::home::paths::PlakPaths;

    #[test]
    fn diagnostics_log_is_created_private() {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        let path = paths.diagnostics_log_file();
        open_append_log(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn invalid_filter_directive_is_rejected() {
        let error = EnvFilter::try_new("info=not_a_real_level").unwrap_err();
        assert_ne!(error.to_string(), "");
    }

    #[test]
    fn a_running_process_rotates_its_log_and_follows_another_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("plak.log");
        let open = |path: &Path| {
            Arc::new(Mutex::new(OpenLog {
                path: path.to_owned(),
                file: open_append_log(path).unwrap(),
                written_since_check: 0,
            }))
        };
        let first = open(&path);
        let second = open(&path);
        let chunk = vec![b'x'; usize::try_from(ROTATION_CHECK_BYTES).unwrap()];
        for _ in 0..=(ROTATE_AT_BYTES / ROTATION_CHECK_BYTES) {
            LogFile(Arc::clone(&first)).write_all(&chunk).unwrap();
        }
        // The first writer passed the bound, renamed the shared log and
        // reopened a fresh one at the path.
        assert!(std::fs::metadata(rotated_path(&path)).unwrap().len() > ROTATE_AT_BYTES);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        // The second writer still holds the rotated file; at its next check it
        // must follow the path rather than rename the fresh log away.
        LogFile(Arc::clone(&second)).write_all(&chunk).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "the second writer's chunk lands in the rotated file it held"
        );
        LogFile(Arc::clone(&second)).write_all(b"after\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"after\n");
    }
}
