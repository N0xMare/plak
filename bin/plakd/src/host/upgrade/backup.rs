//! A detached, bounded backup is for inspection, never automatic restoration.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use plakous_recursive_subagents_store::admission::copy_snapshot_bounded;

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

const MAX_BYTES: u64 = 512 * 1024 * 1024;
const COPY_TIME: Duration = Duration::from_secs(60);
const KEEP: usize = 3;

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
    tidy(&root, None).map_err(|error| failure(&error))?;
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
    fs::rename(staging.path(), &published).map_err(|error| failure(&error))?;
    tidy(&root, Some(&published)).map_err(|error| failure(&error))?;
    File::open(&root)
        .and_then(|dir| dir.sync_all())
        .map_err(|error| failure(&error))?;
    Ok(published)
}

// Called only by the serialized requester. Ignore unrelated names and links.
fn tidy(root: &Path, published: Option<&Path>) -> std::io::Result<()> {
    let mut older = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        if name.starts_with(".pending-") {
            fs::remove_dir_all(&path)?;
        } else if published.is_some_and(|current| current != path)
            && uuid::Uuid::parse_str(name).is_ok_and(|id| id.to_string() == name)
        {
            older.push((entry.metadata()?.modified()?, path));
        }
    }
    older.sort_unstable_by(|a, b| b.cmp(a));
    for (_, path) in older.into_iter().skip(KEEP - 1) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
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
        let root = backup.parent().unwrap();
        fs::create_dir(root.join(".pending-interrupted")).unwrap();
        fs::create_dir(root.join("unrelated")).unwrap();
        for _ in 0..KEEP + 1 {
            create_with_bound(&paths, size * 2).unwrap();
        }
        assert!(!root.join(".pending-interrupted").exists());
        assert!(root.join("unrelated").exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), KEEP + 1);
    }
}
