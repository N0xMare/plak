//! The host's journaled connection to its home-local process daemon.

use std::sync::Arc;
use std::time::Duration;

use plakous::runner::{ClientWindows, ConnectError, FileCallJournal, RunnerClient};
use plakous_hosted::error::{CliFailure, CliResult};

use super::home::{LocalRunnerHome, private_directory};
use crate::home::paths::PlakPaths;

/// Dials before starting, and retries both daemon-exit races under one bound.
pub(crate) async fn dial(paths: &PlakPaths) -> CliResult<RunnerClient> {
    let home = LocalRunnerHome::of(paths);
    plakous::runner::daemon::check_socket_path(&home.socket_path())
        .map_err(|error| CliFailure::new(error.to_string()))?;
    private_directory(home.root())?;
    let journal = Arc::new(
        FileCallJournal::open(
            home.root()
                .join("calls")
                .join(crate::home::state::state_key()),
        )
        .map_err(|error| CliFailure::new(format!("the local runner journal failed: {error}")))?,
    );
    let bind = super::bind(super::host_id(paths)?, false);
    let attempt = async {
        loop {
            match tokio::net::UnixStream::connect(home.socket_path()).await {
                Ok(stream) => {
                    match RunnerClient::connect_home_local(
                        stream,
                        bind.clone(),
                        ClientWindows::default(),
                        Some(journal.clone()),
                    )
                    .await
                    {
                        Ok((client, _)) => return Ok(client),
                        // An idle daemon can exit with a connection queued.
                        Err(ConnectError::Closed) => {}
                        Err(error) => return Err(CliFailure::new(error.to_string())),
                    }
                }
                Err(_) => {
                    // A previous starter can lose to a daemon that removed
                    // its socket but has not yet released its lock. Starting
                    // once, then only connecting, would never recover.
                    let _ = start(paths, &home).await;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(super::START_WAIT, attempt)
        .await
        .map_err(|_| CliFailure::new("the home-local runner did not answer within 10 s"))?
}

async fn start(paths: &PlakPaths, home: &LocalRunnerHome) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let executable = std::env::current_exe()?;
    if std::env::var_os("INVOCATION_ID").is_some() && !force_detached() {
        use sha2::{Digest as _, Sha256};
        let digest = Sha256::digest(paths.home().as_os_str().as_encoded_bytes());
        let hash = digest[..12]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let unit = format!("plak-runner-{hash}-v{}", super::version().protocol);
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
            .arg(&executable)
            .args(["runner", "serve", "--local"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if matches!(tokio::time::timeout(Duration::from_secs(1), manager.status()).await, Ok(Ok(status)) if status.success())
        {
            return Ok(());
        }
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(home.root().join("serve.log"))?;
    let mut command = std::process::Command::new(&executable);
    command.args(["runner", "serve", "--local"]);
    command
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
    command.spawn().map(drop)
}

fn force_detached() -> bool {
    #[cfg(feature = "test-hooks")]
    {
        std::env::var_os("PLAK_TEST_RUNNER_DETACHED").as_deref() == Some(std::ffi::OsStr::new("1"))
    }
    #[cfg(not(feature = "test-hooks"))]
    {
        false
    }
}
