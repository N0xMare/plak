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
use crate::{CliFailure, CliFailureKind, CliResult, ProcessOutcome};

const SNAPSHOT_TRIES: usize = 5;
const LOCK_WAIT: Duration = Duration::from_millis(200);
const COPY_BUDGET: Duration = Duration::from_secs(120);
type Cause = Box<dyn std::error::Error>;
type BackupResult<T> = Result<T, Refusal>;

#[derive(Debug)]
enum Refusal {
    Busy(&'static str),
    Destination(Cause),
    Source(Cause),
}

impl Refusal {
    fn destination(error: impl Into<Cause>) -> Self {
        Self::Destination(error.into())
    }

    fn source(error: impl Into<Cause>) -> Self {
        Self::Source(error.into())
    }

    fn sqlite(error: rusqlite::Error, fallback: fn(rusqlite::Error) -> Self) -> Self {
        match error.sqlite_error_code() {
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked) => {
                Self::Busy("SQLite is busy; retry the backup")
            }
            Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
                Self::source(error)
            }
            _ => fallback(error),
        }
    }

    fn into_cli(self) -> CliFailure {
        let (code, detail, kind) = match self {
            Self::Busy(reason) => (
                "store_backup_busy",
                reason.to_owned(),
                CliFailureKind::Retryable,
            ),
            Self::Destination(error) => (
                "store_backup_destination",
                error.to_string(),
                CliFailureKind::Internal,
            ),
            Self::Source(error) => (
                "store_backup_source",
                error.to_string(),
                CliFailureKind::Internal,
            ),
        };
        CliFailure::classified(kind, format!("{code}: {detail}"))
    }
}

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
    let manifest =
        publish(paths, directory, &sources(paths), COPY_BUDGET).map_err(Refusal::into_cli)?;
    out.write_all(&manifest)
        .map_err(|error| CliFailure::new(format!("backup published but stdout failed: {error}")))?;
    Ok(ProcessOutcome::Success)
}

fn sources(paths: &PlakPaths) -> [(&'static str, PathBuf); 3] {
    [
        ("runtime.sqlite3", paths.runtime_database()),
        ("product.sqlite3", paths.product_database()),
        ("subagents.sqlite3", paths.subagents_database()),
    ]
}

fn publish(
    paths: &PlakPaths,
    directory: &Path,
    sources: &[(&'static str, PathBuf); 3],
    budget: Duration,
) -> BackupResult<Vec<u8>> {
    let target = target_directory(paths, sources, directory)?;
    let stage = tempfile::Builder::new()
        .prefix(".plak-backup-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(target.parent().map_err(Refusal::destination)?)
        .map_err(Refusal::destination)?;
    {
        let pairs = sources
            .iter()
            .map(|(name, path)| open_pair(name, path))
            .collect::<BackupResult<Vec<_>>>()?;
        pin_common_snapshot(&pairs, || Ok(()))?;
        let deadline = Instant::now() + budget;
        for ((name, _), pair) in sources.iter().zip(&pairs) {
            copy(&pair.snapshot, &stage.path().join(name), deadline)?;
        }
    } // Release every read transaction before hashing or publishing.
    check_current_copies(stage.path())?;
    for (name, _) in sources {
        seal(&stage.path().join(name)).map_err(Refusal::destination)?;
    }
    let files = sources
        .iter()
        .map(|(name, _)| fingerprint(name, &stage.path().join(name)).map_err(Refusal::destination))
        .collect::<BackupResult<_>>()?;
    let manifest = Manifest {
        version: 1,
        state_key: crate::home::state::state_key(),
        files,
    };
    let mut bytes = serde_json::to_vec_pretty(&manifest).map_err(Refusal::destination)?;
    bytes.push(b'\n');
    publish_directory(&target, stage.path(), &bytes).map_err(Refusal::destination)?;
    Ok(bytes)
}

fn publish_directory(target: &Target, stage: &Path, manifest: &[u8]) -> io::Result<()> {
    let mut manifest_file = private_file(&stage.join("manifest.json"))?;
    manifest_file.write_all(manifest)?;
    manifest_file.sync_all()?;
    File::open(stage)?.sync_all()?;
    target.parent()?;
    let stage_name = stage
        .file_name()
        .ok_or_else(|| io::Error::other("backup staging name is missing"))?;
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
    })
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
        .ok_or_else(|| Refusal::destination("destination must name a new directory"))?;
    let parent = fs::canonicalize(
        target
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )
    .map_err(Refusal::destination)?;
    let target_path = parent.join(name);
    let home = fs::canonicalize(paths.home()).map_err(Refusal::source)?;
    let home_metadata = home.metadata().map_err(Refusal::source)?;
    let uid = rustix::process::geteuid().as_raw();
    if home_metadata.uid() != uid || home_metadata.mode() & 0o777 != 0o700 {
        return Err(Refusal::source(
            "PLAK_HOME must be a private directory you own",
        ));
    }
    if target_path.starts_with(&home) {
        return Err(Refusal::destination(
            "destination cannot be inside PLAK_HOME",
        ));
    }
    for (_, source) in sources {
        if let Some(source_dir) = source.parent() {
            let source_dir = fs::canonicalize(source_dir).map_err(Refusal::source)?;
            if !source_dir.starts_with(&home) {
                return Err(Refusal::source(
                    "source store directory must be inside PLAK_HOME",
                ));
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
    )
    .map_err(Refusal::destination)?;
    let parent_file = File::from(fd);
    let metadata = parent_file.metadata().map_err(Refusal::destination)?;
    if metadata.uid() != uid || metadata.mode() & 0o022 != 0 {
        return Err(Refusal::destination(
            "destination parent must be owned by you and not writable by group or others",
        ));
    }
    match fs::symlink_metadata(&target_path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(Refusal::destination(error)),
        Ok(_) => return Err(Refusal::destination("destination already exists")),
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
        .ok_or_else(|| Refusal::source("source directory is missing"))?
        .metadata()
        .map_err(Refusal::source)?;
    let metadata = fs::symlink_metadata(path).map_err(Refusal::source)?;
    let uid = rustix::process::geteuid().as_raw();
    // Product and tree privacy comes from their enclosing private directory.
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o022 != 0
        || parent.uid() != uid
        || parent.mode() & 0o022 != 0
    {
        return Err(Refusal::source(format!(
            "{name}: source entry and directory must be owned by you and not writable by others"
        )));
    }
    let open = || -> BackupResult<Connection> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
        .map_err(source_sql)?;
        connection.busy_timeout(LOCK_WAIT).map_err(source_sql)?;
        let mode: String = connection
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(source_sql)?;
        if mode != "wal" {
            return Err(Refusal::source(format!(
                "{name}: source must be an initialized WAL store"
            )));
        }
        Ok(connection)
    };
    Ok(Source {
        snapshot: open()?,
        observer: open()?,
    })
}

fn source_sql(error: rusqlite::Error) -> Refusal {
    Refusal::sqlite(error, Refusal::source)
}

fn destination_sql(error: rusqlite::Error) -> Refusal {
    Refusal::sqlite(error, Refusal::destination)
}

fn check_current_copies(path: &Path) -> BackupResult<()> {
    use plakous::sessions::{SessionBackend, SessionPersistenceShape};
    if !matches!(
        SessionBackend::classify_sqlite(path.join("runtime.sqlite3")).map_err(Refusal::source)?,
        SessionPersistenceShape::Current
    ) {
        return Err(Refusal::source(
            "runtime.sqlite3: source is not an initialized current runtime store",
        ));
    }
    let open = |name| {
        Connection::open_with_flags(
            path.join(name),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )
    };
    let product = open("product.sqlite3").map_err(source_sql)?;
    crate::store::ProductStore::admit_read_only(&product).map_err(Refusal::source)?;
    let tree = open("subagents.sqlite3").map_err(source_sql)?;
    plakous_recursive_subagents_store::database::schema()
        .require_current(&tree)
        .map_err(Refusal::source)?;
    quick_check(
        "runtime.sqlite3",
        &open("runtime.sqlite3").map_err(source_sql)?,
    )?;
    quick_check("product.sqlite3", &product)?;
    quick_check("subagents.sqlite3", &tree)?;
    Ok(())
}

fn quick_check(name: &str, connection: &Connection) -> BackupResult<()> {
    let mut command = connection
        .prepare("PRAGMA quick_check")
        .map_err(source_sql)?;
    let mut rows = command.query([]).map_err(source_sql)?;
    let ok = rows.next().map_err(source_sql)?.is_some_and(|row| {
        row.get_ref(0)
            .is_ok_and(|value| value.as_str().is_ok_and(|text| text == "ok"))
    });
    if !ok || rows.next().map_err(source_sql)?.is_some() {
        return Err(Refusal::source(format!(
            "{name}: staged copy failed SQLite quick_check"
        )));
    }
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
        let before = sources
            .iter()
            .map(version)
            .collect::<Result<Vec<_>, _>>()
            .map_err(source_sql)?;
        for (index, source) in sources.iter().enumerate() {
            source.snapshot.execute_batch("BEGIN").map_err(source_sql)?;
            source
                .snapshot
                .query_row("SELECT count(*) FROM sqlite_schema", [], |_| Ok(()))
                .map_err(source_sql)?;
            if index == 0 {
                after_first_pin()?;
            }
        }
        let after = sources
            .iter()
            .map(version)
            .collect::<Result<Vec<_>, _>>()
            .map_err(source_sql)?;
        if before == after {
            return Ok(());
        }
        for source in sources {
            source
                .snapshot
                .execute_batch("ROLLBACK")
                .map_err(source_sql)?;
        }
        if attempt + 1 < SNAPSHOT_TRIES {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Err(Refusal::Busy(
        "stores changed during five snapshot acquisitions; retry when writes are quieter",
    ))
}

fn private_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(path)
}

fn copy(source: &Connection, path: &Path, deadline: Instant) -> BackupResult<()> {
    private_file(path).map_err(Refusal::destination)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut destination = Connection::open_with_flags(path, flags).map_err(destination_sql)?;
    destination
        .busy_timeout(LOCK_WAIT)
        .map_err(destination_sql)?;
    {
        let backup = Backup::new(source, &mut destination).map_err(destination_sql)?;
        loop {
            if Instant::now() >= deadline {
                return Err(Refusal::Busy(
                    "SQLite copy exceeded the online backup time budget",
                ));
            }
            match backup.step(256).map_err(destination_sql)? {
                StepResult::Done => break,
                StepResult::More => (),
                StepResult::Busy | StepResult::Locked => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return Err(Refusal::destination("unsupported SQLite backup step")),
            }
        }
    }
    if Instant::now() >= deadline {
        return Err(Refusal::Busy(
            "SQLite copy exceeded the online backup time budget",
        ));
    }
    // Exact runtime open requires WAL mode, even though the backup must need
    // only the main file to restore.
    let journal: String = destination
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .map_err(destination_sql)?;
    if journal != "wal" {
        return Err(Refusal::destination("backup could not preserve WAL mode"));
    }
    destination
        .close()
        .map_err(|(_, error)| destination_sql(error))?;
    Ok(())
}

fn seal(path: &Path) -> Result<(), Cause> {
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

fn fingerprint(name: &'static str, path: &Path) -> io::Result<CopiedFile> {
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
    fn backup_refuses_direct_inside_writable_parent_and_nonprivate_home() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let assert_refusal = |target: &Path, code: &str| {
            let error = run(&paths, target, &mut Vec::new()).unwrap_err();
            assert_eq!(error.exit_code(), 1);
            assert!(error.message().starts_with(code), "{error}");
            assert!(!target.exists());
        };
        assert_refusal(&paths.home().join("backup"), "store_backup_destination:");
        let shared = root.path().join("shared");
        fs::create_dir(&shared).unwrap();
        for mode in [0o770, 0o707] {
            fs::set_permissions(&shared, fs::Permissions::from_mode(mode)).unwrap();
            assert_refusal(&shared.join("backup"), "store_backup_destination:");
        }
        fs::set_permissions(paths.home(), fs::Permissions::from_mode(0o750)).unwrap();
        assert_refusal(&root.path().join("backup"), "store_backup_source:");
    }

    #[test]
    fn five_unstable_acquisitions_refuse_and_unpin_the_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("fixture.sqlite3");
        let writer = writer(&path);
        let pairs = [open_pair("fixture", &path).unwrap()];
        let mut acquisitions = 0;
        let error = pin_common_snapshot(&pairs, || {
            acquisitions += 1;
            writer.execute("UPDATE counter SET n=n+1", []).unwrap();
            Ok(())
        })
        .unwrap_err();
        assert_eq!(acquisitions, SNAPSHOT_TRIES);
        assert!(pairs[0].snapshot.is_autocommit());
        assert_eq!(error.into_cli().exit_code(), 8);
    }

    #[test]
    fn backup_copy_budget_refuses_without_publishing() {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let sources = sources(&paths);
        let _writers = sources.each_ref().map(|(_, path)| writer(path));
        let target = root.path().join("backup");
        let error = publish(&paths, &target, &sources, Duration::ZERO)
            .unwrap_err()
            .into_cli();
        assert_eq!(error.exit_code(), 8);
        assert!(error.message().contains("copy exceeded"));
        assert!(!target.exists());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn quick_check_refuses_an_invalid_staged_database() {
        let staged = Connection::open_in_memory().unwrap();
        staged.execute_batch(
            "CREATE TABLE fixture(n INTEGER CHECK(n > 0));+             PRAGMA ignore_check_constraints=ON; INSERT INTO fixture VALUES(0);+             PRAGMA ignore_check_constraints=OFF;",
        ).unwrap();
        assert!(matches!(
            quick_check("fixture", &staged),
            Err(Refusal::Source(_))
        ));
    }

    #[tokio::test]
    async fn backup_restores_a_cold_home_using_the_real_stores() {
        use crate::store::sessions::SessionRecord;
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest,
            SESSION_HISTORY_VISIBILITY_REVISION, SessionBackend, SessionHistoryDirection,
            SessionHistoryPageOutcome, SessionHistoryPageRequest, SessionHistoryVisibility,
            SessionOperationId,
        };
        use plakous_offline_crash_harness::{
            BoundaryController, protocol::CrashBoundary, text_runtime_binding,
        };

        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::from_home(root.path().join("home"));
        paths.ensure_layout().unwrap();
        let sessions =
            AgentSessions::new(SessionBackend::sqlite(paths.runtime_database()).unwrap()).unwrap();
        let binding = text_runtime_binding(
            root.path(),
            BoundaryController::quiet_from_evidence_path(
                CrashBoundary::None,
                root.path().join("evidence"),
            )
            .unwrap(),
            "synthetic",
            "synthetic",
        )
        .unwrap();
        let mut record = SessionRecord::new(root.path().to_path_buf());
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new(&record.create_operation_id).unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("new backup fixture")
        };
        session
            .agent()
            .prompt("backup fixture".to_owned())
            .await
            .unwrap()
            .result()
            .await
            .unwrap();
        let locator = session.release().await.unwrap();
        record.bind_plakous_locator(locator).unwrap();
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        drop(sessions);
        let mut tree = plakous_hosted::topology_runtime::test_support::bootstrap(
            &record.id,
            root.path().to_path_buf(),
        );
        crate::store::tree::save_tree(&paths, &mut tree).unwrap();
        assert_cold_sources(&paths);
        let target = root.path().join("backup");
        run(&paths, &target, &mut Vec::new()).unwrap();
        let restored = PlakPaths::from_home(root.path().join("restored"));
        restored.ensure_layout().unwrap();
        for (name, path) in sources(&restored) {
            fs::copy(target.join(name), path).unwrap();
        }
        let copy = crate::store::sessions::load_session(&restored, &record.id).unwrap();
        assert_eq!(copy.plakous_locator, Some(locator));
        assert!(
            crate::store::tree::load_tree(&restored, &record.id)
                .unwrap()
                .is_some()
        );
        let sessions =
            AgentSessions::new(SessionBackend::sqlite(restored.runtime_database()).unwrap())
                .unwrap();
        let SessionHistoryPageOutcome::Page { items, .. } = sessions
            .history_page(SessionHistoryPageRequest::new(
                locator,
                locator.checkpoint_id(),
                SessionHistoryVisibility::Content,
                SESSION_HISTORY_VISIBILITY_REVISION,
                SessionHistoryDirection::Forward,
                32,
                4096,
                None,
            ))
            .await
            .unwrap()
        else {
            panic!("restored history page")
        };
        assert!(
            items
                .iter()
                .any(|item| item.display_text() == Some("backup fixture"))
        );
    }

    fn assert_cold_sources(paths: &PlakPaths) {
        for (_, path) in sources(paths) {
            for suffix in ["-wal", "-shm"] {
                let mut sidecar = path.as_os_str().to_owned();
                sidecar.push(suffix);
                assert!(!Path::new(&sidecar).exists(), "source must be cold");
            }
        }
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
