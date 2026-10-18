//! Structured scrollback blocks for the product TUI.
//!
//! Rendering: cyan gutter for user turns (no `you:` prefix), plain assistant
//! body, quiet tool rows.
//! Composer / sticky prompt glyph matches Grok Build (`❯` U+276F).

use std::borrow::Cow;
use std::time::Duration;

use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui::presentation::{ActivityKind, ActivityState, McpCall};
use plakous_host_protocol::views::CallDetailOutcome;
use plakous_host_protocol::views::transcript::{
    AgentMilestone, MilestoneEvent, NoticePlacement, NoticeTone, ProcessNotice, ProcessOutcome,
    ToolItem, TranscriptRow, WaitFacts,
};

/// Grok Build / expandable-indicator prompt glyph (compressed `>` lookalike).
#[cfg(test)]
pub const PROMPT_GLYPH: &str = "❯";
/// Prompt glyph plus trailing space (composer + sticky first line).
pub const PROMPT_PREFIX: &str = "❯ ";
/// Product read-model retention envelope mirrored by the live presentation.
pub(crate) const MAX_LIVE_TRANSCRIPT_BLOCKS: usize = 10_000;
pub(crate) const MAX_LIVE_TRANSCRIPT_BYTES: usize = 16 * 1024 * 1024;
/// The turn marker an interrupted root turn leaves; activity planning keeps
/// it visible below a folded group.
pub(crate) const INTERRUPTED_MARKER: &str = "⊘ interrupted";
pub(crate) const TRANSCRIPT_TRUNCATION_MARKER: &str =
    "[older product transcript projection omitted]";

/// Admitted display modes for a foldable transcript block (spec 02 §5.4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DisplayMode {
    /// Header line only.
    #[default]
    Collapsed,
    /// Header plus the complete bounded detail.
    Expanded,
}

impl DisplayMode {
    /// True when any detail rows are drawn under the header.
    #[must_use]
    pub const fn shows_detail(self) -> bool {
        matches!(self, Self::Expanded)
    }
}

/// The §5.5 rows that steer transcript rendering.
///
/// These have no representation in `crate::tui::SettingsSnapshot` yet, so
/// the renderer carries its own defaults. The struct is the single place the
/// settings registry has to reach once those rows land.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one field per §5.5 boolean settings row"
)]
pub struct RenderPrefs {
    /// §5.5 `show thinking blocks` (§5.6).
    pub show_thinking: bool,
    /// §5.5 `group tool calls` (§5.5).
    pub group_tool_calls: bool,
    /// §5.5 `reduced motion` — suppresses the spinner, the approval pulse and
    /// camera easing.
    pub reduced_motion: bool,
    /// The local day prompt send times are read against (§5.4). Not a
    /// settings row: part of the prefs so a new day re-renders the rows.
    pub prompt_clock: PromptClock,
}

/// The local calendar day and zone a prompt's send time is shown at.
///
/// The default names no day and reads UTC, so every send time shows its date.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PromptClock {
    /// Julian day number of the local date of "now"; `0` matches no stamp.
    today: i32,
    offset: StampOffset,
}

/// Where a send time's UTC offset comes from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StampOffset {
    /// Every stamp uses this many seconds east of UTC.
    Fixed(i32),
    /// The launch's zone. Each stamp takes the offset it had.
    Zone(&'static jiff::tz::TimeZone),
}

impl Default for StampOffset {
    fn default() -> Self {
        Self::Fixed(0)
    }
}

impl PromptClock {
    /// The clock at `now_unix_ms`, read at a fixed `offset_seconds`.
    #[cfg(test)]
    #[must_use]
    pub fn at(now_unix_ms: u64, offset_seconds: i32) -> Self {
        let today = local_moment(now_unix_ms, offset_seconds)
            .map_or(0, |moment| moment.date().to_julian_day());
        Self {
            today,
            offset: StampOffset::Fixed(offset_seconds),
        }
    }

    /// The clock at `now_unix_ms` in `zone` (the launch's: the system zone
    /// in production, a fixed one in tests).
    ///
    /// Today is that zone's date for `now`; a later label takes the offset
    /// the zone had at the stamp, so a daylight-saving change does not move
    /// an older send time.
    #[must_use]
    pub fn in_zone(zone: &'static jiff::tz::TimeZone, now_unix_ms: u64) -> Self {
        let offset = offset_at(zone, now_unix_ms);
        let today =
            local_moment(now_unix_ms, offset).map_or(0, |moment| moment.date().to_julian_day());
        Self {
            today,
            offset: StampOffset::Zone(zone),
        }
    }

    /// `14:32` for a send time on the local today, `Oct 7 14:32` otherwise.
    #[must_use]
    pub fn label(self, sent_at_ms: u64) -> Option<String> {
        let offset = match self.offset {
            StampOffset::Fixed(seconds) => seconds,
            StampOffset::Zone(zone) => offset_at(zone, sent_at_ms),
        };
        let moment = local_moment(sent_at_ms, offset)?;
        let clock = format!("{:02}:{:02}", moment.hour(), moment.minute());
        if moment.date().to_julian_day() == self.today {
            return Some(clock);
        }
        let month = moment.month().to_string();
        Some(format!(
            "{} {} {clock}",
            month.get(..3).unwrap_or(&month),
            moment.day()
        ))
    }
}

fn offset_at(zone: &jiff::tz::TimeZone, unix_ms: u64) -> i32 {
    let Ok(seconds) = i64::try_from(unix_ms / 1_000) else {
        return 0;
    };
    let Ok(timestamp) = jiff::Timestamp::from_second(seconds) else {
        return 0;
    };
    zone.to_offset(timestamp).seconds()
}

fn local_moment(unix_ms: u64, offset_seconds: i32) -> Option<time::OffsetDateTime> {
    let offset =
        time::UtcOffset::from_whole_seconds(offset_seconds).unwrap_or(time::UtcOffset::UTC);
    let seconds = i64::try_from(unix_ms / 1_000).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(|moment| moment.to_offset(offset))
}

impl From<&crate::tui::SettingsSnapshot> for RenderPrefs {
    /// The settings snapshot is the only authority for these rendering rows; the
    /// renderer holds no second copy of a default that could drift from §5.5.
    fn from(snapshot: &crate::tui::SettingsSnapshot) -> Self {
        Self {
            show_thinking: snapshot.show_thinking,
            group_tool_calls: snapshot.group_tool_calls,
            reduced_motion: snapshot.reduced_motion,
            prompt_clock: PromptClock::default(),
        }
    }
}

impl Default for RenderPrefs {
    /// Derived from the config-schema defaults, so the renderer and the
    /// Settings tab cannot disagree about what a row's default is.
    fn default() -> Self {
        Self::from(&crate::tui::SettingsSnapshot::default())
    }
}

/// The label over assistant text a replayed model request discarded.
pub const DISCARDED_LABEL: &str = "discarded -- retrying";
/// The label over reasoning a replayed model request discarded.
pub const DISCARDED_THINKING_LABEL: &str = "discarded thinking -- retrying";

/// One visual unit in the transcript scrollback.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ScrollBlock {
    /// User-originated message.
    User {
        /// Prompt text.
        text: String,
        /// When the operator sent it, Unix milliseconds (host-stamped).
        sent_at_ms: Option<u64>,
    },
    /// Assistant turn text (streamed or final).
    Assistant(String),
    /// Assistant text or reasoning a model call streamed before its request
    /// failed and was replayed; drawn dimmed and never part of history.
    Discarded {
        /// The discarded text.
        text: String,
        /// It was reasoning, shown only while thinking is shown.
        thinking: bool,
    },
    /// Tool lifecycle line (`running` | `start` | `done` | `rejected…`).
    Tool {
        /// Correlated call id when known.
        call_id: Option<String>,
        /// Outer local-composition marker call owning this row.
        ///
        /// Host-supplied lineage only. A lineage the TUI was not given renders
        /// flat; nesting is never inferred from names or arrival order.
        parent_call_id: Option<String>,
        /// Local-composition cell owning this row.
        cell_id: Option<String>,
        /// Host-classified activity category. Never re-derived here.
        kind: ActivityKind,
        /// Tool name.
        name: String,
        /// Lifecycle / status label.
        state: String,
        /// Optional subject (path, command snippet, query, …).
        subject: Option<String>,
        /// Expandable detail body (args / result preview).
        detail: Option<String>,
        /// Current display mode (spec 02 §5.4).
        mode: DisplayMode,
        /// The operator chose `mode` by hand; streaming must not override it.
        manual: bool,
        /// A verb-group head whose members are shown individually (spec 02 §5.4).
        group_open: bool,
        /// What a wait asked for and what ended it, when the call is a wait.
        wait: Option<WaitFacts>,
        /// The host's start stamp, Unix milliseconds, when it sent one.
        started_at_ms: Option<u64>,
        /// The host's finish stamp, Unix milliseconds, once the call ended.
        finished_at_ms: Option<u64>,
        /// Operator-side detail state: the fetched call text and what is open.
        view: ItemView,
    },
    /// One admitted reasoning block (spec 02 §5.4).
    Thinking {
        /// Opaque per-turn reasoning location identity.
        location_key: String,
        /// Already-redacted reasoning text.
        text: String,
        /// The provider sealed this block.
        sealed: bool,
        /// Wall-clock duration between the TUI seeing the first delta and the
        /// host's seal, in milliseconds. Not a host-measured figure.
        elapsed_ms: Option<u64>,
        /// Current display mode.
        mode: DisplayMode,
        /// The operator chose `mode` by hand.
        manual: bool,
    },
    /// One pending or resolved effect approval.
    Approval {
        /// Host-minted, session-scoped opaque handle.
        approval_id: String,
        /// Canonical tool awaiting the decision.
        tool_name: String,
        /// Bounded subject, when the host had one.
        subject: Option<String>,
        /// Operator-facing prompt, already redaction-safe.
        prompt: Option<String>,
        /// Closed host resolution label once decided.
        resolution: Option<String>,
    },
    /// A non-call item inside a tool group: a retry cause, a process exit or
    /// a clipped observation. Unlike a notice it never ends the group.
    ///
    /// `meta` is right-aligned and dropped before `text` is cut. Only a
    /// process row that names its command uses it (the outcome and duration);
    /// every other item leaves it empty and stays one label.
    Item {
        /// The label a narrow row shortens.
        text: String,
        /// Right-aligned parts (`exited 0`, `21m40s`).
        meta: Vec<String>,
    },
    /// A steer delivered into a running turn: the prompt band with a tag.
    Steer {
        /// Steer text.
        text: String,
        /// When the operator sent it, Unix milliseconds (host-stamped).
        sent_at_ms: Option<u64>,
    },
    /// The muted session header row.
    Header(String),
    /// A child-agent lifecycle row; `open` shows its full text.
    Milestone {
        /// The typed milestone the host persisted.
        milestone: AgentMilestone,
        /// The operator opened the row's detail.
        open: bool,
    },
    /// Dim system / session note. Ends the current tool group.
    System(String),
    /// Error note (red accent).
    Error(String),
}

/// The operator-side state of one tool item's detail (spec 02 §5.4).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ItemView {
    /// The host's answer for this call's full input and output.
    pub fetched: Option<std::sync::Arc<CallDetailOutcome>>,
    /// The output window's hidden middle is shown.
    pub full_output: bool,
    /// A cell's own output is independent of its source and outline.
    pub output_open: bool,
    /// A merged row (`Explored …`, `N agents …`) lists its calls.
    pub members_open: bool,
}

/// The line a spilled tool result keeps naming the file holding all of it.
pub(crate) const FULL_OUTPUT_PREFIX: &str = "full output: ";

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
pub(super) fn subject_tool_verb(name: &str, kind: ActivityKind, subject: Option<&str>) -> ToolVerb {
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

pub(super) fn process_subject_verb(subject: &str) -> Option<ToolVerb> {
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

pub(super) fn tool_is_interrupted(state: &str) -> bool {
    matches!(
        state.split_whitespace().next(),
        Some("interrupted" | "cancelled" | "canceled")
    )
}

impl ScrollBlock {
    /// A prompt block without a send time.
    #[must_use]
    pub fn user(text: String) -> Self {
        Self::User {
            text,
            sent_at_ms: None,
        }
    }

    /// A steer block without a send time.
    #[cfg(test)]
    #[must_use]
    pub fn steer(text: String) -> Self {
        Self::Steer {
            text,
            sent_at_ms: None,
        }
    }

    /// An item row with no right-aligned meta.
    #[must_use]
    pub fn item(text: impl Into<String>) -> Self {
        Self::Item {
            text: text.into(),
            meta: Vec::new(),
        }
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        match self {
            Self::User { text, .. }
            | Self::Assistant(text)
            | Self::Discarded { text, .. }
            | Self::Steer { text, .. }
            | Self::Header(text)
            | Self::System(text)
            | Self::Error(text) => text.len(),
            Self::Item { text, meta } => text.len() + meta.iter().map(String::len).sum::<usize>(),
            Self::Milestone { milestone, .. } => {
                milestone.agent_id.len()
                    + milestone.text.as_ref().map_or(0, String::len)
                    + milestone.paths.iter().map(String::len).sum::<usize>()
            }
            Self::Tool {
                call_id,
                name,
                state,
                subject,
                detail,
                ..
            } => {
                name.len()
                    + state.len()
                    + call_id.as_ref().map_or(0, String::len)
                    + subject.as_ref().map_or(0, String::len)
                    + detail.as_ref().map_or(0, String::len)
            }
            Self::Thinking {
                location_key, text, ..
            } => location_key.len() + text.len(),
            Self::Approval {
                approval_id,
                tool_name,
                subject,
                prompt,
                resolution,
            } => {
                approval_id.len()
                    + tool_name.len()
                    + subject.as_ref().map_or(0, String::len)
                    + prompt.as_ref().map_or(0, String::len)
                    + resolution.as_ref().map_or(0, String::len)
            }
        }
    }

    pub(crate) fn truncate_oversized_tail(&mut self) {
        if self.retained_bytes() <= MAX_LIVE_TRANSCRIPT_BYTES {
            return;
        }
        match self {
            Self::User { text, .. }
            | Self::Assistant(text)
            | Self::Discarded { text, .. }
            | Self::Item { text, .. }
            | Self::Steer { text, .. }
            | Self::Header(text)
            | Self::System(text)
            | Self::Error(text)
            | Self::Thinking { text, .. } => {
                truncate_string_tail(text, MAX_LIVE_TRANSCRIPT_BYTES);
            }
            Self::Tool { .. } | Self::Approval { .. } | Self::Milestone { .. } => {
                let mut plain = self.as_plain_line();
                truncate_string_tail(&mut plain, MAX_LIVE_TRANSCRIPT_BYTES);
                *self = Self::System(plain);
            }
        }
    }

    /// The block one typed transcript row renders as, live and restored alike
    /// (spec 02 §5.4). `None` for rows that never render: session-log-only
    /// notices. This is the one adapter from rows to today's blocks.
    #[must_use]
    pub fn from_row(row: TranscriptRow) -> Option<Self> {
        Some(match row {
            TranscriptRow::SessionHeader {
                session_id,
                carried_from,
            } => Self::Header(match carried_from {
                Some(from) => format!(
                    "carried from {} ({}) · session {}",
                    short_id(&from.session_id),
                    from.state_key,
                    short_id(&session_id)
                ),
                None => format!("session {}", short_id(&session_id)),
            }),
            TranscriptRow::User { text, sent_at_ms } => Self::User { text, sent_at_ms },
            TranscriptRow::Steer { text, sent_at_ms } => Self::Steer { text, sent_at_ms },
            TranscriptRow::Assistant { text } => Self::Assistant(text),
            TranscriptRow::Thinking(thinking) => Self::Thinking {
                location_key: thinking.location_key,
                text: thinking.text,
                sealed: true,
                elapsed_ms: thinking.elapsed_ms,
                mode: DisplayMode::Collapsed,
                manual: false,
            },
            TranscriptRow::Tool(tool) => Self::from_tool_item(tool),
            TranscriptRow::Process(process) => {
                let (text, meta) = process_line(&process);
                Self::Item { text, meta }
            }
            TranscriptRow::Retry(retry) => Self::item(format!(
                "retry {}/{} · {}",
                retry.attempt, retry.max_attempts, retry.cause
            )),
            TranscriptRow::Milestone(milestone) => Self::Milestone {
                milestone,
                open: false,
            },
            // The host no longer writes auto-continue rows; one a session
            // stored earlier is not shown.
            TranscriptRow::AutoContinue(_) => return None,
            TranscriptRow::Notice(notice) => match (notice.placement(), notice.tone) {
                (NoticePlacement::SessionLog, _) => return None,
                (_, NoticeTone::Error) => Self::Error(notice.text),
                (NoticePlacement::GroupItem, _) => Self::item(notice.text),
                (NoticePlacement::NoticeRow, _) => Self::System(notice.text),
            },
            TranscriptRow::Interrupted => Self::System(INTERRUPTED_MARKER.to_owned()),
            TranscriptRow::Truncated => Self::System(TRANSCRIPT_TRUNCATION_MARKER.to_owned()),
        })
    }

    /// A persisted tool row, with the state label a live row of the same call
    /// earned: a resumed `12.3s` does not reload as `12315ms`, a rejection
    /// stays `⊘` and a failure keeps its detail.
    fn from_tool_item(tool: ToolItem) -> Self {
        let kind = tool.kind;
        let state = match tool.activity_state() {
            ActivityState::Pending | ActivityState::Running => "running".to_owned(),
            ActivityState::Succeeded => tool.duration_ms().map_or_else(
                || "done".to_owned(),
                |ms| format!("done:{}", format_tool_elapsed(Duration::from_millis(ms))),
            ),
            ActivityState::Failed { reason } => match reason {
                Some(reason) if reason.starts_with("rejected") || reason == "overflow" => reason,
                Some(reason) => format!("failed:{reason}"),
                None => "failed".to_owned(),
            },
            ActivityState::Cancelled => "cancelled".to_owned(),
        };
        Self::Tool {
            call_id: Some(tool.call_id),
            // Host lineage only: a resumed transcript folds exactly like the
            // live one, and never guesses.
            parent_call_id: tool.parent_call_id,
            cell_id: tool.cell_id,
            kind,
            mode: DisplayMode::Collapsed,
            // Labelled by the name the model called (spec 02 §5.4); the host
            // already classified the row by its canonical name.
            name: tool.exposed_name.unwrap_or(tool.name),
            state,
            subject: tool.subject,
            detail: tool.detail,
            manual: false,
            group_open: false,
            wait: tool.wait,
            started_at_ms: tool.started_at_ms,
            finished_at_ms: tool.finished_at_ms,
            view: ItemView::default(),
        }
    }

    /// Flatten to a single plain line (tests / legacy echo).
    #[must_use]
    pub fn as_plain_line(&self) -> String {
        match self {
            Self::User { text, .. } => format!("you: {text}"),
            Self::Assistant(text) => format!("assistant: {text}"),
            Self::Discarded { text, thinking } => {
                format!("{}: {text}", discarded_label(*thinking))
            }
            Self::Tool {
                name,
                state,
                subject,
                ..
            } => match subject {
                Some(s) if !s.is_empty() => format!("tool: {name} {state} {s}"),
                _ => format!("tool: {name} {state}"),
            },
            Self::Thinking { text, .. } => format!("thinking: {text}"),
            Self::Approval {
                tool_name,
                resolution,
                ..
            } => match resolution {
                Some(resolution) => format!("approval: {tool_name} {resolution}"),
                None => format!("approval: {tool_name} pending"),
            },
            Self::Item { text, meta } => {
                if meta.is_empty() {
                    text.clone()
                } else {
                    format!("{text} {}", meta.join(" · "))
                }
            }
            Self::System(text) | Self::Header(text) => text.clone(),
            Self::Steer { text, .. } => format!("steer: {text}"),
            Self::Milestone { milestone, .. } => milestone_line(milestone),
            Self::Error(text) => format!("error: {text}"),
        }
    }

    /// Content body without chrome prefix (for search / sticky / tests).
    #[must_use]
    pub fn body(&self) -> &str {
        match self {
            Self::User { text: t, .. }
            | Self::Assistant(t)
            | Self::Discarded { text: t, .. }
            | Self::Item { text: t, .. }
            | Self::Steer { text: t, .. }
            | Self::Header(t)
            | Self::System(t)
            | Self::Error(t) => t.as_str(),
            Self::Milestone { milestone, .. } => milestone
                .text
                .as_deref()
                .unwrap_or(milestone.agent_id.as_str()),
            Self::Tool { name, .. } => name.as_str(),
            Self::Thinking { text, .. } => text.as_str(),
            Self::Approval { tool_name, .. } => tool_name.as_str(),
        }
    }

    /// True for user turns (sticky bar source).
    #[must_use]
    pub const fn is_user(&self) -> bool {
        matches!(self, Self::User { .. } | Self::Steer { .. })
    }

    /// A lineage-free, collapsed tool row.
    #[cfg(test)]
    #[must_use]
    pub fn tool(name: &str, state: &str) -> Self {
        Self::tool_row(None, name, state, ActivityKind::Tool, None, None)
    }

    /// One tool row carrying host lineage and classification.
    #[cfg(test)]
    #[must_use]
    pub fn tool_row(
        call_id: Option<String>,
        name: &str,
        state: &str,
        kind: ActivityKind,
        subject: Option<String>,
        detail: Option<String>,
    ) -> Self {
        Self::Tool {
            call_id,
            parent_call_id: None,
            cell_id: None,
            kind,
            name: name.to_owned(),
            state: state.to_owned(),
            subject,
            detail,
            mode: DisplayMode::Collapsed,
            manual: false,
            group_open: false,
            wait: None,
            started_at_ms: None,
            finished_at_ms: None,
            view: ItemView::default(),
        }
    }

    /// The block's current display mode, when it has one.
    #[must_use]
    pub fn display_mode(&self) -> Option<DisplayMode> {
        match self {
            Self::Tool { mode, .. } | Self::Thinking { mode, .. } => Some(*mode),
            Self::Milestone { milestone, open } if milestone_has_detail(milestone) => {
                Some(if *open {
                    DisplayMode::Expanded
                } else {
                    DisplayMode::Collapsed
                })
            }
            _ => None,
        }
    }

    /// Replace the display mode, recording whether the operator chose it.
    pub const fn set_display_mode(&mut self, next: DisplayMode, by_operator: bool) {
        match self {
            Self::Tool { mode, manual, .. } | Self::Thinking { mode, manual, .. } => {
                *mode = next;
                if by_operator {
                    *manual = true;
                }
            }
            Self::Milestone { open, .. } => *open = next.shows_detail(),
            _ => {}
        }
    }

    /// Clear a per-row manual override (`Ctrl-Shift-O`).
    pub const fn clear_manual_override(&mut self) {
        match self {
            Self::Tool { manual, .. } | Self::Thinking { manual, .. } => *manual = false,
            _ => {}
        }
    }

    /// True when the operator pinned this block's mode by hand.
    #[must_use]
    #[cfg(test)]
    pub const fn is_manual(&self) -> bool {
        match self {
            Self::Tool { manual, .. } | Self::Thinking { manual, .. } => *manual,
            _ => false,
        }
    }

    /// True when this block owns a foldable row the keymap can target.
    #[must_use]
    pub fn is_foldable(&self) -> bool {
        match self {
            Self::Tool { .. } | Self::Thinking { .. } => true,
            Self::Milestone { milestone, .. } => milestone_has_detail(milestone),
            _ => false,
        }
    }
}

fn truncate_string_tail(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let payload_limit = limit.saturating_sub(TRANSCRIPT_TRUNCATION_MARKER.len() + 1);
    let mut start = text.len().saturating_sub(payload_limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = format!("{TRANSCRIPT_TRUNCATION_MARKER}\n{}", &text[start..]);
}

/// Render typed transcript rows as blocks.
#[must_use]
pub fn blocks_from_rows(rows: impl IntoIterator<Item = TranscriptRow>) -> Vec<ScrollBlock> {
    rows.into_iter().filter_map(ScrollBlock::from_row).collect()
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Label and right-aligned meta of a process row.
///
/// With its command the id is the label (`p261 make`) and the outcome and
/// duration are meta, so a narrow row cuts the command and keeps them. A
/// row stored without a command stays one label (`job p261 exited 0`).
fn process_line(process: &ProcessNotice) -> (String, Vec<String>) {
    let outcome = match process.outcome {
        ProcessOutcome::Exited { code: Some(code) } => format!("exited {code}"),
        ProcessOutcome::Exited { code: None } => "exited".to_owned(),
        ProcessOutcome::Stopped => "stopped".to_owned(),
    };
    let duration = process.duration_ms.map(format_group_elapsed);
    if let Some(subject) = process
        .subject
        .as_deref()
        .filter(|subject| !subject.is_empty())
    {
        let mut meta = vec![outcome];
        if let Some(duration) = duration {
            meta.push(duration);
        }
        (format!("{} {subject}", process.job_id), meta)
    } else {
        let text = match &duration {
            Some(duration) => format!("job {} {outcome} · {duration}", process.job_id),
            None => format!("job {} {outcome}", process.job_id),
        };
        (text, Vec::new())
    }
}

fn milestone_line(milestone: &AgentMilestone) -> String {
    let (glyph, label) = milestone_parts(milestone);
    format!("{glyph} {label}")
}

/// Whether a milestone row opens to more than it shows: a brief, result,
/// reason or question, more than one conflicting path, or an adoption's facts.
pub(crate) fn milestone_has_detail(milestone: &AgentMilestone) -> bool {
    milestone.event == MilestoneEvent::Adopted
        || milestone
            .text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        || milestone.paths.len() > 1
}

/// The status glyph and the one-line label of a milestone row (spec 02 §5.4).
pub(crate) fn milestone_parts(milestone: &AgentMilestone) -> (&'static str, String) {
    let id = &milestone.agent_id;
    let first = milestone
        .text
        .as_deref()
        .and_then(|text| text.lines().find(|line| !line.trim().is_empty()))
        .map(str::trim)
        .unwrap_or_default();
    let with_first = |head: String| {
        if first.is_empty() {
            head
        } else {
            format!("{head} · {first}")
        }
    };
    match milestone.event {
        MilestoneEvent::Spawned => (
            "◆",
            with_first(match &milestone.template {
                Some(template) => format!("spawned {id} · {template}"),
                None => format!("spawned {id}"),
            }),
        ),
        MilestoneEvent::Retasked => ("◇", with_first(format!("re-tasked {id}"))),
        MilestoneEvent::Finished => (
            "✓",
            match milestone.duration_ms {
                Some(ms) => format!("{id} finished · {}", format_span(ms)),
                None => format!("{id} finished"),
            },
        ),
        MilestoneEvent::Failed => ("✗", with_first(format!("{id} failed"))),
        MilestoneEvent::NeedsRecovery => ("⚠", format!("{id} needs recovery · recover_agent")),
        MilestoneEvent::Asks => ("?", with_first(format!("{id} asks"))),
        MilestoneEvent::Adopted => (
            "⇲",
            match milestone.files {
                Some(files) => format!(
                    "adopted {} · {}",
                    milestone.candidate.as_deref().unwrap_or(id),
                    noun_count(files, "file")
                ),
                None => format!("adopted {}", milestone.candidate.as_deref().unwrap_or(id)),
            },
        ),
        MilestoneEvent::AdoptConflict => (
            "✗",
            match milestone.paths.as_slice() {
                [] => "adopt conflict".to_owned(),
                [path] => format!("adopt conflict · {path}"),
                [path, rest @ ..] => format!("adopt conflict · {path} +{} more", rest.len()),
            },
        ),
    }
}

/// The full text a milestone row opens to: an adoption's facts first, then
/// the brief, result, reason, question or landed paths, then every
/// conflicting path.
pub(crate) fn milestone_detail(milestone: &AgentMilestone) -> Vec<String> {
    let mut rows = if milestone.event == MilestoneEvent::Adopted {
        adoption_facts(milestone)
    } else {
        Vec::new()
    };
    let text = milestone.text.as_deref().map(|text| match milestone.event {
        MilestoneEvent::Finished => indent_result(text),
        _ => Cow::Borrowed(text),
    });
    rows.extend(
        text.iter()
            .flat_map(|text| text.trim_end().lines().map(str::to_owned)),
    );
    if milestone.paths.len() > 1 {
        rows.extend(milestone.paths.iter().cloned());
    }
    rows
}

/// What an adoption row knows: its candidate, the child and its template,
/// and how many files landed.
fn adoption_facts(milestone: &AgentMilestone) -> Vec<String> {
    let mut rows = Vec::with_capacity(3);
    if let Some(candidate) = &milestone.candidate {
        rows.push(format!("candidate {candidate}"));
    }
    if !milestone.agent_id.is_empty() {
        rows.push(match &milestone.template {
            Some(template) => format!("agent {} · {template}", milestone.agent_id),
            None => format!("agent {}", milestone.agent_id),
        });
    }
    if let Some(files) = milestone.files {
        rows.push(format!("{} landed", noun_count(files, "file")));
    }
    rows
}

/// A submitted result as indented lines when it is one line of compact
/// JSON, the host's cut line (`… cut at 16 KiB of 40 KiB`) kept under it.
/// A cut result is not valid JSON, so the indent follows the tokens rather
/// than parsing, and keeps the child's key order.
fn indent_result(text: &str) -> Cow<'_, str> {
    let (body, cut) = match text.rsplit_once('\n') {
        Some((body, cut)) if cut.starts_with("… cut at ") => (body, Some(cut)),
        _ => (text, None),
    };
    let body = body.trim();
    if !(body.starts_with('{') || body.starts_with('[')) || body.contains('\n') {
        return Cow::Borrowed(text);
    }
    let mut indented = indent_json(body);
    if let Some(cut) = cut {
        indented.push('\n');
        indented.push_str(cut);
    }
    Cow::Owned(indented)
}

/// Compact JSON re-indented by two spaces per level, `serde_json`'s pretty
/// layout for valid input; strings are copied as they are.
fn indent_json(json: &str) -> String {
    fn break_line(out: &mut String, depth: usize) {
        out.push('\n');
        out.extend(std::iter::repeat_n("  ", depth));
    }
    let mut out = String::with_capacity(json.len().saturating_mul(2));
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = json.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|next| next.is_whitespace()).is_some() {}
                let close = if c == '{' { '}' } else { ']' };
                if let Some(close) = chars.next_if_eq(&close) {
                    out.push(close);
                } else {
                    depth += 1;
                    break_line(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                break_line(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                break_line(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn noun_count(count: u64, singular: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {singular}s")
    }
}

/// A wait or run span in the coarsest exact unit: `45s`, `12m`, `1m30s`,
/// `2h`, `1h05m`.
#[must_use]
pub fn format_span(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0 => format_tool_elapsed(Duration::from_millis(ms)),
        1..60 => format!("{secs}s"),
        60..=3600 if secs.is_multiple_of(60) => format!("{}m", secs / 60),
        60..3600 => format!("{}m{:02}s", secs / 60, secs % 60),
        _ if secs.is_multiple_of(3600) => format!("{}h", secs / 3600),
        _ => format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// One wait item's label from its host facts (spec 02 §5.4): `head` (what it
/// waited for), the requested ceiling, and once it ended how long it actually
/// waited and what woke it.
#[must_use]
pub fn wait_label(head: String, running: bool, facts: &WaitFacts) -> String {
    let mut parts = vec![head];
    if let Some(timeout) = facts.timeout_ms {
        parts.push(format!("up to {}", format_span(timeout)));
    }
    if !running {
        let woke = facts.woke_by.clone().or_else(|| {
            facts
                .settled_agent
                .as_ref()
                .map(|id| format!("{id} finished"))
        });
        match (facts.waited_ms, woke) {
            (Some(waited), Some(woke)) => {
                parts.push(format!("woke after {} ({woke})", format_span(waited)));
            }
            (Some(waited), None) => parts.push(format!("waited {}", format_span(waited))),
            (None, Some(woke)) => parts.push(woke),
            (None, None) => {}
        }
    }
    parts.join(" · ")
}

pub(crate) enum CellItem<T> {
    Grapheme {
        value: T,
        cells: usize,
        whitespace: bool,
    },
    Break,
}

/// The sole terminal wrapping primitive. Words that fit a complete row move
/// intact; longer words split only between extended grapheme clusters.
pub(crate) fn wrap_cell_items<T>(
    items: impl IntoIterator<Item = CellItem<T>>,
    width: usize,
) -> Vec<Vec<T>> {
    let width = width.max(1);
    let mut rows = vec![Vec::new()];
    let mut used = 0usize;
    let mut word = Vec::new();
    let flush_word = |word: &mut Vec<(T, usize)>, rows: &mut Vec<Vec<T>>, used: &mut usize| {
        let word_width = word
            .iter()
            .fold(0usize, |total, (_, cells)| total.saturating_add(*cells));
        if *used > 0 && used.saturating_add(word_width) > width {
            rows.push(Vec::new());
            *used = 0;
        }
        for (value, cells) in word.drain(..) {
            if *used > 0 && cells > 0 && used.saturating_add(cells) > width {
                rows.push(Vec::new());
                *used = 0;
            }
            if let Some(row) = rows.last_mut() {
                row.push(value);
            } else {
                rows.push(vec![value]);
            }
            *used = used.saturating_add(cells);
        }
    };
    for item in items {
        match item {
            CellItem::Break => {
                flush_word(&mut word, &mut rows, &mut used);
                rows.push(Vec::new());
                used = 0;
            }
            CellItem::Grapheme {
                value,
                cells,
                whitespace,
            } => {
                if !whitespace {
                    word.push((value, cells));
                    continue;
                }
                flush_word(&mut word, &mut rows, &mut used);
                if used > 0 && cells > 0 && used.saturating_add(cells) > width {
                    rows.push(Vec::new());
                    used = 0;
                }
                if let Some(row) = rows.last_mut() {
                    row.push(value);
                } else {
                    rows.push(vec![value]);
                }
                used = used.saturating_add(cells);
            }
        }
    }
    flush_word(&mut word, &mut rows, &mut used);
    rows
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PlainRow {
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Word-aware rows with char-sequence offsets for editor geometry.
pub(crate) fn wrap_plain_rows(text: &str, width: usize) -> Vec<PlainRow> {
    let mut char_index = 0usize;
    let items = text.graphemes(true).map(|grapheme| {
        let start = char_index;
        char_index = char_index.saturating_add(grapheme.chars().count());
        if grapheme == "\n" || grapheme == "\r\n" {
            CellItem::Break
        } else {
            CellItem::Grapheme {
                value: (start, char_index, grapheme),
                cells: grapheme.width().max(1),
                whitespace: grapheme.chars().all(char::is_whitespace),
            }
        }
    });
    wrap_cell_items(items, width)
        .into_iter()
        .scan(0usize, |next_start, row| {
            let start = row.first().map_or(*next_start, |(start, _, _)| *start);
            let end = row.last().map_or(start, |(_, end, _)| *end);
            let mut following = text.chars().skip(end);
            *next_start = end
                + match (following.next(), following.next()) {
                    (Some('\r'), Some('\n')) => 2,
                    (Some('\n'), _) => 1,
                    _ => 0,
                };
            Some(PlainRow {
                start,
                end,
                text: row.into_iter().map(|(_, _, grapheme)| grapheme).collect(),
            })
        })
        .collect()
}

/// Wrap plain text to terminal-cell width without changing whitespace.
#[must_use]
pub fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    wrap_plain_rows(text, width)
        .into_iter()
        .map(|row| row.text)
        .collect()
}

/// A steer's band rows: the first wrapped at `first_width`, the rest at
/// `width`.
pub(crate) fn steer_rows(text: &str, width: usize, first_width: usize) -> Vec<String> {
    let mut rows = wrap_plain_rows(text, first_width).into_iter();
    let Some(first) = rows.next() else {
        return Vec::new();
    };
    // Only the single space a soft wrap broke on. A hard newline is not that
    // space, so the next line's indentation stays.
    let rest = rows.next().map(|next| {
        let mut rest: String = text.chars().skip(next.start).collect();
        let broke_on_space = next.start > 0
            && text
                .chars()
                .nth(next.start - 1)
                .is_some_and(|ch| ch != '\n' && ch != '\r')
            && rest.starts_with(' ');
        if broke_on_space {
            rest.remove(0);
        }
        rest
    });
    std::iter::once(first.text)
        .chain(rest.iter().flat_map(|rest| wrap_plain(rest, width)))
        .collect()
}

pub(crate) use crate::tui::markdown::RowCopy;

/// Layout metadata: for each visual line, which block index produced it (for sticky).
#[derive(Clone, Debug, Default)]
pub struct LineLayout {
    /// Visual lines as plain strings (selection-friendly).
    pub plain: Vec<String>,
    /// Block index for each visual line (`None` for blank separators).
    pub block_index: Vec<Option<usize>>,
    /// Blocks whose row count is a cheap estimate rather than a measured wrap.
    ///
    /// Their rows are blank placeholders holding geometry, never painted: the
    /// viewport promotes everything it reads to an exact measurement first.
    /// Empty for a fully measured projection.
    pub estimated: std::collections::BTreeSet<usize>,
    /// Copy metadata parallel to [`Self::plain`]: leading decoration width,
    /// unclipped code source, and language-label rows.
    pub copy: Vec<RowCopy>,
}

/// Build a line layout from blocks at `content_width` (inner width without gutters).
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn layout_blocks(blocks: &[ScrollBlock], content_width: usize) -> LineLayout {
    layout_blocks_with(blocks, content_width, RenderPrefs::default())
}

/// Plain projection under explicit §5.5 render preferences.
#[must_use]
#[cfg(any(test, feature = "test-support"))]
pub fn layout_blocks_with(
    blocks: &[ScrollBlock],
    content_width: usize,
    prefs: RenderPrefs,
) -> LineLayout {
    let plans = plan_rows(blocks, prefs);
    let mut plain = Vec::new();
    let mut block_index = Vec::new();
    let mut copy = Vec::new();
    let mut previous_drawn: Option<&ScrollBlock> = None;
    for bi in row_order(&plans) {
        let block = &blocks[bi];
        let lines = block_plain_lines(block, &plans[bi], content_width, prefs, blocks);
        if lines.is_empty() {
            continue;
        }
        if wants_separator(previous_drawn, block) {
            plain.push(String::new());
            block_index.push(None);
            copy.push(RowCopy::default());
        }
        for line in lines {
            plain.push(line);
            block_index.push(Some(bi));
            copy.push(RowCopy::default());
        }
        previous_drawn = Some(block);
    }
    LineLayout {
        plain,
        block_index,
        estimated: std::collections::BTreeSet::new(),
        copy,
    }
}

/// Whether a tool block closes with its own blank row (§5.8).
///
/// A Compose and an open verb-group are one rail, so the spacer follows the
/// last child rather than the head or a middle child. Every projection — the
/// plain export, the styled paint and the cheap estimate — reads this one
/// rule, so a Compose is the same height wherever it is measured.
#[must_use]
pub const fn tool_block_closes_with_blank(plan: &RowPlan) -> bool {
    !plan.activity_continues
}

/// Whether a blank row separates `block` from the block drawn before it.
///
/// Grok Build spacing: a blank row between turns, never before the first
/// drawn block. A tool or Compose row carries its own trailing blank, so it
/// takes a leading one only after text — an assistant paragraph, a prompt
/// band or an error — so a short thought or text line never glues to the tool
/// row beneath it. System notes stay compact.
#[must_use]
pub fn wants_separator(previous_drawn: Option<&ScrollBlock>, block: &ScrollBlock) -> bool {
    let Some(previous) = previous_drawn else {
        return false;
    };
    match block {
        ScrollBlock::System(_) | ScrollBlock::Header(_) => false,
        ScrollBlock::Tool { .. } | ScrollBlock::Item { .. } | ScrollBlock::Milestone { .. } => {
            matches!(
                previous,
                ScrollBlock::Assistant(_)
                    | ScrollBlock::User { .. }
                    | ScrollBlock::Steer { .. }
                    | ScrollBlock::Error(_)
            )
        }
        // Tool, thinking, milestone and approval blocks close on their own
        // blank row; a second one under them would double the gap.
        _ => !matches!(
            previous,
            ScrollBlock::Tool { .. }
                | ScrollBlock::Thinking { .. }
                | ScrollBlock::Approval { .. }
                | ScrollBlock::Milestone { .. }
        ),
    }
}
#[cfg(any(test, feature = "test-support"))]
fn block_plain_lines(
    block: &ScrollBlock,
    plan: &RowPlan,
    content_width: usize,
    prefs: RenderPrefs,
    blocks: &[ScrollBlock],
) -> Vec<String> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::User { text, .. } | ScrollBlock::Steer { text, .. } => {
            // Gutter wrap; sticky uses `›` — body layout uses gutter for plain export
            let w = content_width.saturating_sub(2).max(1);
            wrap_plain(text, w)
                .into_iter()
                .map(|l| format!("┃ {l}"))
                .collect()
        }
        ScrollBlock::Assistant(text) => {
            let w = content_width.saturating_sub(2).max(1);
            wrap_plain(text, w)
                .into_iter()
                .map(|line| format!("  {line}"))
                .collect()
        }
        ScrollBlock::System(text) | ScrollBlock::Error(text) | ScrollBlock::Header(text) => {
            wrap_plain(text, content_width.saturating_sub(2).max(1))
                .into_iter()
                .map(|line| format!("  {line}"))
                .collect()
        }
        ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => Vec::new(),
        ScrollBlock::Discarded { text, thinking } => {
            let w = content_width.saturating_sub(2).max(1);
            std::iter::once(format!("  {}", discarded_label(*thinking)))
                .chain(
                    wrap_plain(text, w)
                        .into_iter()
                        .map(|line| format!("  {line}")),
                )
                .collect()
        }
        ScrollBlock::Approval {
            tool_name,
            subject,
            prompt,
            resolution,
            ..
        } => format_approval_block(
            tool_name,
            subject.as_deref(),
            prompt.as_deref(),
            resolution.as_deref(),
        )
        .into_iter()
        .flat_map(|line| wrap_plain(&line, content_width))
        .collect(),
        ScrollBlock::Tool { .. }
        | ScrollBlock::Item { .. }
        | ScrollBlock::Thinking { .. }
        | ScrollBlock::Milestone { .. } => {
            let mut rows = render::block_lines_with(
                block,
                plan,
                &super::theme::Theme::dark(),
                content_width,
                prefs,
                blocks,
            );
            if rows.last().is_some_and(|row| row.to_string().is_empty()) {
                rows.pop();
            }
            rows.into_iter()
                .map(|row| row.to_string().trim_end().to_owned())
                .collect()
        }
    }
}

/// How one block participates in the folded transcript (§5.5, §5.8).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per independent fold/caret axis"
)]
pub struct RowPlan {
    pub(crate) activity_summary: Option<String>,
    pub(crate) activity_action: Option<String>,
    /// Right-aligned meta of a group summary line (exit codes, duration).
    pub(crate) summary_meta: Option<String>,
    /// Right-aligned meta of a cell's own line (its nested exit codes).
    pub(crate) activity_meta: Option<String>,
    pub(crate) activity_failed: bool,
    pub(crate) cell_failed: bool,
    pub(crate) activity_running: bool,
    /// The first call owns the fold even when preceding thoughts open a group.
    pub(crate) group_owner: Option<usize>,
    pub(crate) cell_tails: Vec<CellTail>,
    pub(crate) activity_continues: bool,
    /// The block contributes no rows: folded into a group or outside the
    /// cell's bounded outline.
    pub hidden: bool,
    /// Rows in this one's §5.5 run; `0` when the block is not a group head.
    pub group_count: usize,
    /// The group head is showing the run's name instead of its own subject.
    pub group_folded: bool,
    /// `Some(is_last)` when this row is drawn as a Compose child (§5.8 rails).
    pub compose_child: Option<bool>,
    pub(crate) compose_owner: Option<usize>,
    /// The row is an open §5.5 group head: the header stays drawn and the
    /// spacer belongs after the last member, not after the head.
    pub group_head_open: bool,
    /// `Some(is_last)` when this row is a member of an open §5.5 verb group.
    pub group_child: Option<bool>,
    /// A merged row listing its calls: the head's own call is drawn under it.
    pub members_open: bool,
}

/// Presentation-only footer attached to the final visible outline block.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CellTail {
    pub(crate) owner: usize,
    pub(crate) hidden_calls: usize,
    pub(crate) rail: &'static str,
}

/// Fold plan for every block, in block order.
///
/// Nesting comes only from host-supplied `parent_call_id`; a lineage the TUI
/// was not given produces a flat row (§5.8).
#[must_use]
pub fn plan_rows(blocks: &[ScrollBlock], prefs: RenderPrefs) -> Vec<RowPlan> {
    crate::tui::activity::plan(blocks, prefs)
}

/// Cell outlines stay under their header even when parallel cells interleave.
pub(super) fn row_order(plans: &[RowPlan]) -> Vec<usize> {
    row_order_range(plans, 0, plans.len())
}

pub(super) fn row_order_range(plans: &[RowPlan], start: usize, end: usize) -> Vec<usize> {
    let mut children = std::collections::HashMap::<usize, Vec<usize>>::new();
    for (index, plan) in plans.iter().enumerate().take(end).skip(start) {
        if let Some(owner) = plan.compose_owner {
            children.entry(owner).or_default().push(index);
        }
    }
    let mut order = Vec::with_capacity(end - start);
    let mut visited = std::collections::HashSet::new();
    let roots = (start..end).filter(|index| plans[*index].compose_owner.is_none());
    for root in roots.chain(start..end) {
        let mut stack = vec![root];
        while let Some(index) = stack.pop() {
            if !visited.insert(index) {
                continue;
            }
            order.push(index);
            if let Some(owned) = children.get(&index) {
                stack.extend(owned.iter().rev().copied());
            }
        }
    }
    order
}

/// What a click on one of a block's leading rows toggles.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowTarget {
    /// A group summary: the group's items.
    Group,
    /// A merged row: the calls it stands for.
    Merge,
    /// An item, thinking or milestone row: its own detail.
    Item,
    /// The `… N lines hidden` row of an open item: the rest of its output.
    HiddenLines,
    CellMore(usize),
    CellOutput(usize),
    CellHiddenLines(usize),
}

/// What each of a block's leading one-line rows toggles, from its plan alone
/// (no styling), so hit-testing a frame stays cheap.
#[must_use]
pub fn lead_targets(block: &ScrollBlock, plan: &RowPlan) -> Vec<RowTarget> {
    if plan.hidden {
        return Vec::new();
    }
    match block {
        ScrollBlock::Tool { kind, .. } if *kind != ActivityKind::Compact => {
            let mut targets = Vec::new();
            if plan.activity_summary.is_some() {
                targets.push(RowTarget::Group);
            }
            if !plan.group_folded {
                if plan.activity_action.is_some() && *kind != ActivityKind::Compose {
                    targets.push(RowTarget::Merge);
                    if plan.members_open {
                        targets.push(RowTarget::Item);
                    }
                } else {
                    targets.push(RowTarget::Item);
                }
            }
            targets
        }
        ScrollBlock::Thinking { text, sealed, .. } => {
            let mut targets = Vec::new();
            if plan.activity_summary.is_some() {
                targets.push(RowTarget::Group);
            }
            if !plan.group_folded && *sealed && thinking_has_detail(text) {
                targets.push(RowTarget::Item);
            }
            targets
        }
        ScrollBlock::Milestone { milestone, .. } if milestone_has_detail(milestone) => {
            vec![RowTarget::Item]
        }
        _ => Vec::new(),
    }
}

/// The indent of a thought's detail rows: under its bullet, with an open
/// group's rail continued.
#[must_use]
pub fn thinking_detail_rail(plan: &RowPlan) -> String {
    format!(
        "{}  ",
        row_indent(plan).replace("├─", "│ ").replace("└─", "  ")
    )
}

/// The rail an open item's detail rows carry: the item's own rail continued,
/// then `│` under the item's label.
#[must_use]
pub fn detail_rail(item_rail: &str) -> String {
    format!("{}  │ ", item_rail.replace("├─", "│ ").replace("└─", "  "))
}

/// The row that stands for an output window's hidden middle.
#[must_use]
pub fn hidden_lines_label(hidden: usize) -> String {
    format!("… {hidden} lines hidden ▸")
}

/// An open item's hidden-lines row is marked on its styled line, where the
/// output it stands among cannot reach: cell footer actions use the other alignment markers,
/// and an explicit left alignment paints exactly like none.
fn mark_hidden_lines(line: &mut ratatui::text::Line<'static>) {
    line.alignment = Some(ratatui::layout::Alignment::Left);
}

/// Whether a styled transcript row is an output window's hidden-lines row.
#[must_use]
pub fn is_hidden_lines_row(line: &ratatui::text::Line<'static>) -> bool {
    line.alignment == Some(ratatui::layout::Alignment::Left)
}

/// Footer actions are marked on painted lines, not inferred from output text.
#[must_use]
pub fn cell_footer_target(
    lines: &[ratatui::text::Line<'static>],
    row: usize,
    plan: &RowPlan,
    blocks: &[ScrollBlock],
) -> Option<RowTarget> {
    use ratatui::layout::Alignment;
    let expanded: Vec<_> = plan
        .cell_tails
        .iter()
        .filter(|tail| {
            matches!(
                blocks.get(tail.owner),
                Some(ScrollBlock::Tool {
                    mode: DisplayMode::Expanded,
                    ..
                })
            )
        })
        .collect();
    match lines.get(row)?.alignment? {
        Alignment::Center => {
            let more = lines[..=row]
                .iter()
                .filter(|line| line.alignment == Some(Alignment::Center))
                .count();
            let tail = plan
                .cell_tails
                .iter()
                .filter(|tail| tail.hidden_calls > 0)
                .nth(more.checked_sub(1)?)?;
            Some(RowTarget::CellMore(tail.owner))
        }
        Alignment::Right | Alignment::Left => {
            let outputs = lines[..=row]
                .iter()
                .filter(|line| line.alignment == Some(Alignment::Right))
                .count();
            let tail = expanded.get(outputs.checked_sub(1)?)?;
            if lines[row].alignment == Some(Alignment::Right) {
                Some(RowTarget::CellOutput(tail.owner))
            } else {
                Some(RowTarget::CellHiddenLines(tail.owner))
            }
        }
    }
}

/// Where the live spinner sits in a block's leading rows: `(row offset in
/// the block, glyph column)` for each row whose status glyph is the spinner
/// (spec 02 §5.4), from the block's state and plan, never from row text.
#[must_use]
pub fn spinner_cells(
    block: &ScrollBlock,
    plan: &RowPlan,
    prefs: RenderPrefs,
) -> Vec<(usize, usize)> {
    use unicode_width::UnicodeWidthStr as _;
    if plan.hidden {
        return Vec::new();
    }
    let item = |plan: &RowPlan, state: &str| {
        (tool_is_running(state) || plan.activity_running).then(|| row_indent(plan).width())
    };
    match block {
        ScrollBlock::Thinking { sealed: false, .. } if prefs.show_thinking => vec![(0, 2)],
        ScrollBlock::Tool {
            kind: ActivityKind::Compact,
            state,
            ..
        } => {
            if tool_is_running(state) {
                vec![(0, 2)]
            } else {
                Vec::new()
            }
        }
        ScrollBlock::Tool { kind, state, .. } => {
            let mut row = usize::from(plan.activity_summary.is_some());
            if plan.group_folded {
                return Vec::new();
            }
            let mut cells = Vec::new();
            cells.extend(item(plan, state).map(|column| (row, column)));
            if plan.activity_action.is_some() && *kind != ActivityKind::Compose && plan.members_open
            {
                row += 1;
                let own = RowPlan {
                    activity_action: None,
                    activity_running: false,
                    activity_failed: false,
                    activity_meta: None,
                    compose_child: Some(false),
                    ..plan.clone()
                };
                cells.extend(item(&own, state).map(|column| (row, column)));
            }
            cells
        }
        _ => Vec::new(),
    }
}

/// A unified diff body for one replacement: shared leading and trailing
/// lines as context (at most three each), the rest removed then added.
#[must_use]
pub fn line_diff(old: &str, new: &str) -> Vec<String> {
    const CONTEXT: usize = 3;
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let prefix = old.iter().zip(&new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let mut rows = vec![format!(
        "@@ -{},{} +{},{} @@",
        prefix.saturating_sub(CONTEXT) + 1,
        old.len() - prefix - suffix + prefix.min(CONTEXT) + suffix.min(CONTEXT),
        prefix.saturating_sub(CONTEXT) + 1,
        new.len() - prefix - suffix + prefix.min(CONTEXT) + suffix.min(CONTEXT),
    )];
    rows.extend(
        old[prefix.saturating_sub(CONTEXT)..prefix]
            .iter()
            .map(|line| format!(" {line}")),
    );
    rows.extend(
        old[prefix..old.len() - suffix]
            .iter()
            .map(|line| format!("-{line}")),
    );
    rows.extend(
        new[prefix..new.len() - suffix]
            .iter()
            .map(|line| format!("+{line}")),
    );
    rows.extend(
        old[old.len() - suffix..old.len() - suffix + suffix.min(CONTEXT)]
            .iter()
            .map(|line| format!(" {line}")),
    );
    rows
}

/// Head lines kept when an expanded tool body is truncated.
pub const TOOL_DETAIL_HEAD_LINES: usize = 20;
/// Tail lines kept when an expanded tool body is truncated.
pub const TOOL_DETAIL_TAIL_LINES: usize = 10;

/// Syntax label for a Compose cell's program (§5.8): `javascript` for Code
/// Mode `exec` / `exec_js` and js-ptc, `python` for `exec_py` / py-ptc.
#[must_use]
pub fn compose_program_language(name: &str) -> Option<&'static str> {
    match name {
        "exec" | "exec_js" | "wait" | "wait_js" => Some("javascript"),
        "exec_py" | "wait_py" => Some("python"),
        _ => None,
    }
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

/// Left rail for a cell's nested call or an open group member, or the plain
/// indent. A nested call inside an open group sits one rail level deeper.
#[must_use]
pub const fn row_indent(plan: &RowPlan) -> &'static str {
    match (plan.group_child, plan.compose_child) {
        (Some(false), Some(false)) => "  │   ├─  ",
        (Some(false), Some(true)) => "  │   └─  ",
        (Some(true), Some(false)) => "      ├─  ",
        (Some(true), Some(true)) => "      └─  ",
        (Some(true), None) | (None, Some(true)) => "  └─  ",
        (Some(false), None) | (None, Some(false)) => "  ├─  ",
        (None, None) => "  ",
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

/// The label over a block a replayed model request discarded.
#[must_use]
pub const fn discarded_label(thinking: bool) -> &'static str {
    if thinking {
        DISCARDED_THINKING_LABEL
    } else {
        DISCARDED_LABEL
    }
}

/// First non-empty body line, stripped of markdown decoration for a header.
#[must_use]
pub fn thinking_summary(text: &str) -> String {
    use pulldown_cmark::{Event, Parser, Tag};
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let mut plain = String::new();
        let mut block_html = false;
        for event in Parser::new(line) {
            match event {
                Event::Start(Tag::HtmlBlock) => block_html = true,
                Event::Text(text)
                | Event::Code(text)
                | Event::InlineHtml(text)
                | Event::Html(text) => {
                    plain.push_str(&text);
                }
                Event::SoftBreak | Event::HardBreak => plain.push(' '),
                _ => {}
            }
        }
        let summary = plain.split_whitespace().collect::<Vec<_>>().join(" ");
        if !block_html && !summary.is_empty() {
            return summary;
        }
    }
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim()
        .to_owned()
}

#[must_use]
pub fn thinking_has_detail(text: &str) -> bool {
    text.trim().lines().count() > 1
}

/// Body rows of a thinking block: the entire retained body when expanded.
#[must_use]
pub fn thinking_detail_rows(text: &str, mode: DisplayMode) -> Vec<String> {
    match mode {
        DisplayMode::Collapsed => Vec::new(),
        DisplayMode::Expanded => text.lines().map(str::to_owned).collect(),
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

/// Sticky user preview: last user message fully above the viewport.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StickyUserPreview {
    /// Bounded rendered prompt lines.
    pub lines: Vec<String>,
    /// First transcript row of the source prompt for click-to-jump.
    pub source_start: usize,
}
///
/// Returns up to `max_lines` plain lines of that message body (with gutter).
#[must_use]
pub fn sticky_user_lines(
    blocks: &[ScrollBlock],
    layout: &LineLayout,
    visible_start: usize,
    max_lines: usize,
    content_width: usize,
) -> Option<StickyUserPreview> {
    if visible_start == 0 || max_lines == 0 {
        return None;
    }
    // Reverse-scan from the viewport edge: O(distance to last complete user block).
    let mut i = visible_start.min(layout.block_index.len());
    let (bi, source_start) = loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        let Some(bi) = layout.block_index[i] else {
            continue;
        };
        let mut start = i;
        while start > 0 && layout.block_index[start - 1] == Some(bi) {
            start -= 1;
        }
        let mut end = i + 1;
        while end < layout.block_index.len() && layout.block_index[end] == Some(bi) {
            end += 1;
        }
        if blocks.get(bi).is_some_and(ScrollBlock::is_user)
            && start < visible_start
            && end <= visible_start
        {
            break (bi, start);
        }
        i = start;
    };
    let text = blocks.get(bi)?.body();
    let w = content_width.saturating_sub(2).max(1);
    let wrapped = wrap_plain(text, w);
    let mut lines: Vec<String> = wrapped
        .iter()
        .take(max_lines)
        .enumerate()
        .map(|(i, l)| {
            if i == 0 {
                format!("{PROMPT_PREFIX}{l}")
            } else {
                format!("  {l}")
            }
        })
        .collect();
    if wrapped.len() > max_lines
        && let Some(last) = lines.last_mut()
    {
        if last.chars().count() > 3 {
            *last = format!(
                "{}…",
                last.chars()
                    .take(last.chars().count().saturating_sub(1))
                    .collect::<String>()
            );
        } else {
            last.push('…');
        }
    }
    Some(StickyUserPreview {
        lines,
        source_start,
    })
}

/// The `(block, sub-row)` a transcript row belongs to.
///
/// A row number is only meaningful inside one projection: a resize re-wraps
/// every block and measuring an estimated block corrects the heights below it.
/// Converting to block space before either happens, and back afterwards, is
/// what keeps the operator's place.
///
/// A blank separator row resolves to the block above it, or to the block below
/// when the transcript opens on separators.
#[must_use]
pub fn layout_anchor(layout: &LineLayout, row: usize) -> Option<(usize, usize)> {
    let rows = layout.block_index.len();
    if rows == 0 {
        return None;
    }
    let row = row.min(rows - 1);
    // The separator belongs to the block it introduces, so look forward first;
    // only a row past the last block has to look back.
    let block = layout.block_index[row..]
        .iter()
        .flatten()
        .next()
        .or_else(|| layout.block_index[..row].iter().rev().flatten().next())
        .copied()?;
    let (start, end) = block_span(layout, block)?;
    let last = end.saturating_sub(start).saturating_sub(1);
    Some((block, row.saturating_sub(start).min(last)))
}

/// Rows one block occupies, counting the blank separator drawn above it.
///
/// The separator is charged to the block below so that turning a row into a
/// `(block, sub-row)` anchor and back again lands on the row it started from —
/// which is the whole contract [`layout_anchor`] exists to keep.
/// Test hook for [`block_span`]: whether a block draws any rows at all.
#[cfg(test)]
pub(crate) fn block_span_for_test(layout: &LineLayout, block: usize) -> Option<(usize, usize)> {
    block_span(layout, block)
}

pub(crate) fn resolve_layout_anchor(
    layout: &LineLayout,
    (block, sub_row): (usize, usize),
) -> usize {
    block_span(layout, block).map_or(0, |(start, end)| {
        start.saturating_add(sub_row).min(end.saturating_sub(1))
    })
}

fn block_span(layout: &LineLayout, block: usize) -> Option<(usize, usize)> {
    let first = layout
        .block_index
        .iter()
        .position(|owner| *owner == Some(block))?;
    let mut start = first;
    while start > 0 && layout.block_index[start - 1].is_none() {
        start -= 1;
    }
    let end = first
        + layout.block_index[first..]
            .iter()
            .take_while(|owner| **owner == Some(block))
            .count();
    Some((start, end))
}

mod render {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use unicode_segmentation::UnicodeSegmentation as _;
    use unicode_width::UnicodeWidthStr as _;

    use super::{LineLayout, ScrollBlock};
    use crate::tui::theme::Theme;

    fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
        let line_style = line.style;
        let items = line.spans.into_iter().flat_map(|span| {
            let style = span.style;
            span.content
                .graphemes(true)
                .map(move |grapheme| {
                    if grapheme == "\n" {
                        super::CellItem::Break
                    } else {
                        super::CellItem::Grapheme {
                            value: (grapheme.to_owned(), style),
                            cells: grapheme.width().max(1),
                            whitespace: grapheme.chars().all(char::is_whitespace),
                        }
                    }
                })
                .collect::<Vec<_>>()
        });
        super::wrap_cell_items(items, width)
            .into_iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                for (grapheme, style) in row {
                    if let Some(last) = spans.last_mut().filter(|last| last.style == style) {
                        last.content.to_mut().push_str(&grapheme);
                    } else {
                        spans.push(Span::styled(grapheme, style));
                    }
                }
                Line::from(spans).style(line_style)
            })
            .collect()
    }

    /// Wraps one line and re-lays the rail on every continuation row.
    ///
    /// A long program-source line inside an expanded Compose detail
    /// wrapped to column 0, outside the `│` rail, so the continuation read as
    /// transcript rather than as cell content. The prefix is measured out of
    /// the budget and re-drawn, so the rail is unbroken at 80 columns.
    fn wrap_styled_line_railed(
        prefix: &str,
        continuation: &str,
        prefix_style: Style,
        body: Line<'static>,
        width: usize,
    ) -> Vec<Line<'static>> {
        let gutter = prefix.width().max(continuation.width());
        let body_width = width.saturating_sub(gutter).max(1);
        let rows = wrap_styled_line(body, body_width);
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                let rail = if index == 0 { prefix } else { continuation };
                let mut spans = vec![Span::styled(rail.to_owned(), prefix_style)];
                spans.extend(row.spans);
                Line::from(spans)
            })
            .collect()
    }

    /// The selectable text of one styled row.
    ///
    /// Band and panel backgrounds are painted by padding the row with spaces;
    /// the plain projection copy and selection read must not carry that paint.
    fn plain_row(line: &Line<'static>) -> String {
        line.to_string().trim_end().to_owned()
    }

    /// Selectable text of one styled row of `block`.
    ///
    /// A prompt or steer band pads out to its send time and, for a steer, its
    /// tag. Those are paint. A copy of the row is the prompt text, gutter
    /// included so a partial selection still lines up with the painted cells.
    fn selectable_row(block: &ScrollBlock, line: &Line<'static>) -> String {
        if matches!(block, ScrollBlock::User { .. } | ScrollBlock::Steer { .. }) {
            prompt_selectable(line)
        } else {
            plain_row(line)
        }
    }

    /// Gutter and prompt text, stopping before the unstyled padding that the
    /// send time and steer tag sit on. The prompt's own spaces keep the user
    /// style, so they stay; a row with no chrome is the whole line.
    fn prompt_selectable(line: &Line<'static>) -> String {
        let mut text = String::new();
        for span in &line.spans {
            if span.style == Style::default() {
                break;
            }
            text.push_str(span.content.as_ref());
        }
        text
    }

    fn wrap_styled_lines(
        lines: impl IntoIterator<Item = Line<'static>>,
        width: usize,
    ) -> Vec<Line<'static>> {
        lines
            .into_iter()
            .flat_map(|line| wrap_styled_line(line, width))
            .collect()
    }

    /// Width-sensitive, fully styled transcript projection.
    #[derive(Clone, Debug, Default)]
    pub struct TranscriptRender {
        /// Styled visual lines, ready for viewport slicing.
        pub lines: Vec<Line<'static>>,
        /// Plain visual lines and their source blocks.
        pub layout: LineLayout,
    }

    /// Mutation known by the presentation owner since the preceding draw.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub enum TranscriptChange {
        /// No block changed; the cached projection stands as it is.
        #[default]
        Unchanged,
        /// A stream appended to the trailing assistant block and nothing else.
        TailAssistant,
        /// Blocks were added, removed, or reordered: every row index moves.
        Structural,
        /// One block's content changed in place, at this index.
        Block(usize),
    }

    impl TranscriptChange {
        /// The weakest change covering both, so a frame that saw two of them
        /// re-projects only what neither could promise on its own.
        #[must_use]
        pub const fn merge(self, next: Self) -> Self {
            match (self, next) {
                (Self::Structural, _) | (_, Self::Structural) => Self::Structural,
                (Self::Block(a), Self::Block(b)) if a != b => Self::Structural,
                (Self::Block(_), Self::TailAssistant) | (Self::TailAssistant, Self::Block(_)) => {
                    Self::Structural
                }
                (Self::Block(i), _) | (_, Self::Block(i)) => Self::Block(i),
                (Self::TailAssistant, _) | (_, Self::TailAssistant) => Self::TailAssistant,
                _ => Self::Unchanged,
            }
        }
    }

    /// Stated ceiling for the transcript projection cache.
    ///
    /// The cache holds two copies of every projected row — the styled spans a
    /// frame paints and the plain string selection and copy read — and both
    /// are counted against this number. Crossing it evicts whole block
    /// projections, furthest from the visible window first; the rows the
    /// operator can see and the trailing block a stream is appending to are
    /// never evicted. Evicted rows keep their slot, so row geometry (and with
    /// it every scroll anchor) is unchanged by eviction; the content is
    /// re-projected on demand when the viewport reaches it.
    ///
    /// This bounds what the cache *retains*. A full rebuild still projects the
    /// whole transcript once before the first sweep runs, because
    /// `render_transcript_windowed` is a single pass.
    pub const TRANSCRIPT_RENDER_CACHE_BYTES: usize = 4 * 1024 * 1024;

    /// Byte mark a sweep evicts down to once the ceiling is crossed.
    ///
    /// The gap below the ceiling is hysteresis: sweeping to exactly the
    /// ceiling would re-sweep on the very next appended row.
    const TRANSCRIPT_RENDER_CACHE_LOW_WATER: usize = TRANSCRIPT_RENDER_CACHE_BYTES / 2;

    /// Budget slice above which one block's own projection is *oversized*.
    ///
    /// Such a block goes on a side-list and is evicted ahead of every ordinary
    /// block, so a single huge paste cannot repeatedly push the ordinary
    /// neighbourhood of the viewport out and thrash the cache.
    const OVERSIZED_BLOCK_PROJECTION_BYTES: usize = TRANSCRIPT_RENDER_CACHE_BYTES / 8;

    /// Bytes one projected row retains: styled spans, the plain copy, and an
    /// unclipped code source when the row carries one.
    fn row_bytes(line: &Line<'static>, plain: &str, copy: &super::RowCopy) -> usize {
        plain.len()
            + line
                .spans
                .iter()
                .map(|span| span.content.len())
                .sum::<usize>()
            + copy.code.as_ref().map_or(0, String::len)
    }

    /// Contiguous `(block, start, end)` row ranges of a projection, in row
    /// order. Blank separator rows carry no block and are skipped.
    fn block_ranges(layout: &LineLayout) -> Vec<(usize, usize, usize)> {
        let mut out: Vec<(usize, usize, usize)> = Vec::new();
        for (row, index) in layout.block_index.iter().enumerate() {
            let Some(block) = *index else { continue };
            match out.last_mut() {
                Some(last) if last.0 == block && last.2 == row => last.2 = row + 1,
                _ => out.push((block, row, row + 1)),
            }
        }
        out
    }

    /// Blocks below which the whole transcript is measured in one exact pass.
    ///
    /// Measuring this many costs well under a millisecond; estimating them
    /// would only buy a promotion round on the operator's first scroll.
    const EXACT_LAYOUT_BLOCKS: usize = 512;

    /// Trailing blocks always measured exactly.
    ///
    /// The streaming tail is where every delta lands and where the viewport
    /// sits by default, so it is never an estimate: the bottom of the
    /// transcript cannot move under a promotion.
    const EXACT_TAIL_BLOCKS: usize = 64;

    /// Blocks kept exactly measured at once.
    ///
    /// The viewport and its overscan are a few dozen blocks; the rest of this
    /// budget is scrollback the operator already visited, kept exact so
    /// scrolling back over it costs nothing. Past the budget the blocks
    /// furthest from the view fall back to estimates, which bounds what a
    /// resize has to re-measure however far the operator has scrolled.
    const MAX_EXACT_BLOCKS: usize = 1_024;

    /// Promotion rounds [`TranscriptRenderCache::settle_window`] runs.
    ///
    /// One round measures the estimated blocks the window currently reads.
    /// Correcting their heights can pull a further block into the window, so
    /// it iterates; two rounds settle every realistic case and the bound only
    /// stops a pathological one from looping.
    const SETTLE_ROUNDS: usize = 4;

    /// System rows §5.1 hides outright, so they contribute no visual rows.
    ///
    /// Stated once because both the exact projection and the row-count
    /// estimate have to agree on whether a block is drawn at all: a block the
    /// estimate gives rows and the measurement gives none would shift every
    /// row below it the first time the viewport reached it.
    fn system_row_is_hidden(text: &str) -> bool {
        let trimmed = text.trim();
        trimmed.is_empty()
            || trimmed.starts_with("status: session=")
            || trimmed.starts_with("keybinds:")
    }

    /// Rows `text` plausibly wraps to at `width`, from byte length alone.
    ///
    /// No wrapping and no allocation: this is the whole point of the estimate.
    /// Byte length overcounts multi-byte text, which errs toward a taller
    /// transcript — the harmless direction, because the operator can still
    /// reach every row and the measurement that follows only shortens it.
    fn estimate_wrapped_rows(text: &str, width: usize) -> usize {
        let width = width.max(1);
        text.split('\n')
            .map(|line| line.len().div_ceil(width).max(1))
            .fold(0usize, usize::saturating_add)
            .max(1)
    }

    /// Test hook for the estimator's only load-bearing property.
    ///
    /// The estimate must agree with [`block_lines`] on whether a block draws
    /// any rows at all; everything else about it is deliberately approximate.
    #[cfg(test)]
    pub(crate) fn estimate_rows_for_test(
        block: &ScrollBlock,
        content_width: usize,
        prefs: super::RenderPrefs,
    ) -> usize {
        estimate_block_rows(
            block,
            &super::RowPlan::default(),
            content_width,
            prefs,
            std::slice::from_ref(block),
        )
    }

    /// Cheap row-count estimate for one block's styled projection.
    ///
    /// Agrees with [`block_lines`] on the only thing geometry correctness
    /// depends on — whether the block draws any rows at all — and is
    /// deliberately approximate about how many.
    pub(super) fn estimate_block_rows(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        content_width: usize,
        prefs: super::RenderPrefs,
        blocks: &[ScrollBlock],
    ) -> usize {
        if plan.hidden {
            return 0;
        }
        let body_width = content_width.saturating_sub(2);
        match block {
            // The markdown projection pops trailing empty lines, so an empty
            // or whitespace assistant block — an empty first delta produces one
            // — draws nothing. Estimating a row for it broke the one property
            // this estimate has to hold: a block the anchor lands on vanished
            // when it was measured, and the viewport fell to the tail.
            ScrollBlock::Assistant(text) if text.trim().is_empty() => 0,
            ScrollBlock::User { text, .. }
            | ScrollBlock::Steer { text, .. }
            | ScrollBlock::Assistant(text)
            | ScrollBlock::Error(text)
            | ScrollBlock::Header(text) => estimate_wrapped_rows(text, body_width),
            ScrollBlock::Milestone { milestone, open } => {
                let detail = if *open {
                    super::milestone_detail(milestone)
                        .iter()
                        .map(|row| estimate_wrapped_rows(row, content_width.saturating_sub(6)))
                        .fold(0usize, usize::saturating_add)
                } else {
                    0
                };
                1usize
                    .saturating_add(detail)
                    .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
            }
            ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => 0,
            ScrollBlock::Discarded { text, .. } => {
                estimate_wrapped_rows(text, body_width).saturating_add(1)
            }
            ScrollBlock::System(text) => {
                if system_row_is_hidden(text) {
                    0
                } else {
                    estimate_wrapped_rows(text, body_width)
                }
            }
            ScrollBlock::Item { .. } => 1 + usize::from(super::tool_block_closes_with_blank(plan)),
            // The reasoning, approval, and tool projections all close on one
            // blank row, which the estimate has to carry too.
            ScrollBlock::Thinking {
                text, sealed, mode, ..
            } => {
                if !prefs.show_thinking
                    || (*sealed && text.trim().is_empty() && plan.activity_summary.is_none())
                {
                    return 0;
                }
                let detail = if mode.shows_detail() && !plan.group_folded {
                    let rail = super::thinking_detail_rail(plan).width();
                    estimate_wrapped_rows(text, content_width.saturating_sub(rail))
                } else {
                    0
                };
                detail
                    .saturating_add(
                        1 + usize::from(plan.activity_summary.is_some() && !plan.group_folded),
                    )
                    .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
            }
            ScrollBlock::Approval {
                subject,
                prompt,
                resolution,
                ..
            } => [subject, prompt, resolution]
                .into_iter()
                .flatten()
                .map(|text| estimate_wrapped_rows(text, content_width))
                .fold(2usize, usize::saturating_add),
            ScrollBlock::Tool {
                kind,
                name,
                state,
                subject,
                detail,
                mode,
                ..
            } => {
                let source = if plan.group_folded {
                    0
                } else {
                    plan.cell_tails
                        .iter()
                        .map(|tail| estimate_cell_footer(tail, blocks, content_width))
                        .sum::<usize>()
                };
                let header = usize::from(plan.activity_summary.is_some());
                let item_detail = if plan.group_folded
                    || matches!(
                        kind,
                        super::ActivityKind::Compact | super::ActivityKind::Compose
                    ) {
                    0
                } else {
                    usize::from(plan.members_open).saturating_add(match mode {
                        super::DisplayMode::Expanded => detail
                            .as_deref()
                            .map_or(0, |text| text.lines().count().min(32))
                            .saturating_add(2),
                        super::DisplayMode::Collapsed => 0,
                    })
                };
                let action = if plan.group_folded {
                    0
                } else if *kind == super::ActivityKind::Compact {
                    let action =
                        crate::tui::activity::action(name, state, *kind, subject.as_deref(), plan);
                    estimate_wrapped_rows(
                        action.trim_start_matches(super::row_indent(plan)),
                        content_width.saturating_sub(4),
                    )
                } else {
                    1
                };
                header
                    .saturating_add(action)
                    .saturating_add(item_detail)
                    .saturating_add(source)
                    .saturating_add(usize::from(super::tool_block_closes_with_blank(plan)))
            }
        }
    }

    fn estimate_cell_footer(tail: &super::CellTail, blocks: &[ScrollBlock], width: usize) -> usize {
        let more = usize::from(tail.hidden_calls > 0);
        let Some(ScrollBlock::Tool {
            name,
            kind,
            state,
            subject,
            detail,
            mode: super::DisplayMode::Expanded,
            view,
            ..
        }) = blocks.get(tail.owner)
        else {
            return more;
        };
        let source = detail
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map_or(0, |s| s.lines().count() + 2);
        let output = if view.output_open {
            let facts = ItemFacts {
                name,
                kind: *kind,
                state,
                subject: subject.as_deref(),
                detail: detail.as_deref(),
                view,
            };
            let text = detail_text(&facts);
            let output = text.output.trim_end_matches('\n');
            if output.trim().is_empty() {
                usize::from(text.preview)
            } else {
                let count = output.split('\n').count();
                let head = super::TOOL_DETAIL_HEAD_LINES;
                let tail_lines = super::TOOL_DETAIL_TAIL_LINES;
                let windowed = !view.full_output && count > head + tail_lines + 1;
                let body_width = width.saturating_sub(super::detail_rail(tail.rail).width() + 2);
                let rows = output
                    .split('\n')
                    .enumerate()
                    .filter(|(index, _)| !windowed || *index < head || *index >= count - tail_lines)
                    .map(|(_, line)| estimate_wrapped_rows(line, body_width))
                    .sum::<usize>();
                usize::from(text.preview)
                    + 1
                    + rows
                    + usize::from(windowed)
                    + usize::from(text.output_truncated)
            }
        } else {
            0
        };
        more + source + 1 + output
    }

    /// Width/theme cache with a safe trailing-assistant splice path.
    ///
    /// Bounded by [`TRANSCRIPT_RENDER_CACHE_BYTES`]; see that constant for the
    /// eviction rule.
    #[derive(Clone, Debug, Default)]
    pub struct TranscriptRenderCache {
        revision: Option<u64>,
        plans_revision: Option<u64>,
        plans: Vec<super::RowPlan>,
        width: usize,
        theme: Option<Theme>,
        prefs: super::RenderPrefs,
        block_count: usize,
        tail_row_start: Option<usize>,
        render: TranscriptRender,
        /// Bytes the projection currently retains (styled plus plain).
        projected_bytes: usize,
        /// Blocks whose rows are blanked and must be re-projected on demand.
        evicted: std::collections::BTreeSet<usize>,
        /// Side-list of blocks larger than [`OVERSIZED_BLOCK_PROJECTION_BYTES`].
        oversized: std::collections::BTreeSet<usize>,
        /// Row window the last paint showed; never evicted.
        visible_rows: Option<(usize, usize)>,
        /// Blocks promoted to an exact measurement, newest window first.
        ///
        /// Bounded by [`MAX_EXACT_BLOCKS`]; the trailing tail band and a short
        /// transcript are exact without being listed here.
        exact_blocks: std::collections::BTreeSet<usize>,
        /// Every block is measured exactly because selection or copy asked.
        exact_all: bool,
        /// The streaming assistant tail's settled markdown prefix.
        tail_markdown: crate::tui::markdown::StreamingMarkdown,
        /// Where the tail's settled-prefix rows sit, while they are current.
        tail_settled: Option<SettledTail>,
        #[cfg(test)]
        rebuilds: usize,
        #[cfg(test)]
        full_rows: usize,
        #[cfg(test)]
        tail_rows: usize,
        #[cfg(test)]
        evictions: usize,
        #[cfg(test)]
        reprojections: usize,
        #[cfg(test)]
        promotions: usize,
    }

    /// The rows a streaming assistant tail's settled prefix occupies.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct SettledTail {
        block: usize,
        start: usize,
        /// Settled markdown rows these projected rows came from.
        lines: usize,
        rows: usize,
        bytes: usize,
    }

    impl TranscriptRenderCache {
        /// Complete cached plain projection.
        #[cfg(any(test, feature = "test-support"))]
        #[must_use]
        pub fn plain(&self) -> &[String] {
            &self.render.layout.plain
        }

        /// The cached projection as it stands, without touching the cache.
        #[must_use]
        pub const fn render(&self) -> &TranscriptRender {
            &self.render
        }

        /// Cached visual layout used to pin scroll anchors across rebuilds.
        #[must_use]
        pub fn layout(&self) -> &LineLayout {
            &self.render.layout
        }

        pub(crate) fn row_plans(&self) -> &[super::RowPlan] {
            &self.plans
        }

        /// Rows whose identity is proven unchanged by the pending mutation.
        #[must_use]
        pub fn stable_prefix_rows(&self, change: TranscriptChange) -> Option<usize> {
            match change {
                TranscriptChange::Unchanged => Some(self.render.lines.len()),
                TranscriptChange::TailAssistant => Some(match self.tail_row_start {
                    Some(start) => start,
                    None => self.render.lines.len(),
                }),
                TranscriptChange::Block(idx) => self
                    .render
                    .layout
                    .block_index
                    .iter()
                    .position(|row| *row == Some(idx)),
                TranscriptChange::Structural => None,
            }
        }

        /// Return the current projection, rebuilding only when content or width changed.
        pub fn get(
            &mut self,
            revision: u64,
            blocks: &[ScrollBlock],
            theme: &Theme,
            content_width: usize,
            change: TranscriptChange,
            prefs: super::RenderPrefs,
        ) -> &TranscriptRender {
            if self.plans_revision != Some(revision) || self.prefs != prefs {
                self.plans = super::plan_rows(blocks, prefs);
                self.plans_revision = Some(revision);
            }
            if self.revision == Some(revision)
                && self.width == content_width
                && self.theme.as_ref() == Some(theme)
                && self.prefs == prefs
            {
                return &self.render;
            }
            let width_theme_match = self.width == content_width
                && self.theme.as_ref() == Some(theme)
                && self.prefs == prefs;
            let spliced = width_theme_match
                && if let TranscriptChange::Block(idx) = change {
                    idx < blocks.len()
                        && !self
                            .plans
                            .iter()
                            .any(|plan| plan.cell_tails.iter().any(|tail| tail.owner == idx))
                        && self.splice_block(blocks, theme, content_width, idx, prefs)
                } else {
                    false
                };
            if !spliced {
                let can_splice_tail = change == TranscriptChange::TailAssistant
                    && width_theme_match
                    && matches!(blocks.last(), Some(ScrollBlock::Assistant(_)))
                    && (self.block_count == blocks.len()
                        || (self.block_count.saturating_add(1) == blocks.len()
                            && !matches!(
                                blocks.get(blocks.len().saturating_sub(2)),
                                Some(ScrollBlock::Tool { .. })
                            )))
                    && (self.block_count != blocks.len() || self.tail_row_start.is_some());
                if can_splice_tail {
                    self.splice_tail(blocks, theme, content_width, prefs);
                } else {
                    let exact = self.exact_set(blocks.len());
                    self.render = render_transcript_windowed(
                        blocks,
                        theme,
                        content_width,
                        prefs,
                        &self.plans,
                        exact.as_ref(),
                    );
                    self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
                    self.tail_settled = None;
                    self.recount_projection();
                    #[cfg(test)]
                    {
                        self.rebuilds += 1;
                        self.full_rows += self.render.lines.len();
                    }
                }
            }
            self.revision = Some(revision);
            self.width = content_width;
            self.theme = Some(theme.clone());
            self.prefs = prefs;
            self.block_count = blocks.len();
            self.enforce_byte_ceiling();
            &self.render
        }

        /// Measure exactly everything the viewport reads, and report the row
        /// the anchor now sits on.
        ///
        /// The anchor is `(block, sub-row)`, never a row number: promoting a
        /// block corrects its height and with it every row index below it, and
        /// only a block-space anchor survives that — which is the same reason
        /// it survives the re-wrap a resize forces. Promotion rebuilds, and a
        /// corrected height can pull a further block into the window, so this
        /// iterates to a fixed point.
        ///
        /// `rows` is the viewport height; a band of the same size above and
        /// below is promoted with it, so a scroll of up to one screen in
        /// either direction lands on rows that were already measured.
        pub fn settle_window(
            &mut self,
            blocks: &[ScrollBlock],
            anchor: Option<(usize, usize)>,
            rows: usize,
        ) -> usize {
            let rows = rows.max(1);
            for _ in 0..SETTLE_ROUNDS {
                let start = self.anchor_row(anchor, rows);
                if self.render.layout.estimated.is_empty() {
                    return start;
                }
                let low = start.saturating_sub(rows);
                let high = start.saturating_add(rows.saturating_mul(2));
                let wanted: Vec<usize> = block_ranges(&self.render.layout)
                    .into_iter()
                    .filter(|(block, first, last)| {
                        *first < high && *last > low && self.render.layout.estimated.contains(block)
                    })
                    .map(|(block, _, _)| block)
                    .collect();
                if wanted.is_empty() {
                    return start;
                }
                self.exact_blocks.extend(wanted);
                self.trim_exact_blocks(blocks.len(), anchor);
                self.rebuild(blocks);
            }
            self.anchor_row(anchor, rows)
        }

        /// Rebases every block-indexed set after retention evicted head blocks.
        ///
        /// Retention drains from the front, so a block that survives moves down
        /// by a fixed count and a block below `dropped_below` is gone. Without
        /// this the measured set, the eviction list and the oversized side-list
        /// went on naming indices that now belong to different blocks, so the
        /// cache went stale at its edges exactly at the retention cap
        /// (render budget).
        pub fn shift_blocks(&mut self, dropped_below: usize, shift: usize) {
            if dropped_below == 0 && shift == 0 {
                return;
            }
            let rebase = |set: &std::collections::BTreeSet<usize>| {
                set.iter()
                    .filter(|block| **block >= dropped_below)
                    .filter_map(|block| block.checked_sub(shift))
                    .collect()
            };
            self.exact_blocks = rebase(&self.exact_blocks);
            self.evicted = rebase(&self.evicted);
            self.oversized = rebase(&self.oversized);
            self.tail_settled = None;
            // Row numbers moved with the blocks; the next paint recomputes the
            // window it must not evict.
            self.visible_rows = None;
        }

        /// Demand — or release — an exactly measured projection.
        ///
        /// Selection carries content rows from one frame to the next and copy
        /// reads them all, so while either is live nothing may be an estimate.
        /// Set this before asking for the projection, so the rebuild it may
        /// trigger measures everything in the one pass. Releasing it leaves
        /// the projection in hand alone; the next rebuild is windowed again.
        pub const fn require_exact(&mut self, exact: bool) {
            self.exact_all = exact;
        }

        /// Row the anchor occupies in the projection as it now stands.
        ///
        /// Falls back to the tail when there is no anchor or the anchored
        /// block has gone, which is what following the stream wants anyway.
        fn anchor_row(&self, anchor: Option<(usize, usize)>, rows: usize) -> usize {
            let bottom = self.render.lines.len().saturating_sub(rows);
            let Some((block, sub_row)) = anchor else {
                return bottom;
            };
            let Some((start, end)) = super::block_span(&self.render.layout, block) else {
                // The anchored block draws no rows any more — it was measured
                // to nothing, or retention dropped it. Falling to the tail
                // threw the operator out of the passage they were reading; the
                // next block that still draws rows is where that place is now.
                return block_ranges(&self.render.layout)
                    .into_iter()
                    .find(|(candidate, _, _)| *candidate > block)
                    .map_or(bottom, |(_, first, _)| first.min(bottom));
            };
            start
                .saturating_add(sub_row.min(end.saturating_sub(start).saturating_sub(1)))
                .min(bottom)
        }

        /// The blocks a projection must measure exactly, or `None` when every
        /// block must be.
        fn exact_set(&self, len: usize) -> Option<std::collections::BTreeSet<usize>> {
            if self.exact_all || len <= EXACT_LAYOUT_BLOCKS {
                return None;
            }
            let mut exact = self.exact_blocks.clone();
            exact.extend(len.saturating_sub(EXACT_TAIL_BLOCKS)..len);
            Some(exact)
        }

        /// Drop the exact measurements furthest from the view once the budget
        /// is spent.
        ///
        /// A demoted block reverts to an estimate, which moves the rows below
        /// it — never the anchored row, which is resolved in block space after
        /// every rebuild.
        fn trim_exact_blocks(&mut self, len: usize, anchor: Option<(usize, usize)>) {
            if self.exact_blocks.len() <= MAX_EXACT_BLOCKS {
                return;
            }
            let centre = anchor.map_or(len.saturating_sub(1), |(block, _)| block);
            let mut kept: Vec<usize> = self.exact_blocks.iter().copied().collect();
            kept.sort_by_key(|block| block.abs_diff(centre));
            kept.truncate(MAX_EXACT_BLOCKS);
            self.exact_blocks = kept.into_iter().collect();
        }

        /// Re-project at the width, theme and preferences already in hand.
        fn rebuild(&mut self, blocks: &[ScrollBlock]) {
            let Some(theme) = self.theme.clone() else {
                return;
            };
            let exact = self.exact_set(blocks.len());
            self.render = render_transcript_windowed(
                blocks,
                &theme,
                self.width,
                self.prefs,
                &self.plans,
                exact.as_ref(),
            );
            self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
            self.tail_settled = None;
            self.block_count = blocks.len();
            self.recount_projection();
            #[cfg(test)]
            {
                self.promotions += 1;
            }
        }

        /// Re-project every evicted block that intersects rows `start..end`.
        ///
        /// The paint path calls this once the viewport rows are known, so the
        /// operator never sees a row eviction blanked. Re-projection uses the
        /// width, theme, and preferences the cache was last filled at, so it
        /// reproduces exactly the rows that were evicted.
        pub fn ensure_rows(&mut self, blocks: &[ScrollBlock], start: usize, end: usize) {
            self.visible_rows = Some((start, end));
            if !self.evicted.is_empty() {
                let wanted: Vec<usize> = block_ranges(&self.render.layout)
                    .into_iter()
                    .filter(|(block, first, last)| {
                        *first < end && *last > start && self.evicted.contains(block)
                    })
                    .map(|(block, _, _)| block)
                    .collect();
                self.reproject(blocks, &wanted);
            }
            self.enforce_byte_ceiling();
        }

        /// Measure and project every block.
        ///
        /// Transcript copy reads the complete plain projection and selection
        /// carries row identities across frames, so neither may run against
        /// estimated geometry or the blanks eviction leaves behind. Windowed
        /// rebuilds resume at the next [`Self::relax_exact`]; the byte ceiling
        /// is re-applied by the next paint.
        pub fn materialize_all(&mut self, blocks: &[ScrollBlock]) {
            self.exact_all = true;
            if !self.render.layout.estimated.is_empty() {
                self.rebuild(blocks);
                return;
            }
            if self.evicted.is_empty() {
                return;
            }
            let wanted: Vec<usize> = self.evicted.iter().copied().collect();
            self.reproject(blocks, &wanted);
        }

        /// Bytes the projection retains right now (styled plus plain).
        #[cfg(test)]
        #[must_use]
        pub const fn projected_bytes(&self) -> usize {
            self.projected_bytes
        }

        /// Blocks currently evicted, in block order.
        #[cfg(test)]
        #[must_use]
        pub fn evicted_blocks(&self) -> Vec<usize> {
            self.evicted.iter().copied().collect()
        }

        /// Blocks on the oversized side-list, in block order.
        #[cfg(test)]
        #[must_use]
        pub fn oversized_blocks(&self) -> Vec<usize> {
            self.oversized.iter().copied().collect()
        }

        /// `(evictions, re-projections)` performed so far.
        #[cfg(test)]
        #[must_use]
        pub const fn cache_traffic(&self) -> (usize, usize) {
            (self.evictions, self.reprojections)
        }

        /// Blocks still carrying an estimated row count, in block order.
        #[cfg(test)]
        #[must_use]
        pub fn estimated_blocks(&self) -> Vec<usize> {
            self.render.layout.estimated.iter().copied().collect()
        }

        /// Promotion rebuilds performed so far.
        #[cfg(test)]
        #[must_use]
        pub const fn promotions(&self) -> usize {
            self.promotions
        }

        /// Rebuild the byte account and the oversized side-list from scratch.
        fn recount_projection(&mut self) {
            self.evicted.clear();
            self.oversized.clear();
            self.projected_bytes = self
                .render
                .lines
                .iter()
                .zip(&self.render.layout.plain)
                .zip(&self.render.layout.copy)
                .map(|((line, plain), copy)| row_bytes(line, plain, copy))
                .sum();
            for (block, start, end) in block_ranges(&self.render.layout) {
                let bytes = self.range_bytes(start, end);
                if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
                    self.oversized.insert(block);
                }
            }
        }

        fn range_bytes(&self, start: usize, end: usize) -> usize {
            (start..end)
                .filter_map(|row| {
                    let line = self.render.lines.get(row)?;
                    let plain = self.render.layout.plain.get(row)?;
                    let copy = self.render.layout.copy.get(row)?;
                    Some(row_bytes(line, plain, copy))
                })
                .sum()
        }

        /// Account one block's projection after its rows changed in place.
        fn note_block_bytes(&mut self, block: usize, start: usize, end: usize) {
            let bytes = self.range_bytes(start, end);
            if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
                self.oversized.insert(block);
            } else {
                self.oversized.remove(&block);
            }
        }

        /// Evict projections until the account is back under the low-water
        /// mark, oversized blocks first and then furthest from the visible
        /// window. Never touches the visible rows or the trailing block.
        fn enforce_byte_ceiling(&mut self) {
            if self.projected_bytes <= TRANSCRIPT_RENDER_CACHE_BYTES {
                return;
            }
            let total = self.render.lines.len();
            // With no painted window yet the product is following the tail, so
            // the last row is what must survive.
            let (visible_start, visible_end) = self
                .visible_rows
                .unwrap_or((total.saturating_sub(1), total));
            let tail_block = self.block_count.saturating_sub(1);
            let mut candidates: Vec<(bool, usize, usize, usize, usize)> =
                block_ranges(&self.render.layout)
                    .into_iter()
                    .filter(|(block, start, end)| {
                        // An estimated block holds blank rows already: there
                        // are no bytes in it to reclaim, and evicting it would
                        // lose the fact that it still has to be measured.
                        let empty = self.evicted.contains(block)
                            || self.render.layout.estimated.contains(block);
                        let protected =
                            *block == tail_block || (*start < visible_end && *end > visible_start);
                        !(empty || protected)
                    })
                    .map(|(block, start, end)| {
                        let distance = if end <= visible_start {
                            visible_start - end
                        } else {
                            start.saturating_sub(visible_end)
                        };
                        (self.oversized.contains(&block), distance, block, start, end)
                    })
                    .collect();
            candidates.sort_by(|left, right| {
                right
                    .0
                    .cmp(&left.0)
                    .then_with(|| right.1.cmp(&left.1))
                    .then_with(|| left.2.cmp(&right.2))
            });
            for (_, _, block, start, end) in candidates {
                if self.projected_bytes <= TRANSCRIPT_RENDER_CACHE_LOW_WATER {
                    break;
                }
                for row in start..end {
                    let Some(line) = self.render.lines.get_mut(row) else {
                        continue;
                    };
                    let Some(plain) = self.render.layout.plain.get_mut(row) else {
                        continue;
                    };
                    let Some(copy) = self.render.layout.copy.get_mut(row) else {
                        continue;
                    };
                    self.projected_bytes = self
                        .projected_bytes
                        .saturating_sub(row_bytes(line, plain, copy));
                    // Replace rather than clear: clearing would keep the
                    // allocation and evict nothing the allocator can reuse.
                    *line = Line::default();
                    *plain = String::new();
                    *copy = super::RowCopy::default();
                }
                self.evicted.insert(block);
                #[cfg(test)]
                {
                    self.evictions += 1;
                }
            }
        }

        /// Re-render `wanted` evicted blocks back into their existing rows.
        ///
        /// Rendering is deterministic at a fixed width, theme, and preference
        /// set, so a re-projected block occupies exactly the rows it left
        /// behind. A row-count mismatch would move every anchor below it, so
        /// it is answered by dropping the cache instead of splicing.
        fn reproject(&mut self, blocks: &[ScrollBlock], wanted: &[usize]) {
            let Some(theme) = self.theme.clone() else {
                return;
            };
            if wanted.is_empty() {
                return;
            }
            let requested: std::collections::BTreeSet<usize> = wanted.iter().copied().collect();
            let ranges: Vec<(usize, usize, usize)> = block_ranges(&self.render.layout)
                .into_iter()
                .filter(|(block, _, _)| requested.contains(block))
                .collect();
            // Every requested block leaves the evicted set: either it is
            // projected below, or it no longer has rows to project into.
            self.evicted.retain(|block| !requested.contains(block));
            for (block, start, end) in ranges {
                let (Some(source), Some(plan)) = (blocks.get(block), self.plans.get(block)) else {
                    continue;
                };
                let (styled, copy) =
                    block_rows(source, plan, &theme, self.width, self.prefs, blocks);
                if styled.len() != end - start || copy.len() != styled.len() {
                    // Dropping the cached revision forces the next `get` to
                    // rebuild rather than paint rows that no longer line up.
                    self.revision = None;
                    continue;
                }
                for (offset, (line, copy)) in styled.into_iter().zip(copy).enumerate() {
                    let row = start + offset;
                    let text = selectable_row(source, &line);
                    self.projected_bytes = self
                        .projected_bytes
                        .saturating_add(row_bytes(&line, &text, &copy));
                    self.render.lines[row] = line;
                    self.render.layout.plain[row] = text;
                    self.render.layout.copy[row] = copy;
                }
                self.note_block_bytes(block, start, end);
                #[cfg(test)]
                {
                    self.reprojections += 1;
                }
            }
        }

        fn splice_block(
            &mut self,
            blocks: &[ScrollBlock],
            theme: &Theme,
            content_width: usize,
            idx: usize,
            prefs: super::RenderPrefs,
        ) -> bool {
            let Some((start, end)) = block_row_range(&self.render.layout, idx) else {
                return false;
            };
            let (styled, copy) = block_rows(
                &blocks[idx],
                &self.plans[idx],
                theme,
                content_width,
                prefs,
                blocks,
            );
            // A row that folds away is a structural change, not a splice.
            if styled.is_empty() {
                return false;
            }
            self.tail_settled = None;
            let plain: Vec<String> = styled
                .iter()
                .map(|line| selectable_row(&blocks[idx], line))
                .collect();
            let old_len = end - start;
            let new_len = styled.len();
            self.projected_bytes = self
                .projected_bytes
                .saturating_sub(self.range_bytes(start, end));
            self.render.lines.splice(start..end, styled);
            self.render.layout.plain.splice(start..end, plain);
            self.render.layout.copy.splice(start..end, copy);
            self.render
                .layout
                .block_index
                .splice(start..end, std::iter::repeat_n(Some(idx), new_len));
            if let Some(tail_start) = self.tail_row_start {
                if tail_start >= end {
                    self.tail_row_start =
                        Some(tail_start.saturating_sub(old_len).saturating_add(new_len));
                } else if tail_start >= start {
                    self.tail_row_start = trailing_assistant_row_start(&self.render, blocks);
                }
            }
            self.projected_bytes = self
                .projected_bytes
                .saturating_add(self.range_bytes(start, start + new_len));
            self.evicted.remove(&idx);
            self.render.layout.estimated.remove(&idx);
            self.exact_blocks.insert(idx);
            self.note_block_bytes(idx, start, start + new_len);
            #[cfg(test)]
            {
                self.tail_rows += new_len;
            }
            true
        }

        fn splice_tail(
            &mut self,
            blocks: &[ScrollBlock],
            theme: &Theme,
            content_width: usize,
            prefs: super::RenderPrefs,
        ) {
            let tail_index = blocks.len() - 1;
            let row_start = self.tail_row_start.unwrap_or(self.render.lines.len());
            let streamed = match &blocks[tail_index] {
                ScrollBlock::Assistant(text) => {
                    let streamed = crate::tui::markdown::render_markdown_streaming(
                        text,
                        assistant_markdown_width(content_width),
                        &assistant_markdown_theme(theme),
                        &mut self.tail_markdown,
                    );
                    let held = self.tail_settled.filter(|settled| {
                        streamed.settled_kept >= settled.lines
                            && self.block_count == blocks.len()
                            && settled.block == tail_index
                            && Some(settled.start) == self.tail_row_start
                            && settled.start + settled.rows <= self.render.lines.len()
                    });
                    let from = held.map_or(0, |settled| settled.lines);
                    let fresh_lines = streamed.settled[from..].to_vec();
                    let fresh_copy = streamed.settled_copy[from..].to_vec();
                    Some((
                        held,
                        fresh_lines,
                        fresh_copy,
                        streamed.rest,
                        streamed.rest_copy,
                    ))
                }
                _ => None,
            };
            // The settled rows already projected stay in place: only newly
            // settled rows and the unsettled rest are spliced after them.
            if let Some((Some(mut settled), fresh, fresh_copy, rest, rest_copy)) = streamed {
                let keep = settled.start + settled.rows;
                self.projected_bytes = self
                    .projected_bytes
                    .saturating_sub(self.range_bytes(keep, self.render.lines.len()));
                self.render.lines.truncate(keep);
                self.render.layout.plain.truncate(keep);
                self.render.layout.block_index.truncate(keep);
                self.render.layout.copy.truncate(keep);
                settled.lines += fresh.len();
                let fresh = assistant_rows(fresh, fresh_copy, theme, content_width);
                settled.rows += fresh.0.len();
                self.push_tail_rows(tail_index, fresh.0, fresh.1);
                let fresh_bytes = self.range_bytes(keep, self.render.lines.len());
                settled.bytes = settled.bytes.saturating_add(fresh_bytes);
                let rest_start = self.render.lines.len();
                let rest = assistant_rows(rest, rest_copy, theme, content_width);
                self.push_tail_rows(tail_index, rest.0, rest.1);
                let rest_bytes = self.range_bytes(rest_start, self.render.lines.len());
                self.projected_bytes = self
                    .projected_bytes
                    .saturating_add(fresh_bytes)
                    .saturating_add(rest_bytes);
                self.tail_settled = Some(settled);
                self.note_tail_bytes(tail_index, settled.bytes.saturating_add(rest_bytes));
                return;
            }
            self.projected_bytes = self
                .projected_bytes
                .saturating_sub(self.range_bytes(row_start, self.render.lines.len()));
            self.render.lines.truncate(row_start);
            self.render.layout.plain.truncate(row_start);
            self.render.layout.block_index.truncate(row_start);
            self.render.layout.copy.truncate(row_start);
            if self.block_count < blocks.len() && tail_index > 0 {
                self.render.lines.push(Line::from(""));
                self.render.layout.plain.push(String::new());
                self.render.layout.block_index.push(None);
                self.render.layout.copy.push(super::RowCopy::default());
            }
            let tail_start = self.render.lines.len();
            self.tail_row_start = Some(tail_start);
            self.tail_settled = None;
            if let Some((_, settled, settled_copy, rest, rest_copy)) = streamed {
                let lines = settled.len();
                let settled = assistant_rows(settled, settled_copy, theme, content_width);
                let rows = settled.0.len();
                self.push_tail_rows(tail_index, settled.0, settled.1);
                let bytes = self.range_bytes(tail_start, tail_start + rows);
                self.tail_settled = Some(SettledTail {
                    block: tail_index,
                    start: tail_start,
                    lines,
                    rows,
                    bytes,
                });
                let rest = assistant_rows(rest, rest_copy, theme, content_width);
                self.push_tail_rows(tail_index, rest.0, rest.1);
            } else {
                let (styled, copy) = block_rows(
                    &blocks[tail_index],
                    &super::RowPlan::default(),
                    theme,
                    content_width,
                    prefs,
                    blocks,
                );
                self.push_tail_rows(tail_index, styled, copy);
            }
            let tail_end = self.render.lines.len();
            let bytes = self.range_bytes(tail_start, tail_end);
            self.projected_bytes = self.projected_bytes.saturating_add(bytes);
            self.note_tail_bytes(tail_index, bytes);
        }

        fn push_tail_rows(
            &mut self,
            tail_index: usize,
            styled: Vec<Line<'static>>,
            copy: Vec<super::RowCopy>,
        ) {
            #[cfg(test)]
            {
                self.tail_rows += styled.len();
            }
            self.render
                .layout
                .plain
                .extend(styled.iter().map(plain_row));
            self.render.layout.copy.extend(copy);
            self.render
                .layout
                .block_index
                .extend(std::iter::repeat_n(Some(tail_index), styled.len()));
            self.render.lines.extend(styled);
        }

        fn note_tail_bytes(&mut self, tail_index: usize, bytes: usize) {
            self.evicted.remove(&tail_index);
            self.render.layout.estimated.remove(&tail_index);
            if bytes > OVERSIZED_BLOCK_PROJECTION_BYTES {
                self.oversized.insert(tail_index);
            } else {
                self.oversized.remove(&tail_index);
            }
        }

        /// Full re-projections performed so far.
        #[cfg(test)]
        #[must_use]
        pub const fn rebuilds(&self) -> usize {
            self.rebuilds
        }

        /// `(rows projected in full passes, rows projected by splices)`.
        #[cfg(test)]
        #[must_use]
        pub const fn work_rows(&self) -> (usize, usize) {
            (self.full_rows, self.tail_rows)
        }
    }

    fn block_row_range(layout: &LineLayout, idx: usize) -> Option<(usize, usize)> {
        let start = layout
            .block_index
            .iter()
            .position(|row| *row == Some(idx))?;
        let end = start
            + layout.block_index[start..]
                .iter()
                .take_while(|row| **row == Some(idx))
                .count();
        Some((start, end))
    }

    fn trailing_assistant_row_start(
        render: &TranscriptRender,
        blocks: &[ScrollBlock],
    ) -> Option<usize> {
        let tail_index = blocks.len().checked_sub(1)?;
        matches!(blocks.last(), Some(ScrollBlock::Assistant(_))).then(|| {
            render
                .layout
                .block_index
                .iter()
                .position(|index| *index == Some(tail_index))
                .unwrap_or(render.lines.len())
        })
    }

    #[cfg(test)]
    #[test]
    fn expanded_command_rails_are_muted_without_muting_the_command() {
        let theme = Theme::dark();
        let line = activity_line("  └ $ cargo test".into(), &theme, theme.success);
        assert_eq!(line.spans[0].content, "  └ ");
        assert_eq!(line.spans[0].style, theme.tool_label);
        assert_eq!(line.spans[1].content, "$ cargo test");
        assert_eq!(line.spans[1].style, theme.tool);
    }

    /// A multi-line command is one highlighted unit: a string opened on the
    /// first row keeps its colour on the next, and only the first row's
    /// program takes the accent.
    #[cfg(test)]
    #[test]
    fn a_multi_line_command_highlights_as_one_unit() {
        let theme = Theme::dark();
        let lines = command_lines("cargo test \"a\nb\" &&\n  ls -la", &theme);
        let styled = |line: &Line<'static>, text: &str| {
            line.spans
                .iter()
                .filter(|span| span.content.contains(text))
                .map(|span| span.style)
                .collect::<Vec<_>>()
        };
        assert_eq!(lines.len(), 3);
        assert_eq!(styled(&lines[0], "$"), vec![theme.command_prompt]);
        assert_eq!(styled(&lines[0], "cargo"), vec![theme.command_program]);
        assert_eq!(styled(&lines[1], "b"), vec![theme.command_string]);
        assert!(!lines[2].to_string().contains('$'));
        assert!(
            lines[2]
                .spans
                .iter()
                .all(|span| span.style != theme.command_program),
            "{:?}",
            lines[2]
        );
    }

    fn activity_line(text: String, theme: &Theme, status: Style) -> Line<'static> {
        let glyph = text
            .char_indices()
            .find(|(_, ch)| !matches!(ch, ' ' | '├' | '└' | '─' | '▶' | '▼'));
        let Some((offset, ch)) = glyph else {
            return Line::from(Span::styled(text, theme.tool));
        };
        if !matches!(ch, '✓' | '✗' | '⠼') {
            return Line::from(vec![
                Span::styled(text[..offset].to_owned(), theme.tool_label),
                Span::styled(text[offset..].to_owned(), theme.tool),
            ]);
        }
        Line::from(vec![
            Span::styled(text[..offset].to_owned(), theme.tool_label),
            Span::styled(ch.to_string(), status),
            Span::styled(text[offset + ch.len_utf8()..].to_owned(), theme.tool),
        ])
    }

    /// Source is the sole admitted detail beneath an activity action.
    fn assistant_markdown_theme(theme: &Theme) -> crate::tui::markdown::MarkdownTheme {
        crate::tui::markdown::MarkdownTheme {
            text: theme.assistant,
            headings: [theme.assistant_label; 6],
            strong: theme.assistant_label,
            emphasis: theme.assistant,
            strikethrough: theme.system,
            inline_code: theme.tool,
            code_block: theme.tool,
            code_language: theme.tool_label.add_modifier(Modifier::BOLD),
            syntax: theme.syntax_styles_for_code(theme.tool),
            link: theme.markdown_link,
            path: theme.tool,
            quote_marker: theme.tool_label,
            list_marker: theme.tool_label,
            rule: theme.system,
        }
    }

    const fn assistant_markdown_width(content_width: usize) -> u16 {
        let width = content_width.saturating_sub(2);
        if width == 0 {
            1
        } else if width > u16::MAX as usize {
            u16::MAX
        } else {
            #[allow(clippy::cast_possible_truncation, reason = "bounded just above")]
            {
                width as u16
            }
        }
    }

    /// Indent and wrap rendered markdown rows as an assistant block's rows.
    /// Rows are independent, so any split of the input maps to the same split
    /// of the output. The two-space block indent is decoration, added to the
    /// markdown row's own rail width.
    fn assistant_rows(
        lines: Vec<Line<'static>>,
        copies: Vec<super::RowCopy>,
        theme: &Theme,
        content_width: usize,
    ) -> (Vec<Line<'static>>, Vec<super::RowCopy>) {
        let mut out_lines = Vec::with_capacity(lines.len());
        let mut out_copy = Vec::with_capacity(lines.len());
        for (mut line, mut copy) in lines.into_iter().zip(copies) {
            line.spans.insert(0, Span::styled("  ", theme.assistant));
            copy.decoration = copy.decoration.saturating_add(2);
            let wrapped = wrap_styled_line(line, content_width.max(1));
            if wrapped.len() <= 1 {
                out_lines.extend(wrapped);
                out_copy.push(copy);
                continue;
            }
            // Markdown already fitted the row to the pane minus this indent.
            // A split here means the pane is narrower than that; later
            // fragments are leftover content, not a second copy of the source.
            let mut wrapped = wrapped.into_iter();
            if let Some(first) = wrapped.next() {
                copy.decoration = copy.decoration.min(cells_u16(first.width()));
                copy.code = None;
                copy.language = false;
                out_lines.push(first);
                out_copy.push(copy);
            }
            for extra in wrapped {
                out_lines.push(extra);
                out_copy.push(super::RowCopy::default());
            }
        }
        (out_lines, out_copy)
    }

    fn cells_u16(cells: usize) -> u16 {
        u16::try_from(cells).unwrap_or(u16::MAX)
    }

    /// Text columns a prompt row keeps before its send time is dropped.
    const MIN_PROMPT_COLUMNS: usize = 20;

    /// The prompt band of a user or steer row: the `›` gutter and the text,
    /// with the steer tag and the dimmed send time right-aligned on the first
    /// row (spec 02 §5.4). At narrow widths the send time is dropped first,
    /// then the tag.
    fn prompt_band_lines(
        text: &str,
        tag: Option<&'static str>,
        sent: Option<String>,
        theme: &Theme,
        content_width: usize,
    ) -> Vec<Line<'static>> {
        let w = content_width.saturating_sub(2).max(1);
        let tag_width = tag.map_or(0, unicode_width::UnicodeWidthStr::width);
        let sent = sent
            .map(|label| format!(" {label}"))
            .filter(|label| w >= tag_width + label.width() + 1 + MIN_PROMPT_COLUMNS);
        let trailing_width = tag_width + sent.as_ref().map_or(0, |label| label.width());
        let rows = if trailing_width == 0 {
            super::wrap_plain(text, w)
        } else {
            // The first row keeps room for the trailing labels and a gap.
            super::steer_rows(text, w, w.saturating_sub(trailing_width + 1).max(1))
        };
        let lines = rows
            .into_iter()
            .enumerate()
            .map(|(index, row)| {
                let gutter = if index == 0 {
                    super::PROMPT_PREFIX
                } else {
                    "  "
                };
                let mut line = Line::from(vec![
                    Span::styled(gutter, theme.user_label),
                    Span::styled(row, theme.user),
                ]);
                let padding = content_width.saturating_sub(line.width());
                let mut trailing: Vec<Span<'static>> = Vec::new();
                if index == 0 {
                    let shown_sent = sent.as_ref().filter(|_| padding > trailing_width);
                    if let Some(tag) = tag.filter(|_| padding > tag_width) {
                        trailing.push(Span::styled(tag, theme.tool_label));
                    }
                    if let Some(label) = shown_sent {
                        trailing.push(Span::styled(label.clone(), theme.chrome_dim));
                    }
                }
                let used: usize = trailing.iter().map(Span::width).sum();
                if padding > used {
                    line.spans.push(Span::raw(" ".repeat(padding - used)));
                }
                line.spans.extend(trailing);
                line.style(Style::default().bg(theme.user_bg))
            })
            .collect::<Vec<_>>();
        wrap_styled_lines(lines, content_width)
    }

    fn assistant_lines(text: &str, theme: &Theme, content_width: usize) -> Vec<Line<'static>> {
        assistant_block(text, theme, content_width).0
    }

    /// Rendered assistant rows and the copy metadata parallel to them.
    fn assistant_block(
        text: &str,
        theme: &Theme,
        content_width: usize,
    ) -> (Vec<Line<'static>>, Vec<super::RowCopy>) {
        // Markdown is parsed only for presentation. Source remains the
        // durable assistant text and malformed input stays inert.
        let (lines, copy) = crate::tui::markdown::render_markdown_rows(
            text,
            assistant_markdown_width(content_width),
            &assistant_markdown_theme(theme),
        );
        assistant_rows(lines, copy, theme, content_width)
    }

    /// [`block_lines`] plus copy metadata for each painted row.
    fn block_rows(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
        blocks: &[ScrollBlock],
    ) -> (Vec<Line<'static>>, Vec<super::RowCopy>) {
        if plan.hidden {
            return (Vec::new(), Vec::new());
        }
        if let ScrollBlock::Assistant(text) = block {
            return assistant_block(text, theme, content_width);
        }
        let mut lines = block_content_lines(block, plan, theme, content_width, prefs);
        let mut copy = vec![super::RowCopy::default(); lines.len()];
        if !plan.group_folded && !plan.cell_tails.is_empty() {
            if lines.last().is_some_and(|row| row.width() == 0) {
                lines.pop();
                copy.pop();
            }
            for tail in &plan.cell_tails {
                let (footer, metadata) = cell_footer_rows(tail, blocks, theme, content_width);
                lines.extend(footer);
                copy.extend(metadata);
            }
            if super::tool_block_closes_with_blank(plan) {
                lines.push(Line::from(""));
                copy.push(super::RowCopy::default());
            }
        }
        (lines, copy)
    }

    /// Render one isolated block for tests.
    #[cfg(test)]
    #[must_use]
    pub fn block_lines(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
    ) -> Vec<Line<'static>> {
        block_rows(
            block,
            plan,
            theme,
            content_width,
            prefs,
            std::slice::from_ref(block),
        )
        .0
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn block_lines_with(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
        blocks: &[ScrollBlock],
    ) -> Vec<Line<'static>> {
        block_rows(block, plan, theme, content_width, prefs, blocks).0
    }

    /// Render one block as styled lines.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "localized rendering for all transcript block variants"
    )]
    fn block_content_lines(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
    ) -> Vec<Line<'static>> {
        if plan.hidden {
            return Vec::new();
        }
        match block {
            ScrollBlock::User { text, sent_at_ms } => prompt_band_lines(
                text,
                None,
                sent_at_ms.and_then(|sent| prefs.prompt_clock.label(sent)),
                theme,
                content_width,
            ),
            ScrollBlock::Assistant(text) => assistant_lines(text, theme, content_width),
            ScrollBlock::Discarded { thinking: true, .. } if !prefs.show_thinking => Vec::new(),
            ScrollBlock::Discarded { text, thinking } => {
                let width = content_width.saturating_sub(2).max(1);
                std::iter::once(Line::from(Span::styled(
                    format!("  {}", super::discarded_label(*thinking)),
                    theme.chrome_dim,
                )))
                .chain(
                    super::wrap_plain(text, width).into_iter().map(|line| {
                        Line::from(Span::styled(format!("  {line}"), theme.chrome_dim))
                    }),
                )
                .collect()
            }
            ScrollBlock::Thinking {
                text, sealed, mode, ..
            } => {
                if !prefs.show_thinking {
                    return Vec::new();
                }
                if *sealed && text.trim().is_empty() && plan.activity_summary.is_none() {
                    return Vec::new();
                }
                let mut logical: Vec<_> = lead_rows(block, plan, theme)
                    .iter()
                    .map(|lead| lead.line(theme, content_width))
                    .collect();
                if !plan.group_folded && mode.shows_detail() && super::thinking_has_detail(text) {
                    let rail = super::thinking_detail_rail(plan);
                    for row in super::thinking_detail_rows(text, *mode) {
                        logical.extend(wrap_styled_line_railed(
                            &rail,
                            &rail,
                            theme.system,
                            Line::from(Span::styled(row, theme.system)),
                            content_width,
                        ));
                    }
                }
                if super::tool_block_closes_with_blank(plan) {
                    logical.push(Line::from(""));
                }
                logical
            }
            ScrollBlock::Approval {
                tool_name,
                subject,
                prompt,
                resolution,
                ..
            } => {
                let rows = super::format_approval_block(
                    tool_name,
                    subject.as_deref(),
                    prompt.as_deref(),
                    resolution.as_deref(),
                );
                let head_style = match resolution.as_deref() {
                    None => theme.busy,
                    Some("allowed") => theme.success,
                    Some(_) => theme.error_label,
                };
                let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 1);
                for (index, row) in rows.into_iter().enumerate() {
                    let style = if index == 0 { head_style } else { theme.system };
                    lines.push(Line::from(Span::styled(row, style)));
                }
                let mut logical = wrap_styled_lines(lines, content_width);
                logical.push(Line::from(""));
                logical
            }
            ScrollBlock::Tool {
                kind: super::ActivityKind::Compact,
                name,
                state,
                subject,
                ..
            } => {
                // Compaction rows keep their own grammar (§5.4).
                let running = super::tool_is_running(state);
                let style = if running {
                    theme.busy
                } else if super::tool_is_failed(state) {
                    theme.error_label
                } else {
                    theme.success
                };
                let action = crate::tui::activity::action(
                    name,
                    state,
                    super::ActivityKind::Compact,
                    subject.as_deref(),
                    plan,
                );
                let mut logical = wrap_styled_line_railed(
                    "  ",
                    "    ",
                    theme.tool_label,
                    activity_line(action.trim_start().to_owned(), theme, style),
                    content_width.max(1),
                );
                if super::tool_block_closes_with_blank(plan) {
                    logical.push(Line::from(""));
                }
                logical
            }
            ScrollBlock::Tool {
                kind,
                name,
                state,
                subject,
                detail,
                mode,
                view,
                ..
            } => {
                let leads = lead_rows(block, plan, theme);
                let mut logical: Vec<Line<'static>> = leads
                    .iter()
                    .map(|lead| lead.line(theme, content_width))
                    .collect();
                if !plan.group_folded {
                    if *kind != super::ActivityKind::Compose
                        && *mode == super::DisplayMode::Expanded
                    {
                        let rail = leads.last().map_or("  ", |lead| lead.rail);
                        push_item_detail(
                            &mut logical,
                            &ItemFacts {
                                name,
                                kind: *kind,
                                state,
                                subject: subject.as_deref(),
                                detail: detail.as_deref(),
                                view,
                            },
                            rail,
                            theme,
                            content_width,
                        );
                    }
                }
                if super::tool_block_closes_with_blank(plan) {
                    logical.push(Line::from(""));
                }
                logical
            }
            ScrollBlock::Milestone { milestone, open } => {
                let mut logical: Vec<Line<'static>> = lead_rows(block, plan, theme)
                    .iter()
                    .map(|lead| lead.line(theme, content_width))
                    .collect();
                if *open {
                    for row in super::milestone_detail(milestone) {
                        logical.extend(wrap_styled_line_railed(
                            "    │ ",
                            "    │   ",
                            theme.tool_label,
                            Line::from(Span::styled(row, theme.system)),
                            content_width.max(1),
                        ));
                    }
                }
                if super::tool_block_closes_with_blank(plan) {
                    logical.push(Line::from(""));
                }
                logical
            }
            ScrollBlock::Steer { text, sent_at_ms } => prompt_band_lines(
                text,
                Some(" steer"),
                sent_at_ms.and_then(|sent| prefs.prompt_clock.label(sent)),
                theme,
                content_width,
            ),
            ScrollBlock::Header(text) => wrap_styled_line_railed(
                "  ",
                "    ",
                theme.chrome_dim,
                Line::from(Span::styled(text.clone(), theme.chrome_dim)),
                content_width.max(1),
            ),
            ScrollBlock::Item { .. } => {
                let mut logical: Vec<Line<'static>> = lead_rows(block, plan, theme)
                    .iter()
                    .map(|lead| lead.line(theme, content_width))
                    .collect();
                if super::tool_block_closes_with_blank(plan) {
                    logical.push(Line::from(""));
                }
                logical
            }
            ScrollBlock::System(text) => {
                // Hide pure-noise lines that used to pollute the session log.
                if system_row_is_hidden(text) {
                    return Vec::new();
                }
                // Every notice keeps the transcript gutter (§5.4); a marker
                // glyph takes its own colour.
                let glyph_style = match text.chars().next() {
                    Some('⊘') => Some(theme.warning),
                    Some('✓') => Some(theme.success),
                    _ => None,
                };
                let line = match glyph_style {
                    Some(glyph) => {
                        let split = text.chars().next().map_or(0, char::len_utf8);
                        Line::from(vec![
                            Span::styled(text[..split].to_owned(), glyph),
                            Span::styled(text[split..].to_owned(), theme.system),
                        ])
                    }
                    None => Line::from(Span::styled(text.clone(), theme.system)),
                };
                wrap_styled_line_railed("  ", "    ", theme.system, line, content_width.max(1))
            }
            ScrollBlock::Error(text) => wrap_styled_line_railed(
                "  ",
                "    ",
                theme.error_label,
                Line::from(vec![
                    Span::styled("⚠ ", theme.error_label),
                    Span::styled(text.clone(), theme.error),
                ]),
                content_width.max(1),
            ),
        }
    }

    /// One clickable one-line row at the top of a block: a group summary, a
    /// merged row, an item, a thinking or a milestone row.
    pub(crate) struct LeadRow {
        /// Rail and indent before the glyph.
        pub(crate) rail: &'static str,
        glyph: (&'static str, Style),
        label: Vec<Span<'static>>,
        /// A failure count the cut never hides (`(1 failed)`).
        problems: Option<String>,
        meta: Vec<String>,
        /// Cells the label keeps before a meta part drops.
        label_floor: usize,
        /// Leading meta parts that never drop; the label is cut instead.
        kept_meta: usize,
        caret: &'static str,
        /// The whole label a copy carries: uncut, with the subject's own
        /// newlines rather than `↵`, failure count included.
        copy: String,
        thinking: bool,
    }

    impl LeadRow {
        fn line(&self, theme: &Theme, width: usize) -> Line<'static> {
            let meta: Vec<&str> = self.meta.iter().map(String::as_str).collect();
            item_row(
                vec![
                    Span::styled(self.rail, theme.tool_label),
                    Span::styled(self.glyph.0, self.glyph.1),
                    Span::styled(" ", theme.tool),
                ],
                self.label.clone(),
                self.problems
                    .as_ref()
                    .map(|problems| vec![Span::styled(format!(" {problems}"), theme.tool)])
                    .unwrap_or_default(),
                &meta,
                (self.label_floor, self.kept_meta),
                self.caret,
                theme,
                width,
            )
        }

        /// The row's untruncated text, which a copy of the row carries.
        pub(crate) fn full_text(&self) -> String {
            if self.thinking {
                return format!("◆ {}", self.copy);
            }
            let mut text = format!("{}{} {}", self.rail, self.glyph.0, self.copy);
            // The tip slot is room for paint, not text.
            for meta in self.meta.iter().filter(|meta| !meta.trim().is_empty()) {
                text.push_str(" · ");
                text.push_str(meta);
            }
            text
        }
    }

    /// Cells a running item keeps free at its right edge for the tip's
    /// elapsed (`59m59s` and its gap); dropped like any meta under the
    /// 12-cell subject rule.
    const TIP_META_SLOT: &str = "     ";

    /// A label and the failure count at its end, which a cut must not hide.
    fn split_problems(label: String) -> (String, Option<String>) {
        if (label.ends_with(" failed)") || label.ends_with(" interrupted)"))
            && let Some(at) = label.rfind(" (")
        {
            let problems = label[at + 1..].to_owned();
            let mut body = label;
            body.truncate(at);
            return (body, Some(problems));
        }
        (label, None)
    }

    fn split_meta(meta: Option<&str>) -> Vec<String> {
        meta.map(|meta| meta.split(" · ").map(str::to_owned).collect())
            .unwrap_or_default()
    }

    /// The one-line rows a block starts with, in order (spec 02 §5.4).
    pub(crate) fn lead_rows(
        block: &ScrollBlock,
        plan: &super::RowPlan,
        theme: &Theme,
    ) -> Vec<LeadRow> {
        if plan.hidden {
            return Vec::new();
        }
        match block {
            ScrollBlock::Tool {
                kind,
                name,
                state,
                subject,
                mode,
                wait,
                ..
            } if *kind != super::ActivityKind::Compact => {
                let mut rows = Vec::new();
                rows.extend(summary_row(plan, theme));
                if plan.group_folded {
                    return rows;
                }
                let merged =
                    plan.activity_action.is_some() && *kind != super::ActivityKind::Compose;
                let item = |plan: &super::RowPlan, caret| {
                    let glyph = crate::tui::activity::item_glyph(state, *kind, plan);
                    let glyph_style = match glyph {
                        "⠼" => theme.busy,
                        "✗" => theme.error_label,
                        _ => theme.success,
                    };
                    let copy = crate::tui::activity::item_label_raw(
                        name,
                        state,
                        *kind,
                        subject.as_deref(),
                        wait.as_ref(),
                        plan,
                    );
                    let label = crate::tui::activity::one_line(&copy);
                    let (label, problems) = if let Some(command) = label.strip_prefix("$ ") {
                        (command_spans(command, theme), None)
                    } else {
                        let (label, problems) = split_problems(label);
                        (vec![Span::styled(label, theme.tool)], problems)
                    };
                    let mut meta = split_meta(
                        crate::tui::activity::item_meta(
                            name,
                            state,
                            *kind,
                            subject.as_deref(),
                            plan,
                        )
                        .as_deref(),
                    );
                    if super::tool_is_running(state) || plan.activity_running {
                        meta.push(TIP_META_SLOT.to_owned());
                    }
                    LeadRow {
                        rail: super::row_indent(plan),
                        glyph: (glyph, glyph_style),
                        label,
                        problems,
                        meta,
                        label_floor: MIN_SUBJECT_CELLS,
                        kept_meta: 0,
                        caret,
                        copy,
                        thinking: false,
                    }
                };
                if merged {
                    let caret = if plan.members_open { "▾" } else { "▸" };
                    rows.push(item(plan, caret));
                    if plan.members_open {
                        let own = super::RowPlan {
                            activity_action: None,
                            activity_running: false,
                            activity_failed: false,
                            activity_meta: None,
                            compose_child: Some(false),
                            ..plan.clone()
                        };
                        rows.push(item(&own, crate::tui::activity::item_caret(*mode)));
                    }
                } else {
                    rows.push(item(plan, crate::tui::activity::item_caret(*mode)));
                }
                rows
            }
            ScrollBlock::Milestone { milestone, open } => {
                let (glyph, label) = super::milestone_parts(milestone);
                let glyph_style = match glyph {
                    "✓" => theme.success,
                    "✗" => theme.error_label,
                    "⚠" | "?" => theme.warning,
                    _ => theme.command_program,
                };
                let caret = if !super::milestone_has_detail(milestone) {
                    ""
                } else if *open {
                    "▾"
                } else {
                    "▸"
                };
                vec![LeadRow {
                    rail: "  ",
                    glyph: (glyph, glyph_style),
                    label: vec![Span::styled(
                        crate::tui::activity::one_line(&label),
                        theme.tool,
                    )],
                    problems: None,
                    meta: Vec::new(),
                    label_floor: MIN_SUBJECT_CELLS,
                    kept_meta: 0,
                    caret,
                    copy: label,
                    thinking: false,
                }]
            }
            ScrollBlock::Thinking {
                text,
                sealed,
                elapsed_ms,
                mode,
                ..
            } => {
                let mut rows: Vec<_> = summary_row(plan, theme).into_iter().collect();
                if !plan.group_folded && (!*sealed || !text.trim().is_empty()) {
                    let label = if *sealed {
                        super::thinking_summary(text)
                    } else {
                        "Thinking…".to_owned()
                    };
                    rows.push(LeadRow {
                        rail: super::row_indent(plan),
                        glyph: (
                            if *sealed { "◆" } else { "⠼" },
                            if *sealed {
                                theme.tool_label
                            } else {
                                theme.busy
                            },
                        ),
                        label: vec![Span::styled(label.clone(), theme.user_label)],
                        problems: None,
                        meta: if *sealed {
                            elapsed_ms
                                .map(super::format_group_elapsed)
                                .into_iter()
                                .collect()
                        } else {
                            Vec::new()
                        },
                        label_floor: MIN_SUBJECT_CELLS,
                        kept_meta: 0,
                        caret: if !*sealed || !super::thinking_has_detail(text) {
                            ""
                        } else if mode.shows_detail() {
                            "▾"
                        } else {
                            "▸"
                        },
                        copy: label,
                        thinking: true,
                    });
                }
                rows
            }
            ScrollBlock::Item { text, meta } => vec![LeadRow {
                rail: super::row_indent(plan),
                glyph: ("·", theme.system),
                label: vec![Span::styled(
                    crate::tui::activity::one_line(text),
                    theme.system,
                )],
                problems: None,
                meta: meta.clone(),
                // Only a process row has meta: its job id and outcome
                // outlast the command and the duration.
                label_floor: text.split(' ').next().map_or(0, |id| id.width() + 1),
                kept_meta: 1,
                caret: "",
                copy: text.clone(),
                thinking: false,
            }],
            _ => Vec::new(),
        }
    }

    fn summary_row(plan: &super::RowPlan, theme: &Theme) -> Option<LeadRow> {
        let copy = plan.activity_summary.as_ref()?.clone();
        let (label, problems) = split_problems(crate::tui::activity::one_line(&copy));
        Some(LeadRow {
            rail: "  ",
            glyph: if plan.activity_failed {
                ("✗", theme.error_label)
            } else {
                ("✓", theme.success)
            },
            label: vec![Span::styled(label, theme.tool)],
            problems,
            meta: split_meta(plan.summary_meta.as_deref()),
            label_floor: MIN_SUBJECT_CELLS,
            kept_meta: 0,
            caret: crate::tui::activity::header_caret(plan),
            copy,
            thinking: false,
        })
    }

    fn cell_footer_rows(
        tail: &super::CellTail,
        blocks: &[ScrollBlock],
        theme: &Theme,
        width: usize,
    ) -> (Vec<Line<'static>>, Vec<super::RowCopy>) {
        let Some(ScrollBlock::Tool {
            name,
            kind,
            state,
            subject,
            detail,
            mode,
            view,
            ..
        }) = blocks.get(tail.owner)
        else {
            return (Vec::new(), Vec::new());
        };
        let rail = super::detail_rail(tail.rail);
        let mut rows = Vec::new();
        let mut copy = Vec::new();
        if tail.hidden_calls > 0 {
            let label = format!("… {} more", tail.hidden_calls);
            let more_rail = super::row_indent(&super::RowPlan {
                group_child: match tail.rail {
                    "  ├─  " => Some(false),
                    "  └─  " => Some(true),
                    _ => None,
                },
                compose_child: Some(true),
                ..super::RowPlan::default()
            });
            let mut line = item_row(
                vec![Span::styled(more_rail, theme.tool_label)],
                vec![Span::styled(label, theme.tool_label)],
                Vec::new(),
                &[],
                (0, 0),
                "",
                theme,
                width,
            );
            line.alignment = Some(ratatui::layout::Alignment::Center);
            rows.push(line);
            copy.push(super::RowCopy::default());
        }
        if !mode.shows_detail() {
            return (rows, copy);
        }
        if let Some(source) = detail.as_deref().filter(|source| !source.trim().is_empty()) {
            let language = super::compose_program_language(name).unwrap_or("javascript");
            let fence = "`".repeat(
                source
                    .split(|c| c != '`')
                    .map(str::len)
                    .max()
                    .unwrap_or(0)
                    .max(2)
                    + 1,
            );
            let fenced = format!("{fence}{language}\n{source}\n{fence}");
            let (source_rows, source_copy) = crate::tui::markdown::render_markdown_rows(
                &fenced,
                u16::try_from(width.saturating_sub(rail.width()).max(1)).unwrap_or(u16::MAX),
                &assistant_markdown_theme(theme),
            );
            for (mut row, mut metadata) in source_rows.into_iter().zip(source_copy) {
                row.spans
                    .insert(0, Span::styled(rail.clone(), theme.tool_label));
                metadata.decoration = metadata.decoration.saturating_add(cells_u16(rail.width()));
                rows.push(row);
                copy.push(metadata);
            }
        }
        let mut output = Line::from(truncate_spans(
            vec![Span::styled(
                format!("{rail}output {}", if view.output_open { "▾" } else { "▸" }),
                theme.tool_label,
            )],
            width,
        ));
        output.alignment = Some(ratatui::layout::Alignment::Right);
        rows.push(output);
        copy.push(super::RowCopy {
            decoration: cells_u16(rail.width()),
            ..super::RowCopy::default()
        });
        if view.output_open {
            let first = rows.len();
            push_item_detail(
                &mut rows,
                &ItemFacts {
                    name,
                    kind: *kind,
                    state,
                    subject: subject.as_deref(),
                    detail: detail.as_deref(),
                    view,
                },
                tail.rail,
                theme,
                width,
            );
            copy.extend(std::iter::repeat_n(
                super::RowCopy::default(),
                rows.len() - first,
            ));
        }
        (rows, copy)
    }

    /// One tool item's fields the detail reads.
    struct ItemFacts<'a> {
        name: &'a str,
        kind: super::ActivityKind,
        state: &'a str,
        subject: Option<&'a str>,
        detail: Option<&'a str>,
        view: &'a super::ItemView,
    }

    /// What the detail shows: the host's canonical answer, or the retained
    /// preview (labelled) when the answer is not in yet or the call is no
    /// longer in canonical history.
    struct DetailText<'a> {
        input: &'a str,
        output: &'a str,
        input_truncated: bool,
        output_truncated: bool,
        preview: bool,
        /// Host-parsed sources of a web search or fetch result.
        sources: &'a [plakous_host_protocol::views::SourceLink],
    }

    fn detail_text<'a>(item: &'a ItemFacts<'a>) -> DetailText<'a> {
        use plakous_host_protocol::views::{CallDetailOutcome, CallDetailSource};
        if let Some(fetched) = item.view.fetched.as_deref()
            && let CallDetailOutcome::Detail {
                input,
                output,
                input_truncated,
                output_truncated,
                source,
                sources,
            } = fetched
        {
            let preview = *source == CallDetailSource::Preview;
            return DetailText {
                input,
                output,
                input_truncated: *input_truncated && !preview,
                output_truncated: *output_truncated && !preview,
                preview,
                sources,
            };
        }
        // The host's own preview rule: a running row's detail is its
        // arguments; a finished row's subject stands for the input and its
        // detail is the result preview.
        let running = super::tool_is_running(item.state);
        // A cell's retained detail is its source, drawn above; it has no
        // output preview.
        let (input, output) = if item.kind == super::ActivityKind::Compose {
            ("", "")
        } else if running {
            (item.detail.or(item.subject).unwrap_or_default(), "")
        } else {
            (
                item.subject.unwrap_or_default(),
                item.detail.unwrap_or_default(),
            )
        };
        // Only a settled answer, or the lack of anything to ask, names the
        // preview; a pending fetch shows the preview without the label.
        let preview = item.view.fetched.is_some();
        DetailText {
            input,
            output,
            input_truncated: false,
            output_truncated: false,
            preview,
            sources: &[],
        }
    }

    /// Lines past which a write's detail stops showing the content.
    const WRITE_HEAD_LINES: usize = 20;

    /// Input first, then the output window, every row under the item's rail
    /// with a hanging indent (spec 02 §5.4).
    fn push_item_detail(
        logical: &mut Vec<Line<'static>>,
        item: &ItemFacts<'_>,
        item_rail: &str,
        theme: &Theme,
        width: usize,
    ) {
        let rail = super::detail_rail(item_rail);
        let hanging = format!("{rail}  ");
        let push = |logical: &mut Vec<Line<'static>>, line: Line<'static>| {
            logical.extend(wrap_styled_line_railed(
                &rail,
                &hanging,
                theme.tool_label,
                line,
                width.max(1),
            ));
        };
        let text = detail_text(item);
        if text.preview {
            push(
                logical,
                Line::from(Span::styled("preview", theme.chrome_dim)),
            );
        }
        let verb = super::subject_tool_verb(item.name, item.kind, item.subject);
        // A code cell's source is already drawn above its detail.
        if item.kind != super::ActivityKind::Compose {
            for line in input_lines(verb, text.input, theme) {
                push(logical, line);
            }
            if text.input_truncated {
                push(
                    logical,
                    Line::from(Span::styled("… input truncated", theme.chrome_dim)),
                );
            }
        }
        if matches!(
            verb,
            super::ToolVerb::WebSearch | super::ToolVerb::XSearch | super::ToolVerb::Fetch
        ) && !text.sources.is_empty()
        {
            push(
                logical,
                Line::from(Span::styled("sources", theme.chrome_dim)),
            );
            for source in text.sources {
                let mut spans = Vec::new();
                if !source.title.is_empty() {
                    spans.push(Span::styled(format!("{}  ", source.title), theme.tool));
                }
                spans.push(Span::styled(source.url.clone(), theme.chrome_dim));
                push(logical, Line::from(spans));
            }
        }
        let output = text.output.trim_end_matches('\n');
        if output.trim().is_empty() {
            return;
        }
        push(logical, Line::from(Span::styled("┄", theme.tool_label)));
        let lines: Vec<&str> = output.split('\n').collect();
        let kept = super::TOOL_DETAIL_HEAD_LINES + super::TOOL_DETAIL_TAIL_LINES;
        let style = theme.detail_output;
        if item.view.full_output || lines.len() <= kept + 1 {
            for line in &lines {
                push(logical, Line::from(Span::styled((*line).to_owned(), style)));
            }
        } else {
            for line in &lines[..super::TOOL_DETAIL_HEAD_LINES] {
                push(logical, Line::from(Span::styled((*line).to_owned(), style)));
            }
            let tail_start = lines.len() - super::TOOL_DETAIL_TAIL_LINES;
            // A spill line names where the whole result went; folding the
            // window must never hide it.
            let spills: Vec<&str> = lines[super::TOOL_DETAIL_HEAD_LINES..tail_start]
                .iter()
                .copied()
                .filter(|line| line.trim_start().starts_with(super::FULL_OUTPUT_PREFIX))
                .collect();
            let hidden = lines.len() - kept - spills.len();
            if hidden > 0 {
                push(
                    logical,
                    Line::from(Span::styled(
                        super::hidden_lines_label(hidden),
                        theme.tool_label,
                    )),
                );
                if let Some(row) = logical.last_mut() {
                    super::mark_hidden_lines(row);
                }
            }
            for line in spills {
                push(logical, Line::from(Span::styled(line.to_owned(), style)));
            }
            for line in &lines[tail_start..] {
                push(logical, Line::from(Span::styled((*line).to_owned(), style)));
            }
        }
        if text.output_truncated {
            push(
                logical,
                Line::from(Span::styled("… output truncated", theme.chrome_dim)),
            );
        }
    }

    /// The input part of an item's detail: the full command, a diff, a
    /// write's first lines, or `key: value` arguments, never raw JSON.
    fn input_lines(verb: super::ToolVerb, input: &str, theme: &Theme) -> Vec<Line<'static>> {
        let parsed = serde_json::from_str::<serde_json::Value>(input).ok();
        let Some(serde_json::Value::Object(mut args)) = parsed else {
            return match verb {
                super::ToolVerb::Run => command_lines(input, theme),
                _ => input
                    .lines()
                    .map(|line| Line::from(Span::styled(line.to_owned(), theme.tool)))
                    .collect(),
            };
        };
        let take_str = |args: &mut serde_json::Map<String, serde_json::Value>, keys: &[&str]| {
            keys.iter().find_map(|key| match args.get(*key) {
                Some(serde_json::Value::String(text)) => {
                    let text = text.clone();
                    args.remove(*key);
                    Some(text)
                }
                _ => None,
            })
        };
        let mut lines = Vec::new();
        match verb {
            super::ToolVerb::Run => {
                let command = match args.get("cmd").or_else(|| args.get("command")) {
                    Some(serde_json::Value::Array(parts)) => Some(
                        parts
                            .iter()
                            .map(|part| {
                                part.as_str()
                                    .map_or_else(|| part.to_string(), str::to_owned)
                            })
                            .collect::<Vec<_>>()
                            .join(" "),
                    ),
                    _ => None,
                };
                let command = command.or_else(|| take_str(&mut args, &["cmd", "command"]));
                args.remove("cmd");
                args.remove("command");
                if let Some(command) = command {
                    lines.extend(command_lines(&command, theme));
                }
            }
            super::ToolVerb::Edit => {
                let path = take_str(&mut args, &["file_path", "path"]);
                if let Some(patch) = take_str(&mut args, &["patch", "input"]) {
                    lines.extend(diff_lines(patch.lines().map(str::to_owned), theme));
                } else if let (Some(old), Some(new)) = (
                    take_str(&mut args, &["old_string", "old_text"]),
                    take_str(&mut args, &["new_string", "new_text"]),
                ) {
                    let mut diff = Vec::new();
                    if let Some(path) = &path {
                        diff.push(format!("--- {path}"));
                        diff.push(format!("+++ {path}"));
                    }
                    diff.extend(super::line_diff(&old, &new));
                    lines.extend(diff_lines(diff.into_iter(), theme));
                } else if let Some(path) = path {
                    lines.push(Line::from(Span::styled(path, theme.tool)));
                }
            }
            super::ToolVerb::Write => {
                if let Some(path) = take_str(&mut args, &["file_path", "path"]) {
                    lines.push(Line::from(Span::styled(path, theme.tool)));
                }
                if let Some(content) = take_str(&mut args, &["content", "contents", "text"]) {
                    let all: Vec<&str> = content.lines().collect();
                    for line in all.iter().take(WRITE_HEAD_LINES) {
                        lines.push(Line::from(Span::styled(
                            format!("+{line}"),
                            theme.diff_added,
                        )));
                    }
                    if all.len() > WRITE_HEAD_LINES {
                        lines.push(Line::from(Span::styled(
                            format!("… {} more lines", all.len() - WRITE_HEAD_LINES),
                            theme.chrome_dim,
                        )));
                    }
                }
            }
            _ => {}
        }
        push_key_values(&mut lines, &args, 0, theme);
        lines
    }

    /// `key: value` rows for JSON arguments; nested values indent by two.
    fn push_key_values(
        lines: &mut Vec<Line<'static>>,
        args: &serde_json::Map<String, serde_json::Value>,
        depth: usize,
        theme: &Theme,
    ) {
        let indent = "  ".repeat(depth);
        for (key, value) in args {
            match value {
                serde_json::Value::Object(map) if !map.is_empty() => {
                    lines.push(Line::from(Span::styled(
                        format!("{indent}{key}:"),
                        theme.tool_label,
                    )));
                    push_key_values(lines, map, depth + 1, theme);
                }
                serde_json::Value::Array(items)
                    if items.iter().any(|item| item.is_object() || item.is_array()) =>
                {
                    lines.push(Line::from(Span::styled(
                        format!("{indent}{key}:"),
                        theme.tool_label,
                    )));
                    for item in items {
                        match item {
                            serde_json::Value::Object(map) => {
                                lines.push(Line::from(Span::styled(
                                    format!("{indent}  -"),
                                    theme.tool_label,
                                )));
                                push_key_values(lines, map, depth + 2, theme);
                            }
                            other => lines.push(Line::from(vec![
                                Span::styled(format!("{indent}  - "), theme.tool_label),
                                Span::styled(scalar_text(other), theme.tool),
                            ])),
                        }
                    }
                }
                serde_json::Value::String(text) if text.contains('\n') => {
                    lines.push(Line::from(Span::styled(
                        format!("{indent}{key}:"),
                        theme.tool_label,
                    )));
                    for line in text.lines() {
                        lines.push(Line::from(Span::styled(
                            format!("{indent}  {line}"),
                            theme.tool,
                        )));
                    }
                }
                other => lines.push(Line::from(vec![
                    Span::styled(format!("{indent}{key}: "), theme.tool_label),
                    Span::styled(scalar_text(other), theme.tool),
                ])),
            }
        }
    }

    fn scalar_text(value: &serde_json::Value) -> String {
        match value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(items) => {
                items.iter().map(scalar_text).collect::<Vec<_>>().join(", ")
            }
            other => other.to_string(),
        }
    }

    /// A full command, multi-line, coloured like its item row.
    /// A command's detail rows: highlighted as one unit, so quoting and
    /// heredocs carry across lines, with `$` and the program accent on the
    /// first row only.
    fn command_lines(command: &str, theme: &Theme) -> Vec<Line<'static>> {
        let mut rows =
            crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme)).into_iter();
        let first = rows.next().unwrap_or_default();
        let first_text = command.lines().next().unwrap_or_default();
        std::iter::once(Line::from(prompted_command(first, first_text, theme)))
            .chain(rows.map(Line::from))
            .collect()
    }

    /// Unified-diff rows in the diff colours.
    fn diff_lines(rows: impl Iterator<Item = String>, theme: &Theme) -> Vec<Line<'static>> {
        rows.map(|row| {
            let style = if row.starts_with("+++")
                || row.starts_with("---")
                || row.starts_with("@@")
                || row.starts_with("***")
            {
                theme.diff_hunk
            } else if row.starts_with('+') {
                theme.diff_added
            } else if row.starts_with('-') {
                theme.diff_removed
            } else {
                theme.tool
            };
            Line::from(Span::styled(row, style))
        })
        .collect()
    }

    /// Fewest cells the subject keeps before the meta is dropped (§5.4).
    const MIN_SUBJECT_CELLS: usize = 12;

    /// One item row, never wrapped: `prefix` (rail and glyph), the label cut
    /// with `…`, right-aligned `meta` parts, then `caret`. At narrow widths
    /// meta parts drop from the right first so the label keeps `label_floor`
    /// cells (usually [`MIN_SUBJECT_CELLS`]), except the first `kept_meta`,
    /// which stay while the label is cut further; the caret always stays.
    fn item_row(
        prefix: Vec<Span<'static>>,
        label: Vec<Span<'static>>,
        suffix: Vec<Span<'static>>,
        meta: &[&str],
        (label_floor, kept_meta): (usize, usize),
        caret: &str,
        theme: &Theme,
        width: usize,
    ) -> Line<'static> {
        let spans_width = |spans: &[Span<'static>]| -> usize {
            spans.iter().map(|span| span.content.width()).sum()
        };
        let prefix_width = spans_width(&prefix);
        let caret_width = if caret.is_empty() {
            0
        } else {
            1 + caret.width()
        };
        // The suffix (a failure count) is never cut; the label gives way.
        let room = width.saturating_sub(prefix_width + caret_width + spans_width(&suffix));
        let label_width = spans_width(&label);
        let mut shown = meta.len();
        let meta_text = loop {
            let text = meta[..shown].join(" · ");
            let needed = if text.is_empty() { 0 } else { text.width() + 2 };
            if shown <= kept_meta || label_width + needed <= room || room >= needed + label_floor {
                break text;
            }
            shown -= 1;
        };
        let meta_width = if meta_text.is_empty() {
            0
        } else {
            meta_text.width() + 2
        };
        let label = truncate_spans(label, room.saturating_sub(meta_width));
        let used = prefix_width + spans_width(&label) + spans_width(&suffix);
        let mut spans = prefix;
        spans.extend(label);
        spans.extend(suffix);
        let tail_width = meta_width.saturating_sub(2).saturating_add(caret_width);
        let padding = width.saturating_sub(used + tail_width);
        if padding > 0 && (tail_width > 0) {
            spans.push(Span::raw(" ".repeat(padding)));
        }
        if meta_text.trim().is_empty() {
            // A tip's reserved slot paints like padding.
            spans.push(Span::raw(meta_text));
        } else {
            spans.push(Span::styled(meta_text, theme.tool_label));
        }
        if !caret.is_empty() {
            spans.push(Span::styled(format!(" {caret}"), theme.tool_label));
        }
        Line::from(spans)
    }

    /// `spans` cut to `budget` cells, ending `…` when anything was cut.
    fn truncate_spans(spans: Vec<Span<'static>>, budget: usize) -> Vec<Span<'static>> {
        let total: usize = spans.iter().map(|span| span.content.width()).sum();
        if total <= budget {
            return spans;
        }
        let mut out = Vec::new();
        let mut used = 0;
        let keep = budget.saturating_sub(1);
        let mut last_style = Style::default();
        'spans: for span in spans {
            last_style = span.style;
            let mut kept = String::new();
            for grapheme in span.content.graphemes(true) {
                let cells = grapheme.width();
                if used + cells > keep {
                    if !kept.is_empty() {
                        out.push(Span::styled(kept, span.style));
                    }
                    break 'spans;
                }
                used += cells;
                kept.push_str(grapheme);
            }
            out.push(Span::styled(kept, span.style));
        }
        if budget > 0 {
            out.push(Span::styled("…", last_style));
        }
        out
    }

    fn command_styles(theme: &Theme) -> crate::tui::highlight::HighlightStyles {
        crate::tui::highlight::HighlightStyles {
            default: theme.command_arg,
            comment: theme.command_arg,
            string: theme.command_string,
            constant: theme.command_arg,
            keyword: theme.command_arg,
            function: theme.command_arg,
            type_name: theme.command_arg,
            parameter: theme.command_arg,
            invalid: theme.command_arg,
        }
    }

    /// A one-line command coloured with syntect's bash grammar: `$` muted,
    /// the program in the accent, arguments in body text, quoted strings in
    /// the string colour (§5.4).
    fn command_spans(command: &str, theme: &Theme) -> Vec<Span<'static>> {
        let highlighted =
            crate::tui::highlight::lines(command, Some("bash"), &command_styles(theme))
                .into_iter()
                .next()
                .unwrap_or_default();
        prompted_command(highlighted, command, theme)
    }

    /// `$ ` and the highlighted first row of a command, its first token (the
    /// program) in the accent.
    fn prompted_command(
        highlighted: Vec<Span<'static>>,
        command: &str,
        theme: &Theme,
    ) -> Vec<Span<'static>> {
        let program_start = command.len() - command.trim_start().len();
        let program_end = command[program_start..]
            .find(char::is_whitespace)
            .map_or(command.len(), |end| program_start + end);
        let mut spans = vec![
            Span::styled("$", theme.command_prompt),
            Span::styled(" ", theme.command_arg),
        ];
        let mut offset = 0;
        for span in highlighted {
            let text = span.content.into_owned();
            let (from, to) = (offset, offset + text.len());
            offset = to;
            // Only plain argument text in the program's range takes the accent;
            // a quoted program keeps the string colour.
            if span.style != theme.command_arg || to <= program_start || from >= program_end {
                spans.push(Span::styled(text, span.style));
                continue;
            }
            let cut_start = program_start.max(from) - from;
            let cut_end = program_end.min(to) - from;
            if cut_start > 0 {
                spans.push(Span::styled(text[..cut_start].to_owned(), span.style));
            }
            spans.push(Span::styled(
                text[cut_start..cut_end].to_owned(),
                theme.command_program,
            ));
            if cut_end < text.len() {
                spans.push(Span::styled(text[cut_end..].to_owned(), span.style));
            }
        }
        spans
    }

    /// Project `blocks`, measuring exactly only the blocks named by `exact`.
    ///
    /// `None` measures everything. Otherwise every other block contributes
    /// [`estimate_block_rows`] blank rows: they hold geometry so the scroll
    /// bar and the anchor have a transcript to address, and the paint path
    /// promotes to an exact measurement everything the viewport reads, so a
    /// blank placeholder is never drawn.
    #[must_use]
    fn render_transcript_windowed(
        blocks: &[ScrollBlock],
        theme: &Theme,
        content_width: usize,
        prefs: super::RenderPrefs,
        plans: &[super::RowPlan],
        exact: Option<&std::collections::BTreeSet<usize>>,
    ) -> TranscriptRender {
        let mut lines = Vec::new();
        let mut plain = Vec::new();
        let mut block_index = Vec::new();
        let mut copy = Vec::new();
        let mut estimated = std::collections::BTreeSet::new();
        let mut previous_drawn: Option<&ScrollBlock> = None;
        for bi in super::row_order(plans) {
            let block = &blocks[bi];
            let separated = super::wants_separator(previous_drawn, block);
            if exact.is_none_or(|exact| exact.contains(&bi)) {
                let (styled, row_copy) =
                    block_rows(block, &plans[bi], theme, content_width, prefs, blocks);
                // Hidden system noise intentionally contributes no visual rows.
                if styled.is_empty() {
                    continue;
                }
                if separated {
                    lines.push(Line::from(""));
                    plain.push(String::new());
                    block_index.push(None);
                    copy.push(super::RowCopy::default());
                }
                let block_plain = styled
                    .iter()
                    .map(|line| selectable_row(block, line))
                    .collect::<Vec<_>>();
                block_index.extend(std::iter::repeat_n(Some(bi), styled.len()));
                lines.extend(styled);
                plain.extend(block_plain);
                copy.extend(row_copy);
                previous_drawn = Some(block);
                continue;
            }
            let rows = estimate_block_rows(block, &plans[bi], content_width, prefs, blocks);
            if rows == 0 {
                continue;
            }
            previous_drawn = Some(block);
            if separated {
                lines.push(Line::from(""));
                plain.push(String::new());
                block_index.push(None);
                copy.push(super::RowCopy::default());
            }
            estimated.insert(bi);
            block_index.extend(std::iter::repeat_n(Some(bi), rows));
            lines.extend(std::iter::repeat_n(Line::default(), rows));
            plain.extend(std::iter::repeat_n(String::new(), rows));
            copy.extend(std::iter::repeat_n(super::RowCopy::default(), rows));
        }
        TranscriptRender {
            lines,
            layout: LineLayout {
                plain,
                block_index,
                estimated,
                copy,
            },
        }
    }
}

#[cfg(test)]
pub(super) use render::TRANSCRIPT_RENDER_CACHE_BYTES;
pub(super) use render::{LeadRow, TranscriptChange, TranscriptRenderCache, lead_rows};

#[cfg(test)]
mod tests;
