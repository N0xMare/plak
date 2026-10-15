//! Lowering of projected tool arguments into canonical tool invocations.

use std::path::{Component, Path, PathBuf};

use plakous_model::{ProjectedToolPresentationV2, SemanticJsonV1};
use serde_json::{Map, Value, json};

use super::map::registration_error;
use super::projection::{
    CanonicalProcessOperationV2, ToolProjectionExposureV2, ToolProjectionLoweringV2,
};
use crate::capabilities::{CLAUDE_BASH_OBSERVATION_BYTES, PROCESS_OBSERVATION_BYTES};
use crate::{ExecutionEnvironment, ToolRuntimeError, ToolRuntimeErrorKind};

pub(super) fn placeholder_for(schema: &Value) -> Result<Value, ToolRuntimeError> {
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return Ok(value.clone());
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("string") => Ok(Value::String(
            "x".repeat(
                usize::try_from(schema.get("minLength").and_then(Value::as_u64).unwrap_or(1))
                    .map_err(|_| registration_error("projection string bound is not portable"))?,
            ),
        )),
        Some("integer" | "number") => Ok(schema.get("minimum").cloned().unwrap_or(Value::from(0))),
        Some("boolean") => Ok(Value::Bool(false)),
        Some("array") => {
            let min_items = schema
                .get("minItems")
                .and_then(json_positive_u64)
                .unwrap_or(0);
            if min_items == 0 {
                return Ok(Value::Array(Vec::new()));
            }
            let count = usize::try_from(min_items)
                .map_err(|_| registration_error("projection array bound is not portable"))?;
            let items = schema
                .get("items")
                .ok_or_else(|| registration_error("projection schema type is unsupported"))?;
            let element = placeholder_for(items)?;
            Ok(Value::Array(vec![element; count]))
        }
        Some("object") => {
            let mut object = Map::new();
            for name in schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let property = schema
                    .get("properties")
                    .and_then(|properties| properties.get(name))
                    .ok_or_else(|| registration_error("projection schema type is unsupported"))?;
                object.insert(name.to_owned(), placeholder_for(property)?);
            }
            Ok(Value::Object(object))
        }
        _ => Err(registration_error("projection schema type is unsupported")),
    }
}

fn json_u64(value: &Value) -> Option<u64> {
    if let Some(number) = value.as_u64() {
        return Some(number);
    }
    let raw = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => return None,
    };
    raw.parse::<u64>().ok().or_else(|| {
        raw.trim_end_matches('0')
            .trim_end_matches('.')
            .parse::<u64>()
            .ok()
    })
}

fn json_positive_u64(value: &Value) -> Option<u64> {
    json_u64(value).filter(|number| *number >= 1)
}

/// Reference unified exec clamps every `exec_command` start and poll yield to
/// this window. A model-sent `yield_time_ms: 0` must not observe an empty pipe
/// and report a still-running process as the whole result.
pub const UNIFIED_EXEC_MIN_YIELD_MS: u64 = 250;
/// Upper bound of one unified exec observation; longer requests are clamped.
pub const UNIFIED_EXEC_MAX_YIELD_MS: u64 = 600_000;
const UNIFIED_EXEC_DEFAULT_YIELD_MS: u64 = 10_000;

/// Reference `write_stdin` waits briefly for the output its input provokes.
const WRITE_STDIN_DEFAULT_YIELD_MS: u64 = 250;

fn unified_exec_max_output_bytes(arguments: &Map<String, Value>) -> u64 {
    arguments
        .get("max_output_tokens")
        .and_then(json_u64)
        .and_then(|tokens| tokens.checked_mul(4))
        .unwrap_or(PROCESS_OBSERVATION_BYTES as u64)
        .clamp(1, PROCESS_OBSERVATION_BYTES as u64)
}

pub(super) fn unified_exec_yield_time_ms(arguments: &Map<String, Value>) -> u64 {
    arguments
        .get("yield_time_ms")
        .and_then(json_u64)
        .unwrap_or(UNIFIED_EXEC_DEFAULT_YIELD_MS)
        .clamp(UNIFIED_EXEC_MIN_YIELD_MS, UNIFIED_EXEC_MAX_YIELD_MS)
}

/// Claude Code's own `Bash` timeout ceiling.
const CLAUDE_BASH_MAX_TIMEOUT_MS: u64 = 600_000;
/// All start vocabularies share the foreground hint ceiling.
const GROK_TERMINAL_MAX_TIMEOUT_MS: u64 = UNIFIED_EXEC_MAX_YIELD_MS;

/// Pi's typed refusal of an image path: pi 1.1.0's `read` here is text only.
pub const PI_READ_IMAGE_REFUSAL: &str =
    "read returns text only; image files (png, jpg, jpeg, gif, webp, bmp) are not supported";

/// Whether a path names one of the image formats pi's `read` would attach.
fn is_image_path(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
                .iter()
                .any(|image| extension.eq_ignore_ascii_case(image))
        })
}

/// Pi's `wait` without `timeout_ms` awaits as long as `wait_agent` does.
const PI_WAIT_DEFAULT_TIMEOUT_MS: u64 = 300_000;
/// Every await presentation clamps to one hour.
const PI_WAIT_MAX_TIMEOUT_MS: u64 = 3_600_000;

/// A pi process id as the model writes it: `N` or `pN`, nonzero. Anything
/// else is refused before any effect, without claiming to know what it names:
/// pi's `wait` and `stop` cover processes; agent tools cover agents.
fn pi_process_id(value: &Value) -> Result<u64, ToolRuntimeError> {
    let text = value.as_str().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "ids must be strings",
        )
    })?;
    text.strip_prefix('p')
        .unwrap_or(text)
        .parse::<u32>()
        .ok()
        .filter(|number| *number != 0)
        .map(u64::from)
        .ok_or_else(|| {
            ToolRuntimeError::new(
                ToolRuntimeErrorKind::Policy,
                format!("id `{text}` is not a process id; this agent has no agent tools"),
            )
        })
}

/// Claude addresses a background shell by the string form of its `session_id`.
fn process_task_id(value: &Value, field: &str) -> Result<u64, ToolRuntimeError> {
    value.as_str()
        .and_then(process_task_number)
        .ok_or_else(|| ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            format!("{field} must contain process ids returned by run_terminal_command (N or pN); agent ids are not accepted"),
        ))
}

/// A process task id as the model writes it: `N` or `pN`, nonzero.
fn process_task_number(text: &str) -> Option<u64> {
    text.strip_prefix('p')
        .unwrap_or(text)
        .parse::<u32>()
        .ok()
        .filter(|number| *number != 0)
        .map(u64::from)
}

/// Grok's output and kill tools as presented to a node without agent tools:
/// anything that is not a process id is refused before any effect, without
/// claiming to know what else it names.
fn grok_process_task_id(value: &Value) -> Result<u64, ToolRuntimeError> {
    let text = value.as_str().ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::InvalidArguments,
            "task ids must be strings",
        )
    })?;
    process_task_number(text).ok_or_else(|| {
        ToolRuntimeError::new(
            ToolRuntimeErrorKind::Policy,
            format!("task id `{text}` is not a process id; this agent has no agent tools"),
        )
    })
}

fn capability_operation(
    entry: &ToolProjectionExposureV2,
) -> Result<CanonicalProcessOperationV2, ToolRuntimeError> {
    entry
        .canonical_operations
        .keys()
        .copied()
        .next()
        .ok_or_else(|| registration_error("projection capability binding is empty"))
}

// Keep the canonical input contract's wire shape after lowering. A UTF-8
// projection is raw text, while structured projections are semantic JSON.
pub(super) enum LoweredProjectionArguments {
    Utf8(String),
    Structured(SemanticJsonV1),
}

impl LoweredProjectionArguments {
    pub(super) fn into_request_arguments(self) -> Result<String, ToolRuntimeError> {
        match self {
            Self::Utf8(text) => Ok(text),
            Self::Structured(json) => std::str::from_utf8(json.canonical_bytes())
                .map(str::to_owned)
                .map_err(|_| registration_error("canonical projection arguments are not UTF-8")),
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "closed lowering programs stay in one audited match"
)]
pub(super) fn lower_projection(
    entry: &ToolProjectionExposureV2,
    projected: &Value,
    environment: Option<&ExecutionEnvironment>,
) -> Result<(CanonicalProcessOperationV2, LoweredProjectionArguments), ToolRuntimeError> {
    if entry.lowering == ToolProjectionLoweringV2::CodexApplyPatch {
        let patch = match projected {
            Value::String(text) => text.clone(),
            Value::Object(object) => object
                .get("patch")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    ToolRuntimeError::new(
                        ToolRuntimeErrorKind::InvalidArguments,
                        "apply_patch requires patch envelope text",
                    )
                })?,
            _ => {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "apply_patch input must be patch envelope text",
                ));
            }
        };
        return Ok((
            capability_operation(entry)?,
            LoweredProjectionArguments::Utf8(patch),
        ));
    }
    if matches!(
        entry.presentation(),
        ProjectedToolPresentationV2::CustomUtf8 { .. }
    ) {
        let Value::String(text) = projected else {
            return Err(registration_error(
                "custom projection did not validate as UTF-8 text",
            ));
        };
        if entry.lowering != ToolProjectionLoweringV2::CanonicalPassthrough {
            return Err(registration_error(
                "custom projection has no UTF-8 lowering",
            ));
        }
        return Ok((
            capability_operation(entry)?,
            LoweredProjectionArguments::Utf8(text.clone()),
        ));
    }
    let mut arguments = entry
        .defaults
        .value()
        .as_object()
        .cloned()
        .ok_or_else(|| registration_error("projection defaults are not an object"))?;
    for (name, value) in projected
        .as_object()
        .ok_or_else(|| registration_error("projected input is not an object"))?
    {
        arguments.insert(name.clone(), value.clone());
    }
    let (operation, mut canonical) = match entry.lowering {
        ToolProjectionLoweringV2::CodexExecCommand => {
            let max_output_bytes = unified_exec_max_output_bytes(&arguments);
            let cmd = arguments
                .get("cmd")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|command| !command.is_empty());
            // Official unified-exec: cmd starts a process. session_id without
            // cmd polls. Models often send a hallucinated session_id with cmd;
            // treating that as Poll loops on "unknown or stale" forever.
            if let Some(command) = cmd {
                (
                    CanonicalProcessOperationV2::Start,
                    json!({
                        "command": command,
                        "workdir": arguments.get("workdir").cloned().unwrap_or_else(|| Value::String(String::from("."))),
                        "login": arguments.get("login").cloned().unwrap_or(Value::Bool(true)),
                        "tty": arguments.get("tty").cloned().unwrap_or(Value::Bool(false)),
                        "keep_stdin_open": arguments.get("tty").cloned().unwrap_or(Value::Bool(false)),
                        "yield_time_ms": if projected.get("yield_time_ms").is_some() {
                            unified_exec_yield_time_ms(&arguments).min(600_000)
                        } else {
                            environment.map_or(30_000, |environment| environment.config().limits().foreground_seconds() * 1000)
                        },
                        "max_output_bytes": max_output_bytes,
                    }),
                )
            } else if arguments
                .get("session_id")
                .and_then(json_positive_u64)
                .is_some()
            {
                (
                    CanonicalProcessOperationV2::Poll,
                    json!({
                        "session_id": arguments.get("session_id").cloned().unwrap_or(Value::Null),
                        "yield_time_ms": unified_exec_yield_time_ms(&arguments),
                        "max_output_bytes": max_output_bytes,
                        "max_output_records": 256,
                    }),
                )
            } else {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "exec_command requires cmd to start, or session_id to read more output",
                ));
            }
        }
        ToolProjectionLoweringV2::CodexWriteStdin => {
            let empty = arguments
                .get("chars")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty);
            let yield_time_ms = arguments
                .get("yield_time_ms")
                .and_then(json_u64)
                .unwrap_or(if empty {
                    5_000
                } else {
                    WRITE_STDIN_DEFAULT_YIELD_MS
                })
                .clamp(
                    if empty {
                        5_000
                    } else {
                        UNIFIED_EXEC_MIN_YIELD_MS
                    },
                    if empty { 300_000 } else { 30_000 },
                );
            (
                CanonicalProcessOperationV2::Write,
                json!({
                    "session_id": arguments.get("session_id").cloned().unwrap_or(Value::Null),
                    "input": arguments.get("chars").cloned().unwrap_or_else(|| Value::String(String::new())),
                    "disposition": "keep_open",
                    "yield_time_ms": yield_time_ms,
                    "max_output_bytes": unified_exec_max_output_bytes(&arguments),
                }),
            )
        }
        ToolProjectionLoweringV2::GrokRunTerminalCommand | ToolProjectionLoweringV2::ClaudeBash => {
            let claude = matches!(entry.lowering, ToolProjectionLoweringV2::ClaudeBash);
            let background = claude
                && arguments
                    .get("run_in_background")
                    .and_then(Value::as_bool)
                    .is_some_and(|background| background);
            let page = if claude {
                CLAUDE_BASH_OBSERVATION_BYTES
            } else {
                PROCESS_OBSERVATION_BYTES
            };
            let timeout = arguments
                .get(if claude { "timeout" } else { "block_until_ms" })
                .and_then(json_u64)
                .unwrap_or_else(|| {
                    environment.map_or(30_000, |environment| {
                        environment.config().limits().foreground_seconds() * 1000
                    })
                })
                .clamp(
                    UNIFIED_EXEC_MIN_YIELD_MS,
                    if claude {
                        CLAUDE_BASH_MAX_TIMEOUT_MS
                    } else {
                        GROK_TERMINAL_MAX_TIMEOUT_MS
                    },
                );
            let mut start = json!({
                "command": arguments.get("command").cloned().unwrap_or(Value::Null),
                "workdir": ".",
                "login": false,
                "tty": false,
                "keep_stdin_open": false,
                // A Claude background start returns the first moment of
                // output and its session; the rest arrives in the completion
                // notice and, when one was issued, the output file `Read` reads.
                "yield_time_ms": if background { UNIFIED_EXEC_MIN_YIELD_MS } else { timeout },
                "max_output_bytes": page,
            });
            if let Some(description) = arguments
                .get("description")
                .cloned()
                .filter(|value| !value.is_null())
            {
                start["model_description"] = description;
            }
            (CanonicalProcessOperationV2::Start, start)
        }
        ToolProjectionLoweringV2::GrokGetCommandOutput => {
            let ids = arguments.get("task_ids").and_then(Value::as_array);
            let session_ids = ids
                .into_iter()
                .flatten()
                .map(grok_process_task_id)
                .collect::<Result<Vec<_>, _>>()?;
            (
                CanonicalProcessOperationV2::Poll,
                json!({
                    "session_ids": session_ids,
                    "yield_time_ms": arguments.get("timeout_ms").and_then(json_u64).unwrap_or(0).min(3_600_000),
                    "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                    "max_output_records": 256,
                }),
            )
        }
        ToolProjectionLoweringV2::PiBash => {
            // `timeout` is seconds and only an observation bound: omitted is
            // the bound foreground budget, 0 starts in the background.
            let yield_time_ms = match arguments.get("timeout").and_then(json_u64) {
                Some(0) => UNIFIED_EXEC_MIN_YIELD_MS,
                Some(seconds) => seconds
                    .saturating_mul(1000)
                    .clamp(UNIFIED_EXEC_MIN_YIELD_MS, UNIFIED_EXEC_MAX_YIELD_MS),
                None => environment.map_or(30_000, |environment| {
                    environment.config().limits().foreground_seconds() * 1000
                }),
            };
            (
                CanonicalProcessOperationV2::Start,
                json!({
                    "command": arguments.get("command").cloned().unwrap_or(Value::Null),
                    "workdir": ".",
                    "login": false,
                    "tty": false,
                    "keep_stdin_open": false,
                    "yield_time_ms": yield_time_ms,
                    "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                }),
            )
        }
        ToolProjectionLoweringV2::PiWait => {
            let mut wait = json!({
                "wake": "any",
                "yield_time_ms": arguments
                    .get("timeout_ms")
                    .and_then(json_u64)
                    .unwrap_or(PI_WAIT_DEFAULT_TIMEOUT_MS)
                    .min(PI_WAIT_MAX_TIMEOUT_MS),
                "max_output_bytes": PROCESS_OBSERVATION_BYTES,
                "max_output_records": 256,
            });
            if let Some(ids) = arguments.get("ids").and_then(Value::as_array) {
                wait["session_ids"] = ids
                    .iter()
                    .map(pi_process_id)
                    .collect::<Result<Vec<_>, _>>()?
                    .into();
            }
            (CanonicalProcessOperationV2::Poll, wait)
        }
        ToolProjectionLoweringV2::PiStop => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": pi_process_id(arguments.get("id").unwrap_or(&Value::Null))?}),
        ),
        ToolProjectionLoweringV2::PiRead => {
            let path = arguments
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if is_image_path(path) {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    PI_READ_IMAGE_REFUSAL,
                ));
            }
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("path").cloned().unwrap_or(Value::Null),
            );
            if let Some(offset) = arguments
                .get("offset")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                read.insert("offset".to_owned(), offset);
            }
            // The canonical page holds at most 2000 lines; a larger `limit`
            // reads that page and says where to continue.
            let limit = arguments
                .get("limit")
                .and_then(json_positive_u64)
                .unwrap_or(2000)
                .min(2000);
            read.insert("limit".to_owned(), Value::from(limit));
            (capability_operation(entry)?, Value::Object(read))
        }
        ToolProjectionLoweringV2::PiEdit => {
            let edits = arguments
                .get("edits")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|edit| {
                    let old_text = edit.get("oldText").cloned().unwrap_or(Value::Null);
                    if old_text.as_str().is_some_and(str::is_empty) {
                        return Err(ToolRuntimeError::new(
                            ToolRuntimeErrorKind::InvalidArguments,
                            "edit requires a non-empty edits[].oldText; use write to create a file",
                        ));
                    }
                    Ok(json!({
                        "old_string": old_text,
                        "new_string": edit.get("newText").cloned().unwrap_or(Value::Null),
                    }))
                })
                .collect::<Result<Vec<_>, _>>()?;
            (
                capability_operation(entry)?,
                json!({
                    "file_path": arguments.get("path").cloned().unwrap_or(Value::Null),
                    "edits": edits,
                }),
            )
        }
        ToolProjectionLoweringV2::PiWrite => (
            capability_operation(entry)?,
            json!({
                "file_path": arguments.get("path").cloned().unwrap_or(Value::Null),
                "old_string": "",
                "new_string": arguments.get("content").cloned().unwrap_or(Value::Null),
                "replace_all": true,
            }),
        ),
        ToolProjectionLoweringV2::GrokKillCommand => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": grok_process_task_id(arguments.get("task_id").unwrap_or(&Value::Null))?}),
        ),
        ToolProjectionLoweringV2::ClaudeTaskStop => (
            CanonicalProcessOperationV2::Terminate,
            json!({"session_id": process_task_id(arguments.get("task_id").unwrap_or(&Value::Null), "task_id")?}),
        ),
        ToolProjectionLoweringV2::CanonicalPassthrough => (
            capability_operation(entry)?,
            Value::Object(arguments.clone()),
        ),
        ToolProjectionLoweringV2::CodexApplyPatch => {
            return Err(registration_error(
                "CodexApplyPatch lowering must be handled before structured merge",
            ));
        }
        ToolProjectionLoweringV2::GrokReadFile => {
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("target_file").cloned().unwrap_or(Value::Null),
            );
            let offset = arguments
                .get("offset")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(1));
            read.insert("offset".to_owned(), offset);
            let limit = arguments
                .get("limit")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(1000));
            read.insert("limit".to_owned(), limit);
            (capability_operation(entry)?, Value::Object(read))
        }
        ToolProjectionLoweringV2::GrokGrep => {
            if arguments
                .get("multiline")
                .and_then(Value::as_bool)
                .is_some_and(|enabled| enabled)
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "grep multiline mode is unsupported",
                ));
            }
            let mut search = Map::new();
            search.insert(
                "query".to_owned(),
                arguments.get("pattern").cloned().unwrap_or(Value::Null),
            );
            search.insert(
                "path".to_owned(),
                arguments
                    .get("path")
                    .cloned()
                    .filter(|value| !value.is_null())
                    .unwrap_or_else(|| Value::String(String::from("."))),
            );
            let case_sensitive = arguments
                .get("-i")
                .and_then(Value::as_bool)
                .is_some_and(|insensitive| !insensitive);
            search.insert("case_sensitive".to_owned(), Value::Bool(case_sensitive));
            search.insert("use_regex".to_owned(), Value::Bool(true));
            if let Some(glob) = arguments
                .get("glob")
                .cloned()
                .filter(|value| !value.is_null())
            {
                search.insert("filename_glob".to_owned(), glob);
            } else if let Some(file_type) = arguments
                .get("type")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                search.insert("filename_glob".to_owned(), grok_type_to_glob(file_type)?);
            }
            if let Some(head_limit) = arguments
                .get("head_limit")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                search.insert("head_limit".to_owned(), head_limit);
            } else {
                search.insert("head_limit".to_owned(), json!(200));
            }
            for (arg, field) in [
                ("-B", "before_context"),
                ("-A", "after_context"),
                ("-C", "context"),
            ] {
                if let Some(lines) = arguments.get(arg).and_then(json_positive_u64) {
                    search.insert(field.to_owned(), Value::from(lines));
                }
            }
            (capability_operation(entry)?, Value::Object(search))
        }
        ToolProjectionLoweringV2::GrokSearchReplace | ToolProjectionLoweringV2::ClaudeEdit => {
            let old_string = arguments.get("old_string").cloned().unwrap_or(Value::Null);
            if entry.lowering == ToolProjectionLoweringV2::ClaudeEdit
                && old_string.as_str().is_some_and(str::is_empty)
            {
                return Err(ToolRuntimeError::new(
                    ToolRuntimeErrorKind::InvalidArguments,
                    "Edit requires a non-empty old_string; use Write to create a new file",
                ));
            }
            let replace_all = if old_string.as_str() == Some("") {
                Value::Bool(false)
            } else {
                arguments
                    .get("replace_all")
                    .cloned()
                    .unwrap_or(Value::Bool(false))
            };
            (
                capability_operation(entry)?,
                json!({
                    "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
                    "old_string": old_string,
                    "new_string": arguments.get("new_string").cloned().unwrap_or(Value::Null),
                    "replace_all": replace_all,
                }),
            )
        }
        // Claude Code's Write creates or overwrites, like Grok's `write`.
        ToolProjectionLoweringV2::GrokWrite | ToolProjectionLoweringV2::ClaudeWrite => (
            capability_operation(entry)?,
            json!({
                "file_path": arguments.get("file_path").cloned().unwrap_or(Value::Null),
                "old_string": "",
                "new_string": arguments.get("content").cloned().unwrap_or(Value::Null),
                "replace_all": true,
            }),
        ),
        ToolProjectionLoweringV2::GrokListDir => (
            capability_operation(entry)?,
            json!({
                "path": arguments.get("target_directory").cloned().unwrap_or(Value::Null),
                // `search_repository` requires `query` even when it only
                // lists: a listing carries the empty needle.
                "query": "",
                "list_directory": true,
            }),
        ),
        ToolProjectionLoweringV2::ClaudeReadFile => {
            let mut read = Map::new();
            read.insert(
                "path".to_owned(),
                arguments.get("file_path").cloned().unwrap_or(Value::Null),
            );
            if let Some(offset) = arguments
                .get("offset")
                .and_then(json_positive_u64)
                .map(Value::from)
            {
                read.insert("offset".to_owned(), offset);
            }
            let limit = arguments
                .get("limit")
                .cloned()
                .filter(|value| !value.is_null())
                .unwrap_or(json!(2000));
            read.insert("limit".to_owned(), limit);
            (capability_operation(entry)?, Value::Object(read))
        }
    };
    if operation == CanonicalProcessOperationV2::Start
        && matches!(
            entry.lowering,
            ToolProjectionLoweringV2::CodexExecCommand
                | ToolProjectionLoweringV2::GrokRunTerminalCommand
                | ToolProjectionLoweringV2::ClaudeBash
                | ToolProjectionLoweringV2::PiBash
        )
    {
        canonical["hint_source"] = Value::String(
            if arguments.get("run_in_background").and_then(Value::as_bool) == Some(true)
                || (entry.lowering == ToolProjectionLoweringV2::PiBash
                    && arguments.get("timeout").and_then(json_u64) == Some(0))
            {
                "background"
            } else if ["yield_time_ms", "block_until_ms", "timeout"]
                .iter()
                .any(|key| projected.get(*key).is_some())
            {
                "model_hint"
            } else {
                "default"
            }
            .into(),
        );
    }
    if !entry.canonical_operations.contains_key(&operation) {
        return Err(registration_error(
            "projection lowering selected an unenrolled canonical operation",
        ));
    }
    if entry.lowering == ToolProjectionLoweringV2::CodexExecCommand
        && operation == CanonicalProcessOperationV2::Start
        && let Some(environment) = environment
    {
        let workdir = canonical
            .get("workdir")
            .and_then(Value::as_str)
            .ok_or_else(projection_workdir_error)?;
        canonical["workdir"] = Value::String(
            canonical_process_workdir(environment, Path::new(workdir))?
                .to_string_lossy()
                .into_owned(),
        );
    }
    let canonical = SemanticJsonV1::from_value(canonical).map_err(|error| {
        ToolRuntimeError::from_sensitive_source(
            ToolRuntimeErrorKind::InvalidArguments,
            "projected input could not be lowered canonically",
            &error,
        )
    })?;
    Ok((operation, LoweredProjectionArguments::Structured(canonical)))
}

pub(super) fn canonical_process_workdir(
    environment: &ExecutionEnvironment,
    workdir: &Path,
) -> Result<PathBuf, ToolRuntimeError> {
    let relative = if workdir.is_absolute() {
        workdir
            .strip_prefix(environment.cwd())
            .map_err(|_| projection_workdir_error())?
    } else {
        workdir
    };
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_) | Component::CurDir))
    {
        return Err(projection_workdir_error());
    }
    if relative.as_os_str().is_empty() {
        Ok(PathBuf::from("."))
    } else {
        Ok(relative.to_path_buf())
    }
}

fn projection_workdir_error() -> ToolRuntimeError {
    ToolRuntimeError::new(
        ToolRuntimeErrorKind::WorkspacePolicy,
        "process working directory must be the active workspace cwd or one of its descendants",
    )
}

fn grok_type_to_glob(file_type: &str) -> Result<Value, ToolRuntimeError> {
    let glob = match file_type {
        "rust" => "*.rs",
        "py" => "*.py",
        "js" => "*.js",
        "ts" => "*.{ts,tsx}",
        "go" => "*.go",
        "java" => "*.java",
        "md" => "*.md",
        "json" => "*.json",
        other => {
            return Err(ToolRuntimeError::new(
                ToolRuntimeErrorKind::InvalidArguments,
                format!("grep type `{other}` is unsupported; use glob instead"),
            ));
        }
    };
    Ok(Value::String(glob.to_owned()))
}
