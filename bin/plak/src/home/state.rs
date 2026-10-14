//! One state directory per persisted-format key.
//!
//! Every database and session log this build writes lives under
//! `state/<key>/`, where the key names the runtime schema, the runtime format
//! epoch, the product schema and the agent-tree schema. A build that reads a
//! different format never opens another build's files as session authority:
//! it starts fresh in its own directory and names the others once. Explicit
//! legacy pruning uses schema-blind lock probes; nothing is migrated, moved
//! or deleted automatically.

use std::fs;
use std::io::{self, BufRead as _, Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, Instant};

use plakous::sessions::{AgentSessionsErrorKind, SessionBackend};

use crate::home::paths::PlakPaths;

/// This build's state directory name. Derived, never hand-bumped: a change to
/// any persisted format moves the key with it.
#[must_use]
pub fn state_key() -> String {
    format!(
        "r{}.e{}.p{}.t{}",
        plakous::sessions::RUNTIME_STORE_SCHEMA_VERSION,
        plakous::sessions::RUNTIME_FORMAT_EPOCH,
        crate::store::SCHEMA_VERSION,
        plakous_recursive_subagents_store::SCHEMA_VERSION,
    )
}

/// Creates this build's state directory and, once per process, names whatever
/// else `state/` holds. A runtime store in this build's own directory that
/// still does not open is refused; that can only be a damaged or hand-placed
/// file.
pub fn ensure_usable_state(paths: &PlakPaths, notice: &mut dyn Write) -> Result<(), String> {
    static REPORTED: Once = Once::new();
    ensure_usable_state_with_notice(paths, notice, &REPORTED)
}

fn ensure_usable_state_with_notice(
    paths: &PlakPaths,
    notice: &mut dyn Write,
    reported: &Once,
) -> Result<(), String> {
    paths.ensure_private_state_home()?;
    paths.ensure_layout()?;
    report_other_state_once(paths, notice, reported);
    if let Err(error) = SessionBackend::classify_sqlite(paths.runtime_database())
        && error.kind() == AgentSessionsErrorKind::IncompatibleBuild
    {
        return Err(format!(
            "{} was created by an incompatible plak build; move it aside to start fresh",
            paths.runtime_database().display()
        ));
    }
    Ok(())
}

fn report_other_state_once(paths: &PlakPaths, notice: &mut dyn Write, reported: &Once) {
    report_other_state_once_until(paths, notice, reported, None);
}

fn report_other_state_once_until(
    paths: &PlakPaths,
    notice: &mut dyn Write,
    reported: &Once,
    deadline: Option<Instant>,
) {
    reported.call_once(|| {
        let deadline = deadline.unwrap_or_else(|| Instant::now() + notice_size_budget());
        let entries = match legacy_state_until(paths, Some(deadline)) {
            Ok(entries) if entries.is_empty() => return,
            Ok(entries) => entries,
            Err(error) => {
                let _ = writeln!(notice, "plak: legacy state size unknown: {error}; inspect with `plak doctor`");
                return;
            }
        };
        let size = entries.iter().try_fold(0_u64, |bytes, entry| {
            entry.bytes.as_ref().ok().map(|size| bytes.saturating_add(*size))
        }).map_or_else(|| "size unknown".to_owned(), |bytes| format!("{bytes} bytes"));
        let noun = if entries.len() == 1 { "entry" } else { "entries" };
        let verb = if entries.len() == 1 { "remains" } else { "remain" };
        let _ = writeln!(
            notice,
            "plak: {} legacy state {noun} ({size}) {verb} in {}; this build starts fresh in {}; inspect with `plak doctor`, remove explicitly with `plak state prune-legacy`",
            entries.len(), paths.state_root().display(), paths.state_dir().display()
        );
    });
}

/// How long the launch notice may spend sizing legacy entries before it says
/// `size unknown`: admission never waits on a large or slow legacy tree.
const NOTICE_SIZE_BUDGET: Duration = Duration::from_millis(20);

fn notice_size_budget() -> Duration {
    #[cfg(test)]
    if let Some(budget) = tests::notice_size_budget() {
        return budget;
    }
    NOTICE_SIZE_BUDGET
}

/// Everything under `state/` that is not this build's directory, sorted.
pub(super) fn other_state(paths: &PlakPaths) -> Vec<PathBuf> {
    let current = paths.state_dir();
    let Ok(entries) = fs::read_dir(paths.state_root()) else {
        return Vec::new();
    };
    let mut others = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| *path != current)
        .collect::<Vec<_>>();
    others.sort();
    others
}

/// Non-current entries and their retained file sizes, without following links.
struct LegacyEntry {
    path: PathBuf,
    bytes: io::Result<u64>,
}

fn legacy_state_until(
    paths: &PlakPaths,
    deadline: Option<Instant>,
) -> io::Result<Vec<LegacyEntry>> {
    match fs::symlink_metadata(paths.state_root()) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(io::Error::other("state root is not a direct directory"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    let entries = match fs::read_dir(paths.state_root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut result = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let path = entry.path();
        if path == paths.state_dir() {
            continue;
        }
        result.push(LegacyEntry {
            bytes: file_bytes(&path, deadline),
            path,
        });
    }
    result.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(result)
}

fn check_size_deadline(deadline: Option<Instant>) -> io::Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "size inspection budget exhausted",
        ));
    }
    Ok(())
}

fn file_bytes(path: &Path, deadline: Option<Instant>) -> io::Result<u64> {
    check_size_deadline(deadline)?;
    #[cfg(test)]
    tests::before_size(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(metadata.len());
    }
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut bytes = 0_u64;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        bytes = bytes.saturating_add(file_bytes(&entry.path(), deadline)?);
    }
    Ok(bytes)
}

pub(crate) fn report_legacy_state(paths: &PlakPaths, output: &mut dyn Write) -> io::Result<usize> {
    report_legacy_state_until(paths, output, Some(Instant::now() + Duration::from_secs(2)))
}

pub(super) fn report_legacy_state_until(
    paths: &PlakPaths,
    output: &mut dyn Write,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    let mut warnings = 0;
    let entries = match legacy_state_until(paths, deadline) {
        Ok(entries) => entries,
        Err(error) => {
            writeln!(output, "Legacy state: WARNING · size unavailable: {error}")?;
            warnings += 1;
            Vec::new()
        }
    };
    for entry in entries {
        let size = match entry.bytes {
            Ok(bytes) => format!("{bytes} bytes"),
            Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                format!("size unavailable: {error}")
            }
            Err(error) => {
                warnings += 1;
                format!("WARNING · size unavailable: {error}")
            }
        };
        writeln!(
            output,
            "Legacy state: {} · {}",
            crate::cli_output::safe_terminal_block(&entry.path.display().to_string()),
            crate::cli_output::safe_terminal_block(&size)
        )?;
    }
    let (unowned, incomplete) = super::state_artifacts::unowned_worktrees(paths);
    if incomplete {
        warnings += 1;
        writeln!(
            output,
            "Unowned worktrees: WARNING · ownership inventory unavailable; unowned list may be incomplete or overstated; verify ownership before manual removal"
        )?;
    }
    for path in unowned {
        let size = file_bytes(&path, deadline).map_or_else(
            |error| format!("size unavailable: {error}"),
            |bytes| format!("{bytes} bytes"),
        );
        writeln!(
            output,
            "unowned worktree: {} · {} · informational; retained",
            crate::cli_output::safe_terminal_block(&path.display().to_string()),
            crate::cli_output::safe_terminal_block(&size)
        )?;
    }
    Ok(warnings)
}

/// Explicit removal only. Maintenance reads physical ownership columns, not
/// foreign session authority; unsupported inventories are retained.
#[derive(PartialEq, Eq)]
pub(crate) struct PruneInventory {
    removals: Vec<PathBuf>,
    artifacts: Vec<super::state_artifacts::Artifacts>,
    carried: std::collections::BTreeMap<String, super::state_artifacts::Artifacts>,
    options: PruneOptions,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PruneOptions {
    pub(crate) forget_missing_launches: bool,
    pub(crate) prune_orphaned_references: bool,
}

struct LockedPruneInventory {
    inventory: PruneInventory,
    holders: Vec<fs::File>,
    databases: Vec<(PathBuf, rusqlite::Connection)>,
    sidecars: Vec<PathBuf>,
    references: Vec<PathBuf>,
}

fn build_prune_inventory(
    paths: &PlakPaths,
    options: PruneOptions,
) -> Result<LockedPruneInventory, String> {
    paths.ensure_private_state_home()?;
    if let Ok(metadata) = fs::symlink_metadata(paths.state_root()) {
        use std::os::unix::fs::MetadataExt as _;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            return Err("refusing unsafe state root".to_owned());
        }
    }
    let mut entries = match fs::read_dir(paths.state_root()) {
        Ok(entries) => entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.to_string()),
    };
    entries.retain(|path| *path != paths.state_dir());
    entries.sort();
    let mut removals = Vec::new();
    let mut sidecars = Vec::new();
    let mut holders = Vec::new();
    let mut databases = Vec::new();
    for path in &entries {
        // Last-close checkpointing can remove flat WALs between the phases.
        // Sidecars still receive the same safety checks and removal locks.
        let database = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| {
                name.strip_suffix("-wal")
                    .or_else(|| name.strip_suffix("-shm"))
                    .or_else(|| name.strip_suffix("-journal"))
                    .filter(|name| name.ends_with(".sqlite3"))
                    .map(|name| path.with_file_name(name))
            });
        if database.is_some_and(|database| entries.binary_search(&database).is_ok()) {
            sidecars.push(path.clone());
        } else {
            removals.push(path.clone());
        }
    }
    // Validate supplied sidecars before SQLite can open them.
    for path in sidecars.iter().chain(&removals) {
        probe_legacy_locks(path, &mut holders, &mut databases)?;
    }
    if paths.state_dir().exists() {
        probe_state_locks(
            &paths.state_dir(),
            &mut holders,
            &mut databases,
            Some(&paths.job_outputs()),
        )?;
    }
    let sessions = super::state_artifacts::sessions(&databases, paths)?;
    let (artifacts, carried) = super::state_artifacts::inventory(paths, &databases, &sessions)?;
    let references = if options.prune_orphaned_references {
        orphaned_references(paths, &sessions)?
    } else {
        Vec::new()
    };
    for path in &references {
        probe_legacy_locks(path, &mut holders, &mut databases)?;
        removals.push(path.clone());
    }
    removals.sort();
    Ok(LockedPruneInventory {
        inventory: PruneInventory {
            removals,
            artifacts,
            carried,
            options,
        },
        holders,
        databases,
        sidecars,
        references,
    })
}

#[derive(Default)]
pub(crate) struct PruneCounts {
    pub(crate) state_entries: usize,
    pub(crate) artifact_groups: usize,
    pub(crate) forgotten_groups: usize,
    pub(crate) kept_entries: usize,
    pub(crate) kept_groups: usize,
}

#[cfg(test)]
pub(crate) fn prune_legacy(paths: &PlakPaths) -> Result<usize, String> {
    let count = prune_legacy_checked(paths, None, &mut io::sink())?;
    Ok(count.state_entries + count.artifact_groups)
}

#[cfg(test)]
pub(crate) fn prune_confirmed(
    paths: &PlakPaths,
    expected: &PruneInventory,
) -> Result<usize, String> {
    let count = prune_confirmed_with_report(paths, expected, &mut io::sink())?;
    Ok(count.state_entries + count.artifact_groups)
}

pub(crate) fn prune_confirmed_with_report(
    paths: &PlakPaths,
    expected: &PruneInventory,
    stderr: &mut dyn Write,
) -> Result<PruneCounts, String> {
    prune_legacy_checked(paths, Some(expected), stderr)
}

fn prune_legacy_checked(
    paths: &PlakPaths,
    expected: Option<&PruneInventory>,
    stderr: &mut dyn Write,
) -> Result<PruneCounts, String> {
    let LockedPruneInventory {
        inventory,
        holders,
        mut databases,
        sidecars,
        references: _,
    } = build_prune_inventory(
        paths,
        expected.map_or_else(PruneOptions::default, |expected| expected.options),
    )?;
    let PruneInventory {
        removals,
        artifacts,
        carried,
        ..
    } = inventory;
    if let Some(expected) = expected {
        let component = if expected.removals != removals {
            Some("removals")
        } else if expected.artifacts != artifacts {
            Some("artifacts")
        } else if expected.carried != carried {
            Some("carried")
        } else {
            None
        };
        if let Some(component) = component {
            return Err(format!(
                "prune inventory changed since confirmation ({component}); nothing removed; run prune-legacy again"
            ));
        }
    }
    // Validate all potential retained owners before even repository removal.
    ensure_owners_covered(
        &artifacts.iter().collect::<Vec<_>>(),
        &removals,
        &paths.state_dir(),
    )?;
    // Re-inspect under the removal locks. Retaining a group also retains
    // state inventory so a future invocation can account for it again.
    // Preserve carried source ownership before its old state can disappear.
    for (id, artifact) in carried {
        super::state_artifacts::remember_artifacts(paths, &id, artifact)?;
    }
    let mut count = PruneCounts::default();
    let mut retained = Vec::new();
    let mut forgotten = Vec::new();
    for artifact in &artifacts {
        if expected.is_some_and(|expected| expected.options.forget_missing_launches)
            && artifact.launch_missing()
        {
            forgotten.push(artifact);
            continue;
        }
        let result = artifact.check_clean().and_then(|()| artifact.remove());
        match result {
            Ok(()) => count.artifact_groups += 1,
            Err(error) => {
                retained.push(artifact);
                writeln!(
                    stderr,
                    "Retained artifact group: {} · {}",
                    artifact.label(),
                    crate::cli_output::safe_terminal_block(&error)
                )
                .map_err(|error| error.to_string())?;
            }
        }
    }
    count.kept_groups = retained.len();
    let retention_reasons = retained
        .iter()
        .map(|artifact| artifact.kept_reason())
        .collect::<Vec<_>>();
    let mut kept = std::collections::BTreeSet::new();
    for path in removals.iter().chain(&sidecars) {
        let owners = retained
            .iter()
            .zip(&retention_reasons)
            .filter(|(artifact, _)| artifact.owns_entry(path))
            .collect::<Vec<_>>();
        if owners.is_empty() {
            continue;
        }
        kept.insert(path.clone());
        writeln!(
            stderr,
            "kept: {} · {}",
            crate::cli_output::safe_terminal_block(&path.display().to_string()),
            owners
                .iter()
                .map(|(_, reason)| reason.as_str())
                .collect::<Vec<_>>()
                .join("; ")
        )
        .map_err(|error| error.to_string())?;
    }
    count.kept_entries = kept.len();
    let cleanup_sidecars = removals
        .iter()
        .filter(|path| {
            !kept.contains(*path)
                && path
                    .extension()
                    .is_some_and(|extension| extension == "sqlite3")
        })
        .flat_map(|path| {
            ["wal", "shm", "journal"].map(|suffix| path.with_extension(format!("sqlite3-{suffix}")))
        })
        .chain(
            sidecars
                .iter()
                .filter(|path| !kept.contains(*path))
                .cloned(),
        )
        .collect::<std::collections::BTreeSet<_>>();
    for path in removals.iter().filter(|path| !kept.contains(*path)) {
        #[cfg(test)]
        tests::before_remove(path)?;
        let removal = fs::symlink_metadata(path).and_then(|metadata| {
            if metadata.is_dir() {
                fs::remove_dir_all(path)
            } else {
                fs::remove_file(path)
            }
        });
        if let Err(error) = removal {
            return Err(format!("could not remove {}: {error}", path.display()));
        }
    }
    let retained_inventory = retained
        .iter()
        .flat_map(|artifact| artifact.owners().keys().cloned())
        .collect();
    super::state_artifacts::forget_deleted_inventory(paths, &databases, &retained_inventory)?;
    // Keep every fence through database/artifact removal. Last-close SQLite
    // cleanup can recreate flat WAL paths, so reclaim sidecars only afterwards.
    databases.retain(|(path, _)| path.starts_with(paths.state_dir()));
    drop(holders);
    #[cfg(test)]
    tests::before_sidecar_cleanup()?;
    for path in cleanup_sidecars {
        #[cfg(test)]
        tests::before_remove(&path)?;
        let database = path.with_extension("sqlite3");
        match fs::symlink_metadata(&database) {
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("could not inspect {}: {error}", database.display())),
        }
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
        {
            return Err(format!("could not remove {}: {error}", path.display()));
        }
    }
    count.state_entries = removals.len() + sidecars.len() - kept.len();
    for artifact in forgotten
        .into_iter()
        .filter(|artifact| !kept.iter().any(|path| artifact.owns_entry(path)))
    {
        count.forgotten_groups += 1;
        writeln!(
            stderr,
            "Forgotten ownership: {} · launch missing",
            artifact.label()
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(count)
}

pub(super) fn ensure_owners_covered(
    artifacts: &[&super::state_artifacts::Artifacts],
    entries: &[PathBuf],
    current: &Path,
) -> Result<(), String> {
    if artifacts.iter().any(|artifact| {
        artifact.owners().is_empty()
            || artifact.owners().keys().any(|owner| {
                !owner.starts_with(current) && !entries.iter().any(|entry| owner.starts_with(entry))
            })
    }) {
        return Err(
            "retained artifact ownership is not covered by kept state; nothing removed".to_owned(),
        );
    }
    Ok(())
}

fn orphaned_references(
    paths: &PlakPaths,
    sessions: &std::collections::BTreeSet<String>,
) -> Result<Vec<PathBuf>, String> {
    use std::os::unix::fs::MetadataExt as _;
    let dir = paths.home().join("references");
    let metadata = match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
        Ok(metadata) => metadata,
    };
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err("refusing unsafe references directory".to_owned());
    }
    let mut references = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|error| error.to_string())? {
        let path = entry.map_err(|error| error.to_string())?.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_suffix(".json"))
        else {
            continue;
        };
        if uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                // A concurrent session delete already removed it.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.to_string()),
            };
            references.push((id.to_owned(), path, metadata));
        }
    }
    let mut result = Vec::new();
    for (id, path, metadata) in references {
        if sessions.contains(&id) {
            continue;
        }
        if !metadata.is_file()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
        {
            return Err(format!(
                "refusing unsafe orphaned reference {}",
                path.display()
            ));
        }
        result.push(path);
    }
    result.sort();
    Ok(result)
}

fn probe_legacy_locks(
    path: &Path,
    holders: &mut Vec<fs::File>,
    databases: &mut Vec<(PathBuf, rusqlite::Connection)>,
) -> Result<(), String> {
    probe_state_locks(path, holders, databases, None)
}

/// Probes legacy state (`current_job_outputs: None`) or the current state
/// directory, whose live job output files are neither locked nor removed and
/// may vanish under a retention sweep, so their root is not walked.
fn probe_state_locks(
    path: &Path,
    holders: &mut Vec<fs::File>,
    databases: &mut Vec<(PathBuf, rusqlite::Connection)>,
    current_job_outputs: Option<&Path>,
) -> Result<(), String> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    #[cfg(test)]
    tests::before_probe(path)?;
    let legacy = current_job_outputs.is_none();
    if current_job_outputs == Some(path) {
        return Ok(());
    }
    let scope = if legacy { "legacy" } else { "current" };
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if (!metadata.is_dir() && !metadata.is_file())
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err(format!("refusing unsafe {scope} state {}", path.display()));
    }
    if metadata.is_dir() {
        // Current sessions protect current artifacts, not legacy state. Their
        // holder leases need not be acquired; product/tree inventory stays fenced.
        if !legacy
            && path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("-holders"))
        {
            return Ok(());
        }
        let mut entries = fs::read_dir(path)
            .map_err(|error| error.to_string())?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?;
        entries.sort();
        for path in entries {
            probe_state_locks(&path, holders, databases, current_job_outputs)?;
        }
        return Ok(());
    }
    if path
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name.to_string_lossy().ends_with("-holders"))
    {
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| format!("cannot prove legacy state is unused: {error}"))?;
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| format!("legacy state {} is in use (holder lock)", path.display()))?;
        let mut identity = String::new();
        file.read_to_string(&mut identity)
            .map_err(|error| error.to_string())?;
        let uname = rustix::system::uname();
        let host = uname
            .nodename()
            .to_str()
            .map_err(|error| error.to_string())?;
        if !identity.is_empty() && !identity.starts_with(&format!("plak-holder-v1 {host} ")) {
            return Err(format!(
                "cannot prove legacy holder {} is local and unused",
                path.display()
            ));
        }
        holders.push(file);
    }
    if path
        .extension()
        .is_some_and(|extension| extension == "sqlite3")
    {
        if !legacy
            && !path
                .file_name()
                .is_some_and(|name| name == "product.sqlite3" || name == "subagents.sqlite3")
        {
            return Ok(());
        }
        let connection = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        )
        .map_err(|error| {
            format!(
                "cannot lock {scope} database inventory {}: {error}",
                path.display()
            )
        })?;
        if legacy {
            // Retained inventories keep their WAL/SHM; explicit cleanup owns
            // sidecars only after their database has actually been removed.
            connection
                .set_db_config(
                    rusqlite::config::DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE,
                    true,
                )
                .map_err(|error| error.to_string())?;
        }
        connection
            .busy_timeout(if legacy {
                Duration::ZERO
            } else {
                Duration::from_millis(250)
            })
            .map_err(|error| error.to_string())?;
        connection
            // Current inventory must exclude writers until removal ends, so a
            // new session cannot claim artifacts after we checked membership.
            // Normal-mode IMMEDIATE coexists with idle WAL TUI connections.
            .execute_batch(if legacy {
                "PRAGMA locking_mode=EXCLUSIVE; BEGIN EXCLUSIVE;"
            } else {
                "BEGIN IMMEDIATE;"
            })
            .map_err(|error| {
                format!(
                    "{scope} state {} is in use or unreadable: {error}",
                    path.display()
                )
            })?;
        databases.push((path.to_path_buf(), connection));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn confirm_prune_legacy(
    paths: &PlakPaths,
    yes: bool,
    terminal: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> Result<PruneInventory, String> {
    confirm_prune_legacy_options(paths, yes, PruneOptions::default(), terminal, stdin, stderr)
}

pub(crate) fn confirm_prune_legacy_options(
    paths: &PlakPaths,
    yes: bool,
    options: PruneOptions,
    terminal: bool,
    stdin: &mut dyn io::BufRead,
    stderr: &mut dyn Write,
) -> Result<PruneInventory, String> {
    if !yes && !terminal {
        return Err(
            "state prune-legacy requires interactive confirmation or explicit --yes".to_owned(),
        );
    }
    let LockedPruneInventory {
        inventory,
        holders,
        databases,
        sidecars,
        references,
    } = build_prune_inventory(paths, options)?;
    let mut state_entries = inventory
        .removals
        .iter()
        .filter(|path| !references.contains(path))
        .chain(&sidecars)
        .collect::<Vec<_>>();
    state_entries.sort();
    for path in state_entries {
        let size = file_bytes(path, None).map_or_else(
            |error| format!("WARNING · size unavailable: {error}"),
            |bytes| format!("{bytes} bytes"),
        );
        writeln!(
            stderr,
            "Legacy state: {} · {}",
            crate::cli_output::safe_terminal_block(&path.display().to_string()),
            crate::cli_output::safe_terminal_block(&size)
        )
        .map_err(|_| "stderr failed".to_owned())?;
    }
    for artifact in &inventory.artifacts {
        artifact.report(stderr, options.forget_missing_launches)?;
    }
    let count = inventory.removals.len() - references.len() + sidecars.len();
    for path in &references {
        let size = file_bytes(path, None).map_or_else(
            |_| "size unknown".to_owned(),
            |bytes| format!("{bytes} bytes"),
        );
        writeln!(
            stderr,
            "Orphaned reference: {} · {size}",
            crate::cli_output::safe_terminal_block(&path.display().to_string())
        )
        .map_err(|_| "stderr failed".to_owned())?;
    }
    // Confirmation must not keep a running session blocked behind maintenance.
    drop(databases);
    drop(holders);
    if options.forget_missing_launches {
        writeln!(stderr, "Explicit --forget-missing-launches: forget ownership only for missing launches (ENOENT) whose parent directory exists, unless a retained group shares that inventory; no repository artifacts are removed for those groups.")
            .map_err(|error| error.to_string())?;
    }
    if yes {
        return Ok(inventory);
    }
    write!(stderr, "Delete {count} listed non-current state entries and their sessions, plus {} listed orphaned references and {} owned artifact group(s)? Unprunable groups, uncommitted changes and unadopted work are retained together with their ownership inventory. These may belong to older or newer builds. This cannot be undone. Continue? [y/N] ", references.len(), inventory.artifacts.len())
        .and_then(|()| stderr.flush()).map_err(|_| "stderr failed".to_owned())?;
    let mut answer = String::new();
    stdin
        .take(16)
        .read_line(&mut answer)
        .map_err(|_| "could not read confirmation".to_owned())?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(inventory)
    } else {
        Err("state prune-legacy confirmation declined".to_owned())
    }
}

/// The refusal for a session id this build's state does not hold but another
/// build's does: its log is the one trace this build can read without opening
/// another build's databases. It names the way across: the build that wrote
/// the session exports it, this one imports it.
#[must_use]
pub fn incompatible_session_refusal(paths: &PlakPaths, prefix: &str) -> Option<String> {
    foreign_session(paths, prefix).map(|(key, id)| {
        format!(
            "session {id} was created by an incompatible plak build (state {key}); carry it forward with that build's `plak session export {id}` and then `plak session import <file>`, or start a new session"
        )
    })
}

/// The `(state key, session id)` of the one session another build's state
/// logs under `prefix`, read from event-log names alone. The prefix must be
/// at least the shortest accepted id prefix and match exactly one session
/// across every other build; anything else names nothing.
#[must_use]
pub fn foreign_session(paths: &PlakPaths, prefix: &str) -> Option<(String, String)> {
    if prefix.chars().count() < crate::commands::session::MIN_SESSION_ID_PREFIX {
        return None;
    }
    let mut matches = other_state(paths).into_iter().flat_map(|root| {
        let key = root
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned);
        let mut ids = logged_sessions(&root, prefix);
        ids.extend(logged_sessions(&root.join("sessions"), prefix));
        ids.into_iter()
            .filter_map(move |id| key.clone().map(|key| (key, id)))
    });
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn logged_sessions(dir: &Path, prefix: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".events.jsonl")
                .filter(|id| id.starts_with(prefix))
                .map(str::to_owned)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    enum SizeFault {
        Unreadable,
        Vanish,
    }

    thread_local! {
        static SIZE_FAULT: std::cell::RefCell<Option<(PathBuf, SizeFault)>> = const { std::cell::RefCell::new(None) };
        static REMOVE_FAULT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
        static RECREATE_DATABASE: std::cell::RefCell<Option<(PathBuf, bool)>> = const { std::cell::RefCell::new(None) };
        static CHECK_CURRENT_FENCES: std::cell::RefCell<Vec<PathBuf>> = const { std::cell::RefCell::new(Vec::new()) };
        static PROBE_VANISH: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
        static NOTICE_SIZE_BUDGET: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn notice_size_budget() -> Option<Duration> {
        NOTICE_SIZE_BUDGET.with(std::cell::Cell::get)
    }

    /// Removes the armed path as the probe reaches it, as a concurrent job
    /// output retention sweep would.
    pub(super) fn before_probe(path: &Path) -> Result<(), String> {
        PROBE_VANISH.with(|vanish| {
            let mut vanish = vanish.borrow_mut();
            if vanish.as_deref() == Some(path) {
                fs::remove_file(vanish.take().unwrap()).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }

    #[test]
    fn legacy_cleanup_does_not_walk_current_job_output_files() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_home, paths, old) = fixture();
        let session = paths
            .job_outputs()
            .join("00000000-0000-0000-0000-0000000000b0");
        fs::create_dir_all(&session).unwrap();
        for directory in [paths.state_dir(), paths.job_outputs(), session.clone()] {
            fs::set_permissions(directory, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let file = session.join("b-p1-0123456789abcdef.log");
        fs::write(&file, b"output").unwrap();
        // A sweep deleting the file mid-walk must not fail the cleanup.
        PROBE_VANISH.with(|vanish| *vanish.borrow_mut() = Some(file.clone()));
        prune_legacy(&paths).unwrap();
        assert!(!old.exists());
        assert!(file.exists(), "the probe walked into the job output root");
        PROBE_VANISH.with(|vanish| *vanish.borrow_mut() = None);
    }

    pub(super) fn before_sidecar_cleanup() -> Result<(), String> {
        CHECK_CURRENT_FENCES.with(|paths| {
            for path in paths.borrow_mut().drain(..) {
                let connection = rusqlite::Connection::open(path).unwrap();
                connection.busy_timeout(Duration::ZERO).unwrap();
                assert!(connection.execute_batch("BEGIN IMMEDIATE").is_err());
            }
        });
        RECREATE_DATABASE.with(|path| {
            if let Some((path, symlink)) = path.borrow_mut().take() {
                if symlink {
                    std::os::unix::fs::symlink("missing", &path)
                        .map_err(|error| error.to_string())?;
                } else {
                    fs::write(&path, b"replacement").map_err(|error| error.to_string())?;
                }
                for suffix in ["wal", "shm", "journal"] {
                    fs::write(
                        path.with_extension(format!("sqlite3-{suffix}")),
                        b"replacement",
                    )
                    .map_err(|error| error.to_string())?;
                }
            }
            Ok(())
        })
    }

    #[test]
    fn legacy_sidecar_cleanup_preserves_recreated_database_sidecars() {
        for symlink in [false, true] {
            let (_home, paths, _old) = fixture();
            let database = paths.state_root().join("runtime.sqlite3");
            drop(rusqlite::Connection::open(&database).unwrap());
            RECREATE_DATABASE.with(|path| *path.borrow_mut() = Some((database.clone(), symlink)));
            prune_legacy(&paths).unwrap();
            for suffix in ["wal", "shm", "journal"] {
                assert_eq!(
                    fs::read(database.with_extension(format!("sqlite3-{suffix}"))).unwrap(),
                    b"replacement"
                );
            }
        }
    }

    #[test]
    fn legacy_cleanup_holds_current_fences_through_dotdot_home() {
        let (_home, paths, old) = fixture();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        rusqlite::Connection::open(paths.subagents_database()).unwrap()
            .execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT, launch_checkout TEXT);").unwrap();
        CHECK_CURRENT_FENCES.with(|fences| {
            *fences.borrow_mut() = vec![paths.product_database(), paths.subagents_database()];
        });
        fs::create_dir(paths.home().join("alias")).unwrap();
        let paths = PlakPaths::explicit(paths.home().join("alias/.."));
        prune_legacy(&paths).unwrap();
        assert!(!old.exists());
    }

    pub(super) fn before_remove(path: &Path) -> Result<(), String> {
        REMOVE_FAULT.with(|fault| {
            let mut fault = fault.borrow_mut();
            if fault.as_deref() == Some(path) {
                fs::remove_file(fault.take().unwrap()).map_err(|error| error.to_string())?;
            }
            Ok(())
        })
    }

    pub(super) fn before_size(path: &Path) -> io::Result<()> {
        SIZE_FAULT.with(|fault| {
            let mut fault = fault.borrow_mut();
            if fault.as_ref().is_none_or(|(target, _)| target != path) {
                return Ok(());
            }
            match fault.take().unwrap().1 {
                SizeFault::Unreadable => Err(io::Error::from(io::ErrorKind::PermissionDenied)),
                SizeFault::Vanish => fs::remove_file(path),
            }
        })
    }

    #[test]
    fn unreadable_and_vanishing_entries_do_not_fail_stateful_admission_or_doctor() {
        // The injected fault, not the 20 ms launch budget, decides each size:
        // on a loaded host the budget alone can expire before the entry is
        // reached, which `exhausted_notice_budget_keeps_entry_count_and_prune_hint`
        // covers on its own.
        NOTICE_SIZE_BUDGET.with(|budget| budget.set(Some(Duration::from_secs(60))));
        for doctor in [false, true] {
            for unreadable in [false, true] {
                let (_root, paths, old) = fixture();
                let entry = old.join("changing");
                fs::write(&entry, b"bytes").unwrap();
                SIZE_FAULT.with(|fault| {
                    *fault.borrow_mut() = Some((
                        entry,
                        if unreadable {
                            SizeFault::Unreadable
                        } else {
                            SizeFault::Vanish
                        },
                    ));
                });
                let mut output = Vec::new();
                if doctor {
                    let warnings = report_legacy_state_until(&paths, &mut output, None).unwrap();
                    assert_eq!(warnings, usize::from(unreadable));
                } else {
                    ensure_usable_state_with_notice(&paths, &mut output, &Once::new()).unwrap();
                    assert!(paths.state_dir().exists());
                }
                let text = String::from_utf8(output).unwrap();
                assert!(
                    text.contains(if unreadable {
                        if doctor {
                            "size unavailable:"
                        } else {
                            "size unknown"
                        }
                    } else {
                        "0 bytes"
                    }),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn notice_inspects_once_and_ignores_output_failures() {
        let (_root, paths, old) = fixture();
        let once = Once::new();
        let mut output = Vec::new();
        ensure_usable_state_with_notice(&paths, &mut output, &once).unwrap();
        let entry = old.join("must-not-be-inspected");
        fs::write(&entry, b"keep").unwrap();
        SIZE_FAULT.with(|fault| *fault.borrow_mut() = Some((entry.clone(), SizeFault::Vanish)));
        ensure_usable_state_with_notice(&paths, &mut output, &once).unwrap();
        assert!(entry.exists());
        SIZE_FAULT.with(|fault| *fault.borrow_mut() = None);
        let mut full = &mut [][..];
        ensure_usable_state_with_notice(&paths, &mut full, &Once::new()).unwrap();
    }

    #[test]
    fn exhausted_startup_size_budget_returns_unknown_without_walking() {
        let (_root, _paths, old) = fixture();
        assert_eq!(
            file_bytes(&old, Some(Instant::now())).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[test]
    fn exhausted_notice_budget_keeps_entry_count_and_prune_hint() {
        let (_root, paths, _old) = fixture();
        fs::write(paths.state_root().join("flat"), b"legacy").unwrap();
        let deadline = Instant::now();
        let entries = legacy_state_until(&paths, Some(deadline)).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .all(|entry| entry.bytes.as_ref().unwrap_err().kind() == io::ErrorKind::TimedOut)
        );
        let mut notice = Vec::new();
        report_other_state_once_until(&paths, &mut notice, &Once::new(), Some(deadline));
        let text = String::from_utf8(notice).unwrap();
        assert!(
            text.contains("2 legacy state entries (size unknown) remain"),
            "{text}"
        );
        assert!(text.contains("`plak state prune-legacy`"), "{text}");
    }

    #[test]
    fn prune_lists_and_removes_only_references_absent_from_the_current_store() {
        let (_root, paths, old) = fixture();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        let dir = paths.home().join("references");
        fs::create_dir(&dir).unwrap();
        let current = dir.join(format!("{}.json", record.id));
        let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        fs::write(&current, b"current reference").unwrap();
        fs::write(&orphan, b"orphan").unwrap();
        let unrelated = dir.join("not-a-session.json");
        fs::write(&unrelated, b"unrelated").unwrap();
        let mut output = Vec::new();
        let inventory = confirm_prune_legacy_options(
            &paths,
            false,
            PruneOptions {
                forget_missing_launches: false,
                prune_orphaned_references: true,
            },
            true,
            &mut &b"yes\n"[..],
            &mut output,
        )
        .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains(&format!(
                "Orphaned reference: {} · 6 bytes",
                orphan.display()
            )),
            "{text}"
        );
        assert!(!text.contains(&current.display().to_string()));
        assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 2);
        assert!(!old.exists());
        assert!(!orphan.exists());
        assert!(current.exists());
        assert!(unrelated.exists());
    }

    #[test]
    fn unsafe_orphaned_references_refuse_before_any_removal() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let (root, paths, old) = fixture();
        let dir = paths.home().join("references");
        fs::create_dir(&dir).unwrap();
        let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
        let outside = root.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &orphan).unwrap();
        assert!(
            confirm_prune_legacy_options(
                &paths,
                true,
                PruneOptions {
                    forget_missing_launches: false,
                    prune_orphaned_references: true
                },
                false,
                &mut &b""[..],
                &mut Vec::new()
            )
            .err()
            .unwrap()
            .contains("unsafe orphaned reference")
        );
        assert!(old.exists());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
        fs::remove_file(&orphan).unwrap();
        fs::write(&orphan, b"archive").unwrap();
        fs::set_permissions(&orphan, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(
            confirm_prune_legacy_options(
                &paths,
                true,
                PruneOptions {
                    forget_missing_launches: false,
                    prune_orphaned_references: true
                },
                false,
                &mut &b""[..],
                &mut Vec::new()
            )
            .err()
            .unwrap()
            .contains("unsafe orphaned reference")
        );
        assert!(old.exists());
        assert!(orphan.exists());
    }

    #[test]
    fn prune_keeps_current_sessions_source_references_even_when_explicitly_selected() {
        for selected in [false, true] {
            let (_root, paths, old) = fixture();
            let source = uuid::Uuid::new_v4().to_string();
            let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
            record.link_carried_forward(&source, "fixture-previous-build");
            crate::store::sessions::save_session(&paths, &mut record).unwrap();
            let dir = paths.home().join("references");
            fs::create_dir(&dir).unwrap();
            let source_reference = dir.join(format!("{source}.json"));
            let current_reference = dir.join(format!("{}.json", record.id));
            let orphan = dir.join(format!("{}.json", uuid::Uuid::new_v4()));
            for path in [&source_reference, &current_reference, &orphan] {
                fs::write(path, b"fixture export").unwrap();
            }
            let mut output = Vec::new();
            let inventory = confirm_prune_legacy_options(
                &paths,
                true,
                PruneOptions {
                    prune_orphaned_references: selected,
                    ..PruneOptions::default()
                },
                false,
                &mut &b""[..],
                &mut output,
            )
            .unwrap();
            prune_confirmed(&paths, &inventory).unwrap();
            assert!(
                source_reference.exists(),
                "a current session's source is not an orphan"
            );
            assert!(current_reference.exists());
            assert_eq!(orphan.exists(), !selected);
            assert!(!old.exists());
            let output = String::from_utf8(output).unwrap();
            assert!(!output.contains(&source_reference.display().to_string()));
            assert!(!output.contains(&current_reference.display().to_string()));
        }
    }

    #[test]
    fn prune_confirmation_lists_exactly_its_fenced_state_inventory() {
        let (_root, paths, _old) = fixture();
        let vanished = paths.state_root().join("vanishing");
        fs::write(&vanished, b"fixture state").unwrap();
        SIZE_FAULT.with(|fault| *fault.borrow_mut() = Some((vanished.clone(), SizeFault::Vanish)));
        let mut output = Vec::new();
        let inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        let rows = output
            .lines()
            .filter(|line| line.starts_with("Legacy state: "))
            .collect::<Vec<_>>();
        let expected = inventory
            .removals
            .iter()
            .map(|path| format!("Legacy state: {} · 0 bytes", path.display()))
            .collect::<Vec<_>>();
        assert_eq!(rows, expected);
        assert!(inventory.removals.contains(&vanished));
        assert!(
            prune_confirmed(&paths, &inventory)
                .unwrap_err()
                .contains("(removals)")
        );
    }

    #[test]
    fn flat_sqlite_journal_is_a_transient_prune_sidecar() {
        let (_root, paths, _old) = fixture();
        let database = paths.state_root().join("runtime.sqlite3");
        drop(rusqlite::Connection::open(&database).unwrap());
        let journal = database.with_extension("sqlite3-journal");
        fs::write(&journal, b"").unwrap();
        let locked = build_prune_inventory(&paths, PruneOptions::default()).unwrap();
        assert!(locked.sidecars.contains(&journal));
        assert!(!locked.inventory.removals.contains(&journal));
        let LockedPruneInventory {
            inventory,
            databases,
            holders,
            ..
        } = locked;
        drop(databases);
        drop(holders);
        if journal.try_exists().unwrap() {
            fs::remove_file(&journal).unwrap();
        }
        prune_confirmed(&paths, &inventory).unwrap();
        assert!(!database.exists());
    }

    #[test]
    fn prune_accepts_sidecar_cleanup_after_fenced_inventory() {
        for attached in [true, false] {
            let (_root, paths, old) = fixture();
            let database = paths.state_root().join("legacy.sqlite3");
            if attached {
                drop(rusqlite::Connection::open(&database).unwrap());
            }
            let sidecar = database.with_extension("sqlite3-shm");
            fs::write(&sidecar, b"").unwrap();
            REMOVE_FAULT.with(|fault| *fault.borrow_mut() = Some(sidecar.clone()));
            let result = prune_legacy(&paths);
            REMOVE_FAULT.with(|fault| *fault.borrow_mut() = None);
            assert_eq!(result.is_ok(), attached, "sidecar removal: {result:?}");
            assert!(!database.exists());
            assert!(!sidecar.exists());
            assert_eq!(old.exists(), !attached);
        }
    }

    fn fixture() -> (tempfile::TempDir, PlakPaths, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(root.path().join("home"));
        paths.ensure_private_state_home().unwrap();
        let old = paths.state_root().join("r1.e1.p1.t1");
        fs::create_dir_all(&old).unwrap();
        (root, paths, old)
    }

    #[test]
    fn prune_inventory_refusals_name_only_the_changed_component() {
        for component in ["removals", "artifacts", "carried"] {
            let (_root, paths, old) = fixture();
            let mut inventory =
                confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
            match component {
                "removals" => inventory.removals.push(paths.state_root().join("changed")),
                "artifacts" => inventory
                    .artifacts
                    .push(super::super::state_artifacts::Artifacts::default()),
                "carried" => {
                    inventory.carried.insert(
                        "fixture source".to_owned(),
                        super::super::state_artifacts::Artifacts::default(),
                    );
                }
                _ => unreachable!(),
            }
            assert_eq!(
                prune_confirmed(&paths, &inventory).unwrap_err(),
                format!(
                    "prune inventory changed since confirmation ({component}); nothing removed; run prune-legacy again"
                )
            );
            assert!(old.exists());
        }
    }

    #[test]
    fn standalone_sqlite_sidecars_remain_prune_inventory_targets() {
        for suffix in ["wal", "shm", "journal"] {
            let (_root, paths, old) = fixture();
            let sidecar = paths.state_root().join(format!("absent.sqlite3-{suffix}"));
            fs::write(&sidecar, b"standalone state").unwrap();
            let inventory =
                confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
            assert!(inventory.removals.contains(&sidecar));
            fs::remove_file(&sidecar).unwrap();
            assert!(
                prune_confirmed(&paths, &inventory)
                    .unwrap_err()
                    .contains("(removals)")
            );
            assert!(old.exists());
        }
    }

    #[test]
    fn unsafe_flat_database_sidecars_refuse_prune_before_sqlite_opens_them() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        for (suffix, linked) in ["wal", "shm", "journal"]
            .into_iter()
            .flat_map(|suffix| [true, false].map(|linked| (suffix, linked)))
        {
            let (root, paths, old) = fixture();
            let database = paths.state_root().join("runtime.sqlite3");
            drop(rusqlite::Connection::open(&database).unwrap());
            let sidecar = database.with_extension(format!("sqlite3-{suffix}"));
            let outside = root.path().join("outside");
            fs::write(&outside, b"keep").unwrap();
            if linked {
                symlink(&outside, &sidecar).unwrap();
            } else {
                fs::write(&sidecar, b"unsafe").unwrap();
                fs::set_permissions(&sidecar, fs::Permissions::from_mode(0o666)).unwrap();
            }
            let error = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new())
                .err()
                .unwrap();
            assert!(error.contains("refusing unsafe legacy state"), "{error}");
            assert!(old.exists());
            assert!(database.exists());
            assert_eq!(fs::read(outside).unwrap(), b"keep");
        }
    }

    #[test]
    fn legacy_sizes_and_pruning_remove_flat_files_and_leave_the_current_key_alone() {
        let (_root, paths, old) = fixture();
        fs::write(old.join("saved"), b"12345").unwrap();
        fs::create_dir_all(paths.state_dir()).unwrap();
        fs::write(paths.state_dir().join("current"), b"keep").unwrap();
        fs::write(paths.state_root().join("flat"), b"old flat").unwrap();
        let entries = legacy_state_until(&paths, None).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == old
                    && entry.bytes.as_ref().is_ok_and(|bytes| *bytes == 5))
        );
        assert_eq!(prune_legacy(&paths).unwrap(), 2);
        assert!(!old.exists());
        assert!(paths.state_dir().join("current").exists());
        assert!(!paths.state_root().join("flat").exists());
        let mut notice = Vec::new();
        report_other_state_once(&paths, &mut notice, &Once::new());
        assert_eq!(notice, [] as [u8; 0]);
    }

    #[test]
    fn current_product_writer_refuses_pruning_without_removals() {
        let (_root, paths, old) = fixture();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        let inventory =
            confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
        let writer = rusqlite::Connection::open(paths.product_database()).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE;").unwrap();
        assert!(
            prune_confirmed(&paths, &inventory)
                .unwrap_err()
                .contains("in use")
        );
        assert!(old.exists());
        assert!(paths.product_database().exists());
    }

    #[test]
    fn current_runtime_writer_does_not_block_legacy_pruning() {
        let (_root, paths, old) = fixture();
        let mut record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
        crate::store::sessions::save_session(&paths, &mut record).unwrap();
        let writer = rusqlite::Connection::open(paths.runtime_database()).unwrap();
        writer.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value); BEGIN IMMEDIATE; INSERT INTO fixture VALUES(1);").unwrap();
        assert_eq!(prune_legacy(&paths).unwrap(), 1);
        assert!(!old.exists());
        assert!(paths.runtime_database().exists());
    }

    #[test]
    fn current_idle_wal_connections_do_not_block_legacy_pruning() {
        for committed_write in [false, true] {
            let (_root, paths, old) = fixture();
            let record = crate::store::sessions::SessionRecord::new(paths.home().to_path_buf());
            crate::store::sessions::save_session(&paths, &mut record.clone()).unwrap();
            let mut connections = Vec::new();
            for path in [
                paths.runtime_database(),
                paths.product_database(),
                paths.subagents_database(),
            ] {
                let database = rusqlite::Connection::open(path).unwrap();
                database
                    .execute_batch(
                        "PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS idle_fixture(value);",
                    )
                    .unwrap();
                if committed_write {
                    database
                        .execute_batch("INSERT INTO idle_fixture VALUES(1);")
                        .unwrap();
                }
                connections.push(database);
            }
            connections[2].execute_batch("PRAGMA user_version=3; CREATE TABLE agent_trees(tree_id TEXT,launch_checkout TEXT); CREATE TABLE agent_workspaces(tree_id TEXT,path TEXT,mode TEXT); CREATE TABLE agent_candidates(tree_id TEXT,candidate_id TEXT,adopted_by TEXT);").unwrap();
            let inventory =
                confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut Vec::new()).unwrap();
            assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 1);
            assert!(!old.exists());
        }
    }

    #[test]
    fn current_holder_does_not_block_legacy_pruning() {
        let (_root, paths, old) = fixture();
        let locks = paths.state_dir().join("runtime.sqlite3-holders");
        fs::create_dir_all(&locks).unwrap();
        let holder = fs::File::create(locks.join("session.lock")).unwrap();
        rustix::fs::flock(&holder, rustix::fs::FlockOperation::LockExclusive).unwrap();
        let mut report = Vec::new();
        let inventory = confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut report)
            .expect("current holder is not legacy");
        assert_eq!(prune_confirmed(&paths, &inventory).unwrap(), 1);
        assert!(!old.exists());
        assert!(locks.join("session.lock").exists());
    }

    #[test]
    fn holder_and_sqlite_locks_refuse_before_any_directory_is_removed() {
        let (_root, paths, old) = fixture();
        let unlocked = paths.state_root().join("r0.e0.p0.t0");
        fs::create_dir(&unlocked).unwrap();
        let locks = old.join("runtime.sqlite3-holders");
        fs::create_dir(&locks).unwrap();
        let file = fs::File::create(locks.join("branch")).unwrap();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        assert!(prune_legacy(&paths).unwrap_err().contains("holder lock"));
        assert!(unlocked.exists());
        drop(file);
        let connection = rusqlite::Connection::open(old.join("runtime.sqlite3")).unwrap();
        connection
            .execute_batch("CREATE TABLE fixture(value); BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
        assert!(unlocked.exists());
        drop(connection);
        assert_eq!(prune_legacy(&paths).unwrap(), 2);
    }

    #[test]
    fn flat_files_must_pass_mode_and_database_lock_checks_before_any_removal() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_root, paths, old) = fixture();
        let flat = paths.state_root().join("pre-key.log");
        fs::write(&flat, b"old").unwrap();
        fs::set_permissions(&flat, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(prune_legacy(&paths).unwrap_err().contains("unsafe"));
        assert!(old.exists());
        assert!(flat.exists());
        fs::set_permissions(&flat, fs::Permissions::from_mode(0o600)).unwrap();
        let database = paths.state_root().join("runtime.sqlite3");
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute_batch("CREATE TABLE fixture(value); BEGIN EXCLUSIVE;")
            .unwrap();
        assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
        assert!(old.exists());
        assert!(flat.exists());
        drop(connection);
        assert_eq!(prune_legacy(&paths).unwrap(), 3);
    }

    #[test]
    fn an_idle_open_wal_database_is_still_in_use() {
        let (_root, paths, old) = fixture();
        let connection = rusqlite::Connection::open(old.join("runtime.sqlite3")).unwrap();
        connection
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE fixture(value);")
            .unwrap();
        assert!(prune_legacy(&paths).unwrap_err().contains("in use"));
        assert!(old.exists());
        drop(connection);
        assert_eq!(prune_legacy(&paths).unwrap(), 1);
    }

    #[test]
    fn symlinks_and_unreadable_databases_are_doubt_not_permission_to_delete() {
        let (root, paths, old) = fixture();
        let outside = root.path().join("outside");
        fs::write(&outside, b"must survive").unwrap();
        std::os::unix::fs::symlink(&outside, old.join("link")).unwrap();
        assert!(prune_legacy(&paths).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"must survive");
        fs::remove_file(old.join("link")).unwrap();
        fs::write(old.join("product.sqlite3"), b"not a readable database").unwrap();
        assert!(prune_legacy(&paths).is_err());
        assert!(old.exists());
        fs::remove_dir_all(paths.state_root()).unwrap();
        let outside_dir = root.path().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();
        fs::write(outside_dir.join("keep"), b"outside state").unwrap();
        std::os::unix::fs::symlink(&outside_dir, paths.state_root()).unwrap();
        assert!(prune_legacy(&paths).is_err());
        assert!(outside_dir.join("keep").exists());
    }

    #[test]
    fn confirmation_requires_a_terminal_or_yes_and_defaults_to_no() {
        let (_root, paths, _old) = fixture();
        let mut output = Vec::new();
        assert!(
            confirm_prune_legacy(&paths, false, false, &mut &b"yes\n"[..], &mut output).is_err()
        );
        assert_eq!(output, [] as [u8; 0]);
        assert!(confirm_prune_legacy(&paths, false, true, &mut &b"\n"[..], &mut output).is_err());
        assert!(confirm_prune_legacy(&paths, false, true, &mut &b"yes\n"[..], &mut output).is_ok());
        let text = String::from_utf8(output.clone()).unwrap();
        assert!(text.contains(&paths.state_root().join("r1.e1.p1.t1").display().to_string()));
        assert!(text.contains("Delete 1 listed non-current state entries"));
        assert!(confirm_prune_legacy(&paths, true, false, &mut &b""[..], &mut output).is_ok());
    }
}
