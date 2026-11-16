//! Installed-build inspection for host reload observations; no terminal re-exec.

use super::document::{VERSION, write_document};
use super::export::{export_session, new_carry_file};
use super::reload::{CarryCapacityProbe, ReloadProbe, ReloadTarget, ReopenProbe};
use crate::cli_output::safe_terminal_block;
use crate::home::paths::PlakPaths;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

impl ReloadTarget {
    pub(crate) fn refusal(&self, running_key: &str) -> Option<String> {
        if let Some(error) = &self.capacity_error {
            return Some(error.clone());
        }
        let probe = self.probe.as_ref()?;
        if let Some(error) = &probe.config_error {
            return Some(format!(
                "reload kept this session open: {error}; fix the named file and retry the host upgrade"
            ));
        }
        if probe.state_key != running_key && !probe.export_versions.contains(&VERSION) {
            return Some(format!(
                "reload kept this session open: the new build cannot import this build's version {VERSION} export; install a build that imports it and retry the host upgrade"
            ));
        }
        if let Some(ReopenProbe::Refused { reason }) = self.reopen.as_deref() {
            return Some(format!(
                "reload kept this session open: the installed build would not reopen it: {}",
                safe_terminal_block(reason)
            ));
        }
        None
    }

    /// Whether a reload onto this build carries into a new session (another
    /// state key, or a binding the build derives differently) rather than
    /// resuming this one.
    pub(crate) fn carries(&self, running_key: &str) -> bool {
        self.state_key
            .as_deref()
            .is_some_and(|key| key != running_key || self.binding_change().is_some())
    }

    /// The frozen binding input the installed build derives differently, when
    /// exact reopen there would refuse for that reason.
    pub(crate) fn binding_change(&self) -> Option<&str> {
        match self.reopen.as_deref() {
            Some(ReopenProbe::BindingChanged { change }) => Some(change),
            _ => None,
        }
    }
}

/// Resolves the installed `plak` and asks its state key; `None` when nothing
/// is installed on `PATH`.
fn reload_target(workspace: &Path) -> Option<ReloadTarget> {
    let executable = crate::build_identity::installed_executable()?;
    Some(probe_reload_target(executable, workspace))
}

fn probe_reload_target(executable: PathBuf, workspace: &Path) -> ReloadTarget {
    // Model executable-launch latency (for example, an OS first-run scan),
    // outside the running child's response budget.
    if let Some(delay) = crate::reload_probe_latency::DELAY.get() {
        std::thread::sleep(*delay);
    }
    probe_reload_target_with_timeout(executable, workspace, STATE_KEY_TIMEOUT)
}

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
            reopen: None,
            request: None,
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
        reopen: None,
        request: None,
    }
}

/// Resolves carry capacity while the old session is still attached, from
/// the session's product context `paths`.
pub(crate) fn reload_target_for_session(
    paths: &PlakPaths,
    workspace: &Path,
    session_id: Option<&str>,
) -> Option<ReloadTarget> {
    let mut target = reload_target(workspace)?;
    let running_key = crate::home::state::state_key();
    if target.refusal(&running_key).is_some() || target.state_key.is_none() {
        return Some(target);
    }
    let Some(id) = session_id else {
        return Some(target);
    };
    if target.state_key.as_deref() == Some(running_key.as_str()) {
        target.reopen = reopen_verdict(&target.executable, workspace, id).map(Box::new);
        // Only a carry needs the destination's capacity inspection.
        if target.binding_change().is_none() {
            return Some(target);
        }
    }
    let inspect = || -> Result<CarryCapacityProbe, String> {
        let record = crate::store::ProductStore::open(paths)
            .map_err(|error| error.to_string())?
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
            return Err(carry_probe_failure(
                output.status.code(),
                &safe_terminal_block(&cause),
            ));
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

/// This build's refusal of a reload onto `target`: the target's own, or the
/// work `in_flight` when the reload carries into another session.
pub(crate) fn reload_refusal(
    target: &ReloadTarget,
    running_key: &str,
    in_flight: Option<&'static str>,
) -> Option<String> {
    target.refusal(running_key).or_else(|| {
        target
            .carries(running_key)
            .then_some(in_flight)
            .flatten()
            .map(str::to_owned)
    })
}

/// A destination that exits 2 does not know the carry probe, so it predates
/// this build's export version; any other failure is its route's own refusal.
fn carry_probe_failure(code: Option<i32>, cause: &str) -> String {
    if code == Some(2) {
        format!("destination build too old for v{VERSION} carry: {cause}")
    } else {
        format!(
            "destination route capacity inspection failed: {cause}; fix configuration/auth and retry the host upgrade"
        )
    }
}

/// Asks the installed build whether exact reopen of `id` would attach there.
/// A build that predates the question, fails, or does not answer in time
/// gives no verdict, and the reload resumes plainly as before.
fn reopen_verdict(executable: &Path, workspace: &Path, id: &str) -> Option<ReopenProbe> {
    let mut command = std::process::Command::new(executable);
    command
        .arg("version")
        .arg("--reopen-probe")
        .arg(id)
        .current_dir(workspace);
    let output = run_bounded(&mut command, REOPEN_PROBE_TIMEOUT).ok()??;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

/// Asks `executable` for its state key with the hidden `version --state-key`.
/// A build that predates the question, answers late, or answers with anything
/// but one key reads as unknown.
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

fn is_state_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
}

const REOPEN_PROBE_TIMEOUT: Duration = Duration::from_secs(60);
const STATE_KEY_TIMEOUT: Duration = Duration::from_secs(5);
const SNAPSHOT_EXPORT_TIMEOUT: Duration = Duration::from_secs(120);

#[cfg(test)]
mod tests;
