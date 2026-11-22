//! Online backup of the three current SQLite authorities, without taking a write lock.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::backup::{Backup, StepResult};
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult, ProcessOutcome};

const SNAPSHOT_TRIES: usize = 5;
const LOCK_WAIT: Duration = Duration::from_millis(200);
const COPY_BUDGET: Duration = Duration::from_secs(120);
type BackupResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Serialize)]
struct Manifest {
    version: u32,
    state_key: String,
    files: Vec<CopiedFile>,
}

#[derive(Serialize)]
struct CopiedFile {
    name: &'static str,
    bytes: u64,
    sha256: String,
}

pub(crate) fn run(
    paths: &PlakPaths,
    directory: &Path,
    out: &mut dyn Write,
) -> CliResult<ProcessOutcome> {
    let sources = [
        ("runtime.sqlite3", paths.runtime_database()),
        ("product.sqlite3", paths.product_database()),
        ("subagents.sqlite3", paths.subagents_database()),
    ];
    let manifest = publish(paths, directory, &sources)
        .map_err(|error| CliFailure::new(format!("store backup: {error}")))?;
    out.write_all(&manifest)
        .map_err(|error| CliFailure::new(format!("backup published but stdout failed: {error}")))?;
    Ok(ProcessOutcome::Success)
}

fn publish(
    paths: &PlakPaths,
    directory: &Path,
    sources: &[(&'static str, PathBuf); 3],
) -> BackupResult<Vec<u8>> {
    let target = target_directory(paths, sources, directory)?;
    let stage = tempfile::Builder::new()
        .prefix(".plak-backup-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(target.parent()?)?;
    {
        let pairs = sources
            .iter()
            .map(|(name, path)| open_pair(name, path))
            .collect::<BackupResult<Vec<_>>>()?;
        pin_common_snapshot(&pairs, || Ok(()))?;
        let deadline = Instant::now() + COPY_BUDGET;
        for ((name, _), pair) in sources.iter().zip(&pairs) {
            copy(&pair.snapshot, &stage.path().join(name), deadline)?;
        }
    } // Release every read transaction before hashing or publishing.
    check_current_copies(stage.path())?;
    for (name, _) in sources {
        seal(&stage.path().join(name))?;
    }
    let files = sources
        .iter()
        .map(|(name, _)| fingerprint(name, &stage.path().join(name)))
        .collect::<BackupResult<_>>()?;
    let manifest = Manifest {
        version: 1,
        state_key: crate::home::state::state_key(),
        files,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    let mut manifest_file = private_file(&stage.path().join("manifest.json"))?;
    manifest_file.write_all(&bytes)?;
    manifest_file.sync_all()?;
    File::open(stage.path())?.sync_all()?;
    target.parent()?;
    let stage_name = stage
        .path()
        .file_name()
        .ok_or("backup staging name is missing")?;
    rustix::fs::renameat_with(
        &target.parent,
        stage_name,
        &target.parent,
        &target.name,
        rustix::fs::RenameFlags::NOREPLACE,
    )?;
    target.parent.sync_all().map_err(|error| {
        io::Error::other(format!(
            "backup published but its parent could not be synced: {error}"
        ))
    })?;
    Ok(bytes)
}

struct Target {
    parent: File,
    path: PathBuf,
    name: std::ffi::OsString,
}

impl Target {
    fn parent(&self) -> io::Result<PathBuf> {
        // Linux and macOS offer no common fd-relative tempfile API.
        // Do not stage if the path no longer points to the opened parent.
        let metadata = self.path.metadata()?;
        let opened = self.parent.metadata()?;
        if (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
            return Err(io::Error::other("destination parent moved"));
        }
        Ok(self.path.clone())
    }
}

fn target_directory(
    paths: &PlakPaths,
    sources: &[(&str, PathBuf)],
    target: &Path,
) -> BackupResult<Target> {
    let name = target
        .file_name()
        .ok_or("destination must name a new directory")?;
    let parent = fs::canonicalize(
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    let target_path = parent.join(name);
    let home = fs::canonicalize(paths.home())?;
    let home_metadata = home.metadata()?;
    let uid = rustix::process::geteuid().as_raw();
    if home_metadata.uid() != uid || home_metadata.mode() & 0o777 != 0o700 {
        return Err("PLAK_HOME must be a private directory you own".into());
    }
    if target_path.starts_with(&home) {
        return Err("destination cannot be inside PLAK_HOME".into());
    }
    for (_, source) in sources {
        if let Some(source_dir) = source.parent() {
            let source_dir = fs::canonicalize(source_dir)?;
            if !source_dir.starts_with(&home) {
                return Err("source store directory must be inside PLAK_HOME".into());
            }
        }
    }
    let fd = rustix::fs::open(
        &parent,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let parent_file = File::from(fd);
    let metadata = parent_file.metadata()?;
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(
            "destination parent must be owned by you and not writable by group or others".into(),
        );
    }
    match fs::symlink_metadata(&target_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("destination already exists".into()),
    }
    Ok(Target {
        parent: parent_file,
        path: parent,
        name: name.to_owned(),
    })
}

struct Source {
    snapshot: Connection,
    observer: Connection,
}

fn open_pair(name: &str, path: &Path) -> BackupResult<Source> {
    let parent = path
        .parent()
        .ok_or("source directory is missing")?
        .metadata()?;
    let metadata = fs::symlink_metadata(path)?;
    let uid = rustix::process::geteuid().as_raw();
    // Product and tree privacy comes from their enclosing private directory.
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
        || parent.uid() != uid
        || parent.mode() & 0o022 != 0
    {
        return Err(format!(
            "{name}: source entry and directory must be owned by you and not writable by others"
        )
        .into());
    }
    let open = || -> BackupResult<Connection> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(LOCK_WAIT)?;
        let mode: String = connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        if mode != "wal" {
            return Err("source must be an initialized WAL store".into());
        }
        Ok(connection)
    };
    let result = (|| -> BackupResult<_> {
        Ok(Source {
            snapshot: open()?,
            observer: open()?,
        })
    })();
    result.map_err(|error| format!("{name}: {error}").into())
}

fn check_current_copies(path: &Path) -> BackupResult<()> {
    use plakous::sessions::{SessionBackend, SessionPersistenceShape};
    if !matches!(
        SessionBackend::classify_sqlite(path.join("runtime.sqlite3"))?,
        SessionPersistenceShape::Current
    ) {
        return Err("runtime.sqlite3: source is not an initialized current runtime store".into());
    }
    let open = |name| {
        Connection::open_with_flags(
            path.join(name),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    };
    crate::store::ProductStore::admit_read_only(&open("product.sqlite3")?)?;
    plakous_recursive_subagents_store::database::schema()
        .require_current(&open("subagents.sqlite3")?)?;
    Ok(())
}

fn version(source: &Source) -> rusqlite::Result<i64> {
    source
        .observer
        .pragma_query_value(None, "data_version", |row| row.get(0))
}

fn pin_common_snapshot(
    sources: &[Source],
    mut after_first_pin: impl FnMut() -> BackupResult<()>,
) -> BackupResult<()> {
    // The observation windows all cover the interval in which reads pin
    // snapshots. Any commit in that interval invalidates the whole attempt.
    for attempt in 0..SNAPSHOT_TRIES {
        let before = sources.iter().map(version).collect::<Result<Vec<_>, _>>()?;
        for (index, source) in sources.iter().enumerate() {
            source.snapshot.execute_batch("BEGIN")?;
            source
                .snapshot
                .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))?;
            if index == 0 {
                after_first_pin()?;
            }
        }
        let after = sources.iter().map(version).collect::<Result<Vec<_>, _>>()?;
        if before == after {
            return Ok(());
        }
        for source in sources {
            source.snapshot.execute_batch("ROLLBACK")?;
        }
        if attempt + 1 < SNAPSHOT_TRIES {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Err("stores changed during snapshot acquisition; retry when writes are quieter".into())
}

fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(path)
}

fn copy(source: &Connection, path: &Path, deadline: Instant) -> BackupResult<()> {
    private_file(path)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut destination = Connection::open_with_flags(path, flags)?;
    destination.busy_timeout(LOCK_WAIT)?;
    {
        let backup = Backup::new(source, &mut destination)?;
        loop {
            if Instant::now() >= deadline {
                return Err("SQLite copy exceeded the online backup time budget".into());
            }
            match backup.step(256)? {
                StepResult::Done => break,
                StepResult::More => (),
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return Err("unsupported SQLite backup step".into()),
            }
        }
    }
    // Exact runtime open requires WAL mode, even though the backup must need
    // only the main file to restore.
    let journal: String = destination.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    if journal != "wal" {
        return Err("backup could not preserve WAL mode".into());
    }
    destination.close().map_err(|(_, error)| error)?;
    Ok(())
}

fn seal(path: &Path) -> BackupResult<()> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(path, flags)?;
    let busy: i64 = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| r.get(0))?;
    if busy != 0 {
        return Err("backup could not checkpoint the private copy".into());
    }
    connection.close().map_err(|(_, error)| error)?;
    for suffix in ["-wal", "-shm"] {
        let mut sidecar = path.as_os_str().to_owned();
        sidecar.push(suffix);
        match fs::symlink_metadata(sidecar) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
            Ok(_) => return Err("backup copy still has a SQLite sidecar".into()),
        }
    }
    Ok(())
}

fn fingerprint(name: &'static str, path: &Path) -> BackupResult<CopiedFile> {
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut hash = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    file.sync_all()?;
    Ok(CopiedFile {
        name,
        bytes,
        sha256: encode_hash(&hash.finalize()),
    })
}

fn encode_hash(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn writer(path: &Path) -> Connection {
        private_file(path).unwrap();
        let connection = Connection::open(path).unwrap();
        connection.execute_batch(
            "PRAGMA journal_mode=WAL; CREATE TABLE counter (n INTEGER NOT NULL); INSERT INTO counter VALUES (0)"
        ).unwrap();
        connection
    }

    #[test]
    fn backup_retries_mixed_snapshot_and_keeps_a_common_cut_while_writers_advance() {
        let dir = tempfile::tempdir().unwrap();
        let names = ["runtime.sqlite3", "product.sqlite3", "subagents.sqlite3"];
        let paths = names.map(|name| dir.path().join(name));
        let writers = paths.each_ref().map(|path| writer(path));
        let pairs = names
            .iter()
            .zip(&paths)
            .map(|(name, path)| open_pair(name, path).unwrap())
            .collect::<Vec<_>>();
        let advance = |n| {
            for writer in &writers {
                assert_eq!(writer.execute("UPDATE counter SET n=?1", [n]).unwrap(), 1);
            }
        };
        let mut acquisitions = 0;
        pin_common_snapshot(&pairs, || {
            acquisitions += 1;
            if acquisitions == 1 {
                advance(1);
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(acquisitions, 2);
        advance(2);
        for (name, pair) in names.iter().zip(&pairs) {
            let path = dir.path().join(format!("backup-{name}"));
            copy(
                &pair.snapshot,
                &path,
                Instant::now() + Duration::from_secs(5),
            )
            .unwrap();
            let connection =
                Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
            assert_eq!(
                connection
                    .query_row("SELECT n FROM counter", [], |r| r.get::<_, u32>(0))
                    .unwrap(),
                1
            );
        }
    }

    #[test]
    fn backup_publishes_verified_private_files_and_refuses_alias_and_replacement() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let _runtime = plakous::sessions::SessionBackend::sqlite(paths.runtime_database()).unwrap();
        let _product = crate::store::ProductStore::open(&paths).unwrap();
        let _tree = plakous_recursive_subagents_store::database::SubagentsDatabase::open(
            &paths.subagents_database(),
        )
        .unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(paths.home(), &alias).unwrap();
        assert!(run(&paths, &alias.join("backup"), &mut Vec::new()).is_err());
        let target = root.path().join("backup");
        let mut out = Vec::new();
        run(&paths, &target, &mut out).unwrap();
        assert_eq!(fs::read(target.join("manifest.json")).unwrap(), out);
        let manifest: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(manifest["files"].as_array().unwrap().len(), 3);
        assert_eq!(fs::read_dir(&target).unwrap().count(), 4);
        assert_eq!(target.metadata().unwrap().mode() & 0o077, 0);
        for row in manifest["files"].as_array().unwrap() {
            let file = target.join(row["name"].as_str().unwrap());
            let bytes = fs::read(&file).unwrap();
            assert_eq!(row["sha256"], encode_hash(&Sha256::digest(&bytes)));
            assert_eq!(file.metadata().unwrap().mode() & 0o077, 0);
        }
        assert!(run(&paths, &target, &mut Vec::new()).is_err());
        assert_eq!(fs::read(target.join("manifest.json")).unwrap(), out);
    }
}
