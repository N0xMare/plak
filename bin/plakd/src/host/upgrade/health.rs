use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(in crate::host) use crate::carry::upgrade::timing::ADMISSION as ADMISSION_TIME;
use crate::carry::upgrade::timing::STORE as STORE_TIME;
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

/// The checked stores, in the fixed order their failures are reported.
pub(in crate::host) fn databases(paths: &PlakPaths) -> [(&'static str, PathBuf); 3] {
    [
        ("product", paths.product_database()),
        ("tree", paths.subagents_database()),
        ("runtime", paths.runtime_database()),
    ]
}

pub(in crate::host) fn deadline() -> Instant {
    Instant::now() + STORE_TIME
}

pub(in crate::host) fn store(label: &str, path: &Path, deadline: Instant) -> CliResult<()> {
    if crate::product_doctor::upgrade_database_healthy(path, deadline) {
        return Ok(());
    }
    Err(CliFailure::new(format!(
        "host_upgrade_health_refused: {label} store did not pass the bounded integrity check"
    )))
}

/// Opens each existing store once, read-only, and reads its schema, so the
/// WAL index exists before concurrent checks open the same file. Waits for
/// a busy store within `deadline`. Keep the result alive across the checks.
pub(in crate::host) fn pin(
    paths: &PlakPaths,
    deadline: Instant,
) -> CliResult<Vec<rusqlite::Connection>> {
    pin_paths(&databases(paths), deadline)
}

fn pin_paths(
    databases: &[(&str, PathBuf)],
    deadline: Instant,
) -> CliResult<Vec<rusqlite::Connection>> {
    use rusqlite::{Connection, OpenFlags};
    let mut pinned = Vec::new();
    for (label, path) in databases {
        if !path.is_file() {
            continue;
        }
        let open = || -> rusqlite::Result<Connection> {
            let connection = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            connection.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
            connection.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
            Ok(connection)
        };
        pinned.push(open().map_err(|error| {
            CliFailure::new(format!(
                "host_upgrade_health_refused: {label} store could not be opened for the gate: {error}"
            ))
        })?);
    }
    Ok(pinned)
}

pub(in crate::host) fn stores(paths: &PlakPaths) -> CliResult<()> {
    timed_stores(paths).map(drop)
}

/// The serial check with each store's elapsed time, for the gate projection.
pub(in crate::host) fn timed_stores(paths: &PlakPaths) -> CliResult<Vec<(&'static str, Duration)>> {
    let deadline = deadline();
    databases(paths)
        .into_iter()
        .map(|(label, path)| {
            let started = Instant::now();
            store(label, &path, deadline)?;
            Ok((label, started.elapsed()))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Holds an exclusive lock on a rollback-journal store for `hold`.
    fn busy_store(hold: Duration) -> (tempfile::TempDir, PathBuf, std::thread::JoinHandle<()>) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.sqlite3");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE t(x); BEGIN EXCLUSIVE; INSERT INTO t VALUES (1);")
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(hold);
            writer.execute_batch("COMMIT").unwrap();
        });
        (directory, path, release)
    }

    #[test]
    fn a_busy_store_is_waited_for_within_the_gate_bound() {
        let (_directory, path, held) = busy_store(Duration::from_millis(700));
        assert!(store("runtime", &path, Instant::now() + Duration::from_secs(10)).is_ok());
        held.join().unwrap();

        let (_directory, path, held) = busy_store(Duration::from_millis(1500));
        let started = Instant::now();
        assert!(
            store(
                "runtime",
                &path,
                Instant::now() + Duration::from_millis(300)
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_millis(1400));
        held.join().unwrap();
    }

    #[test]
    fn pinning_waits_for_a_busy_store_within_its_bound_and_skips_absent_ones() {
        let (directory, busy, held) = busy_store(Duration::from_millis(700));
        let stores = [
            ("runtime", busy),
            ("tree", directory.path().join("absent.sqlite3")),
        ];
        let pinned = pin_paths(&stores, Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(pinned.len(), 1);
        held.join().unwrap();

        let (_directory, busy, held) = busy_store(Duration::from_millis(1500));
        let refused = pin_paths(
            &[("runtime", busy)],
            Instant::now() + Duration::from_millis(300),
        );
        assert!(
            refused
                .unwrap_err()
                .message()
                .contains("runtime store could not be opened")
        );
        held.join().unwrap();
    }
}
