//! The skill-root read exception: regular files under a session's canonical
//! skill roots are readable even outside the workspace (spec 04 §2.1).
//!
//! The roots are canonical user skill directories that existed when the
//! session's tools were built; the project skill directory is inside the
//! workspace and never one of them. A path must name a root lexically, and its
//! canonical form must stay inside the same root, so a link never reads out of
//! the root.

use std::io::{Read as _, Seek as _};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{Mode, OFlags};

use plakous_tools::{
    MAX_STANDARD_TEXT_BYTES, MAX_WORKSPACE_FILE_BYTES, ToolRuntimeError, ToolRuntimeErrorKind,
};

/// The file state pinned while a runner skill read collects its slices.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct SkillFileIdentity {
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

fn file_identity(file: &std::fs::File) -> Result<SkillFileIdentity, ToolRuntimeError> {
    let metadata = file
        .metadata()
        .map_err(|_| refusal("skill file metadata could not be read".into()))?;
    Ok(SkillFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    })
}

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
    let mut file = open_file(root, path)?;
    let shown = path.display();
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

fn open_file(root: &Path, path: &Path) -> Result<std::fs::File, ToolRuntimeError> {
    let shown = path.display();
    if containing_root(&[root.to_path_buf()], path).is_none() {
        return Err(refusal(format!(
            "path is outside the skills directory: {shown}"
        )));
    }
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
    let file = std::fs::File::from(
        rustix::fs::open(
            &canonical,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC | OFlags::NOCTTY,
            Mode::empty(),
        )
        .map_err(|_| unopenable())?,
    );
    regular(&file.metadata().map_err(|_| unopenable())?)?;
    Ok(file)
}

pub(crate) fn read_slice(
    root: &Path,
    path: &Path,
    offset: u64,
) -> Result<(Vec<u8>, bool, SkillFileIdentity), ToolRuntimeError> {
    if offset >= MAX_WORKSPACE_FILE_BYTES {
        return Err(refusal("skill read offset exceeds the file bound".into()));
    }
    let mut file = open_file(root, path)?;
    let identity = file_identity(&file)?;
    file.seek(std::io::SeekFrom::Start(offset))
        .map_err(|_| refusal("skill file could not be positioned".into()))?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_STANDARD_TEXT_BYTES.min(MAX_WORKSPACE_FILE_BYTES - offset))
        .read_to_end(&mut bytes)
        .map_err(|_| refusal("skill file could not be read".into()))?;
    let mut probe = [0_u8; 1];
    let complete = file
        .read(&mut probe)
        .map_err(|_| refusal("skill file could not be read".into()))?
        == 0;
    if file_identity(&file)? != identity {
        return Err(refusal("skill file changed during the read".into()));
    }
    Ok((bytes, complete, identity))
}

pub(super) async fn read_on_runner(
    context: &plakous_tools::ToolContext,
    runner: &plakous_tools::internal::runner::RunnerServices,
    root: &Path,
    path: &Path,
    wanted_lines: u64,
) -> Result<(Vec<u8>, bool), ToolRuntimeError> {
    use plakous_tools::internal::runner::{RunnerCall, hex};
    let mut bytes = Vec::new();
    let mut newlines = 0_u64;
    let mut pinned = None;
    loop {
        context.check_cancelled()?;
        let payload = serde_json::json!({
            "op": "read_skill_slice", "path": hex(path.as_os_str().as_encoded_bytes()),
            "root": hex(root.as_os_str().as_encoded_bytes()), "offset": bytes.len(),
        });
        let control = plakous_tools::operation_context(context);
        let call = RunnerCall::read_instructions(payload, &control);
        let answer = await_read(context, runner.backend().link().call(call)).await?;
        let (slice, complete, identity) = decode_slice(&answer)?;
        if pinned.is_some_and(|pinned| pinned != identity) {
            return Err(refusal("skill file changed between read slices".into()));
        }
        pinned = Some(identity);
        if slice.len() as u64 > MAX_STANDARD_TEXT_BYTES
            || bytes.len() as u64 + slice.len() as u64 > MAX_WORKSPACE_FILE_BYTES
            || (slice.is_empty() && !complete)
        {
            return Err(refusal(
                "the runner returned an invalid skill read bound".into(),
            ));
        }
        newlines += slice
            .iter()
            .fold(0_u64, |count, byte| count + u64::from(*byte == b'\n'));
        bytes.extend(slice);
        if complete || newlines >= wanted_lines || bytes.len() as u64 >= MAX_WORKSPACE_FILE_BYTES {
            return Ok((bytes, complete));
        }
    }
}

fn decode_slice(
    answer: &serde_json::Value,
) -> Result<(Vec<u8>, bool, SkillFileIdentity), ToolRuntimeError> {
    let invalid = || refusal("the runner returned an invalid skill read".into());
    let bytes = answer
        .get("bytes")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(invalid)?;
    let bytes = plakous_tools::internal::runner::unhex(bytes).map_err(|_| invalid())?;
    let complete = answer
        .get("complete")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(invalid)?;
    let identity = serde_json::from_value(answer["identity"].clone()).map_err(|_| invalid())?;
    Ok((bytes, complete, identity))
}

async fn await_read(
    context: &plakous_tools::ToolContext,
    mut answer: plakous_tools::internal::runner::RunnerCallFuture,
) -> Result<serde_json::Value, ToolRuntimeError> {
    use std::future::Future as _;
    use std::task::Poll;
    let mut cancelled = std::pin::pin!(context.cancellation().cancelled());
    std::future::poll_fn(|cx| {
        // Poll the link too so it can forward cancellation before we stop
        // waiting; an unavailable peer need not acknowledge Stop first.
        let result = answer.as_mut().poll(cx);
        if cancelled.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::Cancelled,
                "tool execution was cancelled",
            )));
        }
        result.map(|result| result.map_err(runner_refusal))
    })
    .await
}

fn runner_refusal(failure: plakous_tools::internal::runner::RunnerFailure) -> ToolRuntimeError {
    use plakous_tools::internal::runner::RunnerFailure;
    let reason = match failure {
        RunnerFailure::Backend(value) => value
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("the runner refused the skill read")
            .to_owned(),
        RunnerFailure::Unavailable(reason)
        | RunnerFailure::NotSent(reason)
        | RunnerFailure::ReadInterrupted(reason)
        | RunnerFailure::OutcomeUnknown(reason)
        | RunnerFailure::Refused(reason) => reason,
        _ => "the runner cannot answer the skill read".into(),
    };
    let reason: String = reason.chars().take(512).collect();
    refusal(format!(
        "skill file could not be read on the runner: {reason}"
    ))
}

fn refusal(message: String) -> ToolRuntimeError {
    ToolRuntimeError::new(ToolRuntimeErrorKind::WorkspacePolicy, message)
        .with_proven_no_effect(true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
