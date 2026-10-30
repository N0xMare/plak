//! What a `plak run` user sees on stderr.
//!
//! The one-shot face used to forward every host observation it recognised as
//! its own line: `tool running: exec` then `tool done: exec`, thirty-odd lines
//! for a single turn, and `tool failed: read_file` with no reason at all. This
//! module is the whole of that rendering, so the text a user reads is decided
//! in one place and can be pinned by tests without a live provider.
//!
//! Three rules shape it:
//!
//! * A tool speaks once, when it finishes, unless the reader asked for the
//!   progress lines with `PLAK_VERBOSE`.
//! * A failure carries its typed reason on the same line. The reason is the
//!   tool's own message; this module only lifts it out of the bounded detail
//!   preview and puts it on one line.
//! * A failure the model recovered from is an event, not an alarm, so a turn
//!   that ended well says so after the fact rather than leaving the reader with
//!   the failure as the last word.

use crate::surface_protocol::HostEvent;
use plakous::tools::PROCESS_OBSERVATION_BYTES;
use plakous_hosted::sessions::{NoticePlacement, TranscriptRow};

/// The marker a process tool appends when it cut its own output.
///
/// It is model-visible pinned text, so it is matched here and never reworded.
const OBSERVATION_TRUNCATED_MARKER: &str = "[observation truncated]";

/// How much of a typed failure reason reaches one stderr line.
const MAX_REASON_BYTES: usize = 200;

/// The anchor every locally executed tool failure carries.
///
/// `plakous`'s runtime normalises a tool failure to
/// `tool invocation failed: <message>`, so the reason can be lifted out of the
/// combined argument/result preview exactly rather than guessed at.
const FAILURE_ANCHOR: &str = "tool invocation failed: ";

/// Whether child agents can be spawned in this session, and what to say if not.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ChildAgents {
    /// The roster names at least one template and the topology is enabled.
    Available,
    /// Delegation cannot work here. The string names what is missing and the
    /// exact file that configures it.
    Unavailable(String),
}

/// Whether this product home can spawn child agents, read once per run.
///
/// Delegation needs three things at once: the topology switch on, a template
/// file on disk, and that template named in the roster. A reader who asked for
/// delegation and got silence needs to be told which one is missing and the
/// exact file that sets it, so the answer is composed here rather than left to
/// the model's prose.
pub(crate) fn child_agents(
    config: &crate::product_config::ProductConfig,
    paths: &crate::home::paths::PlakPaths,
) -> ChildAgents {
    let config_file = paths.config_file().display().to_string();
    let agents_dir = paths.user_agents_dir().display().to_string();
    if config.templates.is_empty() {
        return ChildAgents::Unavailable(format!(
            "child agents are not configured, so delegation cannot run here: no template files under {agents_dir}"
        ));
    }
    if !config.subagents {
        return ChildAgents::Unavailable(format!(
            "child agents are off, so delegation cannot run here: set [orchestration] subagents = true in {config_file}"
        ));
    }
    if config.working_set.is_empty() {
        return ChildAgents::Unavailable(format!(
            "no child agent is spawnable, so delegation cannot run here: name templates in [roster] working_set in {config_file}"
        ));
    }
    ChildAgents::Available
}

/// The stateful part of the face: what has already been said this run.
pub(crate) struct CliFace {
    session_id: Option<String>,
    verbose: bool,
    child_agents: ChildAgents,
    announced_session: bool,
    announced_delegation: bool,
    announced_truncation: Vec<String>,
    announced_approvals: std::collections::BTreeSet<String>,
    failed_tools: usize,
}

impl CliFace {
    /// A face for one run.
    pub(crate) const fn new(verbose: bool, child_agents: ChildAgents) -> Self {
        Self {
            session_id: None,
            verbose,
            child_agents,
            announced_session: false,
            announced_delegation: false,
            announced_truncation: Vec::new(),
            announced_approvals: std::collections::BTreeSet::new(),
            failed_tools: 0,
        }
    }

    pub(crate) fn with_session_id(mut self, session_id: &str) -> Self {
        self.session_id = Some(session_id.to_owned());
        self
    }

    /// Whether `PLAK_VERBOSE` asks for the per-tool progress lines.
    pub(crate) fn verbose_from_environment() -> bool {
        std::env::var_os("PLAK_VERBOSE")
            .is_some_and(|value| !matches!(value.to_str(), Some("") | Some("0") | Some("false")))
    }

    /// Every stderr line one observation produces, in order.
    ///
    /// Returning the lines rather than writing them keeps the whole rendering
    /// decision pure, so a test pins the text a user reads without a host.
    pub(crate) fn lines(&mut self, event: &HostEvent) -> Vec<String> {
        match event {
            HostEvent::StorageAdmissionRefused(diagnostic) => vec![prefixed(
                &crate::notice_text::storage_admission(diagnostic.as_str()),
            )],
            HostEvent::RestoredPause(pause) => vec![prefixed(&crate::notice_text::restored_pause(
                *pause,
                crate::notice_text::RemedyContext {
                    frontend: crate::notice_text::Frontend::Run,
                    session_id: self.session_id.as_deref(),
                },
            ))],
            HostEvent::ApprovalRequired {
                approval_id,
                tool_name,
                ..
            } => {
                if !self.announced_approvals.insert(approval_id.clone()) {
                    return Vec::new();
                }
                vec![prefixed(&format!(
                    "denied approval for {tool_name}: run is non-interactive"
                ))]
            }
            HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.placement() != NoticePlacement::SessionLog =>
            {
                vec![prefixed(&crate::notice_text::with_remedy(
                    &notice.text,
                    notice.remedy.as_ref(),
                    crate::notice_text::RemedyContext {
                        frontend: crate::notice_text::Frontend::Run,
                        session_id: self.session_id.as_deref(),
                    },
                ))]
            }
            HostEvent::ModelRetry(retry) => vec![prefixed(&retry_line(retry))],
            HostEvent::SessionInfo(info) => self.session_line(info).into_iter().collect(),
            HostEvent::ToolLine {
                name,
                state,
                subject,
                detail,
                duration_ms,
                ..
            } => self.tool_lines(
                name,
                state,
                subject.as_deref(),
                detail.as_deref(),
                *duration_ms,
            ),
            _ => Vec::new(),
        }
    }

    /// The closing line, if the run earned one.
    ///
    /// Completion establishes the turn's lifecycle outcome. It does not prove
    /// that the model recovered from a tool failure or fulfilled the task.
    pub(crate) fn closing_line(&self, succeeded: bool) -> Option<String> {
        if !succeeded || self.failed_tools == 0 {
            return None;
        }
        let calls = if self.failed_tools == 1 {
            "1 tool call".to_owned()
        } else {
            format!("{} tool calls", self.failed_tools)
        };
        Some(format!(
            "plak: {calls} failed during this turn; the turn completed"
        ))
    }

    /// One line naming the active composition and what it means for tools.
    ///
    /// `--composition direct` used to produce an empty stderr for a turn that
    /// really ran: no tool reached a state worth a line and nothing else spoke.
    /// The session line is unconditional, so every run says what it is.
    fn session_line(
        &mut self,
        info: &plakous_host_protocol::views::presentation::SessionInfoView,
    ) -> Option<String> {
        self.session_id = Some(info.session_id.clone());
        if self.announced_session {
            return None;
        }
        self.announced_session = true;
        let policy = info.tool_policy.as_deref().unwrap_or("unresolved");
        let mut line = format!(
            "plak: composition {policy}: {}",
            tool_policy_explanation(policy)
        );
        if let ChildAgents::Unavailable(reason) = &self.child_agents {
            line.push_str(&format!("; {reason}"));
            self.announced_delegation = true;
        }
        Some(line)
    }

    /// The lines one tool lifecycle observation produces.
    fn tool_lines(
        &mut self,
        name: &str,
        state: &str,
        subject: Option<&str>,
        detail: Option<&str>,
        duration_ms: Option<u64>,
    ) -> Vec<String> {
        if !is_terminal_state(state) {
            if self.verbose {
                return vec![format!("plak: tool {state}: {name}")];
            }
            return Vec::new();
        }
        let outcome = terminal_outcome(state);
        // The word stays before the name, exactly where the lifecycle lines put
        // it: a reader scanning `^plak: tool <state>: <name>` takes the name as
        // the first whitespace-delimited token, so a subject may follow it.
        let mut line = format!("plak: tool {outcome}: {name}");
        if let Some(subject) = subject.and_then(one_line) {
            line.push(' ');
            line.push_str(&subject);
        }
        if let Some(elapsed) = duration_ms {
            line.push_str(&format!(" in {}", human_duration(elapsed)));
        }
        let mut lines = Vec::new();
        if outcome == "done" {
            lines.push(line);
            lines.extend(self.truncation_note(name, detail));
            return lines;
        }
        // A call the operator's own interrupt stopped is not a failure of the
        // tool and must not be counted as one: the closing line would otherwise
        // tell a reader who pressed Ctrl-C that their tools had failed.
        if outcome == "cancelled" {
            lines.push(line);
            return lines;
        }
        self.failed_tools += 1;
        let typed_reason = detail.and_then(|raw| {
            crate::refusal_text::tool_error_text(
                name,
                raw,
                crate::notice_text::RemedyContext {
                    frontend: crate::notice_text::Frontend::Run,
                    session_id: self.session_id.as_deref(),
                },
            )
        });
        match typed_reason.or_else(|| failure_reason(detail)) {
            Some(reason) => line.push_str(&format!(": {reason}")),
            None => line.push_str(": no reason was reported"),
        }
        lines.push(line);
        lines.extend(self.truncation_note(name, detail));
        if let Some(hint) = self.delegation_hint(name) {
            lines.push(hint);
        }
        lines
    }

    /// The note a self-truncating tool earns, said once per tool name.
    ///
    /// This is the tool cutting its **own** output before the result ever left
    /// it, which is a different cut from the runtime clipping a returned result
    /// against the remaining context budget. That second one is the host's
    /// `observation clipped` note and arrives as a `SystemNote` with its own
    /// figures; the two must not read as the same event, so this one names the
    /// process bound it was cut at.
    ///
    /// A user driving a large read was told about truncation only if the model
    /// chose to mention it in prose. The tool marks its own output when it cuts
    /// it, so the face can say it directly, and name the bound the cut was made
    /// against.
    fn truncation_note(&mut self, name: &str, detail: Option<&str>) -> Option<String> {
        if !detail.is_some_and(|detail| detail.contains(OBSERVATION_TRUNCATED_MARKER)) {
            return None;
        }
        if self.announced_truncation.iter().any(|seen| seen == name) {
            return None;
        }
        self.announced_truncation.push(name.to_owned());
        Some(format!(
            "plak: tool {name} cut its own output at the {PROCESS_OBSERVATION_BYTES}-byte process observation bound; the model saw a prefix, not the whole output"
        ))
    }

    /// The one-line note a failed delegation earns, said at most once.
    ///
    /// Without it a run that could not delegate exits 0 with nothing on stderr
    /// but the word `failed`, and the reader has no way to learn that child
    /// agents are configuration rather than a missing feature.
    fn delegation_hint(&mut self, name: &str) -> Option<String> {
        if self.announced_delegation || !is_topology_tool(name) {
            return None;
        }
        let ChildAgents::Unavailable(reason) = &self.child_agents else {
            return None;
        };
        self.announced_delegation = true;
        Some(format!("plak: {reason}"))
    }
}

/// Tool names that ask the topology for a child agent.
fn is_topology_tool(name: &str) -> bool {
    matches!(
        name,
        "spawn_agent"
            | "send_agent_message"
            | "wait_agent"
            | "list_agents"
            | "adopt_candidate"
            | "discard_workspace"
    )
}

/// Whether this lifecycle label ends a call.
///
/// The host publishes `running` for progress and a settled label once. A
/// rejection carries its reason in the label itself (`rejected:<reason>`), so
/// the prefix is what is matched.
fn is_terminal_state(state: &str) -> bool {
    matches!(state, "done" | "failed" | "cancelled")
        || state.starts_with("rejected")
        || state.starts_with("failed:")
}

/// The word a settled call reads as.
fn terminal_outcome(state: &str) -> &'static str {
    if state == "done" {
        "done"
    } else if state == "cancelled" {
        "cancelled"
    } else if state.starts_with("rejected") || state.starts_with("failed:rejected") {
        "rejected"
    } else {
        "failed"
    }
}

/// A duration a person reads without converting it.
fn human_duration(millis: u64) -> String {
    if millis < 1_000 {
        return format!("{millis}ms");
    }
    let seconds = millis / 1_000;
    if seconds < 60 {
        return format!("{}.{}s", seconds, (millis % 1_000) / 100);
    }
    format!("{}m{:02}s", seconds / 60, seconds % 60)
}

/// The typed failure reason, on one line.
///
/// The host's `detail` is a bounded preview of the call's arguments followed by
/// its result, so the reason is the text after the runtime's own failure
/// anchor. Absent the anchor the last non-empty line is the closest honest
/// answer, and absent any text at all the caller says that no reason came.
fn failure_reason(detail: Option<&str>) -> Option<String> {
    let detail = detail?;
    let tail = detail
        .rfind(FAILURE_ANCHOR)
        .map(|index| &detail[index + FAILURE_ANCHOR.len()..]);
    let reason = match tail {
        Some(tail) => tail.lines().next()?.trim(),
        None => detail
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())?,
    };
    if reason.is_empty() {
        return None;
    }
    Some(clip(reason, MAX_REASON_BYTES))
}

/// One announced replay: `retrying in 1.5s (2/4): provider busy (HTTP 503)`,
/// noting when the failed request's streamed output was discarded.
fn retry_line(retry: &plakous_host_protocol::ModelRetryNotice) -> String {
    let seconds = std::time::Duration::from_millis(retry.delay_ms).as_secs_f32();
    let discarded = if retry.output_discarded {
        "; partial output discarded"
    } else {
        ""
    };
    format!(
        "retrying in {seconds:.1}s ({}/{}): {}{discarded}",
        retry.next_attempt, retry.max_attempts, retry.cause
    )
}

/// A host note as one stderr line, never prefixed twice.
///
/// Some host notes already carry the `plak: ` prefix because they are also read
/// out of the session log. Stripping it keeps `plak: plak: ...` off the face.
fn prefixed(note: &str) -> String {
    let note = note.strip_prefix("plak: ").unwrap_or(note);
    format!("plak: {}", note.replace('\n', " "))
}

/// A subject flattened to one line, or nothing when it is empty.
fn one_line(subject: &str) -> Option<String> {
    let flattened = subject.replace('\n', " ");
    let trimmed = flattened.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(clip(trimmed, MAX_REASON_BYTES))
}

/// Clip on a character boundary, marking that the text was cut.
fn clip(text: &str, maximum: usize) -> String {
    if text.len() <= maximum {
        return text.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// What a resolved tool policy means for the tools the model can call.
///
/// The walkthrough's reader was told `the read_file tool is not available in
/// this session` and had no way to find out why. The policy label alone does
/// not answer that; the shape of the call does.
fn tool_policy_explanation(policy: &str) -> &'static str {
    let lower = policy.to_ascii_lowercase();
    if lower.contains("direct") {
        "the model calls plak's tools directly"
    } else if lower == "py-sdk" {
        "the model uses the typed Python SDK in fresh, type-checked cells"
    } else if lower.contains("js") {
        "the model calls plak's tools from JavaScript inside a Code Mode cell"
    } else if lower.contains("py") {
        "the model calls plak's tools from Python inside a Code Mode cell"
    } else if lower.contains("code mode") || lower.contains("ptc") {
        "the model calls plak's tools from code inside a Code Mode cell"
    } else {
        "change it with --composition"
    }
}

#[cfg(test)]
mod tests;
