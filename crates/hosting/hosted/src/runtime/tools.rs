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
            // A process's command, from its row or its start, is the
            // runtime's displayed command.
            let command = locator
                .and_then(|locator| {
                    let key = process_key(locator, id);
                    processes
                        .iter()
                        .find(|process| process.id == key && !process.command.is_empty())
                        .map(|process| process.command.as_str())
                        .or_else(|| start_commands.get(&key).map(String::as_str))
                })
                .map(crate::jobs::display::redacted_job_command);
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
#[allow(
    clippy::too_many_lines,
    reason = "Tool subjects follow the canonical tool vocabulary and its distinct path, process, search, and topology argument shapes."
)]
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
mod tests;
