//! The private sidecar a reloading build leaves for the build it starts.
//!
//! It carries what the export document does not: why a reload carried. The
//! old build writes it 0600 under the home's private carry directory and names
//! it with the hidden `--reload-handoff <path>` argument (the writer is the
//! interactive reload's); the destination reads it once and deletes it. It is never part of the export document, and
//! a process started without that argument reads none.

use super::document::privately_owned;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const VERSION: u32 = 1;
const MAX_BYTES: u64 = 64 * 1024;
const MAX_CAUSE_CHARS: usize = 1024;

/// What a reload hands the build it starts. Fields are additive; a reader
/// ignores fields it does not know and refuses another version.
#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct ReloadHandoff {
    version: u32,
    /// Why the reload carried, as the one notice the successor records.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cause: Option<String>,
}

impl Default for ReloadHandoff {
    fn default() -> Self {
        Self {
            version: VERSION,
            cause: None,
        }
    }
}

impl ReloadHandoff {
    /// Reads and deletes the sidecar at `path`. Anything but a private regular
    /// file this user owns, directly under `home`'s carry directory, within
    /// the size bound and of this version reads as no handoff.
    pub(crate) fn take(path: &Path, home: &Path) -> Option<Self> {
        let bytes = read_private(path, home);
        if bytes.is_some() {
            let _ = fs::remove_file(path);
        }
        let handoff: Self = serde_json::from_slice(&bytes?).ok()?;
        (handoff.version == VERSION).then(|| handoff.bounded())
    }

    fn bounded(mut self) -> Self {
        self.cause = self.cause.as_deref().and_then(|cause| {
            let line = cause.lines().next()?.trim();
            (!line.is_empty()).then(|| line.chars().take(MAX_CAUSE_CHARS).collect())
        });
        self
    }
}

/// Opens `path` without following a link and validates the opened file, so
/// what is read is what was checked: a regular file this user owns, private
/// to them and within the size bound, directly in `home`'s carry directory,
/// which must itself be private and this user's.
fn read_private(path: &Path, home: &Path) -> Option<Vec<u8>> {
    use rustix::fs::{FileType, Mode, OFlags};
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    let carry = fs::canonicalize(home.join("carry")).ok()?;
    let directory = fs::metadata(&carry).ok()?;
    if !directory.is_dir() || !privately_owned(&directory) || directory.mode() & 0o077 != 0 {
        return None;
    }
    if path
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok())
        != Some(carry)
    {
        return None;
    }
    let file = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .ok()?;
    let stat = rustix::fs::fstat(&file).ok()?;
    let exposed = Mode::from_raw_mode(stat.st_mode).intersects(Mode::RWXG | Mode::RWXO);
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || exposed
        || u64::try_from(stat.st_size)
            .ok()
            .is_none_or(|size| size > MAX_BYTES)
    {
        return None;
    }
    let mut bytes = Vec::new();
    fs::File::from(file)
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= MAX_BYTES).then_some(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
