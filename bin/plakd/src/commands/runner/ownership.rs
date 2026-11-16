//! Home-wide runner custody: hosts share it, standalone composition excludes them.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt as _;

use plakous_hosted::error::{CliFailure, CliResult};

use crate::home::paths::PlakPaths;

#[cfg(any(test, feature = "web"))]
mod host;

#[derive(Debug)]
pub(crate) enum StandaloneRunnerError {
    ImportThroughRunningHost,
    HomeBusy,
    Setup(CliFailure),
}

impl std::fmt::Display for StandaloneRunnerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ImportThroughRunningHost => formatter.write_str(
                "import through the running host, or stop it before a standalone remote import",
            ),
            Self::HomeBusy => formatter.write_str(
                "another standalone runner operation owns this home; retry after it finishes",
            ),
            Self::Setup(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for StandaloneRunnerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Setup(error) => Some(error),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct RunnerOwnership {
    standalone: bool,
    _lock: File,
}

impl RunnerOwnership {
    pub(crate) fn is_standalone(&self) -> bool {
        self.standalone
    }

    pub(crate) fn standalone(paths: &PlakPaths) -> Result<Self, StandaloneRunnerError> {
        let file = lock_file(paths).map_err(StandaloneRunnerError::Setup)?;
        match file.try_lock() {
            Ok(()) => Ok(Self {
                standalone: true,
                _lock: file,
            }),
            Err(TryLockError::Error(error)) => Err(StandaloneRunnerError::Setup(CliFailure::new(
                format!("runner ownership lock: {error}"),
            ))),
            Err(TryLockError::WouldBlock) => match file.try_lock_shared() {
                Ok(()) => Err(StandaloneRunnerError::ImportThroughRunningHost),
                Err(TryLockError::WouldBlock) => Err(StandaloneRunnerError::HomeBusy),
                Err(TryLockError::Error(error)) => Err(StandaloneRunnerError::Setup(
                    CliFailure::new(format!("runner ownership lock: {error}")),
                )),
            },
        }
    }
}

fn lock_file(paths: &PlakPaths) -> CliResult<File> {
    let directory = paths.home().join("runners");
    super::private_directory(&directory)?;
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("runtime-owner.lock"))
        .map_err(|error| CliFailure::new(format!("runner ownership lock: {error}")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated ownership fixtures")]
mod tests {
    use super::*;

    const CHILD_HOME: &str = "PLAK_TEST_RUNNER_CUSTODY_HOME";
    const CHILD_KIND: &str = "PLAK_TEST_RUNNER_CUSTODY_KIND";

    #[test]
    fn custody_holder_process() {
        let Some(home) = std::env::var_os(CHILD_HOME) else {
            return;
        };
        let paths = PlakPaths::explicit(home.into());
        let _owner = if std::env::var(CHILD_KIND).unwrap() == "host" {
            RunnerOwnership::host(&paths).unwrap()
        } else {
            RunnerOwnership::standalone(&paths).unwrap()
        };
        std::fs::write(paths.home().join("held"), b"held").unwrap();
        let _ = std::io::Read::read_to_end(&mut std::io::stdin(), &mut Vec::new());
    }

    struct Holder(std::process::Child);

    impl Drop for Holder {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_killed_holder_releases_custody_without_stale_pid_or_socket_repair() {
        for kind in ["host", "standalone"] {
            let home = tempfile::tempdir().unwrap();
            let paths = PlakPaths::explicit(home.path().to_path_buf());
            let mut child = Holder(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "commands::runner::ownership::tests::custody_holder_process",
                        "--nocapture",
                    ])
                    .env(CHILD_HOME, home.path())
                    .env(CHILD_KIND, kind)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::inherit())
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !home.path().join("held").exists() {
                assert!(child.0.try_wait().unwrap().is_none());
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            match RunnerOwnership::standalone(&paths) {
                Err(StandaloneRunnerError::ImportThroughRunningHost) if kind == "host" => {}
                Err(StandaloneRunnerError::HomeBusy) if kind == "standalone" => {}
                other => panic!("incorrect custody: {other:?}"),
            }
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            assert!(RunnerOwnership::standalone(&paths).is_ok());
            assert!(RunnerOwnership::host(&paths).is_ok());
        }
    }

    #[test]
    fn standalone_custody_excludes_hosts_and_other_standalone_binds() {
        let home = tempfile::tempdir().unwrap();
        let paths = PlakPaths::explicit(home.path().to_path_buf());
        let first = RunnerOwnership::host(&paths).unwrap();
        let second = RunnerOwnership::host(&paths).unwrap();
        assert!(matches!(
            RunnerOwnership::standalone(&paths),
            Err(StandaloneRunnerError::ImportThroughRunningHost)
        ));
        drop(first);
        assert!(matches!(
            RunnerOwnership::standalone(&paths),
            Err(StandaloneRunnerError::ImportThroughRunningHost)
        ));
        drop(second);
        let standalone = RunnerOwnership::standalone(&paths).unwrap();
        assert!(standalone.is_standalone());
        assert!(RunnerOwnership::host(&paths).is_err());
        assert!(matches!(
            RunnerOwnership::standalone(&paths),
            Err(StandaloneRunnerError::HomeBusy)
        ));
        drop(standalone);
        assert!(RunnerOwnership::host(&paths).is_ok());
    }
}
