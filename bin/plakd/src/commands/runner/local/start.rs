//! One launch policy per dial; an ambiguous manager result never falls back.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::super::dial::children::{launcher_output, launcher_status};
use super::{LocalRunnerHome, PlakPaths};

const DETACHED_LAUNCH: &str = "__runner-detach";
const DETACHED_WAIT: &str = "__runner-wait";
const DETACHED_START_WINDOW: Duration = Duration::from_secs(1);

pub(super) struct Starter {
    managed: Option<bool>,
    requested: bool,
    detached_retry_at: Option<Instant>,
}

impl Starter {
    pub(super) fn new() -> Self {
        Self {
            managed: None,
            requested: false,
            detached_retry_at: None,
        }
    }

    pub(super) async fn start(
        &mut self,
        paths: &PlakPaths,
        home: &LocalRunnerHome,
    ) -> io::Result<()> {
        if self.requested
            || self
                .detached_retry_at
                .is_some_and(|retry| Instant::now() < retry)
            || !lock_free(paths.home(), &home.socket_path())?
        {
            return Ok(());
        }
        let managed = if let Some(managed) = self.managed {
            managed
        } else {
            let managed = crate::environment::service_managed()
                && !crate::test_hooks::runner_detached()
                && manager_reachable().await;
            self.managed = Some(managed);
            managed
        };
        if managed {
            self.requested = true;
            managed_start(paths).await;
            return Ok(());
        }
        self.detached_retry_at = Some(Instant::now() + DETACHED_START_WINDOW);
        detached_start(paths, home).await
    }
}

fn lock_free(home: &Path, socket: &Path) -> io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;
    super::super::home::private_below(
        home,
        socket
            .parent()
            .ok_or_else(|| io::Error::other("runner socket has no directory"))?,
    )
    .map_err(|error| io::Error::other(error.to_string()))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(socket.with_extension("lock"))?;
    match lock.try_lock() {
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

async fn manager_reachable() -> bool {
    let mut probe = Command::new("systemctl");
    probe
        .args(["--user", "is-system-running"])
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(Ok(output)) =
        tokio::time::timeout(Duration::from_secs(1), launcher_output(&mut probe)).await
    {
        String::from_utf8_lossy(&output).trim() != "offline"
    } else {
        // An ambiguous probe cannot authorize a start inside the host unit.
        true
    }
}

async fn managed_start(paths: &PlakPaths) {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(paths.home().as_os_str().as_encoded_bytes());
    let hash = digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let build = LocalRunnerHome::build_key();
    let unit = format!("plak-runner-{hash}-{build}");
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut manager = Command::new("systemd-run");
    manager
        .args([
            "--user",
            "--collect",
            "--unit",
            &unit,
            "--property=Type=exec",
        ])
        .arg(format!("--setenv=PLAK_HOME={}", paths.home().display()))
        .arg("--setenv=PATH=/usr/bin:/bin");
    if let Some(home) = crate::environment::inherited_home() {
        manager.arg(format!("--setenv=HOME={home}"));
    }
    for (name, value) in crate::test_hooks::upgrade_runner_environment() {
        let mut argument = OsString::from(format!("--setenv={name}="));
        argument.push(value);
        manager.arg(argument);
    }
    manager
        .arg("--")
        .arg(executable)
        .args(["runner", "serve", "--local"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _ = tokio::time::timeout(Duration::from_secs(1), launcher_status(&mut manager)).await;
}

async fn detached_start(paths: &PlakPaths, home: &LocalRunnerHome) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let executable = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.root().join("serve.log"))?;
    let mut command = detached_command(&executable);
    command
        .arg(DETACHED_LAUNCH)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PLAK_HOME", paths.home())
        .envs(crate::test_hooks::upgrade_runner_environment())
        .stderr(log);
    if let Some(home) = crate::environment::inherited_home() {
        command.env("HOME", home);
    }
    let status = launcher_status(&mut command).await?;
    if !status.success() {
        return Err(io::Error::other("detached runner launcher failed"));
    }
    Ok(())
}

fn detached_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .process_group(0);
    command
}

/// The host joins the launcher; the independently parented waiter joins the
/// daemon. Neither step uses a shell, so asynchronous-list signal overrides
/// never reach the daemon or its jobs.
pub(crate) fn detached_helper(arguments: &[OsString]) -> Option<io::Result<()>> {
    let [stage] = arguments else { return None };
    let waiter = if stage == DETACHED_WAIT {
        true
    } else if stage == DETACHED_LAUNCH {
        false
    } else {
        return None;
    };
    Some(std::env::current_exe().and_then(|executable| {
        let mut command = detached_command(&executable);
        if waiter {
            command.args(["runner", "serve", "--local"]).status()?;
        } else {
            // The short-lived launcher surrenders the waiter to the OS.
            drop(command.arg(DETACHED_WAIT).spawn()?);
        }
        Ok(())
    }))
}

#[cfg(test)]
mod tests;
