//! Tool verbs and titles, run-state labels, and tool and approval rows.

#[cfg(test)]
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::blocks::{SUBMILLISECOND_ELAPSED, format_tool_elapsed, wrap_plain};
use crate::tui::presentation::{ActivityKind, McpCall};

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
    operation_verb(plakous_host_protocol::views::ToolOperation::classify(
        name, kind, None,
    ))
}

/// Terminal wording for an already host-classified operation.
#[must_use]
pub fn operation_verb(operation: plakous_host_protocol::views::ToolOperation) -> ToolVerb {
    use plakous_host_protocol::views::ToolOperation;
    match operation {
        ToolOperation::Run => ToolVerb::Run,
        ToolOperation::WaitProcess => ToolVerb::WaitProcess,
        ToolOperation::ReadProcess => ToolVerb::ReadProcess,
        ToolOperation::InputProcess => ToolVerb::InputProcess,
        ToolOperation::InterruptProcess => ToolVerb::InterruptProcess,
        ToolOperation::StopProcess => ToolVerb::StopProcess,
        ToolOperation::Read => ToolVerb::Read,
        ToolOperation::Search => ToolVerb::Search,
        ToolOperation::Edit => ToolVerb::Edit,
        ToolOperation::Write => ToolVerb::Write,
        ToolOperation::Fetch => ToolVerb::Fetch,
        ToolOperation::WebSearch => ToolVerb::WebSearch,
        ToolOperation::XSearch => ToolVerb::XSearch,
        ToolOperation::GenerateImage => ToolVerb::GenerateImage,
        ToolOperation::Mcp => ToolVerb::Mcp,
        ToolOperation::List => ToolVerb::List,
        ToolOperation::Plan => ToolVerb::Plan,
        ToolOperation::Compose => ToolVerb::Compose,
        ToolOperation::Agent => ToolVerb::Agent,
        ToolOperation::SpawnAgent => ToolVerb::SpawnAgent,
        ToolOperation::WaitAgent => ToolVerb::WaitAgent,
        ToolOperation::ListAgents => ToolVerb::ListAgents,
        ToolOperation::MessageAgent => ToolVerb::MessageAgent,
        ToolOperation::Adopt => ToolVerb::Adopt,
        ToolOperation::CloseAgent => ToolVerb::CloseAgent,
        ToolOperation::InterruptAgent => ToolVerb::InterruptAgent,
        ToolOperation::RecoverAgent => ToolVerb::RecoverAgent,
        ToolOperation::DiscardWorkspace => ToolVerb::DiscardWorkspace,
        ToolOperation::StopAll => ToolVerb::StopAll,
        ToolOperation::SubmitResult => ToolVerb::SubmitResult,
        ToolOperation::Compact => ToolVerb::Compact,
        ToolOperation::Generic => ToolVerb::Generic,
    }
}
/// Host-projected process operations disambiguate aliases such as `write_stdin`.
pub(in crate::tui) fn subject_tool_verb(
    name: &str,
    kind: ActivityKind,
    subject: Option<&str>,
) -> ToolVerb {
    operation_verb(plakous_host_protocol::views::ToolOperation::classify(
        name, kind, subject,
    ))
}

pub(in crate::tui) fn process_subject_verb(subject: &str) -> Option<ToolVerb> {
    plakous_host_protocol::views::ToolOperation::process_subject(subject).map(operation_verb)
}

/// Display title for a tool row (§5.2 verbs; else humanized name).
#[must_use]
#[cfg(test)]
pub fn display_tool_title(name: &str) -> String {
    display_tool_title_for(name, ActivityKind::Tool)
}

/// Display title for a tool row with its host-classified kind.
#[must_use]
#[cfg(test)]
pub fn display_tool_title_for(name: &str, kind: ActivityKind) -> String {
    display_tool_title_with_verb(name, tool_verb(name, kind))
}

/// Tool title from a host-supplied semantic verb, without reclassifying its label.
#[must_use]
pub fn display_tool_title_with_verb(name: &str, verb: ToolVerb) -> String {
    if verb == ToolVerb::Mcp
        && let Some(call) = McpCall::parse(name)
    {
        return format!("mcp:{} {}", call.server, call.tool);
    }
    verb.label()
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

/// True for the exact shapes [`crate::tui::blocks::format_tool_elapsed`]
/// produces (`12ms`, `1.2s`, `<1ms`).
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

/// The same lifecycle label for live observations and keyed/restored tools.
/// Host stamps take precedence; an unstamped update retains an embedded time.
pub(in crate::tui) fn presented_tool_state(
    state: &str,
    elapsed: Option<std::time::Duration>,
) -> String {
    let state = match state {
        "start" => "running",
        "succeeded" | "completed" => "done",
        _ => state,
    };
    if tool_is_running(state) {
        return state.to_owned();
    }
    let Some(elapsed) = elapsed else {
        return state.to_owned();
    };
    let bare = if tool_duration_label(state).is_some() {
        state.split_once(':').map_or(state, |(state, _)| state)
    } else if recorded_tool_duration(state).is_some() {
        state.rsplit_once(" · ").map_or(state, |(state, _)| state)
    } else {
        state
    };
    let elapsed = format_tool_elapsed(elapsed);
    if matches!(bare, "done" | "overflow") {
        format!("{bare}:{elapsed}")
    } else {
        format!("{bare} · {elapsed}")
    }
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
    use crate::tui::blocks::{DisplayMode, RowPlan};

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
    mode: crate::tui::blocks::DisplayMode,
    plan: &crate::tui::blocks::RowPlan,
) -> String {
    use crate::tui::blocks::{DisplayMode, row_indent};

    let verb = tool_verb(name, kind);
    let grouped = plan.group_count >= 2;
    let label = if grouped {
        group_header(verb, plan.group_count)
    } else {
        display_tool_title_with_verb(name, verb)
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

/// The open approval row's clickable chip that allows it.
pub const APPROVAL_ALLOW_CHIP: &str = "[allow]";

/// The open approval row's clickable chip that denies it.
pub const APPROVAL_DENY_CHIP: &str = "[deny]";

/// The open approval's chips in its header wrapped at `width`: the row
/// within the block, the display column and the chip, `[allow]` first. The
/// status that carries them ends the header, after the model-written
/// subject, so each chip is its label's last occurrence in the header rows;
/// the prompt rows below are never searched.
#[must_use]
pub fn approval_chip_cells(
    tool_name: &str,
    subject: Option<&str>,
    width: usize,
) -> Vec<(usize, usize, &'static str)> {
    let Some(header) = format_approval_block(tool_name, subject, None, None)
        .into_iter()
        .next()
    else {
        return Vec::new();
    };
    let rows = wrap_plain(&header, width);
    [APPROVAL_ALLOW_CHIP, APPROVAL_DENY_CHIP]
        .into_iter()
        .filter_map(|label| {
            rows.iter().enumerate().rev().find_map(|(row, text)| {
                text.rfind(label).map(|at| (row, text[..at].width(), label))
            })
        })
        .collect()
}

/// Plain rows for one §5.2 approval block.
#[must_use]
pub fn format_approval_block(
    tool_name: &str,
    subject: Option<&str>,
    prompt: Option<&str>,
    resolution: Option<&str>,
) -> Vec<String> {
    let glyph = match resolution {
        None => "◆",
        Some("allowed") => "✓",
        Some(_) => "⊘",
    };
    let status = resolution.unwrap_or("awaiting decision · [allow] Alt+A · [deny] Alt+D");
    let mut header = format!("  {glyph} Approval  {}", humanize_tool(tool_name));
    if let Some(subject) = subject.filter(|subject| !subject.is_empty()) {
        header.push(' ');
        header.push_str(subject);
    }
    header.push_str(" · ");
    header.push_str(status);
    let mut lines = vec![header];
    if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
        lines.push(format!("  └ {prompt}"));
    }
    lines
}
