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
    RunnerCall, RunnerExtension, RunnerLink, RunnerVerb, ServeFailure, ServeFuture, call_blocking,
    process_runner,
};
use serde_json::{Value, json};

/// The reader of the workspaces sessions run in: the selected runner's, with
/// reads under `host_local` (the product home) kept on this host, or this
/// process's own filesystem.
pub(crate) fn instruction_reader(host_local: Option<&Path>) -> Arc<dyn InstructionSourceReader> {
    match process_runner() {
        Some(runner) => Arc::new(RunnerInstructionSourceReader {
            link: runner.backend().link(),
            runtime: runner.runtime().clone(),
            host_local: host_local.map(Path::to_path_buf),
        }),
        None => Arc::new(LocalInstructionSourceReader),
    }
}

/// An [`InstructionSourceReader`] whose answers come from a runner. Each
/// read is one request, waited for on a thread of its own; any link failure
/// is [`SourceReadError::Unavailable`], never an absent answer.
struct RunnerInstructionSourceReader {
    link: Arc<dyn RunnerLink>,
    runtime: tokio::runtime::Handle,
    host_local: Option<PathBuf>,
}

impl RunnerInstructionSourceReader {
    fn local(&self, path: &Path) -> bool {
        self.host_local
            .as_ref()
            .is_some_and(|root| path.starts_with(root))
    }

    fn ask(&self, question: Value) -> Result<Value, SourceReadError> {
        call_blocking(
            self.link.as_ref(),
            &self.runtime,
            RunnerCall::unkeyed(RunnerVerb::ReadInstructions, question),
        )
        .map_err(|_| SourceReadError::Unavailable)
    }
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
        if self.local(path) {
            return LocalInstructionSourceReader.canonicalize(path);
        }
        let answer = self.ask(json!({ "op": "canonicalize", "path": bytes(path) }))?;
        Ok(match tag(&answer).ok_or(INVALID)? {
            "resolved" => SourceResolution::Resolved(path_of(answer.get("path")).ok_or(INVALID)?),
            "not_found" => SourceResolution::NotFound,
            "unresolvable" => SourceResolution::Unresolvable,
            _ => return Err(INVALID),
        })
    }

    fn anchored_directory(&self, directory: &Path) -> Result<DirectoryAnswer, SourceReadError> {
        if self.local(directory) {
            return LocalInstructionSourceReader.anchored_directory(directory);
        }
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
        if self.local(directory) {
            return LocalInstructionSourceReader
                .read_instruction_chain(directory, components, limit);
        }
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
        if self.local(path) {
            return LocalInstructionSourceReader.is_directory(path);
        }
        self.ask(json!({ "op": "is_directory", "path": bytes(path) }))?
            .get("directory")
            .and_then(Value::as_bool)
            .ok_or(INVALID)
    }

    fn entry_kind(&self, path: &Path) -> Result<Option<SourceEntryKind>, SourceReadError> {
        if self.local(path) {
            return LocalInstructionSourceReader.entry_kind(path);
        }
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
        if self.local(directory) {
            return LocalInstructionSourceReader.directory_entries(directory);
        }
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
        if self.local(path) {
            return LocalInstructionSourceReader.read_file(path, max_len, limit);
        }
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
    Arc::new(TestRunnerExtension {
        vcs: plakous_recursive_subagents::remote_vcs::vcs_runner_extension(
            Arc::new(|_| true),
            None,
        ),
    })
}

struct TestRunnerExtension {
    vcs: Arc<dyn RunnerExtension>,
}

impl RunnerExtension for TestRunnerExtension {
    fn serve(&self, verb: RunnerVerb, payload: Value) -> ServeFuture {
        match verb {
            RunnerVerb::VcsRead | RunnerVerb::VcsEffect => self.vcs.serve(verb, payload),
            _ => InstructionReaderExtension.serve(verb, payload),
        }
    }
}

struct InstructionReaderExtension;

impl RunnerExtension for InstructionReaderExtension {
    fn serve(&self, verb: RunnerVerb, payload: Value) -> ServeFuture {
        if verb != RunnerVerb::ReadInstructions {
            return Box::pin(std::future::ready(Err(ServeFailure::Unsupported)));
        }
        Box::pin(async move {
            tokio::task::spawn_blocking(move || answer(&payload))
                .await
                .ok()
                .flatten()
                .ok_or(ServeFailure::Unsupported)
        })
    }
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
