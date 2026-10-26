//! Best-effort cleanup of obsolete retained reference files.

use crate::home::paths::PlakPaths;
use std::fs;

pub(crate) fn remove_retained_archive(paths: &PlakPaths, session_id: &str) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt as _;
    let id = uuid::Uuid::parse_str(session_id)
        .map_err(|_| "session reference requires a canonical session id".to_owned())?;
    if id.to_string() != session_id {
        return Err("session reference requires a canonical session id".to_owned());
    }
    let path = paths
        .home()
        .join("references")
        .join(format!("{session_id}.json"));
    // Reference cleanup is best-effort; it must never strand a deletion.
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return Ok(());
    };
    if !metadata.is_file() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Ok(());
    }
    let _ = fs::remove_file(&path);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
