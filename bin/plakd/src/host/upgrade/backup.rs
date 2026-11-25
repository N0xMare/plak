//! A detached, bounded backup is for inspection, never automatic restoration.

use std::fs::File;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use plakous_recursive_subagents_store::admission::copy_snapshot_bounded;

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const COPY_TIME: Duration = Duration::from_secs(60);

pub(super) fn create(paths: &PlakPaths) -> CliResult<PathBuf> {
    create_with_bound(paths, MAX_BYTES)
}

fn create_with_bound(paths: &PlakPaths, limit: u64) -> CliResult<PathBuf> {
    let failure = |error: &dyn std::fmt::Display| {
        CliFailure::new(format!(
            "host_upgrade_backup_refused: {error}; no source surrendered"
        ))
    };
    let root = paths.state_dir().join("upgrade-backups");
    crate::commands::runner::private_directory(&root)?;
    File::open(paths.state_dir())
        .and_then(|dir| dir.sync_all())
        .map_err(|error| failure(&error))?;
    let staging = tempfile::Builder::new()
        .prefix(".pending-")
        .tempdir_in(&root)
        .map_err(|error| failure(&error))?;
    let deadline = Instant::now() + COPY_TIME;
    let mut remaining = limit;
    for (name, source) in [
        ("product.sqlite3", paths.product_database()),
        ("subagents.sqlite3", paths.subagents_database()),
        ("runtime.sqlite3", paths.runtime_database()),
    ] {
        let size = copy_snapshot_bounded(&source, &staging.path().join(name), remaining, deadline)
            .map_err(|error| failure(&error))?;
        remaining -= size;
    }
    File::open(staging.path())
        .and_then(|dir| dir.sync_all())
        .map_err(|error| failure(&error))?;
    let published = root.join(uuid::Uuid::new_v4().to_string());
    std::fs::rename(staging.path(), &published).map_err(|error| failure(&error))?;
    File::open(&root)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| failure(&error))?;
    Ok(published)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_combined_overflow_and_only_publishes_a_complete_backup() {
        let (_temp, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        for db in [paths.product_database(), paths.runtime_database()] {
            let connection = rusqlite::Connection::open(db).unwrap();
            connection
                .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE sample(i); INSERT INTO sample VALUES(42);")
                .unwrap();
        }
        let size = paths.product_database().metadata().unwrap().len();
        assert!(create_with_bound(&paths, size).is_err());
        assert_eq!(
            std::fs::read_dir(paths.state_dir().join("upgrade-backups"))
                .unwrap()
                .count(),
            0
        );
        let backup = create_with_bound(&paths, size * 2).unwrap();
        let copied = rusqlite::Connection::open(backup.join("runtime.sqlite3")).unwrap();
        assert_eq!(
            copied
                .query_row("SELECT i FROM sample", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            42
        );
    }
}
