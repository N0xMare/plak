//! One launch policy per dial; an ambiguous manager result never falls back.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{LocalRunnerHome, PlakPaths, private_directory};

pub(super) struct Starter {
    managed: Option<bool>,
    requested: bool,
    child: Arc<AtomicBool>,
}

impl Starter {
    pub(super) fn new() -> Self {
        Self {
            managed: None,
            requested: false,
            child: Arc::default(),
        }
    }

    pub(super) async fn start(
        &mut self,
        paths: &PlakPaths,
        home: &LocalRunnerHome,
    ) -> std::io::Result<()> {
        if self.requested || self.child.load(Ordering::Acquire) || !lock_free(&home.socket_path())?
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
        detached_start(paths, home, &self.child)
    }
}

fn lock_free(socket: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::OpenOptionsExt as _;
    private_directory(
        socket
            .parent()
            .ok_or_else(|| std::io::Error::other("runner socket has no directory"))?,
    )
    .map_err(|error| std::io::Error::other(error.to_string()))?;
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
    let mut probe = tokio::process::Command::new("systemctl");
    probe
        .args(["--user", "is-system-running"])
        .kill_on_drop(true);
    if let Ok(Ok(output)) = tokio::time::timeout(Duration::from_secs(1), probe.output()).await {
        String::from_utf8_lossy(&output.stdout).trim() != "offline"
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
    let unit = format!("plak-runner-{hash}-v{}", super::super::version().protocol);
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let mut manager = tokio::process::Command::new("systemd-run");
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
    manager
        .arg("--")
        .arg(executable)
        .args(["runner", "serve", "--local"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let _ = tokio::time::timeout(Duration::from_secs(1), manager.status()).await;
}

fn detached_start(
    paths: &PlakPaths,
    home: &LocalRunnerHome,
    pending: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let executable = std::env::current_exe()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.root().join("serve.log"))?;
    let mut command = std::process::Command::new(&executable);
    command
        .args(["runner", "serve", "--local"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PLAK_HOME", paths.home())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log);
    if let Some(home) = crate::environment::inherited_home() {
        command.env("HOME", home);
    }
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let child = command.spawn()?;
    reap(child, pending);
    Ok(())
}

fn reap(mut child: std::process::Child, pending: &Arc<AtomicBool>) {
    pending.store(true, Ordering::Release);
    let pending = Arc::clone(pending);
    std::thread::spawn(move || {
        let _ = child.wait();
        pending.store(false, Ordering::Release);
    });
}

#[cfg(test)]
mod tests;
