//! Tool verbs and titles, run-state and duration labels, and tool rows.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the blocks module that shares its vocabulary"
)]

use super::*;

/// Humanize a canonical tool name (`run_process` → `run process`).
#[must_use]
pub fn humanize_tool(name: &str) -> String {
    name.trim_start_matches("mcp__")
        .replace("__", " · ")
        .replace('_', " ")
}

/// The closed set of normative §5.2 verbs.
///
/// A verb outside this set renders as the humanized tool name with the generic
/// `Label: content` split.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolVerb {
    /// `Run {command}`.
    Run,
    WaitProcess,
    ReadProcess,
    InputProcess,
    InterruptProcess,
    StopProcess,
    /// `Read {path} (…)`.
    Read,
    /// `Search "{pattern}" …`.
    Search,
    /// `Edit {path} +{ins}/-{del}`.
    Edit,
    /// `Wrote {path}`: a whole-file write.
    Write,
    /// `Fetch {host}`: native web fetch and `fetch_url`.
    Fetch,
    /// `Searched the web "{query}"`: the hosted web search tool.
    WebSearch,
    /// `Searched X "{query}"`: the hosted X search tool.
    XSearch,
    /// `Generated image {path} ({size})`: `image_gen`.
    GenerateImage,
    /// `mcp:{server} {tool} {arguments}`: a tool exposed as
    /// `mcp__<server>__<tool>`.
    Mcp,
    /// `List {path} ({n} entries)`.
    List,
    /// `Plan` — `update_plan` / `todo_write` / `merge_plan`, collapsed by default.
    Plan,
    /// One local-composition cell (§5.8).
    Compose,
    /// One topology/subagent row (§5.9): `Agent {class} · {brief}`.
    Agent,
    /// `Spawn Agent {class} · {brief}` — `spawn_agent` (§5.9).
    SpawnAgent,
    /// `Wait Agent {ids}` — `wait_agent` (§5.9).
    WaitAgent,
    /// `list agents` — `list_agents`.
    ListAgents,
    /// `message {agent}` — `send_agent_message`.
    MessageAgent,
    /// `Adopt {candidate}` — `adopt_candidate`. Landing a
    /// candidate is the one topology call that changes the human's checkout, so
    /// it says so instead of collapsing into the generic `Agent` row.
    Adopt,
    CloseAgent,
    InterruptAgent,
    RecoverAgent,
    DiscardWorkspace,
    StopAll,
    SubmitResult,
    /// One compaction workflow (§5.4): header plus before → after sizes.
    Compact,
    /// Anything else; the label is the humanized tool name.
    Generic,
}

impl ToolVerb {
    /// Fixed display label for the normative verbs.
    #[must_use]
    pub const fn label(self) -> Option<&'static str> {
        match self {
            Self::Run => Some("Run"),
            Self::WaitProcess => Some("Waiting on process"),
            Self::ReadProcess => Some("Read output of process"),
            Self::InputProcess => Some("Sent input to process"),
            Self::InterruptProcess => Some("Sent interrupt to process"),
            Self::StopProcess => Some("Stopped process"),
            Self::Read => Some("Read"),
            Self::Search => Some("Search"),
            Self::Edit => Some("Edit"),
            Self::Write => Some("Write"),
            Self::Fetch => Some("Fetch"),
            Self::WebSearch => Some("Web search"),
            Self::XSearch => Some("X search"),
            Self::GenerateImage => Some("Generate image"),
            Self::List => Some("List"),
            Self::Plan => Some("Plan"),
            Self::Compose => Some("Compose"),
            Self::Agent => Some("Agent"),
            Self::SpawnAgent => Some("Spawn Agent"),
            Self::WaitAgent => Some("Wait Agent"),
            Self::ListAgents => Some("list"),
            Self::MessageAgent => Some("message"),
            Self::Adopt => Some("Adopt"),
            Self::CloseAgent => Some("Close agent"),
            Self::InterruptAgent => Some("Interrupt agent"),
            Self::RecoverAgent => Some("Recover agent"),
            Self::DiscardWorkspace => Some("Discard workspace"),
            Self::StopAll => Some("Stop all agents"),
            Self::SubmitResult => Some("Submit result"),
            Self::Compact => Some("Compact"),
            Self::Mcp | Self::Generic => None,
        }
    }

    /// Plural §5.5 group header for a run of `count` calls.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub const fn group_noun(self) -> &'static str {
        match self {
            Self::Read => "Read {n} files",
            Self::Search => "Searched {n} patterns",
            Self::List => "Listed {n} dirs",
            Self::Fetch => "Fetched {n} websites",
            _ => "",
        }
    }
}

/// Host-classified verb for one tool row (§5.2).
///
/// `kind` is host authority and wins for Compose and Agent rows;
/// the canonical tool name selects the remaining normative verbs.
#[must_use]
pub fn tool_verb(name: &str, kind: ActivityKind) -> ToolVerb {
    // The three read/mail topology tools carry their own verb. Collapsing
    // every `ActivityKind::Agent` row to `Agent` printed `▸ ✓ Agent · 6.3s`
    // for a `wait_agent`, which says nothing about what is being waited on.
    match name {
        "spawn_agent" => return ToolVerb::SpawnAgent,
        "wait_agent" => return ToolVerb::WaitAgent,
        "list_agents" => return ToolVerb::ListAgents,
        "send_agent_message" => return ToolVerb::MessageAgent,
        // The tool is named `adopt_candidate`; without an arm here the call
        // falls through to `ActivityKind::Agent` and the transcript says
        // "Agent" for the one call that rewrites the human's checkout.
        "adopt_candidate" => return ToolVerb::Adopt,
        "close_agent" => return ToolVerb::CloseAgent,
        "interrupt_agent" => return ToolVerb::InterruptAgent,
        "recover_agent" => return ToolVerb::RecoverAgent,
        "discard_workspace" => return ToolVerb::DiscardWorkspace,
        "stop_all" => return ToolVerb::StopAll,
        "submit_result" => return ToolVerb::SubmitResult,
        _ => {}
    }
    match kind {
        ActivityKind::Compose => return ToolVerb::Compose,
        ActivityKind::Agent => return ToolVerb::Agent,
        ActivityKind::Compact => return ToolVerb::Compact,
        _ => {}
    }
    match name {
        // "Shell" is the host's normalized process-start label.
        "run_process"
        | "exec_command"
        | "run_terminal_cmd"
        | "run_terminal_command"
        | "Shell"
        | "shell"
        | "bash"
        | "Bash"
        | "process_session_start" => ToolVerb::Run,
        "write_stdin"
        | "BashOutput"
        | "get_command_or_subagent_output"
        | "process_session_poll" => ToolVerb::WaitProcess,
        "process_session_write" => ToolVerb::InputProcess,
        "process_session_terminate" | "KillShell" | "TaskStop" | "kill_command_or_subagent" => {
            ToolVerb::StopProcess
        }
        "read_file" | "view_file" | "view_image" | "inspect_image" | "Read" => ToolVerb::Read,
        "search_repository" | "grep" | "Grep" | "glob" | "Glob" | "codebase_search"
        | "file_search" => ToolVerb::Search,
        "apply_patch" | "search_replace" | "edit_file" | "Edit" => ToolVerb::Edit,
        "write_file" | "create_file" | "Write" | "write" => ToolVerb::Write,
        "web_fetch" | "WebFetch" | "fetch" | "fetch_url" | "web__run" | "open_url" => {
            ToolVerb::Fetch
        }
        "web_search" | "WebSearch" => ToolVerb::WebSearch,
        "x_search" => ToolVerb::XSearch,
        "image_gen" => ToolVerb::GenerateImage,
        "list_dir" | "list_directory" | "ls" => ToolVerb::List,
        "update_plan" | "todo_write" | "merge_plan" => ToolVerb::Plan,
        name if McpCall::parse(name).is_some() => ToolVerb::Mcp,
        _ => ToolVerb::Generic,
    }
}

/// Host-projected process operations disambiguate aliases such as `write_stdin`.
pub(in crate::tui) fn subject_tool_verb(
    name: &str,
    kind: ActivityKind,
    subject: Option<&str>,
) -> ToolVerb {
    let verb = tool_verb(name, kind);
    if (verb == ToolVerb::Run && name != "exec_command")
        || !matches!(
            verb,
            ToolVerb::Run
                | ToolVerb::WaitProcess
                | ToolVerb::ReadProcess
                | ToolVerb::InputProcess
                | ToolVerb::InterruptProcess
                | ToolVerb::StopProcess
        )
    {
        return verb;
    }
    process_subject_verb(subject.unwrap_or_default()).unwrap_or(verb)
}

pub(in crate::tui) fn process_subject_verb(subject: &str) -> Option<ToolVerb> {
    match subject {
        subject
            if subject.starts_with("Waiting on process ")
                || subject.starts_with("Waited on process ") =>
        {
            Some(ToolVerb::WaitProcess)
        }
        subject if subject.starts_with("Read output of process ") => Some(ToolVerb::ReadProcess),
        subject if subject.starts_with("Sent input to process ") => Some(ToolVerb::InputProcess),
        subject if subject.starts_with("Sent interrupt to process ") => {
            Some(ToolVerb::InterruptProcess)
        }
        subject if subject.starts_with("Stopped process ") => Some(ToolVerb::StopProcess),
        _ => None,
    }
}

/// Display title for a tool row (§5.2 verbs; else humanized name).
#[must_use]
#[cfg(test)]
pub fn display_tool_title(name: &str) -> String {
    display_tool_title_for(name, ActivityKind::Tool)
}

/// Display title for a tool row with its host-classified kind.
#[must_use]
pub fn display_tool_title_for(name: &str, kind: ActivityKind) -> String {
    if tool_verb(name, kind) == ToolVerb::Mcp
        && let Some(call) = McpCall::parse(name)
    {
        return format!("mcp:{} {}", call.server, call.tool);
    }
    tool_verb(name, kind)
        .label()
        .map_or_else(|| capitalize(&humanize_tool(name)), str::to_owned)
}

/// `ask user question` → `Ask user question`: a generic `Label: content` row
/// reads as a title (Grok Build `other.rs`).
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// True when a tool block is still in-flight (start/running).
#[must_use]
pub fn tool_is_running(state: &str) -> bool {
    matches!(state, "start" | "running")
}

/// True when a lifecycle state truthfully denotes unsuccessful completion.
#[must_use]
pub fn tool_is_failed(state: &str) -> bool {
    state.starts_with("rejected")
        || state.starts_with("failed")
        || state == "overflow"
        || state.starts_with("overflow:")
        || state == "error"
        || tool_is_interrupted(state)
}

pub(in crate::tui) fn tool_is_interrupted(state: &str) -> bool {
    matches!(
        state.split_whitespace().next(),
        Some("interrupted" | "cancelled" | "canceled")
    )
}

/// Leading graphemes of `text` that fit `budget` display cells.
#[cfg(test)]
fn head_cells(text: &str, budget: usize) -> String {
    let mut used = 0usize;
    let mut out = String::new();
    for grapheme in text.graphemes(true) {
        let cells = grapheme.width().max(1);
        if used.saturating_add(cells) > budget {
            break;
        }
        used = used.saturating_add(cells);
        out.push_str(grapheme);
    }
    out
}

/// Trailing graphemes of `text` that fit `budget` display cells.
#[cfg(test)]
fn tail_cells(text: &str, budget: usize) -> String {
    let mut used = 0usize;
    let mut rev: Vec<&str> = Vec::new();
    for grapheme in text.graphemes(true).collect::<Vec<_>>().into_iter().rev() {
        let cells = grapheme.width().max(1);
        if used.saturating_add(cells) > budget {
            break;
        }
        used = used.saturating_add(cells);
        rev.push(grapheme);
    }
    rev.into_iter().rev().collect()
}

/// Fit a tool subject into `budget` display cells (spec 02 §5.4).
///
/// The subject is never dropped: paths surrender their leading directories,
/// everything else truncates on the right, and both keep a visible `…`.
#[must_use]
#[cfg(test)]
pub fn budget_tool_subject(subject: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    if subject.width() <= budget {
        return subject.to_owned();
    }
    if budget == 1 {
        return "…".to_owned();
    }
    let room = budget.saturating_sub(1);
    if subject.contains('/') && !subject.chars().any(char::is_whitespace) {
        format!("…{}", tail_cells(subject, room))
    } else {
        format!("{}…", head_cells(subject, room))
    }
}

/// Extra detail after the tool name (skip lifecycle noise: start/running/done).
#[must_use]
pub fn tool_state_detail(state: &str) -> Option<&str> {
    if tool_is_running(state)
        || state == "done"
        || state == "overflow"
        || state.is_empty()
        || state.starts_with("done:")
        || state.starts_with("overflow:")
    {
        return None;
    }
    if let Some(rest) = state.strip_prefix("rejected") {
        let detail = rest.trim_start_matches(':').trim();
        return (!detail.is_empty()).then_some(detail);
    }
    // `failed:exit 1` reads `× Run cargo test · exit 1`: the glyph already
    // says it failed, the detail says how.
    if let Some(how) = state.strip_prefix("failed:") {
        return (!how.is_empty()).then_some(how);
    }
    Some(state)
}

/// Completed timing baked into `done:1.2s` / `done:12ms`.
#[must_use]
pub fn tool_duration_label(state: &str) -> Option<&str> {
    let timing = state
        .strip_prefix("done:")
        .or_else(|| state.strip_prefix("overflow:"))?;
    (!timing.is_empty()).then_some(timing)
}

/// What a row paints when the host measured a positive but sub-millisecond
/// elapsed: `0ms` claims a call that did work took no time at all,
/// which is the one thing the measurement cannot mean.
pub const SUBMILLISECOND_ELAPSED: &str = "<1ms";

/// Compact elapsed label shared by live running tools and completed `done:` state.
///
/// A call whose start and terminal events land inside the same
/// millisecond used to paint `0ms`. The row reports the floor of what it can
/// resolve — `<1ms` — instead of asserting a zero it never observed.
#[must_use]
pub fn format_tool_elapsed(elapsed: Duration) -> String {
    if elapsed.as_secs() > 0 {
        format!(
            "{}.{:01}s",
            elapsed.as_secs(),
            elapsed.subsec_millis() / 100
        )
    } else if elapsed.as_millis() == 0 {
        SUBMILLISECOND_ELAPSED.to_owned()
    } else {
        format!("{}ms", elapsed.as_millis())
    }
}

/// The one whole-second duration ladder (spec 02 §5.4): `45s` under a
/// minute, then zero-padded `1m05s`, then `1h05m`.
#[must_use]
pub(crate) fn format_duration_secs(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// A group's or a background job's elapsed time: `12.3s` under a minute,
/// then `1m02s`, `1h05m`.
#[must_use]
pub fn format_group_elapsed(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        format_duration_secs(secs)
    } else {
        format_tool_elapsed(Duration::from_millis(ms))
    }
}

/// True for the exact shapes [`format_tool_elapsed`] produces (`12ms`, `1.2s`,
/// `<1ms`).
fn is_elapsed_label(text: &str) -> bool {
    if text == SUBMILLISECOND_ELAPSED {
        return true;
    }
    let digits = |value: &str| !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit());
    text.strip_suffix("ms").is_some_and(digits)
        || text.strip_suffix('s').is_some_and(|seconds| {
            seconds.split_once('.').map_or_else(
                || digits(seconds),
                |(whole, tenths)| digits(whole) && digits(tenths),
            )
        })
}

/// The elapsed time a terminal state already records, in either shape the row
/// uses: `done:1.2s` for success, `<state> · 1.2s` for failure or rejection.
#[must_use]
pub fn recorded_tool_duration(state: &str) -> Option<&str> {
    tool_duration_label(state).or_else(|| {
        state
            .rsplit_once(" · ")
            .map(|(_, timing)| timing)
            .filter(|timing| is_elapsed_label(timing))
    })
}

/// Folds a later terminal update into the state a row already recorded.
///
/// A tool row is timed once, when the host's terminal event for that call id
/// arrives and `tool_started` still holds its start. A Compose head then keeps
/// receiving `emit_compose_update`s — a child terminalising inside a `wait*`
/// continuation re-emits the head with a bare `done` — and the upsert wrote
/// that straight over `done:12.3s`, so a yielded compose lost its duration on
/// screen (§5.8). A terminal update that carries no elapsed of its own never
/// clears the elapsed the row measured; it only replaces the state text.
#[must_use]
pub fn terminal_state_keeping_duration(existing: &str, incoming: String) -> String {
    if recorded_tool_duration(&incoming).is_some() {
        return incoming;
    }
    let Some(timing) = recorded_tool_duration(existing) else {
        return incoming;
    };
    if incoming == "done" {
        format!("done:{timing}")
    } else {
        format!("{incoming} · {timing}")
    }
}

/// State glyph for a tool row (§5.2): `◌` pending, `✓` ok, `×` failed, `⊘` rejected.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn tool_state_glyph(state: &str) -> &'static str {
    if tool_is_running(state) {
        "◌"
    } else if state.starts_with("rejected") || state.starts_with("cancelled") || state == "canceled"
    {
        "⊘"
    } else if tool_is_failed(state) {
        "×"
    } else if state == "done"
        || state.starts_with("done:")
        || matches!(state, "completed" | "succeeded")
    {
        "✓"
    } else {
        "•"
    }
}

/// §5.8: the host counts the calls in a cell, and gluing the plural on
/// produced `1 tools`. Each ` · ` segment is singularized on its own so a
/// compound subject (`3 tools · 1 failed`) keeps its shape.
#[must_use]
#[cfg(test)]
pub fn singularize_counts(subject: &str) -> String {
    subject
        .split(" · ")
        .map(|segment| match segment {
            "1 tools" => "1 tool",
            "1 agents" => "1 agent",
            "1 files" => "1 file",
            "1 nodes" => "1 node",
            "1 children" => "1 child",
            other => other,
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The subject a §5.2 row paints: the host's, with the count grammar fixed.
///
/// A Compose header names only its host-counted tally (`3 tools · 1 failed`);
/// the program itself is one activation away behind the rail (§5.8).
#[must_use]
#[cfg(test)]
pub fn row_subject(subject: Option<&str>) -> Option<String> {
    subject
        .filter(|subject| !subject.is_empty())
        .map(singularize_counts)
}

/// Group-head text such as `Read 3 files` (§5.5).
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub fn group_header(verb: ToolVerb, count: usize) -> String {
    verb.group_noun().replace("{n}", &count.to_string())
}

/// Collapsed tool summary: `  ▸ ◌ Run  cargo test`.
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn format_tool_plain(name: &str, state: &str, subject: Option<&str>) -> String {
    format_tool_summary(name, state, subject, false)
}

/// Summary line: Direct state glyph, or Compose `▶`/`▼` plus state glyph.
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn format_tool_summary(
    name: &str,
    state: &str,
    subject: Option<&str>,
    expanded: bool,
) -> String {
    let mode = if expanded {
        DisplayMode::Expanded
    } else {
        DisplayMode::Collapsed
    };
    format_tool_row(
        name,
        state,
        ActivityKind::Tool,
        subject,
        mode,
        &RowPlan::default(),
    )
}

/// One §5.2 header row, honouring the Compose rails and the §5.5 group head.
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn format_tool_row(
    name: &str,
    state: &str,
    kind: ActivityKind,
    subject: Option<&str>,
    mode: DisplayMode,
    plan: &RowPlan,
) -> String {
    let verb = tool_verb(name, kind);
    let grouped = plan.group_count >= 2;
    let label = if grouped {
        group_header(verb, plan.group_count)
    } else {
        display_tool_title_for(name, kind)
    };
    // Direct rows have no fold triangle. Compose keeps ▶/▼ for the program
    // rail. An open/folded group head uses the same triangles.
    let glyph = tool_state_glyph(state);
    let mut line = if kind == ActivityKind::Compose {
        let fold = if mode == DisplayMode::Expanded {
            "▼"
        } else {
            "▶"
        };
        format!("{}{fold} {glyph} {label}", row_indent(plan))
    } else if grouped {
        let fold = if plan.group_folded { "▶" } else { "▼" };
        format!("{}{fold} {glyph} {label}", row_indent(plan))
    } else {
        format!("{}{glyph} {label}", row_indent(plan))
    };
    // §5.5: a group header names the run, never one member's subject.
    if !grouped && let Some(subject) = subject.filter(|subject| !subject.is_empty()) {
        line.push_str(subject_separator(verb));
        line.push_str(subject);
    }
    if let Some(detail) = tool_state_detail(state) {
        line.push_str(" · ");
        line.push_str(detail);
    }
    if let Some(duration) = tool_duration_label(state) {
        line.push_str(" · ");
        line.push_str(duration);
    }
    line
}

/// What sits between a row's verb and its subject: Grok Build's single space
/// (`Run cargo test`), the generic `Label: content` colon, or the Tact-weight
/// double space after a Compose label.
#[cfg(any(test, feature = "test-support"))]
#[must_use]
pub const fn subject_separator(verb: ToolVerb) -> &'static str {
    match verb {
        ToolVerb::Generic => ": ",
        ToolVerb::Compose => "  ",
        ToolVerb::Compact => "  ",
        _ => " ",
    }
}
