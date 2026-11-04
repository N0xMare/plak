//! Same-user requests that a running interactive session reload onto the
//! installed build (spec 02 §5.1, spec 04 §4.6).
//!
//! The channel is files in one private directory under the product home's
//! transient `run/` directory, `run/reload-requests/`, outside every state
//! key's directory so a request still reaches a session when the installed
//! build reads another state key, and outside `state/` so no legacy-state
//! report or prune ever touches a live host's announcement.
//! The interactive host announces itself with `<session>.listener` (its pid, a
//! random instance nonce, its start time and state key); a requester finds the
//! session through that file and drops `<session>.request` echoing the nonce;
//! the host consumes it, and the outcome lands in `<request>.outcome`. Every
//! file is versioned and additive. There is no listener socket and no network:
//! only a process that can already write this user's product home can ask.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::home::paths::PlakPaths;

const VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 4096;
/// Outcomes nobody collected are pruned after this long.
const OUTCOME_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The host's announcement: which live process accepts requests for one
/// session, and the instance a request must name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct Announcement {
    version: u32,
    pid: i32,
    nonce: String,
    started_at: u64,
    state_key: String,
}

/// One request, as a requester writes it and the host reads it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReloadRequest {
    #[serde(default)]
    version: u32,
    pub request_id: String,
    pub session_id: String,
    /// The installed build the requester expects the session to reload onto.
    pub expect_binary: String,
    /// The announcing host instance the request is for.
    #[serde(default)]
    nonce: String,
}

impl ReloadRequest {
    /// Whether every field is well formed: this version, UUID identities and
    /// a binary id. Only a valid request reaches a session or its root.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.version == VERSION
            && uuid::Uuid::parse_str(&self.request_id).is_ok()
            && uuid::Uuid::parse_str(&self.session_id).is_ok()
            && is_binary_id(&self.expect_binary)
    }
}

/// How a request ended, written for whoever asked.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ReloadOutcome {
    /// The session reloaded onto `binary`; `session` is the carried successor
    /// when the reload carried, otherwise the same session.
    Reloaded { binary: String, session: String },
    /// The host refused; the session stays on its current build.
    Refused { reason: String },
    /// A later request for the same session replaced this one before the
    /// host took it.
    Superseded { by: String },
}

#[derive(Deserialize, Serialize)]
struct OutcomeFile {
    version: u32,
    #[serde(flatten)]
    outcome: ReloadOutcome,
}

/// The host-authored continuation this process's first attach posts, taken
/// once so a later re-attach in the same process never posts it again.
static CONTINUATION: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub(crate) fn set_reload_continuation(text: String) {
    if let Ok(mut slot) = CONTINUATION.lock() {
        *slot = Some(text);
    }
}

pub(crate) fn take_reload_continuation() -> Option<String> {
    CONTINUATION.lock().ok().and_then(|mut slot| slot.take())
}

fn directory(paths: &PlakPaths) -> PathBuf {
    paths.run_dir().join("reload-requests")
}

fn listener_path(paths: &PlakPaths, session: &str) -> PathBuf {
    directory(paths).join(format!("{session}.listener"))
}

fn request_path(paths: &PlakPaths, session: &str) -> PathBuf {
    directory(paths).join(format!("{session}.request"))
}

pub fn outcome_path(paths: &PlakPaths, request_id: &str) -> PathBuf {
    directory(paths).join(format!("{request_id}.outcome"))
}

/// Whether this home accepts requests (`tui.reload_requests`, home-scoped).
pub fn enabled(paths: &PlakPaths) -> bool {
    crate::product_config::ProductConfig::load(paths).is_ok_and(|config| config.reload_requests)
}

fn private_directory(paths: &PlakPaths) -> Result<PathBuf, String> {
    let dir = directory(paths);
    match fs::symlink_metadata(&dir) {
        Ok(metadata) => {
            if !metadata.is_dir() || !owned_private(&metadata, 0o077) {
                return Err(format!("{} is not a private directory", dir.display()));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(true);
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
                .create(&dir)
                .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        }
        Err(error) => return Err(format!("could not inspect {}: {error}", dir.display())),
    }
    Ok(dir)
}

fn owned_private(metadata: &fs::Metadata, forbidden: u32) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    metadata.uid() == rustix::process::geteuid().as_raw() && metadata.mode() & forbidden == 0
}

/// Writes `bytes` to `path` through a private temporary file and a rename,
/// so a reader never sees a partial file.
fn publish(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options
        .open(&temporary)
        .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
        .and_then(|()| fs::rename(&temporary, path));
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary);
        return Err(format!("could not write {}: {error}", path.display()));
    }
    Ok(())
}

/// Opens `path` without following a link and reads it only when the opened
/// file is a regular file this user owns, private to them and within bounds.
fn read_private(path: &Path) -> Option<Vec<u8>> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Read as _;
    let file = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&file).ok()?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO)
        || u64::try_from(stat.st_size)
            .ok()
            .is_none_or(|size| size > MAX_FILE_BYTES)
    {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::from(file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_FILE_BYTES).then_some(bytes)
}

fn read_announcement(path: &Path) -> Option<Announcement> {
    let announcement: Announcement = serde_json::from_slice(&read_private(path)?).ok()?;
    (announcement.version == VERSION).then_some(announcement)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// The interactive host's announcement; removed when dropped.
pub struct Listener {
    path: PathBuf,
    nonce: String,
}

impl Listener {
    /// Announces `session` as accepting requests, when the home opted in. A
    /// request left from before this instance is discarded, and outcomes past
    /// their retention are pruned.
    /// Only a session's UUID ever names a file in the channel.
    pub fn announce(paths: &PlakPaths, session: &str) -> Option<Self> {
        if !enabled(paths) || uuid::Uuid::parse_str(session).is_err() {
            return None;
        }
        let dir = private_directory(paths).ok()?;
        let _ = fs::remove_file(request_path(paths, session));
        prune_outcomes(&dir);
        let announcement = Announcement {
            version: VERSION,
            pid: i32::try_from(std::process::id()).ok()?,
            nonce: uuid::Uuid::new_v4().to_string(),
            started_at: unix_now(),
            state_key: crate::home::state::state_key(),
        };
        let path = listener_path(paths, session);
        publish(&path, &serde_json::to_vec(&announcement).ok()?).ok()?;
        Some(Self {
            path,
            nonce: announcement.nonce,
        })
    }

    /// Whether the announcement file still exists and carries this
    /// instance's nonce. Something may have removed it, or another instance
    /// of the session's host may have replaced it.
    #[must_use]
    pub fn is_current(&self) -> bool {
        read_announcement(&self.path).is_some_and(|announcement| announcement.nonce == self.nonce)
    }

    /// Consumes the pending request for `session`, if any. A request that is
    /// not this user's private file, does not decode, is malformed, names
    /// another session or another instance of this host is discarded; a
    /// well-formed one for another instance is told so.
    pub fn take(&self, paths: &PlakPaths, session: &str) -> Option<ReloadRequest> {
        uuid::Uuid::parse_str(session).ok()?;
        let path = request_path(paths, session);
        let bytes = read_private(&path);
        let _ = fs::remove_file(&path);
        let request: ReloadRequest = serde_json::from_slice(&bytes?).ok()?;
        if request.version != VERSION
            && uuid::Uuid::parse_str(&request.request_id).is_ok()
            && request.session_id == session
        {
            record(
                paths,
                &request.request_id,
                &ReloadOutcome::Refused {
                    reason: "unsupported request version".to_owned(),
                },
            );
            return None;
        }
        if !request.is_valid() || request.session_id != session {
            return None;
        }
        if request.nonce != self.nonce {
            record(
                paths,
                &request.request_id,
                &ReloadOutcome::Refused {
                    reason: "the request named another instance of this session's host".to_owned(),
                },
            );
            return None;
        }
        Some(request)
    }
}

impl Drop for Listener {
    /// Removes the announcement only while it is still this instance's, so a
    /// replaced listener never removes its successor's file.
    fn drop(&mut self) {
        if self.is_current() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn prune_outcomes(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let cutoff = SystemTime::now().checked_sub(OUTCOME_RETENTION);
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = path
            .extension()
            .is_some_and(|extension| extension == "outcome")
            && entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| cutoff.is_some_and(|cutoff| modified < cutoff));
        if stale {
            let _ = fs::remove_file(path);
        }
    }
}

/// The announced session `query` names: an exact id, or a prefix only one
/// announcement matches.
fn announced_session(paths: &PlakPaths, query: &str) -> Result<String, String> {
    let not_open = || {
        format!("session {query} is not open in an interactive plak that accepts reload requests")
    };
    if uuid::Uuid::parse_str(query).is_ok() {
        return Ok(query.to_owned());
    }
    if query.len() < 4
        || !query
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(not_open());
    }
    let mut matches = fs::read_dir(directory(paths))
        .map_err(|_| not_open())?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let session = name.strip_suffix(".listener")?;
            (session.starts_with(query) && uuid::Uuid::parse_str(session).is_ok())
                .then(|| session.to_owned())
        });
    let session = matches.next().ok_or_else(not_open)?;
    if matches.next().is_some() {
        return Err(format!(
            "session prefix {query} names more than one open session"
        ));
    }
    Ok(session)
}

/// `plak session reload <session> --expect-binary <id>`: validates and
/// queues one request for the session an interactive host announced, under
/// any state key, and returns its id and the path its outcome will be
/// written to. The requesting `plak` must itself be the named build; the host
/// checks the installed build again before it performs the request at the
/// session's next idle boundary. A request still pending for the session is
/// replaced, and its requester told so.
pub fn request(
    paths: &PlakPaths,
    session: &str,
    expect_binary: &str,
    requester_binary: &str,
) -> Result<(String, PathBuf), String> {
    if !enabled(paths) {
        return Err(
            "reload requests are off in this home; set `tui.reload_requests = true` in ~/.plak/config.toml"
                .to_owned(),
        );
    }
    if !is_binary_id(expect_binary) {
        return Err("--expect-binary must be a build's binary id".to_owned());
    }
    if requester_binary != expect_binary {
        return Err(format!(
            "this plak is build {requester_binary}, not {expect_binary}; run the installed plak or name its build; nothing was requested"
        ));
    }
    let session = announced_session(paths, session)?;
    let announcement = read_announcement(&listener_path(paths, &session))
        .filter(|announcement| {
            rustix::process::Pid::from_raw(announcement.pid)
                .is_some_and(|pid| rustix::process::test_kill_process(pid).is_ok())
        })
        .ok_or_else(|| {
            format!(
                "session {session} is not open in an interactive plak that accepts reload requests"
            )
        })?;
    private_directory(paths)?;
    let request = ReloadRequest {
        version: VERSION,
        request_id: uuid::Uuid::new_v4().to_string(),
        session_id: session.clone(),
        expect_binary: expect_binary.to_owned(),
        nonce: announcement.nonce,
    };
    let path = request_path(paths, &session);
    if let Some(pending) = read_private(&path)
        .and_then(|bytes| serde_json::from_slice::<ReloadRequest>(&bytes).ok())
        .filter(ReloadRequest::is_valid)
    {
        record(
            paths,
            &pending.request_id,
            &ReloadOutcome::Superseded {
                by: request.request_id.clone(),
            },
        );
    }
    let bytes = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    publish(&path, &bytes)?;
    Ok((session, outcome_path(paths, &request.request_id)))
}

/// Records how `request_id` ended; best effort, never a refusal of its own.
pub fn record(paths: &PlakPaths, request_id: &str, outcome: &ReloadOutcome) {
    if uuid::Uuid::parse_str(request_id).is_err() || private_directory(paths).is_err() {
        return;
    }
    let file = OutcomeFile {
        version: VERSION,
        outcome: outcome.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&file) {
        let _ = publish(&outcome_path(paths, request_id), &bytes);
    }
}

/// The request the reload handoff that started this process named, taken
/// once by the first attach.
static HANDED_OFF: std::sync::Mutex<Option<ReloadRequest>> = std::sync::Mutex::new(None);

/// Keeps the request a reload handoff carried, when it is well formed.
pub fn accept_handoff(request: Option<ReloadRequest>) {
    if let Some(request) = request.filter(ReloadRequest::is_valid)
        && let Ok(mut slot) = HANDED_OFF.lock()
    {
        *slot = Some(request);
    }
}

/// Completes the request a reload handoff passed, when `session` resumes on
/// the new build.
pub fn complete_handoff(
    paths: &PlakPaths,
    session: &crate::store::sessions::SessionRecord,
    resumed: bool,
) {
    let request = HANDED_OFF.lock().ok().and_then(|mut slot| slot.take());
    if resumed && let Some(request) = request {
        complete(paths, &request, session, crate::build_identity::binary_id());
    }
}

/// The destination half of a reload the session requested: record how it
/// landed and arm the host-authored continuation that wakes the root. A
/// destination that is not the build the request named says so instead of
/// claiming the request was met.
pub fn complete(
    paths: &PlakPaths,
    request: &ReloadRequest,
    session: &crate::store::sessions::SessionRecord,
    binary: &str,
) {
    let landed = if session.id == request.session_id {
        "exactly".to_owned()
    } else {
        format!(
            "and carried into session {} from {}",
            session.id, request.session_id
        )
    };
    if binary != request.expect_binary {
        let reason = format!(
            "the reload landed on build {binary}, not the requested {}",
            request.expect_binary
        );
        set_reload_continuation(format!(
            "Plak reloaded this session onto build {binary} {landed}, but the request named build {}; the requested build is not running. Check the installed build before continuing.",
            request.expect_binary
        ));
        record(
            paths,
            &request.request_id,
            &ReloadOutcome::Refused { reason },
        );
        return;
    }
    set_reload_continuation(format!(
        "Plak reloaded this session onto build {binary} {landed}, as requested. Continue the current task, or reply briefly if nothing remains."
    ));
    record(
        paths,
        &request.request_id,
        &ReloadOutcome::Reloaded {
            binary: binary.to_owned(),
            session: session.id.clone(),
        },
    );
}

fn is_binary_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
