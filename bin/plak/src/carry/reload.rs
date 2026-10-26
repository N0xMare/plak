//! Reload probes and snapshot handoff for a carry across builds.

use super::document::IMPORTABLE_VERSIONS;
#[cfg(any(test, feature = "tui"))]
use super::document::{VERSION, write_document};
#[cfg(any(test, feature = "tui"))]
use super::export::{export_session, new_carry_file};
#[cfg(any(test, feature = "tui"))]
use crate::cli_output::safe_terminal_block;
use crate::home::paths::PlakPaths;
#[cfg(any(test, feature = "tui"))]
use crate::shell::services::Services;
use serde::{Deserialize, Serialize};
#[cfg(any(test, feature = "tui"))]
use std::fs;
#[cfg(any(test, feature = "tui"))]
use std::path::{Path, PathBuf};
#[cfg(any(test, feature = "tui"))]
use std::time::{Duration, Instant};

/// What `/reload` does for the session it is leaving.
#[derive(Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "tui"))]
pub(crate) enum ReloadPlan {
    /// The shell never committed a session: start the new build fresh.
    Fresh,
    /// Same state key: the new build opens the session as it is.
    Resume,
    /// Another state key: export here, import there.
    Carry,
    /// The new build did not say its state key: resume plainly and say so.
    ResumeUnverified,
}

/// Chooses the reload path from facts alone, so it is testable without exec.
#[cfg(any(test, feature = "tui"))]
pub(crate) fn reload_plan(
    resumable: bool,
    running_key: &str,
    installed_key: Option<&str>,
) -> ReloadPlan {
    if !resumable {
        return ReloadPlan::Fresh;
    }
    match installed_key {
        Some(key) if key == running_key => ReloadPlan::Resume,
        Some(_) => ReloadPlan::Carry,
        None => ReloadPlan::ResumeUnverified,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct CarryCapacityProbe {
    pub(crate) route: String,
    pub(crate) total_window: Option<u64>,
    pub(crate) admitted_input: Option<u64>,
    pub(crate) estimated_tokens: u64,
    pub(crate) messages: usize,
}

impl CarryCapacityProbe {
    pub(crate) fn warning(&self) -> Option<String> {
        (self.admitted_input.is_some_and(|input|self.estimated_tokens > input)).then(|| format!(
            "estimated history ~{} tokens exceeds destination route {} input window {} tokens (total window {}); the first prompt will refuse unless the history compacts",
            self.estimated_tokens,self.route,self.admitted_input.unwrap_or(0),
            self.total_window.map_or_else(|| "not reported".to_owned(), |n|format!("{n} tokens")),
        ))
    }
}

/// The installed `plak` a reload would start, and the state key it answered.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "tui"))]
pub(crate) struct ReloadTarget {
    pub(crate) executable: PathBuf,
    pub(crate) capacity: Option<Box<CarryCapacityProbe>>,
    pub(crate) capacity_error: Option<String>,
    /// `None` when the build did not answer in time or with one key.
    pub(crate) state_key: Option<String>,
    pub(crate) probe: Option<ReloadProbe>,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ReloadProbe {
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) config_error: Option<String>,
}

pub(crate) fn reload_probe(paths: &PlakPaths) -> ReloadProbe {
    ReloadProbe {
        state_key: crate::home::state::state_key(),
        export_versions: IMPORTABLE_VERSIONS.to_vec(),
        config_error: crate::product_config::SessionPolicy::load(paths).err(),
    }
}

#[cfg(any(test, feature = "tui"))]
impl ReloadTarget {
    pub(crate) fn refusal(&self, running_key: &str) -> Option<String> {
        if let Some(error) = &self.capacity_error {
            return Some(error.clone());
        }
        let probe = self.probe.as_ref()?;
        if let Some(error) = &probe.config_error {
            return Some(format!(
                "reload kept this session open: {error}; fix the named file and retry /reload"
            ));
        }
        if probe.state_key != running_key && !probe.export_versions.contains(&VERSION) {
            return Some(format!(
                "reload kept this session open: the new build cannot import this build's version {VERSION} export; install a build that imports it and retry /reload"
            ));
        }
        None
    }
}

/// Resolves the installed `plak` and asks its state key; `None` when nothing
/// is installed on `PATH`.
#[cfg(any(test, feature = "tui"))]
fn reload_target(workspace: &Path) -> Option<ReloadTarget> {
    let executable = crate::build_identity::installed_executable()?;
    Some(probe_reload_target(executable, workspace))
}

#[cfg(any(test, feature = "tui"))]
fn probe_reload_target(executable: PathBuf, workspace: &Path) -> ReloadTarget {
    probe_reload_target_with_timeout(executable, workspace, STATE_KEY_TIMEOUT)
}

#[cfg(any(test, feature = "tui"))]
fn probe_reload_target_with_timeout(
    executable: PathBuf,
    workspace: &Path,
    timeout: Duration,
) -> ReloadTarget {
    let mut command = std::process::Command::new(&executable);
    command
        .arg("version")
        .arg("--reload-probe")
        .current_dir(workspace);
    let Some(output) = run_bounded(&mut command, timeout).ok().flatten() else {
        // A hung destination has spent its budget; do not ask it a second question.
        return ReloadTarget {
            executable,
            capacity: None,
            capacity_error: None,
            state_key: None,
            probe: None,
        };
    };
    let probe = Some(output)
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice::<ReloadProbe>(&output.stdout).ok())
        .filter(|probe| is_state_key(&probe.state_key));
    let state_key = probe
        .as_ref()
        .map(|probe| probe.state_key.clone())
        .or_else(|| installed_state_key(&executable));
    ReloadTarget {
        executable,
        capacity: None,
        capacity_error: None,
        state_key,
        probe,
    }
}

/// Resolves carry capacity while the old session is still attached.
#[cfg(any(test, feature = "tui"))]
pub(crate) fn reload_target_for_session(
    workspace: &Path,
    session_id: Option<&str>,
) -> Option<ReloadTarget> {
    let mut target = reload_target(workspace)?;
    if target.refusal(&crate::home::state::state_key()).is_some()
        || target.state_key.as_deref() == Some(crate::home::state::state_key().as_str())
        || target.state_key.is_none()
    {
        return Some(target);
    }
    let Some(id) = session_id else {
        return Some(target);
    };
    let inspect = || -> Result<CarryCapacityProbe, String> {
        let services = Services::from_env(None);
        let paths = services.paths().map_err(|e| e.message().to_owned())?;
        let record = services
            .session_records()
            .map_err(|e| e.message().to_owned())?
            .load(id)?;
        let document = export_session(paths, &record).map_err(|e| e.to_string())?;
        let path = new_carry_file(paths, id)?;
        write_document(&path, &document)?;
        let mut command = std::process::Command::new(&target.executable);
        command
            .arg("version")
            .arg("--carry-probe")
            .arg(&path)
            .current_dir(workspace);
        let output = run_bounded(&mut command, SNAPSHOT_EXPORT_TIMEOUT);
        let _ = fs::remove_file(&path);
        let output = output
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "destination capacity inspection timed out".to_owned())?;
        if !output.status.success() {
            let cause = String::from_utf8_lossy(&output.stderr)
                .lines()
                .next()
                .unwrap_or("destination gave no diagnostic")
                .chars()
                .take(512)
                .collect::<String>();
            let cause = safe_terminal_block(&cause);
            return Err(if output.status.code() == Some(2) {
                format!("destination build too old for v8 carry: {cause}")
            } else {
                format!(
                    "destination route capacity inspection failed: {cause}; fix configuration/auth and retry /reload"
                )
            });
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|_| "destination did not report carry capacity".to_owned())
    };
    match inspect() {
        Ok(capacity) => target.capacity = Some(Box::new(capacity)),
        Err(error) => {
            target.capacity_error = Some(format!("reload kept this session open: {error}"));
        }
    }
    Some(target)
}

/// How long an installed build gets to answer `version --state-key`.
#[cfg(any(test, feature = "tui"))]
const STATE_KEY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a kept snapshot gets to export one session.
#[cfg(any(test, feature = "tui"))]
const SNAPSHOT_EXPORT_TIMEOUT: Duration = Duration::from_secs(120);

/// Asks `executable` for its state key with the hidden `version --state-key`.
/// A build that predates the question, answers late, or answers with anything
/// but one key reads as unknown.
#[cfg(any(test, feature = "tui"))]
fn installed_state_key(executable: &Path) -> Option<String> {
    let mut command = std::process::Command::new(executable);
    command.arg("version").arg("--state-key");
    let output = run_bounded(&mut command, STATE_KEY_TIMEOUT).ok()??;
    if !output.status.success() {
        return None;
    }
    let key = String::from_utf8(output.stdout).ok()?;
    let key = key.trim();
    is_state_key(key).then(|| key.to_owned())
}

/// Runs `command` with no stdin and captured output, killing it once
/// `timeout` passes. `Ok(None)` means it was killed.
#[cfg(any(test, feature = "tui"))]
fn run_bounded(
    command: &mut std::process::Command,
    timeout: Duration,
) -> std::io::Result<Option<std::process::Output>> {
    use std::io::Read as _;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(status.map(|status| std::process::Output {
        status,
        stdout,
        stderr,
    }))
}

#[cfg(any(test, feature = "tui"))]
fn is_state_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

/// The newest kept snapshot whose `state-key` manifest names `key`, under
/// any of `roots` (the maintainer and release snapshot roots). Only installed
/// snapshot directories count (a 64-digit hex name, not a staging copy), and
/// only when this user owns them and their executable and nobody else can
/// write either.
#[cfg(any(test, feature = "tui"))]
pub(super) fn snapshot_for_key(roots: &[PathBuf], key: &str) -> Option<PathBuf> {
    roots
        .iter()
        .filter_map(|root| fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if name.len() != 64
                || !name
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return None;
            }
            let dir = entry.path();
            let executable = dir.join("plak");
            let dir_metadata = fs::symlink_metadata(&dir).ok()?;
            let executable_metadata = fs::symlink_metadata(&executable).ok()?;
            if !dir_metadata.is_dir()
                || !executable_metadata.is_file()
                || !super::document::privately_owned(&dir_metadata)
                || !super::document::privately_owned(&executable_metadata)
            {
                return None;
            }
            let manifest = fs::read_to_string(dir.join("state-key")).ok()?;
            (manifest.trim() == key).then(|| Some((dir_metadata.modified().ok()?, executable)))?
        })
        .max_by(|left, right| left.0.cmp(&right.0))
        .map(|(_, executable)| executable)
}

/// Owned by this user and writable by nobody else.
/// Runs `snapshot session export <id> --out <out>` in `workspace`, so the
/// build that owns the session writes its export. A snapshot that does not
/// finish in time is killed and the export fails.
#[cfg(any(test, feature = "tui"))]
pub(super) fn export_with_snapshot(
    snapshot: &Path,
    session_id: &str,
    out: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let result = export_with_snapshot_inner(snapshot, session_id, out, workspace);
    if result.is_err() {
        let _ = fs::remove_file(out);
    }
    result
}

#[cfg(any(test, feature = "tui"))]
fn export_with_snapshot_inner(
    snapshot: &Path,
    session_id: &str,
    out: &Path,
    workspace: &Path,
) -> Result<(), String> {
    let mut command = std::process::Command::new(snapshot);
    command
        .arg("session")
        .arg("export")
        .arg(session_id)
        .arg("--out")
        .arg(out)
        .current_dir(workspace);
    let output = run_bounded(&mut command, SNAPSHOT_EXPORT_TIMEOUT)
        .map_err(|error| format!("could not run {}: {error}", snapshot.display()))?
        .ok_or_else(|| {
            format!(
                "{} did not export session {session_id} within {} seconds",
                snapshot.display(),
                SNAPSHOT_EXPORT_TIMEOUT.as_secs()
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} could not export session {session_id}: {}",
        snapshot.display(),
        stderr.trim()
    ))
}

#[cfg(any(test, feature = "tui"))]
pub(super) fn snapshot_export_version(snapshot: &Path) -> Result<Option<u32>, String> {
    let Some(parent) = snapshot.parent() else {
        return Ok(None);
    };
    let path = parent.join("export-version");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not read {}: {error}", path.display())),
    };
    Ok(text.trim().parse::<u32>().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
