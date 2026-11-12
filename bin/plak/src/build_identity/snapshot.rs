//! Exact installed-image selection without moving the command link.

use std::path::{Path, PathBuf};

use crate::{CliFailure, CliResult};

pub(crate) fn installed_for_binary(binary: &str) -> CliResult<PathBuf> {
    select(&crate::home::paths::installed_snapshot_roots(), binary).ok_or_else(|| {
        CliFailure::new(
            "runner_binary_unavailable: expected a 12-digit lowercase binary ID and an installed, owned, non-shared-writable immutable snapshot with that verified image; install the matching runner snapshot and retry --binary (no runner was started)",
        )
    })
}

fn select(roots: &[PathBuf], binary: &str) -> Option<PathBuf> {
    if !super::valid_binary_id(binary) {
        return None;
    }
    roots
        .iter()
        .filter(|root| safe_root(root))
        .filter_map(|root| std::fs::read_dir(root).ok())
        .flatten()
        .flatten()
        .filter_map(|entry| candidate(&entry.path(), binary))
        .max()
        .map(|(_, executable)| executable)
}

fn owned_directory(path: &Path) -> Option<std::fs::Metadata> {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::symlink_metadata(path).ok().filter(|metadata| {
        metadata.is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o022 == 0
    })
}

fn safe_root(root: &Path) -> bool {
    // Only the install-owned chain below HOME, not a world-writable /tmp
    // ancestor of an isolated home. Never follow an install-directory link.
    root.is_absolute()
        && root
            .ancestors()
            .take(5)
            .all(|directory| owned_directory(directory).is_some())
}

fn candidate(directory: &Path, binary: &str) -> Option<(std::time::SystemTime, PathBuf)> {
    use std::os::unix::fs::MetadataExt as _;
    let executable = directory.join("plak");
    super::installed_snapshot_directory(&executable)?;
    let metadata = owned_directory(directory)?;
    let mut image = super::open_owned_regular_file(&executable, 0o022)?;
    let before = super::cache_fields(&image.metadata().ok()?);
    if image.metadata().ok()?.mode() & 0o111 == 0
        || super::read_manifest_id(&image, &executable).as_deref() != Some(binary)
        || super::hash_image(&mut image).as_deref() != Some(binary)
        || super::cache_fields(&image.metadata().ok()?) != before
    {
        return None;
    }
    Some((metadata.modified().ok()?, executable))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "isolated installed snapshot fixtures")]
mod tests;
