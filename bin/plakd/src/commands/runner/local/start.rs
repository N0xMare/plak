//! One launch policy per dial; an ambiguous manager result never falls back.

use std::io::{self, Read as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::super::dial::children::{launcher_output, launcher_status};
use super::{LocalRunnerHome, PlakPaths};

pub(super) struct Starter {
    managed: Option<bool>,
    requested: bool,
    detached: Option<UnixStream>,
}

impl Starter {
    pub(super) fn new() -> Self {
        Self {
            managed: None,
            requested: false,
            detached: None,
        }
    }

    pub(super) async fn start(
        &mut self,
        paths: &PlakPaths,
        home: &LocalRunnerHome,
    ) -> io::Result<()> {
        if self.requested
            || detached_pending(&mut self.detached)
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
        self.detached = Some(detached_start(paths, home).await?);
        Ok(())
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
        let mut argument = std::ffi::OsString::from(format!("--setenv={name}="));
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

async fn detached_start(paths: &PlakPaths, home: &LocalRunnerHome) -> io::Result<UnixStream> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let executable = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.root().join("serve.log"))?;
    let mut command = detached_launcher(&executable);
    command
        .args(["runner", "serve", "--local"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PLAK_HOME", paths.home())
        .envs(crate::test_hooks::upgrade_runner_environment())
        .stdin(Stdio::null())
        .stderr(log);
    if let Some(home) = crate::environment::inherited_home() {
        command.env("HOME", home);
    }
    launch_detached(&mut command).await
}

fn detached_launcher(executable: &Path) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args([
            "-c",
            "( \"$@\" </dev/null >/dev/null; exit \"$?\" ) &",
            "runner-launch",
        ])
        .arg(executable);
    command
}

async fn launch_detached(command: &mut Command) -> io::Result<UnixStream> {
    // Join the launcher, not the daemon. The background shell waits on the
    // daemon outside the host's parentage and holds a write end until exit;
    // a dial never has to guess whether its previous attempt is still live.
    let (read, write) = UnixStream::pair()?;
    read.set_nonblocking(true)?;
    command
        .process_group(0)
        .stdout(Stdio::from(OwnedFd::from(write)));
    let status = launcher_status(command).await?;
    if !status.success() {
        return Err(io::Error::other("detached runner launcher failed"));
    }
    Ok(read)
}

fn detached_pending(stream: &mut Option<UnixStream>) -> bool {
    let pending = stream
        .as_mut()
        .is_some_and(|stream| match stream.read(&mut [0]) {
            Ok(count) => count != 0,
            Err(error) => matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ),
        });
    if !pending {
        *stream = None;
    }
    pending
}

#[cfg(test)]
mod tests;
