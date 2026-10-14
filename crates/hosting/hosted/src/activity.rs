//! How the host classifies and renders one tool call's activity.
//!
//! The activity category is *host-populated*: the renderer never
//! defaults it, so the host must decide, from the canonical tool name alone,
//! whether a row is a plain tool, a process, a topology action, or the marker
//! of a local-composition cell. The expandable body is the same kind of
//! decision — spec 02 §5.4 says a tool row's detail is readable text, never an
//! `Arguments:`/`Result:` JSON dump.
//!
//! This sat in the product's `presentation.rs` until the 3.4b split, which is
//! why the host loop reached into the product for it thirteen times. Nothing
//! here is product policy: it names canonical tool identifiers and renders
//! their payloads, and both are the hosted session's own vocabulary.

use plakous_host_protocol::views::presentation::{ActivityItem, ActivityKind};

/// Canonical outer markers of a Plakous local-composition cell.
///
/// `wait` and `wait_js` are deliberately absent: they are
/// model-visible yields on an existing cell, attributed to it by `cell_id`, not
/// a second Compose.
const LOCAL_COMPOSITION_MARKERS: [&str; 3] = ["exec", "exec_js", "exec_py"];

/// Canonical topology tools that act on the agent tree.
const TOPOLOGY_TOOLS: [&str; 10] = [
    "spawn_agent",
    "send_agent_message",
    "wait_agent",
    "interrupt_agent",
    "close_agent",
    "recover_agent",
    "submit_result",
    "adopt_candidate",
    "discard_workspace",
    "stop_all",
];

/// Canonical and model-facing aliases that start a shell/process session.
///
/// This is a small closed alias set, deliberately not a fuzzy match against an
/// arbitrary tool label.
const PROCESS_START_ALIASES: [&str; 6] = [
    "process_session_start",
    "process-session-start",
    "plakous.process-session-start.v2",
    "plakous.process-session-start.v3",
    "exec_command",
    "run_process",
];

/// Display label every process-start alias normalizes onto.
pub const PROCESS_ACTIVITY_LABEL: &str = "Shell";

/// Returns `true` when the canonical name owns a local-composition cell.
#[must_use]
pub fn is_local_composition_marker(canonical_name: &str) -> bool {
    LOCAL_COMPOSITION_MARKERS.contains(&canonical_name)
}

/// Canonical `wait*` continuations of a local-composition cell.
const LOCAL_COMPOSITION_CONTINUATIONS: [&str; 3] = ["wait", "wait_js", "wait_py"];

/// Returns `true` when the canonical name resumes an existing cell.
#[must_use]
pub fn is_local_composition_continuation(canonical_name: &str) -> bool {
    LOCAL_COMPOSITION_CONTINUATIONS.contains(&canonical_name)
}

/// Returns `true` when the canonical name is a tree-mutating topology tool.
#[must_use]
pub fn is_topology_tool(canonical_name: &str) -> bool {
    TOPOLOGY_TOOLS.contains(&canonical_name)
}

/// Returns `true` when the label names a process-start alias.
#[must_use]
pub fn is_process_start_alias(label: &str) -> bool {
    let normalized = label.trim().to_ascii_lowercase();
    PROCESS_START_ALIASES.contains(&normalized.as_str())
}

/// Classifies one tool activity row from facts the host already holds.
///
/// The host is the sole authority for [`ActivityKind`]: the
/// renderer never re-derives a kind from a display string.
/// [`ActivityKind::Model`] is not reachable from a tool name — it is reserved
/// for rows produced by a model-side observation rather than a tool call.
#[must_use]
pub fn tool_activity_kind(canonical_name: &str) -> ActivityKind {
    if canonical_name == "compact" {
        ActivityKind::Compact
    } else if is_local_composition_marker(canonical_name) {
        ActivityKind::Compose
    } else if canonical_name == "child_result" || is_topology_tool(canonical_name) {
        ActivityKind::Agent
    } else if is_process_start_alias(canonical_name) {
        ActivityKind::Process
    } else {
        ActivityKind::Tool
    }
}

/// Normalizes a process-start alias row onto one `Shell` row.
///
/// Host-computed; stable identity and detail are retained.
#[must_use]
pub fn normalize_process_start(mut item: ActivityItem) -> ActivityItem {
    if is_process_start_alias(&item.label) {
        item.kind = ActivityKind::Process;
        PROCESS_ACTIVITY_LABEL.clone_into(&mut item.label);
    }
    item
}

/// Object keys that name the command a shell-shaped tool result ran.
const SHELL_COMMAND_KEYS: [&str; 2] = ["command", "cmd"];
/// Object keys that carry a shell-shaped tool result's standard output.
const SHELL_STDOUT_KEYS: [&str; 2] = ["stdout", "output"];
/// Object keys that carry a shell-shaped tool result's standard error.
const SHELL_STDERR_KEYS: [&str; 1] = ["stderr"];
/// Object keys that carry a shell-shaped tool result's exit status.
const SHELL_EXIT_KEYS: [&str; 4] = ["exit_code", "exit_status", "exitCode", "status"];
/// Object keys that carry a file-read tool result's body.
const FILE_CONTENT_KEYS: [&str; 4] = ["content", "contents", "text", "file_text"];

/// Project a tool call's bounded argument and result previews into readable
/// text for the transcript.
///
/// Spec 02 §5.4: tool output renders as text, never as `Arguments:\n{json}`
/// followed by `Result:\n{json}`. Shell shapes become `$ cmd`, their output
/// lines, and an `exit N` line when the status is nonzero; file reads become
/// their content lines; anything else is pretty-printed with real newlines.
/// Input that is not JSON is already human text and passes through unchanged.
#[must_use]
pub fn tool_detail_text(arguments: Option<&str>, result: Option<&str>) -> Option<String> {
    let arguments = arguments.map(preview_text).filter(|text| !text.is_empty());
    let result = result.map(preview_text).filter(|text| !text.is_empty());
    match (arguments, result) {
        (None, None) => None,
        (Some(text), None) | (None, Some(text)) => Some(text),
        (Some(arguments), Some(result)) => {
            // A plan write's arguments and the snapshot result are the same
            // list; keep the post-merge plan, or the write payload if the
            // result is not a plan.
            match (
                looks_like_plan_list(&arguments),
                looks_like_plan_list(&result),
            ) {
                (_, true) => Some(result),
                (true, false) => Some(arguments),
                (false, false) => {
                    // A one-line argument projection (`$ cmd`) reads as the
                    // header of its own output; a multi-line one needs air
                    // before the result.
                    let separator = if arguments.contains('\n') {
                        "\n\n"
                    } else {
                        "\n"
                    };
                    Some(format!("{arguments}{separator}{result}"))
                }
            }
        }
    }
}

/// How a command a tool ran ended, when its result says it did not succeed:
/// `exit 2`, `timed out`, `output limit`, `terminated`.
///
/// The tool call itself succeeded, so without this the row is a green check
/// over a failing command. Read from the bounded result preview, which
/// stays JSON for a structured result however large its output: a nonzero
/// integer exit key, a process `status` beside an `output`, or a text result
/// whose last line is `[exit N]`, `[timed out …]` or `[killed: output limit]`.
#[must_use]
pub fn process_failure(preview: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(preview.trim()).ok();
    let Some(fields) = value.as_ref().and_then(serde_json::Value::as_object) else {
        return trailing_process_marker(preview);
    };
    if let Some(code) = SHELL_EXIT_KEYS
        .iter()
        .find_map(|key| fields.get(*key).and_then(serde_json::Value::as_i64))
        .filter(|code| *code != 0)
    {
        return Some(format!("exit {code}"));
    }
    if fields.contains_key("output")
        && let Some(status) = fields.get("status").and_then(serde_json::Value::as_str)
    {
        let failure = match status {
            "timed_out" => Some("timed out"),
            "output_limit" => Some("output limit"),
            "terminated" => Some("terminated"),
            "failed" => Some("failed"),
            _ => None,
        };
        if let Some(failure) = failure {
            return Some(failure.to_owned());
        }
    }
    fields
        .get("result")
        .and_then(serde_json::Value::as_str)
        .and_then(trailing_process_marker)
}

fn trailing_process_marker(text: &str) -> Option<String> {
    let last = text.trim_end().rsplit('\n').next()?.trim();
    let marker = last.strip_prefix('[')?.strip_suffix(']')?;
    if let Some(code) = marker
        .strip_prefix("exit ")
        .and_then(|code| code.trim().parse::<i64>().ok())
    {
        return (code != 0).then(|| format!("exit {code}"));
    }
    if marker.starts_with("timed out") {
        return Some("timed out".to_owned());
    }
    (marker == "killed: output limit").then(|| "output limit".to_owned())
}

/// Render one bounded preview as text, recognizing the shapes the product ships.
fn preview_text(preview: &str) -> String {
    let trimmed = preview.trim_end();
    if trimmed.is_empty() {
        return String::new();
    }
    if let Some(bullets) = plan_snapshot_bullets(trimmed) {
        return bullets;
    }
    serde_json::from_str::<serde_json::Value>(trimmed)
        .map_or_else(|_| trimmed.to_owned(), |value| value_text(&value))
}

fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.trim_end().to_owned(),
        serde_json::Value::Object(fields) => object_text(fields),
        other => expand_escaped_newlines(
            &serde_json::to_string_pretty(other).unwrap_or_else(|_| other.to_string()),
        ),
    }
}

fn object_text(fields: &serde_json::Map<String, serde_json::Value>) -> String {
    let command = string_field(fields, &SHELL_COMMAND_KEYS);
    let stdout = string_field(fields, &SHELL_STDOUT_KEYS);
    let stderr = string_field(fields, &SHELL_STDERR_KEYS);
    let exit = SHELL_EXIT_KEYS
        .iter()
        .find_map(|key| fields.get(*key).and_then(serde_json::Value::as_i64));
    if command.is_some() || stdout.is_some() || stderr.is_some() {
        let mut lines: Vec<String> = Vec::new();
        if let Some(command) = command {
            lines.push(format!("$ {}", command.trim()));
        }
        lines.extend(stdout.map(|text| text.trim_end().to_owned()));
        lines.extend(stderr.map(|text| text.trim_end().to_owned()));
        if let Some(code) = exit.filter(|code| *code != 0) {
            lines.push(format!("exit {code}"));
        }
        lines.retain(|line| !line.is_empty());
        return lines.join("\n");
    }
    if let Some(content) = string_field(fields, &FILE_CONTENT_KEYS) {
        return content.trim_end().to_owned();
    }
    // `apply_patch { patch }` reads as the patch itself (§5.2 Edit rows).
    if let Some(patch) = string_field(fields, &PATCH_KEYS) {
        return patch.trim_end().to_owned();
    }
    // A `wait*` continuation's arguments are one readable line, not a JSON
    // dump of `{ cell_id, terminate, yield_time_ms }` (§5.7).
    if fields.contains_key("yield_time_ms") || fields.contains_key("terminate") {
        return wait_arguments_text(fields);
    }
    if let Some(text) = plan_items_text(fields) {
        return text;
    }
    // A single string field (`{ "result": "changed_files=1" }`) is that text.
    if fields.len() == 1
        && let Some(only) = fields.values().next().and_then(serde_json::Value::as_str)
    {
        return only.trim_end().to_owned();
    }
    let value = serde_json::Value::Object(fields.clone());
    expand_escaped_newlines(
        &serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    )
}

/// Object keys that carry an `apply_patch` body.
const PATCH_KEYS: [&str; 2] = ["patch", "input"];

fn plan_items_text(fields: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    let items = fields
        .get("todos")
        .or_else(|| fields.get("plan"))?
        .as_array()?;
    if items.is_empty() {
        return Some(String::new());
    }
    Some(
        items
            .iter()
            .map(plan_item_line)
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

fn plan_item_line(item: &serde_json::Value) -> String {
    let status = item
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("pending");
    let text = item
        .get("content")
        .or_else(|| item.get("step"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .trim();
    format!("{} {text}", plan_status_glyph(status))
}

fn plan_status_glyph(status: &str) -> &'static str {
    match status {
        "completed" => "✓",
        "in_progress" => "▶",
        "cancelled" | "canceled" => "×",
        _ => "○",
    }
}

fn plan_snapshot_bullets(text: &str) -> Option<String> {
    let mut rows = Vec::new();
    let mut saw_item = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with("revision=")
            || trimmed.starts_with("explanation:")
        {
            continue;
        }
        let rest = trimmed.strip_prefix("- [")?;
        let (status, content) = rest.split_once(']')?;
        let status = status.trim();
        if !matches!(
            status,
            "pending" | "in_progress" | "completed" | "cancelled" | "canceled"
        ) {
            return None;
        }
        saw_item = true;
        rows.push(format!("{} {}", plan_status_glyph(status), content.trim()));
    }
    saw_item.then(|| rows.join("\n"))
}

fn looks_like_plan_list(text: &str) -> bool {
    let mut any = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !(trimmed.starts_with("✓ ")
            || trimmed.starts_with("▶ ")
            || trimmed.starts_with("○ ")
            || trimmed.starts_with("× "))
        {
            return false;
        }
        any = true;
    }
    any
}

fn wait_arguments_text(fields: &serde_json::Map<String, serde_json::Value>) -> String {
    let mut parts = Vec::new();
    if let Some(cell) = fields.get("cell_id") {
        let cell = cell
            .as_str()
            .map_or_else(|| cell.to_string(), str::to_owned);
        parts.push(format!("cell {cell}"));
    }
    if fields
        .get("terminate")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        parts.push("terminate".to_owned());
    } else if let Some(ms) = fields
        .get("yield_time_ms")
        .and_then(serde_json::Value::as_u64)
    {
        parts.push(format!("yield up to {ms} ms"));
    }
    if parts.is_empty() {
        return "wait".to_owned();
    }
    format!("wait {}", parts.join(" · "))
}

fn string_field<'a>(
    fields: &'a serde_json::Map<String, serde_json::Value>,
    keys: &[&str],
) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| fields.get(*key).and_then(serde_json::Value::as_str))
}

/// Turn JSON string escapes back into the characters a reader expects.
///
/// `serde_json` pretty-printing keeps `\n` inside string values, which is the
/// literal-escape defect the spec forbids. Every line a string expands to is
/// indented like the line the string started on, so nested values stay
/// readable and the string's own indentation is kept exactly. (Measuring the
/// previous expanded line instead added the content's indentation again on
/// every line.)
fn expand_escaped_newlines(pretty: &str) -> String {
    let mut out = String::with_capacity(pretty.len());
    let mut in_string = false;
    let mut string_indent = 0;
    let mut chars = pretty.chars();
    while let Some(ch) = chars.next() {
        if !in_string {
            in_string = ch == '"';
            if in_string {
                string_indent = current_indent(&out);
            }
            out.push(ch);
            continue;
        }
        match ch {
            '"' => {
                in_string = false;
                out.push(ch);
            }
            '\\' => match chars.next() {
                Some('n') => {
                    out.push('\n');
                    out.extend(std::iter::repeat_n(' ', string_indent));
                }
                Some('t') => out.push_str("    "),
                Some('r') => {}
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            other => out.push(other),
        }
    }
    out
}

fn current_indent(out: &str) -> usize {
    let line = out.rsplit('\n').next().unwrap_or("");
    line.len()
        .saturating_sub(line.trim_start_matches(' ').len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_results_render_as_command_output_and_status() {
        let detail = tool_detail_text(
            Some(r#"{"command":"cargo test"}"#),
            Some(r#"{"stdout":"one\ntwo\n","stderr":"warning: slow","exit_code":2}"#),
        )
        .expect("shell projection");
        assert_eq!(detail, "$ cargo test\none\ntwo\nwarning: slow\nexit 2");
    }

    #[test]
    fn a_process_that_ran_and_failed_is_named_by_how_it_ended() {
        assert_eq!(
            process_failure("{\n  \"exit_code\": 101,\n  \"output\": \"error\"\n}").as_deref(),
            Some("exit 101")
        );
        assert_eq!(process_failure(r#"{"exit_code":0,"output":"ok"}"#), None);
        assert_eq!(
            process_failure(r#"{"status":"timed_out","output":"partial"}"#).as_deref(),
            Some("timed out")
        );
        assert_eq!(
            process_failure(r#"{"result":"compiling\n[exit 2]"}"#).as_deref(),
            Some("exit 2")
        );
        assert_eq!(
            process_failure("partial output\n[timed out after 300s]").as_deref(),
            Some("timed out")
        );
        assert_eq!(
            process_failure("[killed: output limit]").as_deref(),
            Some("output limit")
        );
        // A plan snapshot's `status` is not a process status.
        assert_eq!(process_failure(r#"{"status":"failed","items":[]}"#), None);
        assert_eq!(process_failure("plain text"), None);
    }

    #[test]
    fn successful_shell_results_omit_the_exit_line() {
        let detail = tool_detail_text(
            None,
            Some(r#"{"command":"pwd","stdout":"/tmp","exit_code":0}"#),
        )
        .expect("shell projection");
        assert_eq!(detail, "$ pwd\n/tmp");
    }

    #[test]
    fn file_reads_render_as_content_lines() {
        let detail = tool_detail_text(
            Some(r#"{"path":"src/main.rs"}"#),
            Some(r#"{"path":"src/main.rs","content":"fn main() {}\n"}"#),
        )
        .expect("file projection");
        assert!(detail.ends_with("fn main() {}"), "unexpected: {detail}");
        assert!(!detail.contains("\\n"), "literal escape survived: {detail}");
    }

    #[test]
    fn generic_json_is_pretty_printed_with_real_newlines() {
        let detail = tool_detail_text(None, Some(r#"{"summary":"first\nsecond","count":2}"#))
            .expect("generic projection");
        assert!(!detail.contains("\\n"), "literal escape survived: {detail}");
        assert!(detail.contains("first\n"), "unexpected: {detail}");
        assert!(detail.contains("second"), "unexpected: {detail}");
        assert!(detail.contains("\"count\": 2"), "unexpected: {detail}");
    }

    /// A multi-line string keeps its own indentation exactly: each line gets
    /// the indentation of the JSON line it started on and nothing more.
    #[test]
    fn multi_line_arguments_are_not_cumulatively_reindented() {
        let detail = tool_detail_text(
            Some(
                r#"{"file_path":"calc.py","new_string":"def div(a, b):\n    # true division\n    return a / b\n"}"#,
            ),
            None,
        )
        .expect("generic projection");
        assert!(
            detail.contains(
                "\"new_string\": \"def div(a, b):\n      # true division\n      return a / b\n  \""
            ),
            "unexpected: {detail}"
        );
    }

    #[test]
    fn plain_text_previews_and_empty_previews_pass_through() {
        assert_eq!(
            tool_detail_text(None, Some("already human text\n")),
            Some("already human text".to_owned())
        );
        assert_eq!(tool_detail_text(Some("   "), Some("")), None);
    }

    #[test]
    fn json_string_results_lose_their_quotes() {
        assert_eq!(
            tool_detail_text(None, Some(r#""line one\nline two""#)),
            Some("line one\nline two".to_owned())
        );
    }

    /// The `apply_patch` and `wait` previews rendered as `{"patch": …}` and
    /// `{"terminate": false, "yield_time_ms": 30000}` dumps (§5.7).
    #[test]
    fn edit_and_wait_previews_render_as_text_not_json() {
        let patch = "*** Begin Patch\n*** Update File: notes.txt\n@@\n-BETA\n+beta\n*** End Patch";
        let arguments = serde_json::json!({ "patch": patch }).to_string();
        let result = serde_json::json!({ "result": "changed_files=1" }).to_string();
        assert_eq!(
            tool_detail_text(Some(&arguments), Some(&result)),
            Some(format!("{patch}\n\nchanged_files=1"))
        );
        let wait = serde_json::json!({
            "cell_id": "1",
            "max_tokens": "[redacted]",
            "terminate": false,
            "yield_time_ms": 30000,
        })
        .to_string();
        assert_eq!(
            tool_detail_text(Some(&wait), None),
            Some("wait cell 1 · yield up to 30000 ms".to_owned())
        );
        let terminate = serde_json::json!({ "cell_id": 1, "terminate": true }).to_string();
        assert_eq!(
            tool_detail_text(Some(&terminate), None),
            Some("wait cell 1 · terminate".to_owned())
        );
        // A two-field object is not a "single string" and still pretty-prints.
        let two = serde_json::json!({ "a": "x", "b": "y" }).to_string();
        assert!(
            tool_detail_text(Some(&two), None)
                .unwrap()
                .contains("\"a\": \"x\"")
        );
    }

    #[test]
    fn plan_payloads_render_as_bullets_not_json() {
        let arguments = serde_json::json!({
            "merge": true,
            "todos": [
                {"id": "a", "content": "wrap thoughts", "status": "completed"},
                {"id": "b", "content": "rail groups", "status": "in_progress"},
                {"id": "c", "content": "fence json", "status": "pending"}
            ]
        })
        .to_string();
        let detail = tool_detail_text(Some(&arguments), None).expect("plan projection");
        assert_eq!(detail, "✓ wrap thoughts\n▶ rail groups\n○ fence json");

        let snapshot = "revision=3\n- [completed] wrap thoughts\n- [in_progress] rail groups\n- [pending] fence json\n";
        let combined = tool_detail_text(Some(&arguments), Some(snapshot)).expect("merged plan");
        assert_eq!(combined, detail, "the snapshot wins over the write payload");
        assert!(!combined.contains("\"todos\""), "{combined}");
        assert!(!combined.contains("revision="), "{combined}");
    }
}
