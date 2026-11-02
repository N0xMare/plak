//! The skill-root read exception: regular files under a session's canonical
//! skill roots are readable even outside the workspace (spec 04 §2.1).
//!
//! The roots are canonical user skill directories that existed when the
//! session's tools were built; the project skill directory is inside the
//! workspace and never one of them. A path must name a root lexically, and its
//! canonical form must stay inside the same root, so a link never reads out of
//! the root.

use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use plakous_tools::{
    MAX_STANDARD_TEXT_BYTES, MAX_WORKSPACE_FILE_BYTES, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// The root an absolute, `..`-free path lexically names, if any.
pub(super) fn containing_root<'a>(roots: &'a [PathBuf], path: &Path) -> Option<&'a Path> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    roots
        .iter()
        .map(PathBuf::as_path)
        .find(|root| path.starts_with(root) && path != *root)
}

/// Reads from the start of one skill file until `wanted_lines` lines are in
/// hand, the file ends, or the workspace file bound is reached, in the same
/// slices a workspace read takes. Returns the bytes and whether they reach EOF.
pub(super) fn read_leading(
    root: &Path,
    path: &Path,
    wanted_lines: u64,
) -> Result<(Vec<u8>, bool), ToolRuntimeError> {
    let shown = path.display();
    let canonical = std::fs::canonicalize(path).map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => refusal(format!("path does not exist: {shown}")),
        std::io::ErrorKind::NotADirectory => {
            refusal(format!("a parent component is not a directory: {shown}"))
        }
        _ => refusal(format!("skill path could not be resolved: {shown}")),
    })?;
    if !canonical.starts_with(root) {
        return Err(refusal(format!(
            "path resolves outside the skills directory: {shown}"
        )));
    }
    // Only a regular file is opened, without following a link or blocking,
    // and the descriptor is checked again: a FIFO is refused, never read.
    let regular = |metadata: &std::fs::Metadata| -> Result<(), ToolRuntimeError> {
        if metadata.is_dir() {
            return Err(refusal(format!("path is a directory: {shown}")));
        }
        if !metadata.is_file() {
            return Err(refusal(format!("path is not a regular file: {shown}")));
        }
        Ok(())
    };
    let unopenable = || refusal(format!("skill path could not be opened: {shown}"));
    regular(&std::fs::symlink_metadata(&canonical).map_err(|_| unopenable())?)?;
    let mut file = std::fs::File::from(
        rustix::fs::open(
            &canonical,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| unopenable())?,
    );
    regular(&file.metadata().map_err(|_| unopenable())?)?;
    let mut bytes = Vec::new();
    let mut newlines = 0_u64;
    loop {
        let mut slice = Vec::new();
        let read = file
            .by_ref()
            .take(MAX_STANDARD_TEXT_BYTES)
            .read_to_end(&mut slice)
            .map_err(|_| refusal(format!("skill file could not be read: {shown}")))?;
        newlines += slice
            .iter()
            .fold(0_u64, |count, byte| count + u64::from(*byte == b'\n'));
        bytes.extend_from_slice(&slice);
        if (read as u64) < MAX_STANDARD_TEXT_BYTES {
            return Ok((bytes, true));
        }
        if newlines >= wanted_lines || bytes.len() as u64 >= MAX_WORKSPACE_FILE_BYTES {
            let mut probe = [0_u8; 1];
            let more = file
                .read(&mut probe)
                .map_err(|_| refusal(format!("skill file could not be read: {shown}")))?;
            return Ok((bytes, more == 0));
        }
    }
}

fn refusal(message: String) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::WorkspacePolicy, message)
        .with_proven_no_effect(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
