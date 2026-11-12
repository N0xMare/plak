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
///
/// The runtime store's holder-claim files (`runtime.sqlite3-holders/`) have
/// no version of their own in the key: builds of different holder formats are
/// kept apart only by this key, so a holder-format change must ship with a
/// bump of one of these four versions.
#[must_use]
pub fn state_key() -> String {
    #[cfg(feature = "test-hooks")]
    if let Ok(key) = std::env::var("PLAK_TEST_STATE_KEY")
        && crate::carry::upgrade::state_key_valid(&key)
    {
        return key;
    }
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

/// The state keys under this home whose runtime store a live process holds
/// open, sorted. Every process that opens a runtime store holds an exclusive
/// lock on its own `<pid>-<nonce>.lock` in that key's
/// `runtime.sqlite3-holders` directory for as long as the store is open; the
/// kernel releases it when the process exits or execs (a `/reload`). A lock
/// file is probed with a non-blocking exclusive lock that is released at once,
/// as the store's own holder probe does, so a file left behind by a gone or
/// re-executed process is not counted. This process's own open store counts.
pub(crate) fn running_state_keys(paths: &PlakPaths) -> Vec<String> {
    let Ok(entries) = fs::read_dir(paths.state_root()) else {
        return Vec::new();
    };
    let mut keys = entries
        .flatten()
        .filter(|entry| {
            let Ok(holders) = fs::read_dir(entry.path().join("runtime.sqlite3-holders")) else {
                return false;
            };
            holders.flatten().any(|holder| {
                let path = holder.path();
                path.extension()
                    .is_some_and(|extension| extension == "lock")
                    && lock_is_held(&path)
            })
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    keys.sort();
    keys
}

/// Whether another open file description holds `path`'s lock. Taking it
/// without waiting proves it free and releases it on drop; a file that cannot
/// be opened is not a live holder.
fn lock_is_held(path: &Path) -> bool {
    fs::File::open(path).is_ok_and(|file| {
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).is_err()
    })
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

fn old_telemetry_files(paths: &PlakPaths) -> Result<Vec<PathBuf>, String> {
    use std::os::unix::fs::MetadataExt as _;
    let directory = paths.telemetry_dir();
    let metadata = match fs::symlink_metadata(&directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o022 != 0
    {
        return Err("refusing unsafe telemetry directory".to_owned());
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let old = crate::telemetry_store::files::version(name)
            .is_some_and(|version| version < crate::telemetry_store::TELEMETRY_SCHEMA_VERSION);
        if old {
            if !entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_file()
            {
                return Err(format!(
                    "refusing unsafe telemetry file {}",
                    entry.path().display()
                ));
            }
            files.push(entry.path());
        }
    }
    Ok(files)
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
    entries.extend(old_telemetry_files(paths)?);
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

#[allow(
    clippy::too_many_lines,
    reason = "Legacy pruning keeps ownership inventories, live-lock checks, and destructive eligibility decisions in one ordered filesystem traversal."
)]
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
#[allow(
    clippy::too_many_lines,
    reason = "State-lock probing checks each runtime, product, tree, and reference owner without treating unavailable lock evidence as authority to delete."
)]
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
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        // A temporary record is published by a writer holding its process
        // lock, which this probe locks with every other lock file, so a
        // temporary record whose writer is gone is simply removable.
        let temporary = name
            .strip_suffix(".tmp")
            .and_then(|stem| stem.rsplit_once('.'))
            .is_some_and(|(stem, _writer)| stem.ends_with(".holder"));
        if !temporary {
            // A record is evidence, not a lock: it is read, never held open.
            let record = name.ends_with(".holder");
            let mut file = fs::OpenOptions::new()
                .read(true)
                .write(!record)
                .open(path)
                .map_err(|error| format!("cannot prove legacy state is unused: {error}"))?;
            if !record {
                rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                    .map_err(|_| {
                        format!("legacy state {} is in use (holder lock)", path.display())
                    })?;
            }
            let mut identity = String::new();
            file.read_to_string(&mut identity)
                .map_err(|error| error.to_string())?;
            let uname = rustix::system::uname();
            let host = uname
                .nodename()
                .to_str()
                .map_err(|error| error.to_string())?;
            // Every holder format names its host after the format tag;
            // process locks are empty.
            let local = ["plak-holder-v1", "plak-holder-v2", "plak-holder-v3"]
                .iter()
                .any(|format| identity.starts_with(&format!("{format} {host} ")));
            if !identity.is_empty() && !local {
                return Err(format!(
                    "cannot prove legacy holder {} is local and unused",
                    path.display()
                ));
            }
            if !record {
                holders.push(file);
            }
        }
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
mod tests;
