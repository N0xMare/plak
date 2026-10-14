//! Content-free launch and cooked-terminal timings; emission never flushes.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::{fs, io::Write as _};

use plakous_hosted::product_observation::{
    ProductObservation, TuiTimingPhase, session_id_from_text, wall_timestamp,
};

use crate::home::paths::PlakPaths;

static PROCESS_START: OnceLock<Instant> = OnceLock::new();
static TIMING: Mutex<Option<Timing>> = Mutex::new(None);

struct Timing {
    paths: PlakPaths,
    session_id: plakous::model::SessionId,
    resumed: bool,
    first_frame: bool,
    host_ready: bool,
    quit_requested: Option<Instant>,
    restored: Option<Instant>,
    pending: Vec<ProductObservation>,
    /// The first frame asked for the earlier exits; they are claimed only
    /// once a telemetry writer can take them, so a process that never
    /// registers one leaves them for the next open.
    exit_import_due: bool,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExitMeasurement {
    session: String,
    resumed: bool,
    key_to_restore_ns: u64,
    restore_to_exit_ns: u64,
}

fn write_exit_measurement(paths: &PlakPaths, value: &ExitMeasurement) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let path = paths
        .home()
        .join("logs")
        .join(format!("tui-exit-{}.jsonl", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)
}

fn take_exit_measurements(paths: &PlakPaths) -> Vec<ExitMeasurement> {
    use std::io::Read as _;
    let Ok(entries) = fs::read_dir(paths.home().join("logs")) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with("tui-exit-")
                || entry
                    .path()
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
            {
                return None;
            }
            // Claim a line once even when two sessions become promptable together.
            let claimed = entry
                .path()
                .with_extension(format!("importing-{}", std::process::id()));
            fs::rename(entry.path(), &claimed).ok()?;
            let value = (|| {
                let descriptor = rustix::fs::open(
                    &claimed,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .ok()?;
                let file = fs::File::from(descriptor);
                if !file.metadata().ok()?.is_file() {
                    return None;
                }
                let mut bytes = Vec::new();
                file.take(4096).read_to_end(&mut bytes).ok()?;
                serde_json::from_slice(&bytes).ok()
            })();
            let _ = fs::remove_file(claimed);
            value
        })
        .collect()
}

/// The phases an earlier exit recorded, as this open's observations.
fn exit_observations(values: Vec<ExitMeasurement>) -> Vec<ProductObservation> {
    values
        .into_iter()
        .filter_map(|value| Some((session_id_from_text(&value.session)?, value)))
        .flat_map(|(session_id, value)| {
            [
                (TuiTimingPhase::KeyToRestore, value.key_to_restore_ns),
                (TuiTimingPhase::RestoreToExit, value.restore_to_exit_ns),
            ]
            .map(|(phase, elapsed)| ProductObservation::TuiTiming {
                at: wall_timestamp(),
                session_id,
                phase,
                resumed: value.resumed,
                elapsed: Duration::from_nanos(elapsed),
            })
        })
        .collect()
}

/// Imports an earlier exit's measurements once this open's first frame
/// asked for them and a telemetry writer is registered; the first frame
/// usually precedes the host preparation that registers it.
fn import_exit_measurements_when_writable() {
    let paths = TIMING.lock().ok().and_then(|mut timing| {
        let timing = timing.as_mut()?;
        if !timing.exit_import_due
            || crate::telemetry_sink::telemetry_writer_for_home(timing.paths.home()).is_none()
        {
            return None;
        }
        timing.exit_import_due = false;
        Some(timing.paths.clone())
    });
    let Some(paths) = paths else {
        return;
    };
    std::thread::spawn(move || {
        let observations = exit_observations(take_exit_measurements(&paths));
        if let Ok(mut timing) = TIMING.lock()
            && let Some(timing) = timing.as_mut()
        {
            let writer = crate::telemetry_sink::telemetry_writer_for_home(timing.paths.home());
            timing.emit(writer.as_ref(), observations);
        }
    });
}

pub(crate) fn process_started() {
    PROCESS_START.get_or_init(Instant::now);
}

pub(crate) fn attach(paths: &PlakPaths, session: &str, resumed: bool) {
    let Some(session_id) = session_id_from_text(session) else {
        return;
    };
    if let Ok(mut timing) = TIMING.lock() {
        *timing = Some(Timing {
            paths: paths.clone(),
            session_id,
            resumed,
            first_frame: false,
            host_ready: false,
            quit_requested: None,
            restored: None,
            pending: Vec::new(),
            exit_import_due: false,
        });
    }
    record(TuiTimingPhase::ProcessStart, Duration::ZERO);
}

fn record(phase: TuiTimingPhase, elapsed: Duration) {
    let Ok(mut timing) = TIMING.lock() else {
        return;
    };
    let Some(timing) = timing.as_mut() else {
        return;
    };
    let writer = crate::telemetry_sink::telemetry_writer_for_home(timing.paths.home());
    timing.record(writer.as_ref(), phase, elapsed);
}

/// A telemetry writer was registered: flush the buffered launch phases and
/// import the earlier exits the first frame asked for.
pub(crate) fn writer_registered() {
    flush_pending();
    import_exit_measurements_when_writable();
}

pub(crate) fn flush_pending() {
    if let Ok(mut timing) = TIMING.lock()
        && let Some(timing) = timing.as_mut()
        && let Some(writer) = crate::telemetry_sink::telemetry_writer_for_home(timing.paths.home())
    {
        timing.flush_pending(&writer);
    }
}

impl Timing {
    fn flush_pending(&mut self, writer: &crate::telemetry_sink::TelemetryWriter) {
        for pending in self.pending.drain(..) {
            writer.emit_product(pending);
        }
    }

    fn record(
        &mut self,
        writer: Option<&std::sync::Arc<crate::telemetry_sink::TelemetryWriter>>,
        phase: TuiTimingPhase,
        elapsed: Duration,
    ) {
        let observation = ProductObservation::TuiTiming {
            at: wall_timestamp(),
            session_id: self.session_id,
            phase,
            resumed: self.resumed,
            elapsed,
        };
        self.emit(writer, [observation]);
    }

    fn emit(
        &mut self,
        writer: Option<&std::sync::Arc<crate::telemetry_sink::TelemetryWriter>>,
        observations: impl IntoIterator<Item = ProductObservation>,
    ) {
        let Some(writer) = writer else {
            // The finite launch phases can precede background writer registration.
            self.pending.extend(observations);
            return;
        };
        self.flush_pending(writer);
        for observation in observations {
            writer.emit_product(observation);
        }
    }
}

pub(crate) fn first_frame() {
    let first = TIMING.lock().is_ok_and(|mut timing| {
        timing.as_mut().is_some_and(|timing| {
            if timing.first_frame {
                return false;
            }
            timing.first_frame = true;
            true
        })
    });
    if first {
        let elapsed = PROCESS_START.get_or_init(Instant::now).elapsed();
        record(TuiTimingPhase::FirstFrameLocal, elapsed);
        record(TuiTimingPhase::ReadyToTypeLocal, elapsed);
        if let Ok(mut timing) = TIMING.lock()
            && let Some(timing) = timing.as_mut()
        {
            timing.exit_import_due = true;
        }
        import_exit_measurements_when_writable();
    }
}

pub(crate) fn host_ready() {
    let first = TIMING.lock().is_ok_and(|mut timing| {
        timing.as_mut().is_some_and(|timing| {
            if timing.host_ready {
                return false;
            }
            timing.host_ready = true;
            true
        })
    });
    if first {
        record(
            TuiTimingPhase::HostReadyBackground,
            PROCESS_START.get_or_init(Instant::now).elapsed(),
        );
    }
}

pub(crate) fn quit_requested() {
    if let Ok(mut timing) = TIMING.lock()
        && let Some(timing) = timing.as_mut()
    {
        timing.quit_requested.get_or_insert_with(Instant::now);
    }
}

pub(crate) fn restored() {
    if let Ok(mut timing) = TIMING.lock()
        && let Some(timing) = timing.as_mut()
    {
        timing.restored = Some(Instant::now());
    }
}

pub(crate) fn remaining_exit_budget() -> Duration {
    let budget = crate::commands::support::VOLUNTARY_QUIT_GRACE;
    TIMING
        .lock()
        .ok()
        .and_then(|timing| timing.as_ref()?.quit_requested)
        .map_or(budget, |at| budget.saturating_sub(at.elapsed()))
}

pub(crate) fn exiting() {
    flush_pending();
    if let Ok(timing) = TIMING.lock()
        && let Some(timing) = timing.as_ref()
        && let Some(restored) = timing.restored
    {
        let nanos = |elapsed: Duration| u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX);
        let value = ExitMeasurement {
            session: timing.session_id.as_uuid().to_string(),
            resumed: timing.resumed,
            key_to_restore_ns: nanos(
                timing
                    .quit_requested
                    .map_or(Duration::ZERO, |at| restored.saturating_duration_since(at)),
            ),
            restore_to_exit_ns: nanos(restored.elapsed()),
        };
        let _ = write_exit_measurement(&timing.paths, &value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_writer_registration_flushes_launch_phases_without_another_phase()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = PlakPaths::from_home(directory.path().to_path_buf());
        attach(&paths, "00000000-0000-0000-0000-000000000001", false);
        record(TuiTimingPhase::ReadyToTypeLocal, Duration::from_millis(20));
        let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
        crate::telemetry_sink::register_telemetry_writer(paths.home(), &writer);
        assert_eq!(receiver.try_iter().count(), 2);
        Ok(())
    }

    #[test]
    fn launch_phases_wait_for_the_background_telemetry_writer() {
        let paths = PlakPaths::from_home(std::env::temp_dir());
        let session_id = session_id_from_text("00000000-0000-0000-0000-000000000001")
            .unwrap_or_else(|| unreachable!("fixed valid session"));
        let mut timing = Timing {
            paths,
            session_id,
            resumed: false,
            first_frame: false,
            host_ready: false,
            quit_requested: None,
            restored: None,
            pending: Vec::new(),
            exit_import_due: false,
        };
        timing.record(None, TuiTimingPhase::ProcessStart, Duration::ZERO);
        timing.record(
            None,
            TuiTimingPhase::FirstFrameLocal,
            Duration::from_millis(20),
        );
        timing.record(
            None,
            TuiTimingPhase::ReadyToTypeLocal,
            Duration::from_millis(20),
        );
        let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
        timing.record(
            Some(&writer),
            TuiTimingPhase::HostReadyBackground,
            Duration::from_secs(2),
        );
        let phases: Vec<_> =
            receiver
                .try_iter()
                .filter_map(|item| match item {
                    crate::telemetry_store::TelemetryItem::Product(
                        ProductObservation::TuiTiming { phase, elapsed, .. },
                    ) => Some((phase, elapsed)),
                    _ => None,
                })
                .collect();
        assert_eq!(
            phases,
            vec![
                (TuiTimingPhase::ProcessStart, Duration::ZERO),
                (TuiTimingPhase::FirstFrameLocal, Duration::from_millis(20)),
                (TuiTimingPhase::ReadyToTypeLocal, Duration::from_millis(20)),
                (TuiTimingPhase::HostReadyBackground, Duration::from_secs(2)),
            ]
        );
        assert_eq!(timing.pending, [] as [ProductObservation; 0]);
    }

    #[test]
    fn an_exit_imported_before_writer_registration_is_emitted_on_registration() {
        let session = "00000000-0000-0000-0000-000000000001";
        let mut timing = Timing {
            paths: PlakPaths::from_home(std::env::temp_dir()),
            session_id: session_id_from_text(session)
                .unwrap_or_else(|| unreachable!("fixed valid session")),
            resumed: true,
            first_frame: true,
            host_ready: false,
            quit_requested: None,
            restored: None,
            pending: Vec::new(),
            exit_import_due: false,
        };
        let observations = exit_observations(vec![ExitMeasurement {
            session: session.to_owned(),
            resumed: false,
            key_to_restore_ns: 4_000_000,
            restore_to_exit_ns: 12_000_000,
        }]);
        timing.emit(None, observations);
        let (writer, receiver) = crate::telemetry_sink::TelemetryWriter::detached(8);
        timing.flush_pending(&writer);
        let phases: Vec<_> =
            receiver
                .try_iter()
                .filter_map(|item| match item {
                    crate::telemetry_store::TelemetryItem::Product(
                        ProductObservation::TuiTiming { phase, elapsed, .. },
                    ) => Some((phase, elapsed)),
                    _ => None,
                })
                .collect();
        assert_eq!(
            phases,
            vec![
                (TuiTimingPhase::KeyToRestore, Duration::from_millis(4)),
                (TuiTimingPhase::RestoreToExit, Duration::from_millis(12)),
            ]
        );
    }

    #[test]
    fn exit_measurement_survives_until_the_next_open() -> Result<(), Box<dyn std::error::Error>> {
        let directory = tempfile::tempdir()?;
        let paths = PlakPaths::from_home(directory.path().to_path_buf());
        fs::create_dir_all(directory.path().join("logs"))?;
        let measurement = ExitMeasurement {
            session: "00000000-0000-0000-0000-000000000001".to_owned(),
            resumed: true,
            key_to_restore_ns: 4_000_000,
            restore_to_exit_ns: 12_000_000,
        };
        write_exit_measurement(&paths, &measurement)?;
        let imported = take_exit_measurements(&paths);
        assert_eq!(imported.len(), 1);
        assert_eq!(imported[0].restore_to_exit_ns, 12_000_000);
        assert!(take_exit_measurements(&paths).is_empty());
        Ok(())
    }
}
