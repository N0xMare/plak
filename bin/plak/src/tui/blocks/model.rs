//! The transcript block model: blocks, render preferences, and the text
//! and duration labels their notice, mail and milestone rows carry.

use std::borrow::Cow;
use std::time::Duration;

use crate::tui::presentation::ActivityKind;
use plakous_host_protocol::views::CallDetailOutcome;
use plakous_host_protocol::views::transcript::{
    AgentMilestone, MilestoneEvent, NoticePlacement, NoticeScope, NoticeTone, ProcessNotice,
    ProcessOutcome, ToolItem, TranscriptRow, WaitFacts,
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
/// Projected from terminal-owned preferences carried by `SettingsView`;
/// rendering never reads product configuration or storage.
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

impl From<&crate::tui::TuiPreferences> for RenderPrefs {
    /// Terminal-owned preferences supply the renderer's reading and motion rows.
    fn from(snapshot: &crate::tui::TuiPreferences) -> Self {
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
        Self::from(&crate::tui::TuiPreferences::default())
    }
}

/// The label over assistant text a replayed model request discarded.
pub const DISCARDED_LABEL: &str = "discarded -- retrying";

/// The label over reasoning a replayed model request discarded.
pub const DISCARDED_THINKING_LABEL: &str = "discarded thinking -- retrying";

/// One visual unit in the transcript scrollback.
#[cfg_attr(test, derive(Clone))]
#[derive(Debug, Eq, PartialEq)]
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
    /// Delivered child mail; folding and grouping are presentation-only.
    Mail {
        /// The complete bounded notice, unchanged for copying and search.
        text: String,
        /// The operator opened this message's body.
        open: bool,
        /// On a consecutive run's first message, reveal its member rows.
        group_open: bool,
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
            | Self::Mail { text, .. }
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
            | Self::Mail { text, .. }
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
            TranscriptRow::Notice(mut notice) => {
                notice.text = crate::notice_text::with_remedy(
                    &notice.text,
                    notice.remedy.as_ref(),
                    crate::notice_text::RemedyContext {
                        frontend: crate::notice_text::Frontend::Tui,
                        session_id: None,
                    },
                );
                match (notice.placement(), notice.tone) {
                    (NoticePlacement::SessionLog, _) => return None,
                    (_, NoticeTone::Error) => Self::Error(notice.text),
                    _ if notice.scope == NoticeScope::Tree
                        && mail_parts(&notice.text).is_some() =>
                    {
                        Self::Mail {
                            text: notice.text,
                            open: false,
                            group_open: false,
                        }
                    }
                    (NoticePlacement::GroupItem, _) => Self::item(notice.text),
                    (NoticePlacement::NoticeRow, _) => Self::System(notice.text),
                }
            }
            TranscriptRow::Interrupted => Self::System(INTERRUPTED_MARKER.to_owned()),
            TranscriptRow::Truncated => Self::System(TRANSCRIPT_TRUNCATION_MARKER.to_owned()),
        })
    }

    /// A persisted tool row, with the state label a live row of the same call
    /// earned: a resumed `12.3s` does not reload as `12315ms`, a rejection
    /// stays `⊘` and a failure keeps its detail.
    pub(in crate::tui) fn from_tool_item(tool: ToolItem) -> Self {
        let kind = tool.kind;
        let state =
            super::presented_tool_state(&tool.state, tool.duration_ms().map(Duration::from_millis));
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
            Self::System(text) | Self::Header(text) | Self::Mail { text, .. } => text.clone(),
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
            | Self::Mail { text: t, .. }
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
            Self::Mail { open, .. } => Some(if *open {
                DisplayMode::Expanded
            } else {
                DisplayMode::Collapsed
            }),
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
            Self::Milestone { open, .. } | Self::Mail { open, .. } => *open = next.shows_detail(),
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
            Self::Tool { .. } | Self::Thinking { .. } | Self::Mail { .. } => true,
            Self::Milestone { milestone, .. } => milestone_has_detail(milestone),
            _ => false,
        }
    }
}

/// Recognize only the hosted delivered-mail spelling, never arbitrary tree notices.
pub(crate) fn mail_parts(text: &str) -> Option<(&str, &str, &str)> {
    let mut parts = text.splitn(3, " · ");
    let purpose = parts.next()?;
    let agent = parts.next()?;
    let body = parts.next()?;
    if !matches!(
        purpose,
        "coordinate" | "finding" | "question" | "reply" | "followup" | "result"
    ) || !agent.starts_with('#')
        || agent.len() == 1
        || agent.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some((purpose, agent, body))
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
        MilestoneEvent::NeedsRecovery => ("⚠", format!("{id} needs recovery · u retries")),
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

pub(super) fn offset_at(zone: &jiff::tz::TimeZone, unix_ms: u64) -> i32 {
    let Ok(seconds) = i64::try_from(unix_ms / 1_000) else {
        return 0;
    };
    let Ok(timestamp) = jiff::Timestamp::from_second(seconds) else {
        return 0;
    };
    zone.to_offset(timestamp).seconds()
}

pub(super) fn local_moment(unix_ms: u64, offset_seconds: i32) -> Option<time::OffsetDateTime> {
    let offset =
        time::UtcOffset::from_whole_seconds(offset_seconds).unwrap_or(time::UtcOffset::UTC);
    let seconds = i64::try_from(unix_ms / 1_000).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .map(|moment| moment.to_offset(offset))
}

/// A submitted result as indented lines when it is one line of compact
/// JSON, the host's cut line (`… cut at 16 KiB of 40 KiB`) kept under it.
/// A cut result is not valid JSON, so the indent follows the tokens rather
/// than parsing, and keeps the child's key order.
pub(super) fn indent_result(text: &str) -> Cow<'_, str> {
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
pub(super) fn indent_json(json: &str) -> String {
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
