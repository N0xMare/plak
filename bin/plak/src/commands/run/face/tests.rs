use super::*;
use plakous_host_protocol::views::presentation::{ActivityKind, SessionInfoView};

#[test]
fn approval_denial_names_the_tool_and_does_not_repeat_an_offered_id() {
    let mut face = face();
    let mut approval = HostEvent::ApprovalRequired {
        approval_id: "approval-one".into(),
        call_id: Some("call-one".into()),
        tool_name: "loopback_effect".into(),
        subject: None,
        prompt: None,
        expires_at_ms: 123,
    };
    assert_eq!(
        face.lines(&approval),
        ["plak: denied approval for loopback_effect: run is non-interactive"]
    );
    assert!(
        face.lines(&approval).is_empty(),
        "a repeated offer must not print another denial"
    );
    if let HostEvent::ApprovalRequired { approval_id, .. } = &mut approval {
        *approval_id = "approval-two".into();
    }
    assert_eq!(
        face.lines(&approval).len(),
        1,
        "a new decision on the same tool is still denied"
    );
}

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
        vec!["plak: retrying in 0.0s (2/4): request corrupted in transit (TLS record rejected)"]
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
    assert_eq!(
        face.lines(&tool_line("running", None, None)),
        [] as [String; 0]
    );
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
    assert_eq!(face.lines(&HostEvent::SessionInfo(info)), [] as [String; 0]);
}

#[test]
fn python_composition_lines_name_the_observable_selection() {
    for (policy, explanation) in [
        (
            "py-ptc",
            "the model calls plak's tools from Python inside a Code Mode cell",
        ),
        (
            "py-sdk",
            "the model uses the typed Python SDK in fresh, type-checked cells",
        ),
    ] {
        let info = SessionInfoView {
            tool_policy: Some(policy.into()),
            ..SessionInfoView::default()
        };
        assert_eq!(
            face().lines(&HostEvent::SessionInfo(info)),
            vec![format!("plak: composition {policy}: {explanation}")]
        );
    }
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
    assert_eq!(
        face.lines(&tool_line("running", None, None)),
        [] as [String; 0]
    );
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
