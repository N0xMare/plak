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
    reserve_prefixes(common, digest, &[16])
}

pub(super) fn reserve_prefix(common: &Path, digest: &str) -> Result<fs::File, VcsError> {
    reserve_prefixes(common, digest, &[8, 16])
        .map_err(|error| VcsError::mint_unavailable(error.detail))
}

fn reserve_prefixes(common: &Path, digest: &str, lengths: &[usize]) -> Result<fs::File, VcsError> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(VcsError::seal_failed("invalid tree namespace digest"));
    }
    let guard = lock(common)?;
    let dir = common.join("plak-namespaces");
    // Check every existing claim before publishing either new claim. A
    // collision must not leave evidence asserting a different prefix owner.
    for length in lengths {
        if let Some(owner) = read_owner(&dir, &digest[..*length])? {
            check_owner(&owner, digest, *length)?;
        }
    }
    // Publish the longer witness first: a process loss must not strand an
    // eight-hex claim that the sixteen-hex cleanup cannot find.
    for length in lengths.iter().rev() {
        let prefix = &digest[..*length];
        if read_owner(&dir, prefix)?.is_none() {
            publish(&dir, digest, prefix)?;
        }
        let owner = read_owner(&dir, prefix)?
            .ok_or_else(|| VcsError::seal_failed("namespace ownership disappeared"))?;
        check_owner(&owner, digest, *length)?;
    }
    Ok(guard)
}

fn publish(dir: &Path, digest: &str, prefix: &str) -> Result<(), VcsError> {
    let path = dir.join(prefix);
    // An atomic link publishes a complete immutable owner identity. Readers
    // never mistake a concurrent creator's partially written file for a collision.
    let mut scratch = tempfile::NamedTempFile::new_in(dir)
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    scratch
        .write_all(digest.as_bytes())
        .and_then(|()| scratch.as_file().sync_all())
        .map_err(|error| VcsError::seal_failed(error.to_string()))?;
    match scratch.persist_noclobber(&path) {
        Ok(_) => {
            fs::File::open(dir)
                .and_then(|file| file.sync_all())
                .map_err(|error| VcsError::seal_failed(error.to_string()))?;
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(VcsError::seal_failed(error.to_string())),
    }
    Ok(())
}

fn read_owner(dir: &Path, prefix: &str) -> Result<Option<String>, VcsError> {
    let path = dir.join(prefix);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(VcsError::seal_failed(error.to_string())),
    };
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(VcsError::seal_failed("unsafe namespace ownership evidence"));
    }
    let owner =
        fs::read_to_string(path).map_err(|error| VcsError::seal_failed(error.to_string()))?;
    if owner.len() != 64
        || !owner.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !owner.starts_with(prefix)
    {
        return Err(VcsError::seal_failed(
            "invalid namespace ownership evidence",
        ));
    }
    Ok(Some(owner))
}

fn check_owner(owner: &str, digest: &str, length: usize) -> Result<(), VcsError> {
    if owner != digest {
        return Err(VcsError::seal_failed(if length == 8 {
            "tree prefix collision with another tree in this checkout; existing artifacts retained"
        } else {
            "tree ref namespace prefix collision; existing artifacts retained"
        }));
    }
    Ok(())
}

pub(super) fn owner(common: &Path, tree8: &str) -> Result<String, VcsError> {
    if tree8.len() != 8 || !tree8.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(VcsError::mint_unavailable("invalid copy prefix"));
    }
    let _guard = lock(common)?;
    read_owner(&common.join("plak-namespaces"), tree8)?.ok_or_else(|| {
        VcsError::mint_unavailable("copy prefix ownership is missing; artifacts retained")
    })
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
            &format!("refs/heads/plak/{}/", &prefix[..8]),
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
    let tree8 = &prefix[..8];
    let tree_owner = read_owner(&dir, tree8)?;
    if let Some(owner) = &tree_owner {
        let expected = std::str::from_utf8(&digest)
            .map_err(|error| VcsError::seal_failed(error.to_string()))?;
        check_owner(owner, expected, 8)?;
    }
    if tree_owner.is_some() {
        fs::remove_file(dir.join(tree8))
            .map_err(|error| VcsError::seal_failed(error.to_string()))?;
        fs::File::open(&dir)
            .and_then(|file| file.sync_all())
            .map_err(|error| VcsError::seal_failed(error.to_string()))?;
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
