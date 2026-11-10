//! Instruction-source reads on the selected runner: the host's reader that
//! asks the runner, and the runner extension that answers from its own
//! filesystem (spec 01 §8.5). Global sources under the product home stay
//! host-side.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use plakous::prompts::instructions::{
    DirectoryAnswer, DirectoryRefusal, InstructionChainEntry, InstructionSourceReader,
    LocalInstructionSourceReader, SourceEntryKind, SourceFile, SourceReadError, SourceResolution,
};
use plakous::tools::runner::{
    RunnerCall, RunnerExtension, RunnerFailure, RunnerLink, RunnerVerb, ServeFailure, ServeFuture,
    call_blocking,
};
use serde_json::{Value, json};

#[cfg(test)]
pub(crate) mod test_kit;

#[cfg(test)]
mod tests;

/// Reader of this node's workspace authority. Global sources use a separate
/// explicit host reader, never a path-prefix exception in this one.
pub(crate) fn instruction_reader(
    runner: Option<&plakous::runner::RunnerServices>,
) -> Arc<dyn InstructionSourceReader> {
    match runner {
        Some(runner) => Arc::new(RunnerInstructionSourceReader {
            link: runner.backend().link(),
            runtime: runner.runtime().clone(),
        }),
        None => Arc::new(LocalInstructionSourceReader),
    }
}

pub(crate) fn user_skill_home(
    runner: &plakous::runner::RunnerServices,
) -> Result<Option<PathBuf>, SourceReadError> {
    let reader = RunnerInstructionSourceReader {
        link: runner.backend().link(),
        runtime: runner.runtime().clone(),
    };
    let answer = reader.ask(json!({"op": "user_skill_home"}))?;
    if answer.get("home") == Some(&Value::Null) {
        return Ok(None);
    }
    path_of(answer.get("home")).map(Some).ok_or(INVALID)
}

/// An [`InstructionSourceReader`] whose answers come from a runner. Each
/// read is one request, waited for on a thread of its own; any link failure
/// is [`SourceReadError::UnavailableBecause`] naming it, never an absent
/// answer.
struct RunnerInstructionSourceReader {
    link: Arc<dyn RunnerLink>,
    runtime: tokio::runtime::Handle,
}

impl RunnerInstructionSourceReader {
    fn ask(&self, question: Value) -> Result<Value, SourceReadError> {
        call_blocking(
            self.link.as_ref(),
            &self.runtime,
            RunnerCall::unkeyed(RunnerVerb::ReadInstructions, question),
        )
        .map_err(|failure| SourceReadError::UnavailableBecause(failure_reason(&failure)))
    }
}

/// Longest runner failure reason a read error carries.
const MAX_REASON_BYTES: usize = 512;

/// What the runner or its link said, as the display text of a failed read.
fn failure_reason(failure: &RunnerFailure) -> String {
    let reason = match failure {
        RunnerFailure::Backend(error) => error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the runner refused the read")
            .to_owned(),
        RunnerFailure::StaleOwner => "a newer owner holds the line on the runner".to_owned(),
        RunnerFailure::NotHolder => {
            "another process of this host holds the line on the runner".to_owned()
        }
        RunnerFailure::Unsupported => "the runner does not serve instruction reads".to_owned(),
        RunnerFailure::Spent => "the runner's answer was already taken".to_owned(),
        RunnerFailure::Unavailable(detail)
        | RunnerFailure::NotSent(detail)
        | RunnerFailure::ReadInterrupted(detail)
        | RunnerFailure::OutcomeUnknown(detail)
        | RunnerFailure::Refused(detail) => format!("the runner link failed: {detail}"),
        _ => "the runner link failed".to_owned(),
    };
    if reason.len() <= MAX_REASON_BYTES {
        return reason;
    }
    let mut end = MAX_REASON_BYTES;
    while !reason.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &reason[..end])
}

fn bytes(path: &Path) -> String {
    hex(path.as_os_str().as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn unhex(value: Option<&Value>) -> Option<Vec<u8>> {
    let text = value?.as_str()?;
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(text.get(index..index + 2)?, 16).ok())
        .collect()
}

fn path_of(value: Option<&Value>) -> Option<PathBuf> {
    unhex(value).map(|bytes| PathBuf::from(OsString::from_vec(bytes)))
}

fn tag(value: &Value) -> Option<&str> {
    value.get("answer").and_then(Value::as_str)
}

fn encode_refusal(refusal: &DirectoryRefusal) -> Value {
    match refusal {
        DirectoryRefusal::Unsafe { path } => json!({ "answer": "unsafe", "path": bytes(path) }),
        DirectoryRefusal::Unreadable => json!({ "answer": "unreadable" }),
    }
}

fn decode_refusal(value: &Value) -> Option<DirectoryRefusal> {
    Some(match tag(value)? {
        "unsafe" => DirectoryRefusal::Unsafe {
            path: path_of(value.get("path"))?,
        },
        "unreadable" => DirectoryRefusal::Unreadable,
        _ => return None,
    })
}

fn encode_file(file: &SourceFile) -> Value {
    match file {
        SourceFile::Missing => json!({ "answer": "missing" }),
        SourceFile::NotRegular => json!({ "answer": "not_regular" }),
        SourceFile::Unsafe => json!({ "answer": "unsafe" }),
        SourceFile::Unreadable => json!({ "answer": "unreadable" }),
        SourceFile::TooLarge => json!({ "answer": "too_large" }),
        SourceFile::Bytes(content) => json!({ "answer": "bytes", "bytes": hex(content) }),
    }
}

fn decode_file(value: &Value) -> Option<SourceFile> {
    Some(match tag(value)? {
        "missing" => SourceFile::Missing,
        "not_regular" => SourceFile::NotRegular,
        "unsafe" => SourceFile::Unsafe,
        "unreadable" => SourceFile::Unreadable,
        "too_large" => SourceFile::TooLarge,
        "bytes" => SourceFile::Bytes(unhex(value.get("bytes"))?),
        _ => return None,
    })
}

const fn encode_kind(kind: SourceEntryKind) -> &'static str {
    match kind {
        SourceEntryKind::Link => "link",
        SourceEntryKind::Directory => "directory",
        SourceEntryKind::Other => "other",
    }
}

fn decode_kind(text: &str) -> Option<SourceEntryKind> {
    Some(match text {
        "link" => SourceEntryKind::Link,
        "directory" => SourceEntryKind::Directory,
        "other" => SourceEntryKind::Other,
        _ => return None,
    })
}

const INVALID: SourceReadError = SourceReadError::Unavailable;

impl InstructionSourceReader for RunnerInstructionSourceReader {
    fn canonicalize(&self, path: &Path) -> Result<SourceResolution, SourceReadError> {
        let answer = self.ask(json!({ "op": "canonicalize", "path": bytes(path) }))?;
        Ok(match tag(&answer).ok_or(INVALID)? {
            "resolved" => SourceResolution::Resolved(path_of(answer.get("path")).ok_or(INVALID)?),
            "not_found" => SourceResolution::NotFound,
            "unresolvable" => SourceResolution::Unresolvable,
            _ => return Err(INVALID),
        })
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        let answer = self.ask(json!({ "op": "anchored_directory", "path": bytes(directory) }))?;
        Ok(match tag(&answer).ok_or(INVALID)? {
            "directory" => DirectoryAnswer::Directory,
            "not_directory" => DirectoryAnswer::NotDirectory,
            "refused" => {
                DirectoryAnswer::Refused(decode_refusal(&answer["refusal"]).ok_or(INVALID)?)
            }
            _ => return Err(INVALID),
        })
    }

    fn read_instruction_chain(
        &self,
        directory: &Path,
        components: &[OsString],
        limit: usize,
    ) -> Result<Vec<InstructionChainEntry>, SourceReadError> {
        let answer = self.ask(json!({
            "op": "read_instruction_chain",
            "path": bytes(directory),
            "components": components
                .iter()
                .map(|component| hex(component.as_bytes()))
                .collect::<Vec<_>>(),
            "limit": limit,
        }))?;
        answer
            .get("entries")
            .and_then(Value::as_array)
            .ok_or(INVALID)?
            .iter()
            .map(|entry| match tag(entry) {
                Some("file") => decode_file(&entry["file"]).map(InstructionChainEntry::File),
                Some("unopened") => {
                    decode_refusal(&entry["refusal"]).map(InstructionChainEntry::Unopened)
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
            .ok_or(INVALID)
    }

    fn is_directory(&self, path: &Path) -> Result<bool, SourceReadError> {
        self.ask(json!({ "op": "is_directory", "path": bytes(path) }))?
            .get("directory")
            .and_then(Value::as_bool)
            .ok_or(INVALID)
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        let answer = self.ask(json!({ "op": "entry_kind", "path": bytes(path) }))?;
        match answer.get("kind") {
            Some(Value::Null) => Ok(None),
            Some(Value::String(kind)) => decode_kind(kind).map(Some).ok_or(INVALID),
            _ => Err(INVALID),
        }
    }

    fn directory_entries(
        &self,
        directory: &Path,
    ) -> Result<Option<Vec<OsString>>, SourceReadError> {
        let answer = self.ask(json!({ "op": "directory_entries", "path": bytes(directory) }))?;
        match answer.get("entries") {
            Some(Value::Null) => Ok(None),
            Some(Value::Array(entries)) => entries
                .iter()
                .map(|entry| unhex(Some(entry)).map(OsString::from_vec))
                .collect::<Option<Vec<_>>>()
                .map(Some)
                .ok_or(INVALID),
            _ => Err(INVALID),
        }
    }

    fn read_file(
        &self,
        path: &Path,
        max_len: u64,
        limit: usize,
    ) -> Result<SourceFile, SourceReadError> {
        let answer = self.ask(json!({
            "op": "read_file",
            "path": bytes(path),
            "max_len": max_len,
            "limit": limit,
        }))?;
        decode_file(&answer).ok_or(INVALID)
    }
}

/// The runner extension the test route registers (spec 01 §8.5): it
/// answers instruction-source reads and the tree's version control from the
/// runner's own filesystem, for every workspace. Unsupported.
#[doc(hidden)]
#[must_use]
pub fn test_runner_extension() -> Arc<dyn RunnerExtension> {
    runner_extension(
        plakous_recursive_subagents::remote_vcs::vcs_runner_extension(Arc::new(|_| true), None),
        Arc::new(|_| true),
        None,
    )
}

/// Combines runner-owned instruction reads with the admitted VCS extension.
/// Workspace consent and the runner's own read-only user skill root protect
/// instruction reads before filesystem I/O.
#[doc(hidden)]
#[must_use]
pub fn runner_extension(
    vcs: Arc<dyn RunnerExtension>,
    admits: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    login_home: Option<PathBuf>,
) -> Arc<dyn RunnerExtension> {
    Arc::new(NodeRunnerExtension {
        vcs,
        admits,
        login_home,
    })
}

struct NodeRunnerExtension {
    vcs: Arc<dyn RunnerExtension>,
    admits: Arc<dyn Fn(&Path) -> bool + Send + Sync>,
    login_home: Option<PathBuf>,
}

impl RunnerExtension for NodeRunnerExtension {
    fn serve(&self, verb: RunnerVerb, payload: Value) -> ServeFuture {
        if verb == RunnerVerb::ReadInstructions {
            let home = self.login_home.clone();
            let admits = self.admits.clone();
            return Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    answer_node(&payload, home.as_deref(), admits.as_ref())
                })
                .await
                .map_err(|_| ServeFailure::Unsupported)?
            });
        }
        match verb {
            RunnerVerb::VcsRead | RunnerVerb::VcsEffect => self.vcs.serve(verb, payload),
            _ => Box::pin(std::future::ready(Err(
                plakous::tools::runner::environment_failure(
                    &plakous::tools::runner::workspace_refusal(),
                ),
            ))),
        }
    }
}

fn refused_skill(message: &str) -> ServeFailure {
    ServeFailure::Backend {
        error: json!({"message": message}),
        proven_no_effect: true,
    }
}

fn answer_node(
    question: &Value,
    home: Option<&Path>,
    admits: &dyn Fn(&Path) -> bool,
) -> Result<Value, ServeFailure> {
    let op = question
        .get("op")
        .and_then(Value::as_str)
        .ok_or(ServeFailure::Unsupported)?;
    if op == "user_skill_home" {
        return Ok(json!({"home": home.map(bytes)}));
    }
    let user_root = home.map(|home| home.join(".agents/skills"));
    if op == "read_skill_slice" {
        let path = path_of(question.get("path")).ok_or(ServeFailure::Unsupported)?;
        let supplied = path_of(question.get("root")).ok_or(ServeFailure::Unsupported)?;
        let root = user_root
            .as_ref()
            .and_then(|root| std::fs::canonicalize(root).ok())
            .filter(|root| root == &supplied)
            .ok_or_else(|| refused_skill("the runner does not serve this user skill root"))?;
        let offset = question
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or(ServeFailure::Unsupported)?;
        let (slice, complete, identity) =
            plakous::tools::FilesToolPack::read_skill_slice(&root, &path, offset)
                .map_err(|error| refused_skill(error.message().as_str()))?;
        return Ok(json!({"bytes": hex(&slice), "complete": complete, "identity": identity}));
    }
    let path = path_of(question.get("path")).ok_or(ServeFailure::Unsupported)?;
    if admits(&path) {
        return answer(question).ok_or(ServeFailure::Unsupported);
    }
    let root = user_root.ok_or_else(|| refused_skill("the runner has no user skills home"))?;
    let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    if !path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                std::path::Component::ParentDir | std::path::Component::CurDir
            )
        })
        || (!path.starts_with(&root) && !path.starts_with(&canonical_root))
    {
        return Err(plakous::tools::runner::environment_failure(
            &plakous::tools::runner::workspace_refusal(),
        ));
    }
    // Canonicalization may report an escaping target so discovery can give
    // the same diagnostic as local discovery, but it never reads that target.
    if op == "canonicalize" || op == "entry_kind" {
        return answer(question).ok_or(ServeFailure::Unsupported);
    }
    if std::fs::canonicalize(&path).is_ok_and(|path| !path.starts_with(&canonical_root)) {
        return Err(refused_skill(
            "path resolves outside the skills directory on the runner",
        ));
    }
    if op == "read_instruction_chain" {
        return Err(ServeFailure::Unsupported);
    }
    answer(question).ok_or(ServeFailure::Unsupported)
}

/// One read's answer from this process's filesystem; `None` for a request
/// that is not one.
fn answer(question: &Value) -> Option<Value> {
    let reader = LocalInstructionSourceReader;
    let path = path_of(question.get("path"))?;
    Some(match question.get("op")?.as_str()? {
        "canonicalize" => match reader.canonicalize(&path).ok()? {
            SourceResolution::Resolved(resolved) => {
                json!({ "answer": "resolved", "path": bytes(&resolved) })
            }
            SourceResolution::NotFound => json!({ "answer": "not_found" }),
            SourceResolution::Unresolvable => json!({ "answer": "unresolvable" }),
        },
        "anchored_directory" => match reader.anchored_directory(&path).ok()? {
            DirectoryAnswer::Directory => json!({ "answer": "directory" }),
            DirectoryAnswer::NotDirectory => json!({ "answer": "not_directory" }),
            DirectoryAnswer::Refused(refusal) => {
                json!({ "answer": "refused", "refusal": encode_refusal(&refusal) })
            }
        },
        "read_instruction_chain" => {
            let components = question
                .get("components")?
                .as_array()?
                .iter()
                .map(|component| unhex(Some(component)).map(OsString::from_vec))
                .collect::<Option<Vec<_>>>()?;
            let limit = usize::try_from(question.get("limit")?.as_u64()?).ok()?;
            let entries = reader
                .read_instruction_chain(&path, &components, limit)
                .ok()?
                .iter()
                .map(|entry| match entry {
                    InstructionChainEntry::File(file) => {
                        json!({ "answer": "file", "file": encode_file(file) })
                    }
                    InstructionChainEntry::Unopened(refusal) => {
                        json!({ "answer": "unopened", "refusal": encode_refusal(refusal) })
                    }
                })
                .collect::<Vec<_>>();
            json!({ "entries": entries })
        }
        "is_directory" => json!({ "directory": reader.is_directory(&path).ok()? }),
        "entry_kind" => {
            json!({ "kind": reader.entry_kind(&path).ok()?.map(encode_kind) })
        }
        "directory_entries" => json!({
            "entries": reader.directory_entries(&path).ok()?.map(|entries| {
                entries
                    .iter()
                    .map(|entry| hex(entry.as_bytes()))
                    .collect::<Vec<_>>()
            })
        }),
        "read_file" => {
            let max_len = question.get("max_len")?.as_u64()?;
            let limit = usize::try_from(question.get("limit")?.as_u64()?).ok()?;
            encode_file(&reader.read_file(&path, max_len, limit).ok()?)
        }
        _ => return None,
    })
}
