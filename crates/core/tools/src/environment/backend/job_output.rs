//! Private per-job output files and the exact issued-path read exception.
//!
//! The host opens one private jobs root. Every promoted process spools its
//! combined output to `<root>/<session>/<branch>-p<N>-<service>.log`. A file
//! tool may open such a file only when the requested path byte-equals a path
//! this process service issued, and only through the held root descriptor: both
//! components open with `O_NOFOLLOW`, and the opened file must still be the
//! recorded regular, singly linked inode owned by the effective user.
//!
//! A file is created mode 0600. Consuming its job's notice marks it consumed
//! (modification time = consumption time, then mode 0400); retention, the
//! stale sweep and the root budget read only that on-disk state, and act only
//! on single-link regular owned files whose session directory and file names
//! parse as this layout.

use super::{ExecutionBackendError, ExecutionBackendErrorKind, backend_error};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use rustix::fs::{AtFlags, Dir, Mode, OFlags, mkdirat, openat, unlinkat};
use rustix::io::Errno;
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};

/// Content bytes one job output file keeps before it stops spooling.
pub const MAX_JOB_OUTPUT_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Line that ends a job output file which reached [`MAX_JOB_OUTPUT_FILE_BYTES`].
pub const JOB_OUTPUT_TRUNCATED_LINE: &str = "[output truncated at 64 MiB]";
/// Time a job output file is kept after its job's notice was consumed.
pub const JOB_OUTPUT_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
/// Age (since the last write or consumption) after which the first open of a
/// jobs root deletes any job output file, consumed or not.
pub const JOB_OUTPUT_STALE_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Bytes all job output files under one root may occupy when a file is
/// created, counting the new file at its full bound.
pub const JOB_OUTPUT_ROOT_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

const CONSUMED_MODE: u32 = 0o400;

/// Line reported wherever an issued file is named or read after spooling to
/// it failed; `written` is the content bytes the file holds before the failure.
#[must_use]
pub fn job_output_incomplete_line(written: u64) -> String {
    format!("[output file incomplete: write failed after {written} bytes]")
}

/// Bytes a read takes from one issued file: the content bound plus the longest
/// line spooling may append after it. Anything beyond was not spooled.
fn job_output_read_bound() -> u64 {
    let truncated = JOB_OUTPUT_TRUNCATED_LINE.len();
    let incomplete = job_output_incomplete_line(MAX_JOB_OUTPUT_FILE_BYTES).len();
    // Each trailing line is written as "\n<line>\n".
    MAX_JOB_OUTPUT_FILE_BYTES + u64::try_from(truncated.max(incomplete) + 2).unwrap_or(u64::MAX)
}

/// The typed refusal for an issued file that no longer exists.
#[must_use]
pub fn job_output_file_lost(path: &str) -> ExecutionBackendError {
    refusal(ExecutionBackendErrorKind::NotFound, path)
}

/// One host-owned private directory under which job output files live.
///
/// The directory is opened once, without following a symlink at its last
/// component, and must be a mode 0700 directory owned by the effective user.
/// Every later file operation is relative to the held descriptor, never to the
/// path.
#[derive(Clone)]
pub struct JobOutputRoot {
    inner: Arc<JobOutputRootInner>,
}

struct JobOutputRootInner {
    path: String,
    directory: File,
}

impl std::fmt::Debug for JobOutputRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobOutputRoot")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl JobOutputRoot {
    /// Creates (mode 0700) or opens the private jobs root at an absolute path.
    ///
    /// # Errors
    /// Refuses a relative or non-UTF-8 path, a symlinked or foreign directory,
    /// a directory whose mode is not 0700, or an I/O failure.
    pub fn open(path: &Path) -> Result<Self, ExecutionBackendError> {
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return Err(backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "job output root must name a directory",
            ));
        };
        if !path.is_absolute() {
            return Err(backend_error(
                ExecutionBackendErrorKind::InvalidRequest,
                "job output root must be absolute",
            ));
        }
        let parent = std::fs::canonicalize(parent).map_err(|_| {
            backend_error(
                ExecutionBackendErrorKind::Io,
                "job output root parent could not be resolved",
            )
        })?;
        let display = parent
            .join(name)
            .into_os_string()
            .into_string()
            .map_err(|_| {
                backend_error(
                    ExecutionBackendErrorKind::InvalidRequest,
                    "job output root is not UTF-8",
                )
            })?;
        let parent = rustix::fs::open(
            &parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| {
            backend_error(
                ExecutionBackendErrorKind::Io,
                "job output root parent could not be opened",
            )
        })?;
        let directory = private_directory(&parent, name, true)?;
        Ok(Self {
            inner: Arc::new(JobOutputRootInner {
                path: display,
                directory,
            }),
        })
    }

    /// Absolute path the root was opened at.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.inner.path
    }

    /// Deletes every job output file whose last write or consumption is at
    /// least [`JOB_OUTPUT_STALE_AGE`] old, consumed or not, then removes
    /// session directories left empty. Hosts run it once, when they first
    /// open the root.
    pub fn sweep_stale(&self, now: SystemTime) {
        for (session, directory) in self.session_directories() {
            for entry in log_entries(&directory, |_| true) {
                if expired(entry.modified, JOB_OUTPUT_STALE_AGE, now) {
                    let _ = unlinkat(&directory, entry.name.as_str(), AtFlags::empty());
                }
            }
            drop(directory);
            // Fails harmlessly while the directory still holds anything.
            let _ = unlinkat(&self.inner.directory, session.as_str(), AtFlags::REMOVEDIR);
        }
    }

    /// Makes room for one more file at its full bound within
    /// [`JOB_OUTPUT_ROOT_BUDGET_BYTES`]. When the files no eviction may delete
    /// (unconsumed and younger than [`JOB_OUTPUT_STALE_AGE`]) leave no room it
    /// deletes nothing; otherwise it evicts consumed files oldest first, then
    /// stale unconsumed files oldest first, until the new file fits. Returns
    /// the evicted paths and whether the new file fits.
    fn reserve(&self, now: SystemTime) -> (Vec<String>, bool) {
        let mut used = 0_u64;
        let mut kept = 0_u64;
        let mut consumed = Vec::new();
        let mut stale = Vec::new();
        for (session, directory) in self.session_directories() {
            let directory = Arc::new(directory);
            for entry in log_entries(&directory, |_| true) {
                used = used.saturating_add(entry.size);
                let candidate = (entry, session.clone(), Arc::clone(&directory));
                if candidate.0.consumed {
                    consumed.push(candidate);
                } else if expired(candidate.0.modified, JOB_OUTPUT_STALE_AGE, now) {
                    stale.push(candidate);
                } else {
                    kept = kept.saturating_add(candidate.0.size);
                }
            }
        }
        let fits = |used: u64| {
            used.saturating_add(MAX_JOB_OUTPUT_FILE_BYTES) <= JOB_OUTPUT_ROOT_BUDGET_BYTES
        };
        if fits(used) {
            return (Vec::new(), true);
        }
        if !fits(kept) {
            return (Vec::new(), false);
        }
        for candidates in [&mut consumed, &mut stale] {
            candidates.sort_by(|left, right| {
                (left.0.modified, &left.1, &left.0.name).cmp(&(
                    right.0.modified,
                    &right.1,
                    &right.0.name,
                ))
            });
        }
        let mut evicted = Vec::new();
        for (entry, session, directory) in consumed.into_iter().chain(stale) {
            if fits(used) {
                break;
            }
            if unlinkat(&*directory, entry.name.as_str(), AtFlags::empty()).is_ok() {
                used = used.saturating_sub(entry.size);
                evicted.push(format!("{}/{session}/{}", self.inner.path, entry.name));
            }
        }
        (evicted, fits(used))
    }

    /// Every private session directory under the root whose name is a
    /// canonical session id, by name.
    fn session_directories(&self) -> Vec<(String, File)> {
        let Ok(mut entries) = Dir::read_from(&self.inner.directory) else {
            return Vec::new();
        };
        let mut names = Vec::new();
        while let Some(Ok(entry)) = entries.read() {
            if let Ok(name) = entry.file_name().to_str()
                && canonical::<plakous_model::SessionId>(name)
            {
                names.push(name.to_owned());
            }
        }
        names
            .into_iter()
            .filter_map(|name| {
                let directory =
                    private_directory(&self.inner.directory, name.as_str(), false).ok()?;
                Some((name, directory))
            })
            .collect()
    }
}

/// The issued output file of one promoted job.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobOutputFile {
    /// The exact path the process tools print and the read exception admits.
    pub path: String,
    /// Content bytes the file holds when spooling to it failed.
    pub write_failed_after: Option<u64>,
}

/// Identity recorded when a job output file was created.
#[derive(Clone, Debug)]
pub(super) struct IssuedJobOutput {
    pub(super) path: String,
    name: String,
    device: u64,
    inode: u64,
    failure: Arc<OnceLock<u64>>,
}

/// One service's binding to the jobs root and the paths it has issued.
pub(super) struct JobOutputs {
    scope: JobOutputScope,
    issued: BTreeMap<u32, IssuedJobOutput>,
    /// Issued paths this service has since deleted; reads of them are typed
    /// not-found refusals rather than the foreign-path refusal.
    retired: BTreeSet<String>,
}

/// The root, session, branch and service incarnation one service's files
/// live under, detached from the service lock for blocking sweeps.
#[derive(Clone)]
pub(super) struct JobOutputScope {
    root: JobOutputRoot,
    session: String,
    branch: String,
    /// The 16 lowercase hex digits that name this service incarnation's files.
    owner: String,
}

/// What the read exception decides for one requested path.
pub(super) enum JobOutputAdmission {
    /// The path byte-equals a live issued path.
    Issued(IssuedJobOutputLocation),
    /// The path is refused without any I/O.
    Refused(ExecutionBackendError),
}

/// One issued file, detached from the service lock for blocking reads.
#[derive(Clone)]
pub(super) struct IssuedJobOutputLocation {
    root: JobOutputRoot,
    session: String,
    issued: IssuedJobOutput,
}

impl JobOutputs {
    pub(super) fn new(
        root: JobOutputRoot,
        session: plakous_model::SessionId,
        branch: plakous_model::BranchId,
        owner: [u8; 8],
    ) -> Self {
        Self {
            scope: JobOutputScope {
                root,
                session: session.to_string(),
                branch: branch.to_string(),
                owner: format!("{:016x}", u64::from_be_bytes(owner)),
            },
            issued: BTreeMap::new(),
            retired: BTreeSet::new(),
        }
    }

    pub(super) fn scope(&self) -> JobOutputScope {
        self.scope.clone()
    }

    /// The path issued for a job number, if its file was created.
    #[cfg(test)]
    pub(super) fn path(&self, number: u32) -> Option<&str> {
        self.issued.get(&number).map(|issued| issued.path.as_str())
    }

    /// The identities issued for these job numbers, for consumption marking.
    pub(super) fn issued(&self, numbers: &[u32]) -> Vec<IssuedJobOutput> {
        numbers
            .iter()
            .filter_map(|number| self.issued.get(number).cloned())
            .collect()
    }

    /// The file issued for a job number, if it was created and not deleted.
    pub(super) fn file(&self, number: u32) -> Option<JobOutputFile> {
        self.issued.get(&number).map(|issued| JobOutputFile {
            path: issued.path.clone(),
            write_failed_after: issued.failure.get().copied(),
        })
    }

    /// Decides the read exception for `path`; nothing is normalized. `None`
    /// leaves the path to ordinary workspace containment.
    pub(super) fn admit(&self, path: &str) -> Option<JobOutputAdmission> {
        if let Some(issued) = self.issued.values().find(|issued| issued.path == path) {
            return Some(JobOutputAdmission::Issued(IssuedJobOutputLocation {
                root: self.scope.root.clone(),
                session: self.scope.session.clone(),
                issued: issued.clone(),
            }));
        }
        if self.retired.contains(path) {
            return Some(JobOutputAdmission::Refused(job_output_file_lost(path)));
        }
        let under_root = path
            .strip_prefix(self.scope.root.path())
            .is_some_and(|rest| rest.starts_with('/'));
        under_root.then(|| {
            JobOutputAdmission::Refused(backend_error(
                ExecutionBackendErrorKind::WorkspacePolicy,
                format!(
                    "job output file {path} was issued by an earlier process or another agent \
                     and cannot be read with this tool"
                ),
            ))
        })
    }

    /// Forgets issued files a sweep or eviction deleted, by file name or path.
    pub(super) fn retire(&mut self, names: &[String], paths: &[String]) {
        let retired = self
            .issued
            .iter()
            .filter(|(_, issued)| names.contains(&issued.name) || paths.contains(&issued.path))
            .map(|(number, _)| *number)
            .collect::<Vec<_>>();
        for number in retired {
            if let Some(issued) = self.issued.remove(&number) {
                self.retired.insert(issued.path);
            }
        }
    }

    /// Creates one job's file exclusively (mode 0600) and records its identity.
    /// The caller has already reserved room under the root budget.
    pub(super) fn create(&mut self, number: u32) -> Result<JobOutputWriter, ExecutionBackendError> {
        let name = format!("{}-p{number}-{}.log", self.scope.branch, self.scope.owner);
        // A concurrent stale sweep may remove an empty session directory
        // between its creation and the file's; one retry covers that.
        let file = self
            .scope
            .create_file(&name)
            .or_else(|_| self.scope.create_file(&name))?;
        let metadata = file.metadata().map_err(|_| {
            backend_error(
                ExecutionBackendErrorKind::Io,
                "job output file could not be inspected",
            )
        })?;
        let failure = Arc::new(OnceLock::new());
        self.issued.insert(
            number,
            IssuedJobOutput {
                path: format!(
                    "{}/{}/{name}",
                    self.scope.root.inner.path, self.scope.session
                ),
                name,
                device: metadata.dev(),
                inode: metadata.ino(),
                failure: Arc::clone(&failure),
            },
        );
        Ok(JobOutputWriter {
            file,
            written: 0,
            open: true,
            failure,
        })
    }
}

impl JobOutputScope {
    fn create_file(&self, name: &str) -> Result<File, ExecutionBackendError> {
        let directory = private_directory(&self.root.inner.directory, self.session.as_str(), true)?;
        openat(
            &directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map(File::from)
        .map_err(|_| {
            backend_error(
                ExecutionBackendErrorKind::Io,
                "job output file could not be created",
            )
        })
    }

    /// Runs this branch's retention sweep and makes room under the root
    /// budget. Returns the deleted file names, the evicted paths and whether
    /// one more file fits. It walks the whole root: never call it on an async
    /// worker.
    pub(super) fn prepare_create(&self, now: SystemTime) -> (Vec<String>, Vec<String>, bool) {
        let swept = self.sweep(now);
        let (evicted, fits) = self.root.reserve(now);
        (swept, evicted, fits)
    }

    /// Marks the files of just-consumed job numbers, of any incarnation of
    /// this branch's service, consumed (modification time `now`, then mode
    /// 0400 only if that succeeded), then runs the retention sweep. A file of
    /// this incarnation is marked only while it is still the issued inode in
    /// `own`. The marking comes first, so a notice never deletes the file it
    /// just named. Returns the deleted file names.
    pub(super) fn release(
        &self,
        numbers: &[u32],
        own: &[IssuedJobOutput],
        now: SystemTime,
    ) -> Vec<String> {
        #[cfg(test)]
        tests::release_gate(self.root.path());
        let Ok(directory) =
            private_directory(&self.root.inner.directory, self.session.as_str(), false)
        else {
            return Vec::new();
        };
        let mut names = Vec::new();
        read_layout_names(&directory, |name| {
            if name.branch == self.branch && numbers.contains(&name.number) {
                names.push((name.owner == self.owner, name.text.to_owned()));
            }
        });
        for (this_incarnation, name) in names {
            let Some((file, metadata)) = open_job_file(&directory, &name) else {
                continue;
            };
            if this_incarnation
                && !own.iter().any(|issued| {
                    issued.name == name
                        && issued.device == metadata.dev()
                        && issued.inode == metadata.ino()
                })
            {
                continue;
            }
            if file.set_modified(now).is_ok() {
                let _ = file.set_permissions(std::fs::Permissions::from_mode(CONSUMED_MODE));
            }
        }
        drop(directory);
        self.sweep(now)
    }

    /// Deletes this branch's consumed files, of any incarnation, whose
    /// consumption is at least [`JOB_OUTPUT_RETENTION`] old. Returns the
    /// deleted file names.
    fn sweep(&self, now: SystemTime) -> Vec<String> {
        let Ok(directory) =
            private_directory(&self.root.inner.directory, self.session.as_str(), false)
        else {
            return Vec::new();
        };
        log_entries(&directory, |name| name.branch == self.branch)
            .into_iter()
            .filter(|entry| entry.consumed && expired(entry.modified, JOB_OUTPUT_RETENTION, now))
            .filter(|entry| unlinkat(&directory, entry.name.as_str(), AtFlags::empty()).is_ok())
            .map(|entry| entry.name)
            .collect()
    }
}

/// A file name of the job layout: `<branch id>-p<N>-<16 lowercase hex>.log`,
/// with a canonical branch id and job number.
struct LayoutName<'a> {
    text: &'a str,
    branch: &'a str,
    number: u32,
    owner: &'a str,
}

impl<'a> LayoutName<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let (rest, owner) = text.strip_suffix(".log")?.rsplit_once('-')?;
        let (branch, digits) = rest.rsplit_once("-p")?;
        let owner_hex = owner.len() == 16
            && owner
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        let number = digits.parse::<u32>().ok()?;
        (owner_hex && number.to_string() == digits && canonical::<plakous_model::BranchId>(branch))
            .then_some(Self {
                text,
                branch,
                number,
                owner,
            })
    }
}

/// Whether `text` is the canonical spelling of an id of type `T`.
fn canonical<T: std::str::FromStr + std::fmt::Display>(text: &str) -> bool {
    text.parse::<T>().is_ok_and(|id| id.to_string() == text)
}

/// Calls `visit` for every entry of `directory` whose name parses as the job
/// layout.
fn read_layout_names(directory: &File, mut visit: impl FnMut(&LayoutName<'_>)) {
    let Ok(mut entries) = Dir::read_from(directory) else {
        return;
    };
    while let Some(Ok(entry)) = entries.read() {
        if let Ok(text) = entry.file_name().to_str()
            && let Some(name) = LayoutName::parse(text)
        {
            visit(&name);
        }
    }
}

/// One job layout file: regular, singly linked and owned by the effective user.
struct LogEntry {
    name: String,
    size: u64,
    modified: SystemTime,
    consumed: bool,
}

/// Lists the job layout files whose names pass `select`, inspecting each
/// through a no-follow, non-blocking open.
fn log_entries(directory: &File, select: impl Fn(&LayoutName<'_>) -> bool) -> Vec<LogEntry> {
    let mut names = Vec::new();
    read_layout_names(directory, |name| {
        if select(name) {
            names.push(name.text.to_owned());
        }
    });
    names
        .into_iter()
        .filter_map(|name| {
            let (_, metadata) = open_job_file(directory, &name)?;
            Some(LogEntry {
                name,
                size: metadata.size(),
                modified: metadata.modified().ok()?,
                consumed: metadata.mode() & 0o777 == CONSUMED_MODE,
            })
        })
        .collect()
}

/// Opens one name under a held directory without following a symlink or
/// blocking, and keeps it only if it is a regular, singly linked file owned
/// by the effective user.
fn open_job_file(directory: &File, name: &str) -> Option<(File, std::fs::Metadata)> {
    let file = openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .ok()?;
    let metadata = file.metadata().ok()?;
    (metadata.file_type().is_file()
        && metadata.nlink() == 1
        && metadata.uid() == rustix::process::geteuid().as_raw())
    .then_some((file, metadata))
}

fn expired(modified: SystemTime, age: Duration, now: SystemTime) -> bool {
    modified
        .checked_add(age)
        .is_some_and(|deadline| deadline <= now)
}

/// Bytes read from one issued job output file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobOutputSlice {
    /// Bytes starting at the requested offset.
    pub bytes: Vec<u8>,
    /// Whether the read reached the current end of the file. A read never
    /// goes past the content bound plus its truncation line; a file that
    /// extends beyond that is never complete.
    pub complete: bool,
    /// Content bytes the file holds when spooling to it failed.
    pub write_failed_after: Option<u64>,
}

impl IssuedJobOutputLocation {
    /// Opens the issued file through the held root and reads one bounded slice.
    pub(super) fn read(
        &self,
        offset: u64,
        max_bytes: u64,
    ) -> Result<JobOutputSlice, ExecutionBackendError> {
        let path = &self.issued.path;
        let directory = private_directory(&self.root.inner.directory, &self.session, false)
            .map_err(|error| refusal(error.kind(), path))?;
        let file = openat(
            &directory,
            self.issued.name.as_str(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(|errno| refusal(open_error_kind(errno), path))?;
        let metadata = file
            .metadata()
            .map_err(|_| refusal(ExecutionBackendErrorKind::Io, path))?;
        if !metadata.file_type().is_file()
            || metadata.nlink() != 1
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.dev() != self.issued.device
            || metadata.ino() != self.issued.inode
        {
            return Err(refusal(ExecutionBackendErrorKind::IdentityMismatch, path));
        }
        let bound = job_output_read_bound();
        let room = bound.saturating_sub(offset);
        let wanted = usize::try_from(max_bytes.min(room)).unwrap_or(usize::MAX);
        let mut bytes = vec![0; wanted.min(4 * 1024 * 1024)];
        let mut filled = 0;
        while filled < bytes.len() {
            let position = offset.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX));
            match file.read_at(&mut bytes[filled..], position) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(refusal(ExecutionBackendErrorKind::Io, path)),
            }
        }
        let at_bound = offset.saturating_add(u64::try_from(filled).unwrap_or(u64::MAX)) >= bound;
        let complete = filled < bytes.len() || (at_bound && metadata.size() <= bound);
        bytes.truncate(filled);
        Ok(JobOutputSlice {
            bytes,
            complete,
            write_failed_after: self.issued.failure.get().copied(),
        })
    }
}

/// Typed, model-visible refusal for one issued path.
fn refusal(kind: ExecutionBackendErrorKind, path: &str) -> ExecutionBackendError {
    let reason = match kind {
        ExecutionBackendErrorKind::NotFound => "no longer exists",
        ExecutionBackendErrorKind::SymlinkEscape => "crosses a symlink; refusing to read it",
        ExecutionBackendErrorKind::IdentityMismatch => {
            "was replaced, relinked or changed owner; refusing to read it"
        }
        ExecutionBackendErrorKind::Denied => {
            "is in a directory that is not private to this user; refusing to read it"
        }
        _ => "could not be read",
    };
    backend_error(kind, format!("job output file {path} {reason}"))
}

const fn open_error_kind(errno: Errno) -> ExecutionBackendErrorKind {
    match errno {
        Errno::NOENT => ExecutionBackendErrorKind::NotFound,
        Errno::LOOP | Errno::NOTDIR => ExecutionBackendErrorKind::SymlinkEscape,
        _ => ExecutionBackendErrorKind::Io,
    }
}

/// Opens (optionally creating, mode 0700) one directory component without
/// following a symlink, and requires mode 0700 and the effective user as owner.
fn private_directory(
    parent: &impl std::os::fd::AsFd,
    name: impl rustix::path::Arg + Copy,
    create: bool,
) -> Result<File, ExecutionBackendError> {
    if create {
        match mkdirat(parent, name, Mode::RWXU) {
            Ok(()) | Err(Errno::EXIST) => {}
            Err(_) => {
                return Err(backend_error(
                    ExecutionBackendErrorKind::Io,
                    "job output directory could not be created",
                ));
            }
        }
    }
    let directory = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|errno| {
        backend_error(
            open_error_kind(errno),
            "job output directory could not be opened",
        )
    })?;
    let private = directory.metadata().is_ok_and(|metadata| {
        metadata.is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o777 == 0o700
    });
    if !private {
        return Err(backend_error(
            ExecutionBackendErrorKind::Denied,
            "job output directory is not private to this user",
        ));
    }
    Ok(directory)
}

/// Appends one promoted job's combined output up to the file bound.
pub(super) struct JobOutputWriter {
    file: File,
    written: u64,
    open: bool,
    failure: Arc<OnceLock<u64>>,
}

impl JobOutputWriter {
    pub(super) fn detached_output(&mut self, parts: u64) -> Option<(File, u64)> {
        if !self.open || self.file.metadata().ok()?.nlink() == 0 {
            return None;
        }
        let file = self.file.try_clone().ok()?;
        let flags = rustix::fs::fcntl_getfl(&file).ok()?;
        rustix::fs::fcntl_setfl(&file, flags | OFlags::APPEND).ok()?;
        let room = MAX_JOB_OUTPUT_FILE_BYTES.saturating_sub(self.written) / parts;
        self.written = self.written.saturating_add(room);
        Some((file, room))
    }

    /// Writes bytes until the bound, then the truncation line once. A write
    /// failure stops spooling, records the content bytes the file holds
    /// (including the accepted prefix of a partial write) and tries once to
    /// leave the incomplete-file line; the in-memory window is unaffected.
    pub(super) fn append(&mut self, bytes: &[u8]) {
        if !self.open || bytes.is_empty() {
            return;
        }
        let room = MAX_JOB_OUTPUT_FILE_BYTES.saturating_sub(self.written);
        let take = usize::try_from(room).unwrap_or(usize::MAX).min(bytes.len());
        let mut accepted = 0;
        let failed = loop {
            if accepted == take {
                break false;
            }
            match self.file.write(&bytes[accepted..take]) {
                Ok(count) if count > 0 => accepted += count,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Ok(_) | Err(_) => break true,
            }
        };
        self.written = self
            .written
            .saturating_add(u64::try_from(accepted).unwrap_or(u64::MAX));
        if failed {
            self.open = false;
            let _ = self.failure.set(self.written);
            let line = format!("\n{}\n", job_output_incomplete_line(self.written));
            let _ = self.file.write_all(line.as_bytes());
            return;
        }
        if take < bytes.len() {
            let line = format!("\n{JOB_OUTPUT_TRUNCATED_LINE}\n");
            let _ = self.file.write_all(line.as_bytes());
            self.open = false;
        }
    }
}

#[cfg(test)]
mod tests;
