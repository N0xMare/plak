//! Collision evidence for short physical ref namespaces.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};
use std::path::Path;

use super::VcsError;

pub(super) fn lock(common: &Path) -> Result<fs::File, VcsError> {
    let dir = common.join("plak-namespaces");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .or_else(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    let metadata =
        fs::symlink_metadata(&dir).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(VcsError::seal_failed("unsafe namespace directory"));
    }
    let guard = fs::File::open(&dir).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    rustix::fs::flock(&guard, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    Ok(guard)
}

pub(super) fn reserve(common: &Path, digest: &str) -> Result<fs::File, VcsError> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(VcsError::seal_failed("invalid tree namespace digest"));
    }
    let guard = lock(common)?;
    let dir = common.join("plak-namespaces");
    let path = dir.join(&digest[..16]);
    // An atomic link publishes a complete immutable owner identity. Readers
    // never mistake a concurrent creator's partially written file for a collision.
    let mut scratch = tempfile::NamedTempFile::new_in(&dir)
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    scratch
        .write_all(digest.as_bytes())
        .and_then(|()| scratch.as_file().sync_all())
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    match scratch.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(&dir)
                .and_then(|file| file.sync_all())
                .map_err(|error| VcsError::seal_failed(error.to_string()))?;
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(VcsError::seal_failed(error.to_string())),
    }
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(VcsError::seal_failed("unsafe namespace ownership evidence"));
    }
    if fs::read(&path).map_err(|error| VcsError::seal_failed(error.to_string()))?
        != digest.as_bytes()
    {
        return Err(VcsError::seal_failed(
            "tree ref namespace prefix collision; existing artifacts retained",
        ));
    }
    Ok(guard)
}

/// Ref publication holds the same directory lock through its update. Evidence
/// may disappear only when no pin in the namespace remains, under that lock.
pub(super) fn cleanup(common: &Path, prefix: &str) -> Result<(), VcsError> {
    if prefix.len() != 16 || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(VcsError::seal_failed("invalid namespace prefix"));
    }
    let dir = common.join("plak-namespaces");
    if !dir.exists() {
        return Ok(());
    }
    let _guard = lock(common)?;
    let output = super::git_query(
        common,
        &[
            "--git-dir",
            &common.display().to_string(),
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/plak/{prefix}/"),
        ],
    )
    .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if !output.status.success() {
        return Err(VcsError::seal_failed("cannot inspect namespace pins"));
    }
    if !output.stdout.is_empty() {
        return Ok(());
    }
    let path = dir.join(prefix);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(VcsError::seal_failed(error.to_string())),
    };
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(VcsError::seal_failed("unsafe namespace ownership evidence"));
    }
    let digest = fs::read(&path).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if digest.len() != 64
        || !digest.iter().all(u8::is_ascii_hexdigit)
        || &digest[..16] != prefix.as_bytes()
    {
        return Err(VcsError::seal_failed(
            "invalid namespace ownership evidence",
        ));
    }
    fs::remove_file(path).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    fs::File::open(dir)
        .and_then(|file| file.sync_all())
        .map_err(|error| VcsError::seal_failed(error.to_string()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn namespace_reservation_guard_fences_evidence_cleanup() {
        let common = tempfile::tempdir().unwrap();
        let digest = "0123456789abcdef".to_owned() + &"1".repeat(48);
        let guard = super::reserve(common.path(), &digest).unwrap();
        let competing = std::fs::File::open(common.path().join("plak-namespaces")).unwrap();
        assert!(
            rustix::fs::flock(
                &competing,
                rustix::fs::FlockOperation::NonBlockingLockExclusive
            )
            .is_err()
        );
        drop(guard);
        rustix::fs::flock(
            &competing,
            rustix::fs::FlockOperation::NonBlockingLockExclusive,
        )
        .unwrap();
    }

    #[test]
    fn a_short_prefix_collision_never_reassigns_the_namespace() {
        let common = tempfile::tempdir().unwrap();
        let first = "0123456789abcdef".to_owned() + &"1".repeat(48);
        let second = "0123456789abcdef".to_owned() + &"2".repeat(48);
        super::reserve(common.path(), &first).unwrap();
        super::reserve(common.path(), &first).unwrap();
        assert!(
            super::reserve(common.path(), &second)
                .unwrap_err()
                .detail
                .contains("collision")
        );
    }
}
