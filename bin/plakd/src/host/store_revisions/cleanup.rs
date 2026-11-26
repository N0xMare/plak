//! Remove only the known scratch files of abandoned revision attempts.

use std::fs::{self, File};
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use crate::home::paths::PlakPaths;
use crate::store::revisions::STORE_STEMS;

pub(super) fn lock_marker(path: &Path, exclusive: bool) -> Option<File> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .ok()?;
    let file = File::from(fd);
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return None;
    }
    if exclusive {
        file.try_lock().ok()?;
    } else {
        file.try_lock_shared().ok()?;
    }
    Some(file)
}

/// Runs while startup holds the home and state locks. A surviving probe
/// subprocess can still own a shared marker lock after a crashed source.
pub(super) fn stale_copies(paths: &PlakPaths) {
    for entry in directories(&paths.state_dir()) {
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| staging_name(name) || abandoned_runtime_staging(name))
        {
            discard_sqlite(&entry.path(), "copy.sqlite3");
            let _ = fs::remove_dir(entry.path());
        }
    }
    for entry in directories(paths.home()) {
        let name = entry.file_name();
        if name.to_str().is_some_and(probe_name) {
            discard_probe(&entry.path());
        }
    }
}

fn directories(parent: &Path) -> impl Iterator<Item = fs::DirEntry> {
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
}

fn staging_name(name: &str) -> bool {
    STORE_STEMS.into_iter().any(|stem| {
        name.strip_prefix(&format!(".{stem}-backup-"))
            .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok_and(|id| id.to_string() == uuid))
    })
}

fn abandoned_runtime_staging(name: &str) -> bool {
    let Some((pid, nonce)) = name
        .strip_prefix(".runtime-bootstrap-")
        .and_then(|name| name.split_once('-'))
    else {
        return false;
    };
    let Some(pid) = pid
        .parse::<i32>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == pid)
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    nonce.parse::<u64>().is_ok_and(|n| n.to_string() == nonce)
        && rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH)
}

fn probe_name(name: &str) -> bool {
    name.strip_prefix(".tmp").is_some_and(|suffix| {
        suffix.len() == 6 && suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
    })
}

fn discard_probe(directory: &Path) {
    let stores = directory.join("stores");
    if !stores
        .symlink_metadata()
        .is_ok_and(|metadata| metadata.is_dir())
    {
        return;
    }
    let marker = directory.join("scratch-request");
    let Some(_probe) = lock_marker(&marker, true) else {
        return;
    };
    for stem in STORE_STEMS {
        discard_sqlite(&stores, &format!("{stem}.sqlite3"));
    }
    discard_sqlite(&stores, "runtime.sqlite3");
    discard_sqlite(&stores, "runtime.pre-codec-bootstrap.sqlite3");
    if fs::remove_dir(&stores).is_ok() {
        let _ = fs::remove_file(directory.join("service.json"));
        let _ = fs::remove_file(marker);
        let _ = fs::remove_dir(directory);
    }
}

fn discard_sqlite(directory: &Path, file: &str) {
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(directory.join(format!("{file}{suffix}")));
    }
}

#[cfg(test)]
mod tests;
