//! Home-wide runner custody: hosts share it, standalone composition excludes them.

use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::OpenOptionsExt as _;

use plakous_hosted::error::{CliFailure, CliResult};

use crate::home::paths::PlakPaths;

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

    pub(crate) fn host(paths: &PlakPaths) -> CliResult<Self> {
        let file = lock_file(paths)?;
        file.try_lock_shared().map_err(|error| match error {
            TryLockError::WouldBlock => CliFailure::new(
                "a standalone runner operation owns this home; retry after it finishes",
            ),
            TryLockError::Error(error) => {
                CliFailure::new(format!("runner ownership lock: {error}"))
            }
        })?;
        Ok(Self {
            standalone: false,
            _lock: file,
        })
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
