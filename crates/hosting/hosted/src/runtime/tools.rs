//! Tool subject, preview, and result-summary projection.

use std::collections::HashMap;

use plakous::model::ContentBlock;

use crate::session_log::{redact_json_preview, redact_text_preview};

use super::{
    PendingToolInfo, TOOL_OBSERVATION_PREVIEW_BYTES, TOOL_PREVIEW_TRUNCATION_MARKER,
    ToolSubjectStash,
};

/// Product-only operation labels; canonical tool identities remain untouched.
pub(super) fn process_observation_label(
    canonical_name: &str,
    exposed_name: Option<&str>,
    args: &serde_json::Value,
) -> Option<&'static str> {
    if is_pi_wait(canonical_name, exposed_name, args) {
        // Pi's `wait` is an await (300000 ms when `timeout_ms` is omitted);
        // only an explicit zero is a snapshot read.
        return Some(
            if args.get("timeout_ms").and_then(serde_json::Value::as_u64) == Some(0) {
                "process_session_read"
            } else {
                "process_session_wait"
            },
        );
    }
    exposed_name
        .and_then(|name| process_label(name, args))
        .or_else(|| process_label(canonical_name, args))
}

/// The canonical process poll identity every await presentation lowers onto.
const PROCESS_POLL: &str = "process_session_poll";

/// Pi's `wait`: presented as `wait` (or `wait_jobs`) over the process poll on
/// a leaf, or as the tree's `wait` on a node granted agent tools. A history
/// row knows only the presented name. A composition cell-wait marker spelled
/// `wait` names a `cell_id`; pi's never does.
fn is_pi_wait(canonical_name: &str, exposed_name: Option<&str>, args: &serde_json::Value) -> bool {
    let shown = exposed_name.unwrap_or(canonical_name);
    matches!(shown, "wait" | "wait_jobs")
        && matches!(canonical_name, PROCESS_POLL | "wait" | "wait_jobs")
        && args.get("cell_id").is_none()
}

/// A composition cell-wait marker: its canonical identity is its own name.
fn is_cell_wait_marker(canonical_name: &str, exposed_name: &str) -> bool {
    matches!(canonical_name, "wait" | "wait_js" | "wait_py") && canonical_name == exposed_name
}

pub(super) fn process_label(name: &str, args: &serde_json::Value) -> Option<&'static str> {
    let input = args
        .get("input")
        .or_else(|| args.get("chars"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    match name {
        "process_session_start"
        | "run_process"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "Bash"
        | "bash"
        | "Shell" => Some("Shell"),
        "exec_command"
            if args
                .get("cmd")
                .or_else(|| args.get("command"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|cmd| !cmd.is_empty()) =>
        {
            Some("Shell")
        }
        "exec_command" => Some(
            if args
                .get("yield_time_ms")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
            {
                "process_session_read"
            } else {
                "process_session_wait"
            },
        ),
        "write_stdin" | "process_session_write" if input == "\u{3}" => {
            Some("process_session_interrupt")
        }
        "write_stdin" | "process_session_write" if !input.is_empty() => {
            Some("process_session_write")
        }
        "write_stdin" | "process_session_write" => Some("process_session_wait"),
        "TaskStop" | "KillShell" | "kill_command_or_subagent" | "process_session_terminate" => {
            Some("process_session_terminate")
        }
        // Pi's `stop {id}`; one naming a child reads as `interrupt_agent`.
        "stop" if args.get("id").is_some() => Some("process_session_terminate"),
        "BashOutput"
            if args.get("block").and_then(serde_json::Value::as_bool) == Some(false)
                || args
                    .get("yield_time_ms")
                    .and_then(serde_json::Value::as_u64)
                    == Some(0) =>
        {
            Some("process_session_read")
        }
        "BashOutput" => Some("process_session_wait"),
        "process_session_poll" | "get_command_or_subagent_output" => Some(
            if args.get("block").and_then(serde_json::Value::as_bool) == Some(false)
                || args
                    .get("yield_time_ms")
                    .or_else(|| args.get("timeout_ms"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    == 0
            {
                "process_session_read"
            } else {
                "process_session_wait"
            },
        ),
        _ => None,
    }
}

pub(super) fn process_ids(args: &serde_json::Value) -> Vec<String> {
    let id = |value: &serde_json::Value| {
        value
            .as_u64()
            .filter(|n| *n != 0)
            .map(|n| n.to_string())
            .or_else(|| {
                value.as_str().and_then(|s| {
                    let s = s.trim();
                    let s = s.strip_prefix('p').unwrap_or(s);
                    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
                        return None;
                    }
                    s.parse::<u64>()
                        .ok()
                        .filter(|n| *n != 0)
                        .map(|n| n.to_string())
                })
            })
    };
    if let Some(ids) = args
        .get("session_ids")
        .or_else(|| args.get("task_ids"))
        .or_else(|| args.get("ids"))
        .and_then(serde_json::Value::as_array)
    {
        return ids
            .iter()
            .map(|value| id(value).unwrap_or_else(|| "(unknown)".into()))
            .collect();
    }
    ["session_id", "bash_id", "task_id", "shell_id", "id"]
        .iter()
        .find_map(|key| {
            args.get(key)
                .map(|value| id(value).unwrap_or_else(|| "(unknown)".into()))
        })
        .into_iter()
        .collect()
}

pub(super) fn process_subject(
    label: &str,
    args: &serde_json::Value,
    locator: Option<plakous::sessions::SessionLocator>,
    processes: &[plakous_host_protocol::views::ProcessView],
    wait_result: Option<&serde_json::Value>,
) -> String {
    if label == "Shell" {
        return crate::sessions::bound_subject(&redact_text_preview(
            &tool_subject_from_args("process_session_start", args).unwrap_or_default(),
        ));
    }
    process_subject_for_ids(
        label,
        &process_ids(args),
        args.get("input")
            .or_else(|| args.get("chars"))
            .and_then(serde_json::Value::as_str),
        locator,
        processes,
        wait_result,
        &HashMap::new(),
    )
}

pub(super) fn process_key(locator: plakous::sessions::SessionLocator, id: &str) -> String {
    format!("{}:{}:p{id}", locator.session_id(), locator.branch_id())
}

pub(super) fn process_subject_for_ids(
    label: &str,
    ids: &[String],
    input: Option<&str>,
    locator: Option<plakous::sessions::SessionLocator>,
    processes: &[plakous_host_protocol::views::ProcessView],
    wait_result: Option<&serde_json::Value>,
    start_commands: &HashMap<String, String>,
) -> String {
    let limit = 240 - process_action_label(label, wait_result.is_some()).len() - 1;
    let subjects = ids
        .iter()
        .map(|id| {
            let command = locator.and_then(|locator| {
                let key = process_key(locator, id);
                processes
                    .iter()
                    .find(|process| process.id == key && !process.command.is_empty())
                    .map(|process| process.command.as_str())
                    .or_else(|| start_commands.get(&key).map(String::as_str))
            });
            let mut subject = id.clone();
            if label == "process_session_write" {
                subject.push_str(&format!(" · {} bytes", input.unwrap_or_default().len()));
            } else if let Some(command) = command {
                subject.push_str(&format!(" · {command}"));
            }
            let outcome = if label == "process_session_wait" {
                wait_result
                    .and_then(|result| process_wait_outcome(result, id))
                    .map(|outcome| format!(" · {outcome}"))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            format!(
                "{}{outcome}",
                crate::sessions::bound_subject_to(
                    &redact_text_preview(&subject),
                    limit - outcome.len()
                )
            )
        })
        .collect::<Vec<_>>();
    crate::sessions::bound_subject_to(&redact_text_preview(&subjects.join(", ")), limit)
}

pub(super) fn process_wait_outcome(result: &serde_json::Value, id: &str) -> Option<String> {
    let result = if let Some(tasks) = result.get("tasks").and_then(serde_json::Value::as_array) {
        tasks
            .iter()
            .find(|task| process_ids(task).iter().any(|task_id| task_id == id))?
    } else {
        result
    };
    if result.get("status").and_then(serde_json::Value::as_str) == Some("terminated") {
        return Some("stopped".into());
    }
    if let Some(code) = result.get("exit_code").and_then(serde_json::Value::as_i64) {
        return Some(format!("exited {code}"));
    }
    match result.get("status").and_then(serde_json::Value::as_str) {
        Some("running") => Some("still running".into()),
        Some("exited") => Some("exited".into()),
        _ if result.get("status").is_none_or(serde_json::Value::is_null)
            && result.get("session_id").is_some_and(|id| !id.is_null()) =>
        {
            Some("still running".into())
        }
        _ => None,
    }
}

fn process_action_label(label: &str, completed: bool) -> &'static str {
    match label {
        "Shell" => "",
        "process_session_wait" if completed => "Waited on process",
        "process_session_wait" => "Waiting on process",
        "process_session_read" => "Read output of process",
        "process_session_write" => "Sent input to process",
        "process_session_interrupt" => "Sent interrupt to process",
        "process_session_terminate" => "Stopped process",
        _ => "",
    }
}

pub(super) fn process_action_subject(label: &str, subject: &str, completed: bool) -> String {
    let verb = process_action_label(label, completed);
    if verb.is_empty() {
        return subject.to_owned();
    }
    let subject = if subject.is_empty() {
        "(unknown)"
    } else {
        subject
    };
    format!(
        "{verb} {}",
        crate::sessions::bound_subject_to(subject, 240 - verb.len() - 1)
    )
}

/// Compact shell subject: `$ ls -la` not `$ /bin/ls ls -la`.
/// Collect final assistant-visible text from a completed model response.
pub(super) fn harvest_assistant_text(response: &plakous::model::ModelResponse) -> String {
    let mut out = String::new();
    for item in response.output() {
        for block in item.blocks() {
            if let ContentBlock::Text(text) = block {
                let t = text.text().trim();
                if t.is_empty() {
                    continue;
                }
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(t);
            }
        }
    }
    out
}

/// The final answer's text blocks as one reader-facing text.
///
/// A response can carry several text blocks (commentary, then the answer).
/// Concatenated raw they run together mid-sentence, so a block that does not
/// already end in a line break is separated from the next by a blank line.
pub(super) fn joined_assistant_text(response: &plakous::model::ModelResponse) -> String {
    let mut out = String::new();
    for item in response.output() {
        for block in item.blocks() {
            let ContentBlock::Text(text) = block else {
                continue;
            };
            let text = text.text();
            if text.is_empty() {
                continue;
            }
            if !out.is_empty() && !out.ends_with('\n') {
                out.push_str("\n\n");
            }
            out.push_str(text);
        }
    }
    out
}

/// One §5.2 collapsed header subject, built from the tool's own arguments.
///
/// The header formats are normative: `Run {command}`, `Read {path} ({a}-{b})`,
/// `Search "{pattern}"[ in {path}]`, `Edit {path} +{ins}/-{del}`, `Fetch {url}`,
/// `List {path}`. Result-derived counts arrive later from
/// [`tool_result_summary`]; the renderer never parses tool output for them.
pub(super) fn tool_subject_from_args(name: &str, args: &serde_json::Value) -> Option<String> {
    let obj = args.as_object()?;
    if let Some(label) = process_label(name, args).filter(|label| *label != "Shell") {
        return Some(process_subject(label, args, None, &[], None));
    }
    let first_str = |keys: &[&str]| {
        keys.iter().find_map(|k| {
            obj.get(*k)
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| {
                    plakous::tools::preview_redaction::text(s)
                        .chars()
                        .take(160)
                        .collect::<String>()
                })
        })
    };
    let first_u64 = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| obj.get(*k).and_then(serde_json::Value::as_u64))
    };
    match name {
        "exec_command"
        | "run_process"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "Bash"
        | "bash"
        | "Shell"
        | "process_session_start" => first_str(&["cmd", "command"]),
        "read_file" | "view_file" | "inspect_image" | "view_image" | "Read" => {
            let path = first_str(&[
                "path",
                "file",
                "filepath",
                "target",
                "target_file",
                "file_path",
            ])?;
            // `({start}-{end})` only when the caller actually bounded the read.
            let start = first_u64(&["offset", "start", "start_line", "line"]);
            let limit = first_u64(&["limit", "count", "lines"]);
            Some(match (start, limit) {
                // `limit` counts lines, so the last line read is
                // `start + limit - 1`; `start + limit` named a line the call
                // never asked for.
                (Some(start), Some(limit)) if limit > 0 => format!(
                    "{path} ({start}-{})",
                    start.saturating_add(limit).saturating_sub(1)
                ),
                (Some(start), _) => format!("{path} ({start}-)"),
                _ => path,
            })
        }
        "search_repository" | "grep" | "Grep" | "glob" | "Glob" | "codebase_search"
        | "file_search" => {
            let pattern = first_str(&["query", "pattern", "regex"])?;
            let scope = first_str(&["path", "dir", "directory", "glob"]);
            Some(match scope {
                Some(scope) => format!("\"{pattern}\" in {scope}"),
                None => format!("\"{pattern}\""),
            })
        }
        "list_dir" | "list_directory" | "ls" => {
            first_str(&["path", "dir", "directory", "target_directory"])
        }
        "web_fetch" | "WebFetch" | "fetch" | "fetch_url" | "open_url" => {
            first_str(&["url"]).map(|url| url_host(&url).to_owned())
        }
        "web__run" => first_str(&["url", "query", "prompt"]),
        "image_gen" => {
            let path = first_str(&["path"])?;
            Some(match first_str(&["size"]) {
                Some(size) => format!("{path} ({size})"),
                None => path,
            })
        }
        "web_search" | "WebSearch" | "x_search" => {
            first_str(&["query"]).map(|query| format!("\"{query}\""))
        }
        name if plakous_host_protocol::views::presentation::McpCall::parse(name).is_some() => {
            mcp_arguments_subject(obj)
        }
        "search_replace" | "edit_file" | "write_file" | "create_file" | "Edit" | "Write"
        | "write" => {
            let path = first_str(&["file_path", "path", "target_file"])?;
            let added = obj
                .get("new_string")
                .or_else(|| obj.get("contents"))
                .or_else(|| obj.get("content"))
                .and_then(|v| v.as_str())
                .map(|text| text.lines().count());
            let removed = obj
                .get("old_string")
                .and_then(|v| v.as_str())
                .map(|text| text.lines().count());
            Some(match (added, removed) {
                (Some(added), Some(removed)) => format!("{path} +{added}/-{removed}"),
                (Some(added), None) => format!("{path} +{added}/-0"),
                _ => path,
            })
        }
        "todo_write" | "update_plan" | "merge_plan" => plan_header_subject(obj),
        // §5.9: a subagent row names the class it spawned and its brief.
        "spawn_agent" => {
            let class = first_str(&["class"]);
            let brief = first_str(&["description", "prompt", "name"]);
            match (class, brief) {
                (Some(class), Some(brief)) => Some(format!("{class} · {brief}")),
                (Some(only), None) | (None, Some(only)) => Some(only),
                (None, None) => None,
            }
        }
        // The three read-side topology tools carry §5.2 subjects too. Left
        // to the generic fallback they rendered as a bare tool name, because
        // none of their arguments is `path`/`query`/`prompt`/`url`.
        "wait_agent" => {
            // Grok's output tool names its children among `task_ids`, pi's
            // `wait` among `ids`.
            let ids = obj
                .get("agent_ids")
                .or_else(|| obj.get("task_ids"))
                .or_else(|| obj.get("ids"))
                .and_then(serde_json::Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(serde_json::Value::as_str)
                        .filter(|id| !plakous_host_protocol::topology::is_process_task_id(id))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .filter(|ids| !ids.is_empty())?;
            Some(match first_u64(&["timeout_ms"]) {
                Some(ms) => format!("{ids} · up to {}", format_wait_bound(ms)),
                None => ids,
            })
        }
        "list_agents" => Some("agents".to_owned()),
        "send_agent_message" => first_str(&["agent_id"]),
        "close_agent" | "interrupt_agent" | "recover_agent" | "discard_workspace" => {
            first_str(&["agent_id", "task_id", "id"])
        }
        "adopt_candidate" => first_str(&["candidate_id"]),
        // A `wait*` continuation names the cell it resumes and the bound it
        // waits under; the Compose row keeps the counts.
        "wait" | "wait_js" | "wait_py" => {
            let cell = first_str(&["cell_id"])
                .or_else(|| first_u64(&["cell_id"]).map(|id| id.to_string()));
            let bound =
                first_u64(&["yield_time_ms"]).map(|ms| format!("up to {}", format_wait_bound(ms)));
            let terminate = obj
                .get("terminate")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let mut parts = Vec::new();
            if let Some(cell) = cell {
                parts.push(format!("cell {cell}"));
            }
            if terminate {
                parts.push("terminate".to_owned());
            } else if let Some(bound) = bound {
                parts.push(bound);
            }
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        "apply_patch" => {
            let patch = obj
                .get("patch")
                .or_else(|| obj.get("input"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(patch_subject(patch))
        }
        _ => first_str(&["path", "query", "prompt", "url", "name", "cmd", "command"]),
    }
}

/// Web search, fetch, image generation and MCP rows carry the host's own
/// subject: the runtime's generic one would be a raw query, a whole URL or
/// one argument.
pub(super) fn is_web_or_mcp_row(name: &str) -> bool {
    matches!(
        name,
        "web_fetch"
            | "WebFetch"
            | "fetch"
            | "fetch_url"
            | "open_url"
            | "web_search"
            | "WebSearch"
            | "x_search"
            | "image_gen"
    ) || plakous_host_protocol::views::presentation::McpCall::parse(name).is_some()
}

/// The host a fetch row names: never the scheme, credentials, path, query
/// or fragment of the URL, which the expanded detail still shows.
fn url_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    if host.is_empty() { url } else { host }
}

/// Most bytes of an MCP call's argument summary.
const MCP_ARGUMENTS_SUBJECT_BYTES: usize = 120;

/// A bounded `key=value` summary of an MCP call's scalar arguments, in key
/// order; nested values show as `key=…`.
fn mcp_arguments_subject(args: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let mut summary = String::new();
    for (key, value) in args {
        let value = match value {
            serde_json::Value::String(text) => {
                let text = plakous::tools::preview_redaction::text(text.trim());
                let mut cut: String = text.chars().take(40).collect();
                if cut.len() < text.len() {
                    cut.push('…');
                }
                format!("\"{cut}\"")
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => "…".to_owned(),
            scalar => scalar.to_string(),
        };
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(&format!("{key}={value}"));
        if summary.len() > MCP_ARGUMENTS_SUBJECT_BYTES {
            let mut end = MCP_ARGUMENTS_SUBJECT_BYTES;
            while !summary.is_char_boundary(end) {
                end -= 1;
            }
            summary.truncate(end);
            summary.push('…');
            break;
        }
    }
    (!summary.is_empty()).then_some(summary)
}

/// `30s` / `1.5s` / `250ms` for a `wait*` yield bound.
pub(super) fn format_wait_bound(ms: u64) -> String {
    if ms < 1000 {
        return format!("{ms}ms");
    }
    let seconds = ms / 1000;
    let tenths = (ms % 1000) / 100;
    if tenths == 0 {
        format!("{seconds}s")
    } else {
        format!("{seconds}.{tenths}s")
    }
}

/// The §5.2 subject of a call the host learnt from its own `ToolStarted`
/// observation.
///
/// The model-facing (exposed) name is tried first because the arguments are
/// shaped by the exposed schema (`exec_command { cmd }`), then the canonical
/// name. `ResponseCompleted` is a droppable observation and live
/// Codex turns never delivered it before the tool ran, so the started event's
/// own bounded preview is the only reliable source. A preview that no longer
/// parses (truncated past the observation bound) yields no subject — nothing is
/// guessed from a partial argument.
pub(super) fn tool_subject_from_observation(
    canonical_name: &str,
    exposed_name: Option<&str>,
    observation: &plakous::agent::ToolCallObservation,
) -> Option<String> {
    let args = serde_json::from_str::<serde_json::Value>(observation.arguments_preview()).ok()?;
    if let Some(label) = process_observation_label(canonical_name, exposed_name, &args)
        .filter(|label| *label != "Shell")
    {
        return Some(process_subject(label, &args, None, &[], None));
    }
    // The cell-wait subject belongs to the composition markers only: another
    // tool spelled `wait` (pi's jobs await) is not a cell continuation.
    exposed_name
        .filter(|name| {
            !matches!(*name, "wait" | "wait_js" | "wait_py")
                || is_cell_wait_marker(canonical_name, name)
        })
        .and_then(|name| tool_subject_from_args(name, &args))
        .or_else(|| tool_subject_from_args(canonical_name, &args))
}

pub(super) fn started_tool_subject(
    canonical_name: &str,
    exposed_name: Option<&str>,
    observation: Option<&plakous::agent::ToolCallObservation>,
    pending: Option<&PendingToolInfo>,
) -> Option<String> {
    let host_subject = || {
        observation
            .and_then(|item| tool_subject_from_observation(canonical_name, exposed_name, item))
            .or_else(|| pending.map(|item| item.subject.clone()))
    };
    if matches!(
        canonical_name,
        "apply_patch"
            | "search_replace"
            | "edit_file"
            | "write_file"
            | "create_file"
            | "Edit"
            | "Write"
            | "write"
    ) || matches!(exposed_name, Some("Edit" | "Write" | "apply_patch"))
        || [Some(canonical_name), exposed_name]
            .into_iter()
            .flatten()
            .any(is_web_or_mcp_row)
    {
        return host_subject()
            .or_else(|| observation.and_then(|item| item.subject().map(str::to_owned)));
    }
    observation
        .and_then(|item| item.subject().map(str::to_owned))
        .or_else(|| {
            observation
                .and_then(|item| tool_subject_from_observation(canonical_name, exposed_name, item))
        })
        .or_else(|| pending.map(|item| item.subject.clone()))
}

/// `Edit {path} +{ins}/-{del}` from a patch body, per §5.2.
///
/// A patch touching more than one file states the file count instead of a path
/// it would have to pick between.
pub(super) fn patch_subject(patch: &str) -> String {
    if patch.is_empty() {
        return "patch".to_owned();
    }
    let mut files: Vec<&str> = Vec::new();
    for line in patch.lines() {
        for marker in [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "+++ b/",
        ] {
            if let Some(touched) = line.strip_prefix(marker) {
                let touched = touched.trim();
                if !touched.is_empty() && !files.contains(&touched) {
                    files.push(touched);
                }
            }
        }
    }
    let adds = patch
        .lines()
        .filter(|line| line.starts_with('+') && !line.starts_with("+++"))
        .count();
    let dels = patch
        .lines()
        .filter(|line| line.starts_with('-') && !line.starts_with("---"))
        .count();
    match files.as_slice() {
        [only] => format!("{only} +{adds}/-{dels}"),
        [] => format!("patch +{adds}/-{dels}"),
        many => format!("{} files +{adds}/-{dels}", many.len()),
    }
}

/// Collapsed Plan subject: `5 items · 1 in progress`, never the key name `todos`.
fn plan_header_subject(obj: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let items = obj
        .get("todos")
        .or_else(|| obj.get("plan"))
        .and_then(serde_json::Value::as_array)?;
    let n = items.len();
    let in_progress = items
        .iter()
        .filter(|item| {
            item.get("status").and_then(serde_json::Value::as_str) == Some("in_progress")
        })
        .count();
    let noun = if n == 1 { "item" } else { "items" };
    Some(if in_progress > 0 {
        format!("{n} {noun} · {in_progress} in progress")
    } else {
        format!("{n} {noun}")
    })
}

/// Whether a preview is the whole result, so counts over it are totals.
pub(super) fn preview_is_complete(preview: &str) -> bool {
    !preview.trim_end().ends_with(TOOL_PREVIEW_TRUNCATION_MARKER)
        && preview.len() < TOOL_OBSERVATION_PREVIEW_BYTES
}

/// The §5.2 count suffix a completed call adds to its header.
///
/// Only the host reads tool output. The observation it reads is a **2 KiB
/// bounded preview**, so a count over it is a total only when the preview is
/// the whole result; otherwise it is approximate and is marked `~`. A 5,000-line
/// read must never render `(64 lines)` as though that were the file (§5.1 "no
/// invented count"). Anything the host cannot count truthfully stays absent.
pub(super) fn tool_result_summary(name: &str, preview: Option<&str>) -> Option<String> {
    let preview = preview?;
    let complete = preview_is_complete(preview);
    // `~` marks a count taken over the bounded window rather than the result.
    let approx = if complete { "" } else { "~" };
    match name {
        // §5.2 admits `({start}-{end})`, `({start}-{end} of {total})`,
        // `(empty)` and `({n} pages)` for Read — never a bare line count, which
        // over a bounded preview is the preview's own size and not the file's.
        "read_file" | "view_file" => complete
            .then(|| preview.lines().count() == 0)
            .and_then(|empty| empty.then(|| "(empty)".to_owned())),
        "search_repository" | "grep" | "Grep" | "glob" | "Glob" | "codebase_search"
        | "file_search" => {
            let matches = preview
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            let files = preview
                .lines()
                .filter_map(|line| line.split_once(':').map(|(path, _)| path))
                .collect::<std::collections::BTreeSet<_>>()
                .len();
            Some(match (matches, files) {
                (0, _) => "(no matches)".to_owned(),
                (1, _) if complete => "(1 match)".to_owned(),
                // Lines with no `path:` prefix are the path-list shape of
                // `glob` / `file_search`, so the tally is files, not matches.
                (matches, 0) => format!("({approx}{matches} files)"),
                (matches, files) => {
                    format!("({approx}{matches} matches in {approx}{files} files)")
                }
            })
        }
        "list_dir" | "list_directory" | "ls" => {
            let entries = preview
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count();
            Some(format!("({approx}{entries} entries)"))
        }
        _ => None,
    }
}

impl ToolSubjectStash {
    pub(super) fn new() -> Self {
        Self {
            by_name: HashMap::new(),
            by_call_id: HashMap::new(),
        }
    }

    pub(super) fn push(&mut self, name: String, call_id: Option<String>, info: PendingToolInfo) {
        let Some(id) = call_id else {
            return;
        };
        if let Some((prior_name, _)) = self.by_call_id.insert(id.clone(), (name.clone(), info)) {
            self.remove_name_id(&prior_name, &id);
        }
        self.by_name.entry(name).or_default().push_back(id);
    }

    /// Reads a stashed subject without consuming it.
    ///
    /// An approval row is projected while the call is still pending, so its
    /// subject must not be taken away from the eventual `ToolStarted` row.
    pub(super) fn peek(&self, name: &str, call_id: Option<&str>) -> Option<&PendingToolInfo> {
        let id = call_id?;
        let (stashed_name, info) = self.by_call_id.get(id)?;
        (stashed_name == name).then_some(info)
    }

    pub(super) fn take(&mut self, name: &str, call_id: Option<&str>) -> Option<PendingToolInfo> {
        if let Some(id) = call_id {
            let (stashed_name, info) = self.by_call_id.remove(id)?;
            self.remove_name_id(&stashed_name, id);
            return (stashed_name == name).then_some(info);
        }

        let queue = self.by_name.get_mut(name)?;
        queue.retain(|id| {
            self.by_call_id
                .get(id)
                .is_some_and(|(stashed_name, _)| stashed_name == name)
        });
        // A missing envelope identity cannot distinguish parallel same-name
        // calls. Correlate only a single remaining candidate; never guess FIFO.
        if queue.len() != 1 {
            return None;
        }
        let id = queue.pop_front()?;
        self.by_name.remove(name);
        self.by_call_id.remove(&id).map(|(_, info)| info)
    }

    pub(super) fn remove_name_id(&mut self, name: &str, call_id: &str) {
        if let Some(queue) = self.by_name.get_mut(name) {
            queue.retain(|id| id != call_id);
            if queue.is_empty() {
                self.by_name.remove(name);
            }
        }
    }
}

pub(super) fn args_detail_preview(args: &serde_json::Value) -> String {
    redact_json_preview(args)
}

pub(super) fn stash_tool_subjects_from_response(
    pending: &mut ToolSubjectStash,
    response: &plakous::model::ModelResponse,
) {
    for item in response.output() {
        for block in item.blocks() {
            if let ContentBlock::ToolCall(call) = block {
                let name = call.canonical_name().as_str().to_owned();
                let call_id = Some(call.call_id().as_uuid().to_string());
                let args_val = call.arguments().cloned().or_else(|| {
                    let raw = call.raw_arguments().trim();
                    if raw.is_empty() {
                        None
                    } else {
                        serde_json::from_str(raw).ok()
                    }
                });
                let subject = args_val
                    .as_ref()
                    .and_then(|v| tool_subject_from_args(&name, v))
                    .map(|subject| redact_text_preview(&subject))
                    .unwrap_or_else(|| name.clone());
                let detail = args_val
                    .as_ref()
                    .map(args_detail_preview)
                    .unwrap_or_else(|| subject.clone());
                pending.push(name, call_id, PendingToolInfo { subject, detail });
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::*;

    use crate::runtime::test_support::*;

    /// Pi's `wait {ids}` and `stop {id}` rows name their process ids, and a
    /// bare pi `wait` (a 300 s await by default) is labelled a wait, not a
    /// snapshot read.
    #[test]
    fn pi_wait_and_stop_rows_name_their_process_ids() {
        assert_eq!(
            process_observation_label("process_session_poll", Some("wait"), &serde_json::json!({})),
            Some("process_session_wait")
        );
        assert_eq!(
            process_subject(
                "process_session_wait",
                &serde_json::json!({"ids": ["p1"], "timeout_ms": 5000}),
                None,
                &[],
                None,
            ),
            "1"
        );
        assert_eq!(
            process_subject(
                "process_session_terminate",
                &serde_json::json!({"id": "p2"}),
                None,
                &[],
                None,
            ),
            "2"
        );
    }

    #[test]
    fn process_subject_routes_distinguish_start_wait_read_input_and_stop() {
        assert_eq!(
            process_observation_label(
                "process_session_poll",
                Some("BashOutput"),
                &serde_json::json!({"bash_id":"p7"})
            ),
            Some("process_session_wait")
        );
        assert_eq!(
            process_observation_label(
                "process_session_poll",
                Some("BashOutput"),
                &serde_json::json!({"session_id":7,"yield_time_ms":0})
            ),
            Some("process_session_read")
        );
        for (name, args, expected) in [
            ("Bash", serde_json::json!({"command":"cargo test"}), "Shell"),
            (
                "BashOutput",
                serde_json::json!({"bash_id":"p7"}),
                "process_session_wait",
            ),
            (
                "BashOutput",
                serde_json::json!({"bash_id":"p7", "block":false}),
                "process_session_read",
            ),
            (
                "TaskStop",
                serde_json::json!({"task_id":"p7"}),
                "process_session_terminate",
            ),
            (
                "KillShell",
                serde_json::json!({"shell_id":"7"}),
                "process_session_terminate",
            ),
            (
                "exec_command",
                serde_json::json!({"cmd":"cargo test"}),
                "Shell",
            ),
            (
                "exec_command",
                serde_json::json!({"session_id":7}),
                "process_session_wait",
            ),
            (
                "exec_command",
                serde_json::json!({"session_id":7,"yield_time_ms":0}),
                "process_session_read",
            ),
            (
                "write_stdin",
                serde_json::json!({"session_id":7,"chars":""}),
                "process_session_wait",
            ),
            (
                "write_stdin",
                serde_json::json!({"session_id":7,"chars":"yes\n"}),
                "process_session_write",
            ),
            (
                "write_stdin",
                serde_json::json!({"session_id":7,"chars":"\u{3}","tty":false}),
                "process_session_interrupt",
            ),
            (
                "run_terminal_command",
                serde_json::json!({"command":"cargo test"}),
                "Shell",
            ),
            (
                "get_command_or_subagent_output",
                serde_json::json!({"task_ids":["p7"],"timeout_ms":0}),
                "process_session_read",
            ),
            (
                "get_command_or_subagent_output",
                serde_json::json!({"task_ids":["p7"],"timeout_ms":1000}),
                "process_session_wait",
            ),
            (
                "kill_command_or_subagent",
                serde_json::json!({"task_id":"p7"}),
                "process_session_terminate",
            ),
            (
                "process_session_start",
                serde_json::json!({"command":"cargo test"}),
                "Shell",
            ),
            (
                "process_session_write",
                serde_json::json!({"session_id":7,"input":""}),
                "process_session_wait",
            ),
            (
                "process_session_write",
                serde_json::json!({"session_id":7,"input":"yes"}),
                "process_session_write",
            ),
            (
                "process_session_poll",
                serde_json::json!({"session_id":7,"yield_time_ms":0}),
                "process_session_read",
            ),
            (
                "process_session_poll",
                serde_json::json!({"session_id":7,"yield_time_ms":1000}),
                "process_session_wait",
            ),
            (
                "process_session_terminate",
                serde_json::json!({"session_id":7}),
                "process_session_terminate",
            ),
        ] {
            assert_eq!(process_label(name, &args), Some(expected), "{name}");
            assert!(
                !process_subject(expected, &args, None, &[], None).is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn process_subject_uses_the_scoped_registry_and_survives_restore() {
        use plakous_host_protocol::views::{ProcessStatus, ProcessView};
        let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
        let mut process = ProcessView {
            id: format!("{}:{}:p7", locator.session_id(), locator.branch_id()),
            command: "cargo test".into(),
            owner: "root".into(),
            started_at: 0,
            elapsed_ms: 0,
            status: ProcessStatus::Running,
            output: String::new(),
        };
        let mut other_session = process.clone();
        other_session.id = format!("{}:{}:p7", uuid::Uuid::from_u128(9), locator.branch_id());
        other_session.command = "child session command".into();
        let mut other_branch = process.clone();
        other_branch.id = format!("{}:{}:p7", locator.session_id(), uuid::Uuid::from_u128(9));
        other_branch.command = "child branch command".into();
        let args = serde_json::json!({"session_id":7});
        assert_eq!(
            process_subject(
                "process_session_wait",
                &args,
                Some(locator),
                &[other_session.clone(), other_branch.clone()],
                None,
            ),
            "7"
        );
        for (result, expected) in [
            (
                serde_json::json!({"status":"running"}),
                "7 · cargo test · still running",
            ),
            (
                serde_json::json!({"exit_code":0}),
                "7 · cargo test · exited 0",
            ),
            (
                serde_json::json!({"exit_code":7}),
                "7 · cargo test · exited 7",
            ),
        ] {
            process.status = ProcessStatus::OutcomeUnknown;
            let subject = process_subject(
                "process_session_wait",
                &args,
                Some(locator),
                &[other_session.clone(), other_branch.clone(), process.clone()],
                Some(&result),
            );
            assert_eq!(subject, expected);
            let subject = process_action_subject("process_session_wait", &subject, true);
            let encoded = crate::sessions::ToolItem {
                call_id: "wait-7".into(),
                name: "process_session_poll".into(),
                exposed_name: None,
                kind: crate::activity::tool_activity_kind("process_session_poll"),
                state: "done".into(),
                subject: Some(subject.clone()),
                detail: None,
                started_at_ms: None,
                finished_at_ms: None,
                parent_call_id: None,
                cell_id: None,
                wait: None,
            };
            assert_eq!(
                serde_json::from_value::<crate::sessions::ToolItem>(
                    serde_json::to_value(&encoded).unwrap()
                )
                .unwrap()
                .subject
                .as_deref(),
                Some(subject.as_str())
            );
        }
        assert_eq!(
            process_subject(
                "process_session_wait",
                &serde_json::json!({"bash_id":"p8"}),
                Some(locator),
                std::slice::from_ref(&process),
                None
            ),
            "8"
        );
        assert_eq!(
            process_subject(
                "process_session_wait",
                &args,
                None,
                std::slice::from_ref(&process),
                None
            ),
            "7"
        );
        process.command = format!("password={} {}", "x".repeat(250), "界".repeat(200));
        let subject = process_subject(
            "process_session_wait",
            &args,
            Some(locator),
            &[process],
            Some(&serde_json::json!({"exit_code":7})),
        );
        assert!(subject.len() <= 240);
        assert!(subject.contains("password=[redacted]"));
        assert!(!subject.contains(&"x".repeat(10)));
        assert!(subject.ends_with(" · exited 7"));
        let subject = process_action_subject("process_session_wait", &subject, true);
        assert!(subject.len() <= 240);
        assert!(subject.ends_with(" · exited 7"));
        let input = process_subject(
            "process_session_write",
            &serde_json::json!({"session_id":7,"chars":format!("password={}\n{}", "x".repeat(250), "界".repeat(200))}),
            None,
            &[],
            None,
        );
        assert!(input.len() <= 240);
        assert_eq!(input, "7 · 860 bytes");
        let input = process_action_subject("process_session_write", &input, false);
        assert!(input.len() <= 240);
        assert_eq!(input, "Sent input to process 7 · 860 bytes");
    }

    #[test]
    fn process_wait_outcomes_are_observation_local() {
        for (result, expected) in [
            (
                serde_json::json!({"status":"running","exit_code":null}),
                Some("still running"),
            ),
            (
                serde_json::json!({"status":"terminated","exit_code":130}),
                Some("stopped"),
            ),
            (serde_json::json!({"exit_code":7}), Some("exited 7")),
            (serde_json::json!({"session_id":7}), Some("still running")),
            (
                serde_json::json!({"tasks":[{"task_id":"p8","exit_code":0},{"task_id":"p7","status":"running"}]}),
                Some("still running"),
            ),
            (
                serde_json::json!({"tasks":[{"task_id":"p8","exit_code":0}]}),
                None,
            ),
            (serde_json::json!({"status":"not_found"}), None),
            (serde_json::Value::Null, None),
        ] {
            assert_eq!(process_wait_outcome(&result, "7").as_deref(), expected);
        }
    }

    #[test]
    fn process_ids_never_display_model_text() {
        for (raw, expected) in [
            (" p7 ", "7"),
            ("p007", "7"),
            ("0", "(unknown)"),
            ("18446744073709551616", "(unknown)"),
            ("pp7", "(unknown)"),
            ("7 command", "(unknown)"),
            ("p", "(unknown)"),
        ] {
            let subject = process_subject(
                "process_session_read",
                &serde_json::json!({"session_id":raw}),
                None,
                &[],
                None,
            );
            assert_eq!(subject, expected);
        }
    }

    /// Text blocks of one answer never run together mid-sentence.
    #[test]
    fn answer_text_blocks_are_separated_unless_they_end_a_line() {
        use plakous::model::{
            Actor, ConversationItem, Digest, DigestDomain, EntryId, FinishDetails, FinishReason,
            ModelResponse, Provenance, TextBlock,
        };
        use std::str::FromStr as _;
        let block = |text: &str| ContentBlock::Text(TextBlock::new(text, None).unwrap());
        let item = ConversationItem::new(
            EntryId::from_str("00000000-0000-4000-8000-000000000101").unwrap(),
            Actor::Assistant,
            vec![
                block("I checked, and it is actually there."),
                block("`calc/ops.py` has four functions.\n"),
                block("Done."),
            ],
            Provenance::ModelOutput,
        )
        .unwrap();
        let response = ModelResponse::new(
            vec![item],
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"]).unwrap(),
        );
        assert_eq!(
            joined_assistant_text(&response),
            "I checked, and it is actually there.\n\n`calc/ops.py` has four functions.\nDone."
        );
    }

    #[test]
    fn subject_preview_redacts_before_clipping() {
        let command = format!("{}token=abcdefgh", "x ".repeat(74));
        let args = serde_json::json!({"cmd": command});
        let subject = tool_subject_from_args("exec_command", &args).unwrap();
        assert_eq!(subject, format!("{}token=[redac", "x ".repeat(74)));
        assert_eq!(args["cmd"], command);
    }

    /// The `ResponseCompleted` stash was empty at every
    /// `ToolStarted`, so the subject has to come from the started event's own
    /// observation, which carried none for `apply_patch` and `wait`
    /// (rows rendered bare with a JSON dump).
    #[test]
    fn started_observation_yields_edit_wait_and_shell_subjects_without_a_stash() {
        let patch = "*** Begin Patch\n*** Update File: notes.txt\n@@\n-BETA\n+beta\n*** End Patch";
        let edit = started_observation(&serde_json::json!({ "patch": patch }));
        assert_eq!(
            tool_subject_from_observation("apply_patch", Some("apply_patch"), &edit).as_deref(),
            Some("notes.txt +1/-1")
        );
        let wait = started_observation(&serde_json::json!({
            "cell_id": "1",
            "max_tokens": 4096,
            "terminate": false,
            "yield_time_ms": 30000,
        }));
        assert_eq!(
            tool_subject_from_observation("wait", Some("wait"), &wait).as_deref(),
            Some("cell 1 · up to 30s")
        );
        assert_eq!(
            tool_subject_from_observation("wait_py", Some("wait_py"), &wait).as_deref(),
            Some("cell 1 · up to 30s")
        );
        let terminate = started_observation(&serde_json::json!({
            "cell_id": "1",
            "terminate": true,
        }));
        assert_eq!(
            tool_subject_from_observation("wait", None, &terminate).as_deref(),
            Some("cell 1 · terminate")
        );
        // The exposed name shapes the arguments: `exec_command { cmd }` is
        // the canonical `process_session_start`.
        let shell = started_observation(&serde_json::json!({ "cmd": "ls -1", "shell": "/bin/sh" }));
        assert_eq!(
            tool_subject_from_observation("process_session_start", Some("exec_command"), &shell)
                .as_deref(),
            Some("ls -1")
        );
        // A preview truncated past the observation bound is not a subject.
        let truncated: plakous::agent::ToolCallObservation = serde_json::from_value(
            serde_json::json!({ "subject": null, "arguments_preview": "{\n  \"patch\": \"*** Begin" }),
        )
        .unwrap();
        assert!(
            tool_subject_from_observation("apply_patch", Some("apply_patch"), &truncated).is_none()
        );
    }

    /// Pi's `wait` is spelled like the Code Mode cell-wait marker but its
    /// identity is the process poll: its subject names the processes it
    /// awaits, never a cell.
    #[test]
    fn a_pi_wait_subject_names_processes_not_a_cell() {
        let wait = started_observation(&serde_json::json!({ "ids": ["p3"], "cell_id": "9" }));
        let subject =
            tool_subject_from_observation("process_session_poll", Some("wait"), &wait).unwrap();
        assert!(subject.contains('3'), "{subject}");
        assert!(!subject.contains("cell"), "{subject}");
        let bare = started_observation(&serde_json::json!({}));
        assert_eq!(
            process_observation_label("process_session_poll", Some("wait"), &serde_json::json!({})),
            Some("process_session_wait")
        );
        assert!(
            !tool_subject_from_observation("process_session_poll", Some("wait"), &bare)
                .unwrap_or_default()
                .contains("cell")
        );
    }

    #[test]
    fn web_and_mcp_rows_carry_host_subjects() {
        let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
        assert_eq!(
            subject(
                "image_gen",
                serde_json::json!({"prompt": "a fox", "path": "art/fox.png", "size": "1024x1024"})
            )
            .as_deref(),
            Some("art/fox.png (1024x1024)")
        );
        assert_eq!(
            subject(
                "image_gen",
                serde_json::json!({"prompt": "a fox", "path": "fox.png"})
            )
            .as_deref(),
            Some("fox.png")
        );
        for name in ["web_search", "WebSearch", "x_search"] {
            assert_eq!(
                subject(name, serde_json::json!({ "query": "rust 1.99" })).as_deref(),
                Some("\"rust 1.99\""),
                "{name}"
            );
        }
        for name in ["web_fetch", "WebFetch", "fetch_url", "fetch", "open_url"] {
            assert_eq!(
                subject(
                    name,
                    serde_json::json!({
                        "url": "https://user:pw@docs.example.org:8443/a/b?token=x#frag",
                        "prompt": "summarize"
                    })
                )
                .as_deref(),
                Some("docs.example.org:8443"),
                "{name}"
            );
        }
        assert_eq!(
            subject(
                "mcp__docs__search",
                serde_json::json!({ "query": "install", "limit": 5, "filters": { "a": 1 } })
            )
            .as_deref(),
            Some("filters=… limit=5 query=\"install\"")
        );
        let long = subject(
            "mcp__docs__search",
            serde_json::json!({ "a": "x".repeat(60), "b": "y".repeat(60), "c": "z".repeat(60) }),
        )
        .unwrap();
        assert!(
            long.len() <= MCP_ARGUMENTS_SUBJECT_BYTES + '…'.len_utf8(),
            "{long}"
        );
        assert!(long.ends_with('…'), "{long}");
        assert_eq!(subject("mcp__docs__ping", serde_json::json!({})), None);
    }

    /// The runtime's generic subject (a raw query, a whole URL) never wins
    /// over the host's own for web and MCP rows.
    #[test]
    fn web_and_mcp_host_subjects_win_over_the_core_subject() {
        let observation: plakous::agent::ToolCallObservation = serde_json::from_value(
            serde_json::json!({
                "subject": "https://docs.example.org/a?token=x",
                "arguments_preview": "{\"url\":\"https://docs.example.org/a?token=x\",\"prompt\":\"p\"}"
            }),
        )
        .unwrap();
        assert_eq!(
            started_tool_subject("web_fetch", Some("WebFetch"), Some(&observation), None)
                .as_deref(),
            Some("docs.example.org")
        );
        let observation: plakous::agent::ToolCallObservation = serde_json::from_value(
            serde_json::json!({ "subject": "install", "arguments_preview": "{\"query\":\"install\"}" }),
        )
        .unwrap();
        assert_eq!(
            started_tool_subject("mcp__docs__search", None, Some(&observation), None).as_deref(),
            Some("query=\"install\"")
        );
    }

    #[test]
    fn host_diffstat_wins_over_the_bare_core_subject() {
        let observation: plakous::agent::ToolCallObservation = serde_json::from_value(serde_json::json!({"subject":"file.rs", "arguments_preview": "{\"file_path\":\"file.rs\",\"old_string\":\"old\",\"new_string\":\"new\\nline\"}"})).unwrap();
        assert_eq!(
            started_tool_subject("Edit", Some("Edit"), Some(&observation), None).as_deref(),
            Some("file.rs +2/-1")
        );
    }

    #[test]
    fn adoption_names_the_candidate() {
        assert_eq!(
            tool_subject_from_args(
                "adopt_candidate",
                &serde_json::json!({"candidate_id": "candidate-7"})
            )
            .as_deref(),
            Some("candidate-7")
        );
    }

    #[test]
    fn projected_process_tools_have_exact_product_subjects() {
        assert_eq!(
            tool_subject_from_args("exec_command", &serde_json::json!({"cmd":"cargo test"})),
            // §5.2 renders `Run {command}`; the verb is the row's, not the subject's.
            Some("cargo test".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "run_terminal_command",
                &serde_json::json!({"command":"cargo check","description":"check"}),
            ),
            Some("cargo check".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "run_terminal_cmd",
                &serde_json::json!({"command":"cargo check","description":"check"}),
            ),
            Some("cargo check".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "write_stdin",
                &serde_json::json!({"session_id":7,"chars":"yes\n"}),
            ),
            Some("7 · 4 bytes".to_owned())
        );
    }

    #[test]
    fn oauth_costume_tool_subjects_use_wire_field_names() {
        assert_eq!(
            tool_subject_from_args("Bash", &serde_json::json!({"command":"printf hi"})),
            Some("printf hi".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "read_file",
                &serde_json::json!({"target_file":"src/lib.rs"})
            ),
            Some("src/lib.rs".to_owned())
        );
        assert_eq!(
            tool_subject_from_args("Read", &serde_json::json!({"file_path":"a.txt","limit":10})),
            Some("a.txt".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "write",
                &serde_json::json!({"file_path":"a.txt","content":"one\ntwo"})
            ),
            Some("a.txt +2/-0".to_owned())
        );
        assert_eq!(
            tool_subject_from_args("list_dir", &serde_json::json!({"target_directory":"src"})),
            Some("src".to_owned())
        );
        assert_eq!(
            tool_subject_from_args("merge_plan", &serde_json::json!({"todos":[]})),
            Some("0 items".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "todo_write",
                &serde_json::json!({
                    "todos": [
                        {"id": "a", "content": "wrap", "status": "completed"},
                        {"id": "b", "content": "rail", "status": "in_progress"}
                    ]
                })
            ),
            Some("2 items · 1 in progress".to_owned())
        );
        assert_eq!(
            tool_subject_from_args(
                "update_plan",
                &serde_json::json!({
                    "plan": [{"step": "only", "status": "pending"}]
                })
            ),
            Some("1 item".to_owned())
        );
        assert_eq!(
            tool_subject_from_args("Grep", &serde_json::json!({"pattern":"TODO","path":"src"})),
            Some("\"TODO\" in src".to_owned())
        );
        assert_eq!(
            tool_subject_from_args("Glob", &serde_json::json!({"pattern":"**/*.rs"})),
            Some("\"**/*.rs\"".to_owned())
        );
    }

    /// `wait_agent` rendered as
    /// `▸ ✓ Agent · 6.3s` and `list_agents` as a raw humanized tool name,
    /// because neither had a subject arm and both were forced onto the
    /// `Agent` verb.
    #[test]
    fn the_read_and_mail_topology_tools_carry_section_5_2_subjects() {
        let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
        assert_eq!(
            subject(
                "wait_agent",
                serde_json::json!({ "agent_ids": ["c1", "c2"], "timeout_ms": 30_000 })
            )
            .as_deref(),
            Some("c1,c2 · up to 30s")
        );
        assert_eq!(
            subject("wait_agent", serde_json::json!({ "agent_ids": ["c1"] })).as_deref(),
            Some("c1")
        );
        assert_eq!(
            subject("wait_agent", serde_json::json!({ "agent_ids": [] })),
            None
        );
        assert_eq!(
            subject("list_agents", serde_json::json!({})).as_deref(),
            Some("agents")
        );
        assert_eq!(
            subject(
                "send_agent_message",
                serde_json::json!({ "agent_id": "c1", "message": "keep going" })
            )
            .as_deref(),
            Some("c1")
        );
    }

    #[test]
    fn tool_subjects_match_the_normative_header_formats() {
        let subject = |name: &str, args: serde_json::Value| tool_subject_from_args(name, &args);
        assert_eq!(
            subject("exec_command", serde_json::json!({ "cmd": "cargo test" })).as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            subject(
                "read_file",
                serde_json::json!({ "path": "src/lib.rs", "offset": 1, "limit": 120 })
            )
            .as_deref(),
            Some("src/lib.rs (1-120)")
        );
        assert_eq!(
            subject("read_file", serde_json::json!({ "path": "src/lib.rs" })).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            subject(
                "search_repository",
                serde_json::json!({ "query": "ToolLine", "path": "crates" })
            )
            .as_deref(),
            Some("\"ToolLine\" in crates")
        );
        assert_eq!(
            subject("list_dir", serde_json::json!({ "path": "crates/plak" })).as_deref(),
            Some("crates/plak")
        );
        assert_eq!(
            subject("web_fetch", serde_json::json!({ "url": "https://a.test" })).as_deref(),
            Some("a.test")
        );
        assert_eq!(
            subject(
                "apply_patch",
                serde_json::json!({
                    "patch": "*** Update File: src/ui.rs\n+one\n+two\n-old\n"
                })
            )
            .as_deref(),
            Some("src/ui.rs +2/-1")
        );
    }

    #[test]
    fn completed_headers_take_their_counts_from_the_host_not_the_renderer() {
        // §5.2 admits no bare line count for Read; a complete but empty result
        // is the one thing the host can state about it.
        assert_eq!(tool_result_summary("read_file", Some("a\nb\nc")), None);
        assert_eq!(
            tool_result_summary("read_file", Some("")).as_deref(),
            Some("(empty)")
        );
        assert_eq!(
            tool_result_summary("search_repository", Some("a.rs:1:hit\nb.rs:2:hit")).as_deref(),
            Some("(2 matches in 2 files)")
        );
        assert_eq!(
            tool_result_summary("search_repository", Some("")).as_deref(),
            Some("(no matches)")
        );
        assert_eq!(
            tool_result_summary("list_dir", Some("a\nb")).as_deref(),
            Some("(2 entries)")
        );
        // A verb whose count the host cannot state truthfully adds nothing.
        assert_eq!(tool_result_summary("exec_command", Some("out")), None);
    }

    /// The observation the host counts is a 2 KiB bounded preview, so a
    /// count over a truncated one is approximate and must say so (§5.1 "no
    /// invented count"). A 5,000-line read never renders `(64 lines)`.
    #[test]
    fn counts_over_a_bounded_preview_are_marked_approximate_or_omitted() {
        let bounded = |body: &str| format!("{body}\n{TOOL_PREVIEW_TRUNCATION_MARKER}");
        let hits = (0..64)
            .map(|line| format!("a.rs:{line}:hit"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(!preview_is_complete(&bounded("a\nb")));
        assert!(preview_is_complete("a\nb"));
        assert!(!preview_is_complete(&"x".repeat(2 * 1024)));

        // Read states no count at all rather than the preview's own size.
        assert_eq!(
            tool_result_summary("read_file", Some(&bounded("a\nb"))),
            None
        );
        assert_eq!(
            tool_result_summary("search_repository", Some(&bounded(&hits))).as_deref(),
            Some("(~65 matches in ~1 files)")
        );
        assert_eq!(
            tool_result_summary("list_dir", Some(&bounded("a\nb"))).as_deref(),
            Some("(~3 entries)")
        );
        // A single observed hit in a bounded window is not "1 match".
        assert_eq!(
            tool_result_summary("search_repository", Some(&bounded("a.rs:1:hit"))).as_deref(),
            Some("(~2 matches in ~1 files)")
        );
    }

    /// `limit` counts lines, so `(1-120)` is offset 1 with limit 120.
    #[test]
    fn a_bounded_read_names_the_last_line_it_asked_for() {
        assert_eq!(
            tool_subject_from_args(
                "read_file",
                &serde_json::json!({ "path": "src/lib.rs", "offset": 1, "limit": 120 })
            )
            .as_deref(),
            Some("src/lib.rs (1-120)")
        );
        assert_eq!(
            tool_subject_from_args(
                "read_file",
                &serde_json::json!({ "path": "src/lib.rs", "offset": 40, "limit": 0 })
            )
            .as_deref(),
            Some("src/lib.rs (40-)")
        );
    }

    #[test]
    fn tool_subject_stash_correlates_parallel_same_name_lifecycles_exactly() {
        let mut stash = ToolSubjectStash::new();
        stash.push(
            "exec_command".into(),
            Some("call-a".into()),
            pending_tool("same", "arguments-a"),
        );
        stash.push(
            "exec_command".into(),
            Some("call-b".into()),
            pending_tool("same", "arguments-b"),
        );

        assert!(
            stash.take("exec_command", None).is_none(),
            "missing identity must not guess between parallel same-name calls"
        );
        assert_eq!(
            stash
                .take("exec_command", Some("call-b"))
                .map(|info| info.detail),
            Some("arguments-b".into()),
            "reverse start/finish must use the exact call identity"
        );
        assert_eq!(
            stash.take("exec_command", None).map(|info| info.detail),
            Some("arguments-a".into()),
            "a missing identity is safe only after one exact candidate remains"
        );
        assert!(stash.take("exec_command", Some("call-a")).is_none());

        stash.push(
            "model_alias".into(),
            Some("rejected".into()),
            pending_tool("rejected", "rejected-arguments"),
        );
        assert!(
            stash.take("canonical_name", Some("rejected")).is_none(),
            "an alias mismatch fails closed"
        );
        assert!(stash.take("model_alias", None).is_none());

        stash.push(
            "exec_command".into(),
            None,
            pending_tool("unidentified", "must-not-survive"),
        );
        assert!(stash.take("exec_command", None).is_none());

        drop(stash);
        let mut restarted = ToolSubjectStash::new();
        assert!(restarted.take("exec_command", None).is_none());
    }
}
