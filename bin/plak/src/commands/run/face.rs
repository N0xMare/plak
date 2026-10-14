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
    verbose: bool,
    child_agents: ChildAgents,
    announced_session: bool,
    announced_delegation: bool,
    announced_truncation: Vec<String>,
    failed_tools: usize,
}

impl CliFace {
    /// A face for one run.
    pub(crate) const fn new(verbose: bool, child_agents: ChildAgents) -> Self {
        Self {
            verbose,
            child_agents,
            announced_session: false,
            announced_delegation: false,
            announced_truncation: Vec::new(),
            failed_tools: 0,
        }
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
            HostEvent::Row(TranscriptRow::Notice(notice))
                if notice.placement() != NoticePlacement::SessionLog =>
            {
                vec![prefixed(&notice.text)]
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
        match failure_reason(detail) {
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
mod tests {
    use super::*;
    use plakous_host_protocol::views::presentation::{ActivityKind, SessionInfoView};

    fn tool_line(state: &str, detail: Option<&str>, duration_ms: Option<u64>) -> HostEvent {
        HostEvent::ToolLine {
            call_id: Some("call-1".into()),
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Tool,
            name: "read_file".into(),
            state: state.into(),
            subject: Some("odd/missing.txt".into()),
            detail: detail.map(str::to_owned),
            duration_ms,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        }
    }

    /// A replay is one stderr line naming the wait, the budget and the
    /// cause, and saying when the partial output was thrown away.
    #[test]
    fn record_rejected_retry_label_is_honest() {
        let cause: plakous::model::ModelRetryCause =
            serde_json::from_str(r#"{"kind":"record_rejected_in_transit"}"#).unwrap();
        let notice = HostEvent::ModelRetry(plakous_host_protocol::ModelRetryNotice {
            next_attempt: 2,
            max_attempts: 4,
            delay_ms: 0,
            cause_kind: cause.kind().into(),
            cause: cause.to_string(),
            output_discarded: false,
        });
        assert_eq!(
            face().lines(&notice),
            vec![
                "plak: retrying in 0.0s (2/4): request corrupted in transit (TLS record rejected)"
            ]
        );
    }

    #[test]
    fn a_model_retry_is_one_line_with_its_wait_budget_and_cause() {
        let mut face = face();
        let retry = |output_discarded| {
            HostEvent::ModelRetry(plakous_host_protocol::ModelRetryNotice {
                next_attempt: 2,
                max_attempts: 4,
                delay_ms: 1_500,
                cause_kind: "stream_interrupted".into(),
                cause: "response stream interrupted (connection reset)".into(),
                output_discarded,
            })
        };
        assert_eq!(
            face.lines(&retry(false)),
            vec!["plak: retrying in 1.5s (2/4): response stream interrupted (connection reset)"]
        );
        assert_eq!(
            face.lines(&retry(true)),
            vec![
                "plak: retrying in 1.5s (2/4): response stream interrupted (connection reset); partial output discarded"
            ]
        );
    }

    fn face() -> CliFace {
        CliFace::new(false, ChildAgents::Available)
    }

    #[test]
    fn a_running_tool_is_silent_and_its_completion_is_one_line() {
        let mut face = face();
        assert!(face.lines(&tool_line("running", None, None)).is_empty());
        assert_eq!(
            face.lines(&tool_line("done", Some("ok"), Some(1_234))),
            vec!["plak: tool done: read_file odd/missing.txt in 1.2s"]
        );
    }

    #[test]
    fn verbose_keeps_the_progress_line() {
        let mut face = CliFace::new(true, ChildAgents::Available);
        assert_eq!(
            face.lines(&tool_line("running", None, None)),
            vec!["plak: tool running: read_file"]
        );
    }

    #[test]
    fn a_failure_carries_its_typed_reason_on_the_same_line() {
        let mut face = face();
        let detail = "{\"path\":\"odd/missing.txt\"}\ntool invocation failed: odd/missing.txt does not exist in the workspace";
        assert_eq!(
            face.lines(&tool_line("failed", Some(detail), Some(30))),
            vec![
                "plak: tool failed: read_file odd/missing.txt in 30ms: odd/missing.txt does not exist in the workspace"
            ]
        );
    }

    #[test]
    fn a_failure_without_a_reason_says_so_rather_than_nothing() {
        let mut face = face();
        assert_eq!(
            face.lines(&tool_line("failed", None, None)),
            vec!["plak: tool failed: read_file odd/missing.txt: no reason was reported"]
        );
    }

    #[test]
    fn a_recovered_failure_reads_as_an_event_once_the_turn_succeeds() {
        let mut face = face();
        let _ = face.lines(&tool_line("failed", None, None));
        let _ = face.lines(&tool_line("failed", None, None));
        assert_eq!(
            face.closing_line(true).unwrap(),
            "plak: 2 tool calls failed during this turn; the turn completed"
        );
        assert_eq!(face.closing_line(false), None);
    }

    #[test]
    fn a_clean_turn_has_no_closing_line() {
        let mut face = face();
        let _ = face.lines(&tool_line("done", None, Some(5)));
        assert_eq!(face.closing_line(true), None);
    }

    #[test]
    fn the_session_line_names_the_composition_and_what_it_means() {
        let mut face = face();
        let info = SessionInfoView {
            tool_policy: Some("Direct".into()),
            ..SessionInfoView::default()
        };
        assert_eq!(
            face.lines(&HostEvent::SessionInfo(info.clone())),
            vec!["plak: composition Direct: the model calls plak's tools directly"]
        );
        // Said once, however often the host republishes the projection.
        assert!(face.lines(&HostEvent::SessionInfo(info)).is_empty());
    }

    #[test]
    fn a_code_mode_session_says_tools_are_called_from_code() {
        let mut face = face();
        let info = SessionInfoView {
            tool_policy: Some("Code Mode (native-ptc)".into()),
            ..SessionInfoView::default()
        };
        assert_eq!(
            face.lines(&HostEvent::SessionInfo(info)),
            vec![
                "plak: composition Code Mode (native-ptc): the model calls plak's tools from code inside a Code Mode cell"
            ]
        );
    }

    #[test]
    fn an_unconfigured_delegation_names_what_is_missing_and_where() {
        let reason = "delegation is off: no child agent templates; set [orchestration] subagents and [roster] working_set in /home/x/.plak/config.toml".to_owned();
        let mut face = CliFace::new(false, ChildAgents::Unavailable(reason.clone()));
        let info = SessionInfoView {
            tool_policy: Some("Direct".into()),
            ..SessionInfoView::default()
        };
        assert_eq!(
            face.lines(&HostEvent::SessionInfo(info)),
            vec![format!(
                "plak: composition Direct: the model calls plak's tools directly; {reason}"
            )]
        );
    }

    #[test]
    fn a_delegation_failure_names_the_configuration_when_the_session_never_opened() {
        let reason = "delegation is off: no child agent templates".to_owned();
        let mut face = CliFace::new(false, ChildAgents::Unavailable(reason.clone()));
        let lines = face.lines(&HostEvent::ToolLine {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Agent,
            name: "send_agent_message".into(),
            state: "failed".into(),
            subject: None,
            detail: Some("tool invocation failed: no such agent".into()),
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
        assert_eq!(
            lines,
            vec![
                "plak: tool failed: send_agent_message: no such agent".to_owned(),
                format!("plak: {reason}"),
            ]
        );
    }

    /// Truncation used to reach the reader only through the model's prose, if
    /// the model chose to mention it. It is a fact of the run and says so.
    #[test]
    fn a_truncated_result_says_so_with_the_bound_it_was_cut_at() {
        let mut face = face();
        let detail = "$ find /\n... output ...\n[observation truncated]";
        let lines = face.lines(&HostEvent::ToolLine {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Process,
            name: "exec".into(),
            state: "done".into(),
            subject: None,
            detail: Some(detail.into()),
            duration_ms: Some(900),
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
        assert_eq!(
            lines,
            vec![
                "plak: tool done: exec in 900ms".to_owned(),
                format!(
                    "plak: tool exec cut its own output at the {PROCESS_OBSERVATION_BYTES}-byte process observation bound; the model saw a prefix, not the whole output"
                ),
            ]
        );
        // Said once per tool, not once per call, so a loop cannot flood stderr.
        let repeat = face.lines(&HostEvent::ToolLine {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Process,
            name: "exec".into(),
            state: "done".into(),
            subject: None,
            detail: Some(detail.into()),
            duration_ms: Some(10),
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
        assert_eq!(repeat, vec!["plak: tool done: exec in 10ms"]);
    }

    /// The two cuts are different events and must not read as one.
    ///
    /// The tool cutting its own output before returning is this module's note.
    /// The runtime clipping a returned result against the remaining context
    /// budget is the host's `observation clipped` note, which carries its own
    /// figures. A reader who sees both must be able to tell them apart.
    ///
    /// They are not alternatives: both can fire for one call, in that order.
    /// The process tool cuts first, so what reaches the observation budget is
    /// already the cut version, and the budget may cut it again. The host's
    /// `bytes_dropped` counts only what *it* removed from what it was given, so
    /// the two notes account for successive cuts and add up rather than
    /// contradict.
    ///
    /// Which is why this pins distinguishability on the text and not on the
    /// numbers: the process bound and the remaining budget can coincide, as
    /// they deliberately do here, and the notes must still read as two events.
    #[test]
    fn the_host_clip_note_and_the_tool_self_cut_are_distinguishable() {
        let mut face = face();
        let host = "observation clipped: exec returned more than the remaining observation budget; 40000 bytes kept, 182000 dropped, against a 40000-byte bound. The model is answering from the kept portion; narrow the call to see the rest.";
        assert_eq!(
            face.lines(&HostEvent::Row(TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                host
            ))),
            vec![format!("plak: {host}")]
        );
        let own = face.lines(&HostEvent::ToolLine {
            call_id: None,
            parent_call_id: None,
            cell_id: None,
            kind: ActivityKind::Process,
            name: "exec".into(),
            state: "done".into(),
            subject: None,
            detail: Some("$ find /\n[observation truncated]".into()),
            duration_ms: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        });
        assert!(own[1].contains("cut its own output"), "{own:?}");
        assert!(!own[1].contains("observation clipped"), "{own:?}");
    }

    /// The compaction ladder publishes its own note; the face must carry it
    /// through as one line rather than dropping it as it dropped the rest.
    #[test]
    fn a_committed_compaction_rung_reaches_the_face_as_one_line() {
        let mut face = face();
        let note = "replaced 3 oldest tool result(s) (81920 bytes) with placeholders to fit the context window; 190000 → 120000 projected input tokens. Re-run a call to see its content again.";
        let lines = face.lines(&HostEvent::Row(TranscriptRow::info(
            plakous_host_protocol::views::transcript::NoticeScope::Host,
            note,
        )));
        assert_eq!(lines, vec![format!("plak: {note}")]);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains('\n'));
    }

    /// Suppressing the progress line must not make an interrupted tool silent.
    ///
    /// The host settles a call caught inside a cancel window as `cancelled` and
    /// publishes that terminal, so the call still speaks once. It is not
    /// counted as a failure: a reader who pressed Ctrl-C must not be told their
    /// tools failed.
    #[test]
    fn an_interrupted_tool_still_speaks_and_is_not_counted_as_a_failure() {
        let mut face = face();
        assert!(face.lines(&tool_line("running", None, None)).is_empty());
        assert_eq!(
            face.lines(&tool_line("cancelled", None, Some(400))),
            vec!["plak: tool cancelled: read_file odd/missing.txt in 400ms"]
        );
        assert_eq!(face.closing_line(true), None);
    }

    #[test]
    fn a_host_note_is_never_prefixed_twice() {
        let mut face = face();
        assert_eq!(
            face.lines(&HostEvent::Row(TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "plak: this session's output budget was re-derived"
            ))),
            vec!["plak: this session's output budget was re-derived"]
        );
        assert_eq!(
            face.lines(&HostEvent::Row(TranscriptRow::info(
                plakous_host_protocol::views::transcript::NoticeScope::Host,
                "replaced 1 oldest tool result"
            ))),
            vec!["plak: replaced 1 oldest tool result"]
        );
    }

    #[test]
    fn a_long_reason_is_clipped_on_a_character_boundary() {
        let reason = "é".repeat(400);
        let detail = format!("tool invocation failed: {reason}");
        let mut face = face();
        let lines = face.lines(&tool_line("failed", Some(&detail), None));
        assert!(lines[0].ends_with('…'), "{lines:?}");
    }

    #[test]
    fn durations_read_without_conversion() {
        assert_eq!(human_duration(30), "30ms");
        assert_eq!(human_duration(1_234), "1.2s");
        assert_eq!(human_duration(33_000), "33.0s");
        assert_eq!(human_duration(125_000), "2m05s");
    }
}
