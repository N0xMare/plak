//! Live state-key hosts are proved by sockets and kernel-held locks, never pid text alone.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{CliFailure, CliResult, product_paths::PlakPaths};

const MAX_HOLDER_BYTES: u64 = 4096;

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct LiveHost {
    pub(crate) key: String,
    pub(crate) holder: Option<String>,
}

pub(crate) fn home_lock(paths: &PlakPaths) -> PathBuf {
    paths.home().join("host/host.lock")
}

/// Reads all state keys, including a starting host that has not bound its
/// socket. The separate home-wide lock is not one more state-key host.
pub(crate) fn live(paths: &PlakPaths) -> CliResult<Vec<LiveHost>> {
    live_until(paths, Instant::now() + Duration::from_secs(10))
}

pub(crate) fn live_until(paths: &PlakPaths, deadline: Instant) -> CliResult<Vec<LiveHost>> {
    if !directory(paths.home(), 0o077)? {
        return Ok(Vec::new());
    }
    let mut hosts = BTreeMap::<String, LiveHost>::new();
    for entry in entries(&paths.host_run_dir(), 0o077)? {
        let name = entry.file_name().into_string().map_err(|_| {
            CliFailure::new("a host socket name is not UTF-8; host absence is unproven")
        })?;
        let Some(key) = name.strip_suffix(".sock") else {
            continue;
        };
        match socket_until(&entry.path(), deadline) {
            Ok(_) => {
                hosts.insert(
                    key.to_owned(),
                    LiveHost {
                        key: key.to_owned(),
                        holder: None,
                    },
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) => {}
            Err(error) => {
                return Err(CliFailure::new(format!(
                    "host discovery could not verify {name}: {error}"
                )));
            }
        }
    }
    for entry in entries(&paths.state_root(), 0o022)? {
        let kind = entry.file_type().map_err(failed)?;
        if kind.is_symlink() {
            return Err(CliFailure::new(
                "a host state directory is a symlink; host absence is unproven",
            ));
        }
        if !kind.is_dir() {
            continue;
        }
        let Some(holder) = lock_holder(&entry.path().join("host.lock"))? else {
            continue;
        };
        let key = entry.file_name().into_string().map_err(|_| {
            CliFailure::new("a held host state key is not UTF-8; host absence is unproven")
        })?;
        hosts
            .entry(key.clone())
            .or_insert_with(|| LiveHost { key, holder: None })
            .holder = Some(holder);
    }
    Ok(hosts.into_values().collect())
}

/// Unlike the legacy client transport, discovery cannot park on a full
/// listener backlog: it must prove a host or refuse within its budget.
pub(crate) fn socket_until(
    path: &Path,
    deadline: Instant,
) -> io::Result<std::os::unix::net::UnixStream> {
    super::verified_socket_with(path, |path| {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "host discovery deadline expired",
            ));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let stream = runtime.block_on(async {
            tokio::time::timeout(remaining, tokio::net::UnixStream::connect(path))
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "host socket did not connect before the discovery deadline",
                    )
                })?
        })?;
        let stream = stream.into_std()?;
        stream.set_nonblocking(false)?;
        Ok(stream)
    })
}

pub(crate) fn conflict(hosts: &[LiveHost]) -> CliFailure {
    let holders = hosts
        .iter()
        .map(|host| {
            let key = crate::cli_output::safe_terminal_text(&host.key, 200);
            host.holder.as_ref().map_or_else(
                || format!("state key {key}"),
                |holder| format!("state key {key}, holder {holder}"),
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    CliFailure::new(format!(
        "plak host is already running for this home ({holders}); stop the existing host or its unit before starting another state key; no host was stopped"
    ))
}

/// A held kernel lock proves activity even when the diagnostic pid is stale.
/// A free lock proves nothing is held, even when its text names a live pid.
pub(crate) fn lock_holder(path: &Path) -> CliResult<Option<String>> {
    use rustix::fs::{Mode, OFlags};
    let fd = match rustix::fs::open(
        path,
        OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(failed(error.into())),
    };
    let mut file = File::from(fd);
    let metadata = file.metadata().map_err(failed)?;
    if !metadata.file_type().is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_HOLDER_BYTES
    {
        return Err(CliFailure::new(
            "host lock is not this user's bounded private regular file; host absence is unproven",
        ));
    }
    match file.try_lock() {
        Ok(()) => {
            file.unlock().map_err(failed)?;
            Ok(None)
        }
        Err(std::fs::TryLockError::WouldBlock) => {
            let mut holder = String::new();
            (&mut file)
                .take(MAX_HOLDER_BYTES + 1)
                .read_to_string(&mut holder)
                .map_err(failed)?;
            if holder.len() as u64 > MAX_HOLDER_BYTES {
                return Err(CliFailure::new(
                    "host lock diagnostic grew beyond its bound",
                ));
            }
            Ok(Some(crate::cli_output::safe_terminal_text(
                holder.trim(),
                200,
            )))
        }
        Err(std::fs::TryLockError::Error(error)) => Err(failed(error)),
    }
}

fn directory(path: &Path, forbidden_mode: u32) -> CliResult<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(failed(error)),
    };
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & forbidden_mode != 0
    {
        return Err(CliFailure::new(format!(
            "{} is not a safely owned host discovery directory",
            path.display()
        )));
    }
    Ok(true)
}

fn entries(path: &Path, forbidden_mode: u32) -> CliResult<Vec<std::fs::DirEntry>> {
    if !directory(path, forbidden_mode)? {
        return Ok(Vec::new());
    }
    std::fs::read_dir(path)
        .map_err(failed)?
        .collect::<io::Result<Vec<_>>>()
        .map_err(failed)
}

fn failed(error: io::Error) -> CliFailure {
    CliFailure::new(format!(
        "host discovery could not establish absence: {error}"
    ))
}

#[cfg(test)]
mod tests;
