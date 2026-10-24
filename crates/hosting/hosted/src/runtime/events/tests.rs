#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

#[test]
fn retry_notice_names_the_network_phase() {
    use plakous::model::{ModelRetry, ModelRetryCause, ModelRetryLoss, ModelRetryPhase};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let uuid = uuid::Uuid::from_u128(1);
    let retry = ModelRetry::new(
        2,
        4,
        Duration::from_secs(2),
        ModelRetryCause::Connect,
        false,
        "raw transport detail",
    )
    .with_loss(ModelRetryLoss {
        transport_phase: Some(ModelRetryPhase::Resolution),
        ..ModelRetryLoss::default()
    });
    let event: AgentEvent = serde_json::from_value(serde_json::json!({
        "branch_sequence": 1, "event_id": uuid, "session_id": uuid, "branch_id": uuid,
        "agent_id": uuid, "run_id": uuid, "turn_id": null, "attempt_id": null, "call_id": null,
        "timestamp": {"seconds": 1, "nanoseconds": 0}, "causal_parent": null,
        "origin": "Model", "classification": "Public", "event_schema": 3,
        "projection_revision": 6, "visibility": "Content", "scope": "Agent", "event_kind_version": 11,
        "kind": {"Model": {"kind": "Retry", "event": {"Retry": retry}}},
        "retained_bytes": 512, "metadata_retained_bytes": 512, "content_retained_bytes": 0
    })).unwrap();
    let (events, rx) = host_event_sink();
    apply_event(
        &event,
        &roots,
        &rows,
        &store,
        &home,
        &mut session,
        &events,
        &mut String::new(),
        &mut String::new(),
        &mut Instant::now(),
        &mut ToolSubjectStash::new(),
        &mut TurnProjection::default(),
        None,
        "test",
        &mut TreeSnapshotPush::default(),
        &mut false,
        &mut 0,
    );
    let notice = rx
        .try_iter()
        .find_map(|event| match event {
            HostEvent::ModelRetry(notice) => Some(notice),
            _ => None,
        })
        .expect("retry notice");
    assert_eq!(
        notice.cause,
        "provider unreachable (DNS lookup failed before anything was sent); waiting for network, try 2 in 2s"
    );
    assert_eq!(notice.cause_kind, "connect");
}

#[test]
fn completed_round_recovery_notice_does_not_invent_tool_results() {
    let line = abandoned_turn_operator_detail(
        plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
        plakous::agent::AbandonmentCause::ProcessLost,
        true,
        true,
    );
    assert!(line.contains("last round completed"));
    assert!(line.contains("before the next model request"));
    assert!(line.contains("completed work was kept"));
    assert!(!line.contains("tool results"));
}

#[test]
fn a2_real_tool_round_keeps_original_operator_line() {
    assert_eq!(
        abandoned_turn_operator_detail(
            plakous::agent::AbandonedTurnClaim::ToolRoundCompleted,
            plakous::agent::AbandonmentCause::ProcessLost,
            true,
            false
        ),
        "plak: recovered an interrupted turn; its tool round completed, but the turn was interrupted before the next model request; completed tool results were kept"
    );
}

#[test]
fn recovery_review_operator_line_distinguishes_ledger_from_conversation() {
    for cause in [
        plakous::agent::AbandonmentCause::ProcessLost,
        plakous::agent::AbandonmentCause::Interrupted,
    ] {
        assert_eq!(
            abandoned_turn_operator_detail(
                plakous::agent::AbandonedTurnClaim::RunningTool,
                cause,
                false,
                false
            ),
            "plak: recovered an interrupted turn; a tool may have changed this workspace; \
                 its result was not added to the conversation and it is not retried - check \
                 the workspace before carrying on"
        );
    }
}

#[test]
fn process_wait_uses_its_result_after_timeout_exit_race() {
    use plakous::model::{Digest, DigestDomain};
    use plakous::tools::{JobHost, JobOwner, JobStatus, JobView};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let empty_roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("empty-state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    session.plakous_locator = Some(locator);
    let (events, rx) = host_event_sink();
    let mut projection = TurnProjection::default();
    let mut stash = ToolSubjectStash::new();
    let mut assistant = String::new();
    let mut delta = String::new();
    let mut last_flush = Instant::now();
    let mut tree = TreeSnapshotPush::default();
    let mut compacted = false;
    let mut peak = 0;
    let uuid = uuid::Uuid::from_u128(1);
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    {
        let mut dispatch = |roots: &crate::agent::ProductStateRoots,
                            name: &str,
                            call: u128,
                            kind: &str,
                            observation: serde_json::Value| {
            let identity = serde_json::json!({
                "canonical_name": name,
                "exposed_name": null,
                "exposure_map_digest": null,
                "registered_tool_id": "test.process",
                "executor_id": "test.process",
                "definition_digest": digest,
                "schema_digest": digest,
                "effect_metadata_digest": digest,
                "argument_limits_digest": digest,
            });
            let event: AgentEvent = serde_json::from_value(serde_json::json!({
                "branch_sequence": 1,
                "event_id": uuid,
                "session_id": locator.session_id(),
                "branch_id": locator.branch_id(),
                "agent_id": uuid,
                "run_id": uuid,
                "turn_id": null,
                "attempt_id": null,
                "call_id": uuid::Uuid::from_u128(call),
                "timestamp": {"seconds": 1, "nanoseconds": 0},
                "causal_parent": null,
                "origin": "Tool",
                "classification": "Public",
                "event_schema": 3,
                "projection_revision": 6,
                "visibility": "Content",
                "scope": "Agent",
                "event_kind_version": 11,
                "kind": {kind: {
                    "identity": identity,
                    "lineage": plakous::agent::ToolCallerLineageView::direct(),
                    "observation": observation,
                }},
                "retained_bytes": 512,
                "metadata_retained_bytes": 512,
                "content_retained_bytes": 0,
            }))
            .unwrap();
            apply_event(
                &event,
                roots,
                &rows,
                &store,
                &home,
                &mut session,
                &events,
                &mut assistant,
                &mut delta,
                &mut last_flush,
                &mut stash,
                &mut projection,
                None,
                "test",
                &mut tree,
                &mut compacted,
                &mut peak,
            );
        };
        let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
        let completed = |result: serde_json::Value| serde_json::json!({"succeeded":true,"preview":result.to_string()});
        dispatch(
            &roots,
            "process_session_start",
            10,
            "ToolStarted",
            started(serde_json::json!({"command":"cargo test"})),
        );
        dispatch(
            &roots,
            "process_session_start",
            10,
            "ToolCompleted",
            completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
        );
        dispatch(
            &roots,
            "process_session_poll",
            11,
            "ToolStarted",
            started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
        );
        // The wait timed out, then the process exited before the host consumed it.
        roots.job_registry().observe_terminal(
            JobOwner {
                session_id: locator.session_id(),
                branch_id: locator.branch_id(),
                agent_id: plakous::model::AgentId::try_from(uuid).unwrap(),
            },
            &[JobView {
                number: 7,
                command: "cargo test".into(),
                started_at: plakous::model::Timestamp::new(1, 0).unwrap(),
                owner: "root".into(),
                elapsed: Duration::ZERO,
                status: JobStatus::Exited(Some(0)),
                output: String::new(),
                session_id: [7; 16],
                owner_generation: 1,
            }],
        );
        dispatch(
            &roots,
            "process_session_poll",
            11,
            "ToolCompleted",
            completed(serde_json::json!({"session_id":7,"status":"running","exit_code":null})),
        );
        // A later wait still knows the start row's command after registry eviction.
        dispatch(
            &empty_roots,
            "process_session_poll",
            12,
            "ToolStarted",
            started(serde_json::json!({"session_id":7,"yield_time_ms":1000})),
        );
        dispatch(
            &empty_roots,
            "process_session_poll",
            12,
            "ToolCompleted",
            completed(
                serde_json::json!({"tasks":[{"task_id":"p7","status":"exited","exit_code":0}]}),
            ),
        );
    }
    let emitted = rx.try_iter().collect::<Vec<_>>();
    for (call, expected) in [
        (11, "Waited on process 7 · cargo test · still running"),
        (12, "Waited on process 7 · cargo test · exited 0"),
    ] {
        let id = uuid::Uuid::from_u128(call).to_string();
        assert_eq!(
            session
                .transcript
                .tool_activity(&id)
                .unwrap()
                .subject
                .as_deref(),
            Some(expected)
        );
        assert!(emitted.iter().any(|event| matches!(event, HostEvent::ToolLine { call_id:Some(call_id), subject:Some(subject), .. } if call_id == &id && subject == expected)));
    }
}

#[test]
fn live_tool_subject_redaction_is_idempotent() {
    let subject = live_tool_subject("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list");
    assert_eq!(subject, "GITHUB_TOKEN=[redacted] gh pr list");
    assert_eq!(live_tool_subject(&subject), subject);
}

#[test]
fn live_and_restored_subjects_match_at_the_byte_bound() {
    for (raw, expected) in [
        (
            format!("password={} safe subject", "x".repeat(230)),
            Some("password=[redacted] safe subject"),
        ),
        (
            "GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list".into(),
            Some("GITHUB_TOKEN=[redacted] gh pr list"),
        ),
        ("password=a ".repeat(30), None),
    ] {
        let subject = live_tool_subject(&raw);
        let encoded = sessions::ToolItem {
            call_id: "call-1".into(),
            parent_call_id: None,
            cell_id: None,
            name: "read_file".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("read_file"),
            state: "done".into(),
            subject: Some(subject.clone()),
            detail: None,
            started_at_ms: None,
            finished_at_ms: None,
            wait: None,
        };
        let restored: sessions::ToolItem =
            serde_json::from_value(serde_json::to_value(&encoded).unwrap()).unwrap();
        assert_eq!(restored.subject.as_deref(), Some(subject.as_str()));
        if let Some(expected) = expected {
            assert_eq!(subject, expected);
        } else {
            assert!(subject.ends_with('…'));
        }
        assert!(subject.len() <= 240);
    }
}

/// A reopen whose route moved says both figures and the one way to the new one.
///
/// The session keeps the budget it was created with, because that figure is
/// identity-bearing and the persisted artefacts cannot be re-signed. So an
/// old session on a route that has since grown is starved, and the line has
/// to say so: without the larger figure and the instruction, the operator
/// has no way to learn the budget exists or how to reach it.
#[test]
#[allow(
    unused_qualifications,
    reason = "the test module has no NonZeroU32 import of its own"
)]
fn the_rederived_bound_note_names_both_figures() {
    let previous = std::num::NonZeroU32::new(4_096);
    let current = std::num::NonZeroU32::new(32_768);
    let note = output_bound_rederived_note(previous, current);
    assert!(note.contains("keeps its output budget"), "{note}");
    assert!(note.contains("4096"), "{note}");
    assert!(note.contains("32768"), "{note}");
    assert!(note.contains("start a new session"), "{note}");
    // A session created with no bound at all reads honestly, not as zero.
    let unbounded = output_bound_rederived_note(None, current);
    assert!(unbounded.contains("none"), "{unbounded}");
    assert!(unbounded.contains("32768"), "{unbounded}");
}

/// A route that reports usage only on its terminal response still peaks.
///
/// 2026-09-18, live attempt 3: `peak_prepared_input_tokens` was null on
/// every position on every route while the other new fields populated.
/// The peak was recorded only from a mid-stream `ModelEvent::Usage`, which
/// several routes never emit — the driver itself prefers the terminal
/// response's counts (`response.usage().or(observed)`). Recording at the
/// terminal response is what makes it route-independent.
#[test]
fn the_prepared_input_peak_records_a_terminal_response_usage() {
    let mut session = SessionRecord::new(std::env::temp_dir());
    let mut peak = 0_u64;
    // No mid-stream update at all, exactly like the routes that reported
    // null: only the operation's terminal counts.
    for input in [12_000_u64, 31_500, 27_000] {
        record_operation_prepared_input(
            &mut peak,
            &mut session,
            &plakous::model::UsageUpdate::new(plakous::model::Usage::new(input, 1, 0, 0, 0)),
            None,
        );
    }
    assert_eq!(peak, 31_500, "the largest single operation, not the sum");
    assert_ne!(peak, 0, "a turn with model operations must not report null");
}

use super::*;
use crate::runtime::*;

use crate::runtime::test_support::*;

use crate::sessions::{self, SessionRecord};

#[test]
fn a_history_projection_diagnostic_goes_to_the_session_log_only() {
    use plakous::model::{Digest, DigestDomain, ModelDiagnostic};

    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    let session = SessionRecord::new(dir.path().to_path_buf());
    let log = SessionLog::open(&home, &session.id).unwrap();
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let identity = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"first"]).unwrap();
    publish_model_diagnostic(
        &ModelDiagnostic::historical_projection(identity, "projected history"),
        Some(&log),
        &session.id,
        &events,
    );
    assert!(rx.try_iter().next().is_none(), "never rendered");
    assert!(session.transcript.is_empty(), "never persisted");
    let logged = crate::session_log::read_tail_for_kind(
        &home,
        &session.id,
        4,
        Some("model.history_projection"),
    )
    .unwrap();
    assert_eq!(logged.len(), 1);
    assert!(logged[0].contains(&identity.to_string()), "{logged:?}");
}

#[test]
fn a_generic_model_diagnostic_is_a_live_turn_notice_and_never_persisted() {
    use plakous::model::ModelDiagnostic;

    let dir = tempfile::tempdir().unwrap();
    let session = SessionRecord::new(dir.path().to_path_buf());
    let (tx, rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    for _ in 0..2 {
        publish_model_diagnostic(
            &ModelDiagnostic::new("same text"),
            None,
            &session.id,
            &events,
        );
    }
    assert_eq!(
        rx.try_iter().collect::<Vec<_>>(),
        vec![
            HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
            HostEvent::Row(TranscriptRow::info(NoticeScope::Turn, "same text")),
        ]
    );
    assert!(session.transcript.is_empty());
}

#[test]
fn the_compact_row_subject_uses_occupancy_not_prepared_padding() {
    let occupancy =
        compact_terminal_subject(Some(44_900), Some(44_900), true, true).expect("occupancy sizes");
    assert_eq!(occupancy, "44.9k → 44.9k");
    let dropped =
        compact_terminal_subject(Some(395_000), Some(44_900), true, true).expect("overlay drop");
    assert_eq!(dropped, "395.0k → 44.9k");
    // The row states the successor's occupancy figures: the anchored
    // context measure before, the successor's `bytes÷4` after. The
    // prepared-input bound (395k → 223k) is the Context tab's admission
    // fact, not the row.
    let summary = CompactionSummary {
        rung: CompactionRung::ToolResultPlaceholders,
        segments: 1,
        input_tokens_before: 395_000,
        input_tokens_after: 222_700,
        occupancy_tokens_before: Some(138_600),
        occupancy_tokens_after: Some(44_900),
    };
    let (before, after) = compact_row_token_pair(Some(summary));
    let subject = compact_terminal_subject(before, after, true, true).expect("live overlay");
    assert_eq!(subject, "138.6k → 44.9k");
    let (prepared_before, prepared_after) = compact_row_token_pair(Some(CompactionSummary {
        occupancy_tokens_before: None,
        occupancy_tokens_after: None,
        ..summary
    }));
    assert_eq!(prepared_before, Some(395_000));
    assert_eq!(prepared_after, Some(222_700));
    assert_ne!(
        compact_terminal_subject(prepared_before, prepared_after, true, true).as_deref(),
        Some(subject.as_str()),
        "prepared-input padding is not Compact chrome when occupancy is known"
    );
}

#[test]
fn the_compact_row_subject_names_before_and_after_sizes() {
    let subject = compact_terminal_subject(Some(36_617), Some(852), true, true).expect("sizes");
    assert!(
        subject.contains('→') || subject.contains("36.6k"),
        "{subject}"
    );
    assert!(
        subject.contains("852") || subject.contains("0.9k"),
        "{subject}"
    );
}

/// The `wait` row sat flat between
/// children three and four of its cell. The host knows the cell's marker,
/// so the continuation folds under it — without being counted as a child.
/// Codex `exec` arguments are raw program
/// source carried as one JSON string; a Plak PTC body carries the program
/// under `source`. The Compose rail shows the program, never its encoding.
#[test]
fn a_compose_row_carries_its_program_source_as_detail() {
    assert_eq!(
        compose_program_source(Some(
            "// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});"
        ))
        .as_deref(),
        Some("// yield_time_ms=30000\n\nconst r = await tools.read_file({path: \"src/lib.rs\"});")
    );
    assert_eq!(
        compose_program_source(Some(
            r#""const spawned = await tools.spawn_agent({\n  class: \"review\"\n});""#
        ))
        .as_deref(),
        Some("const spawned = await tools.spawn_agent({\n  class: \"review\"\n});")
    );
    assert_eq!(
        compose_program_source(Some(
            r#"{"description":"add gcd and its test","source":"const x = 1;"}"#
        ))
        .as_deref(),
        Some("const x = 1;")
    );
    assert_eq!(
        compose_program_source(Some(r#"{"source":"\n# pragma\nprint(1)"}"#)).as_deref(),
        Some("# pragma\nprint(1)")
    );
    // Nothing to say stays nothing: no invented detail.
    assert_eq!(compose_program_source(None), None);
    assert_eq!(compose_program_source(Some("   \n\n")), None);

    // §5.8: the header is the host-counted tally alone.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    assert_eq!(
        compose_counts_label(projection.cells.get("exec-1").expect("compose"), false),
        "1 tools"
    );
    projection.open_compose("exec-bare", "exec", Some("cell-bare"));
    assert_eq!(
        compose_counts_label(projection.cells.get("exec-bare").expect("compose"), false),
        "0 tools"
    );
}

#[test]
fn wait_continuation_folds_under_its_cell_marker_without_counting() {
    use plakous::agent::ToolCallerLineageView;

    let mut projection = TurnProjection::<TestExtension>::default();
    let wait = ToolCallerLineageView::local_composition_marker(Some("1"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection),
        None,
        "a cell the host never opened stays a flat row"
    );
    projection.open_compose("exec-1", "exec", Some("1"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    assert_eq!(
        continuation_parent_call_id("wait_js", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    assert_eq!(
        continuation_parent_call_id("wait_py", &wait, &projection).as_deref(),
        Some("exec-1")
    );
    // Only a continuation folds: the marker itself and a plain tool do not.
    assert_eq!(
        continuation_parent_call_id("exec", &wait, &projection),
        None
    );
    assert_eq!(
        continuation_parent_call_id("read_file", &wait, &projection),
        None
    );
    let other_cell = ToolCallerLineageView::local_composition_marker(Some("2"));
    assert_eq!(
        continuation_parent_call_id("wait", &other_cell, &projection),
        None
    );
    assert_eq!(
        projection.cells["exec-1"].child_count(),
        0,
        "projecting a parent onto the row tallies nothing"
    );
}

/// A cell that yields before any nested call never named
/// its cell on a start event, so the `wait*` continuation was a flat row on
/// a broken rail and never re-terminalised its compose. The marker's own
/// terminal lineage carries the retired cell.
#[test]
fn a_cell_that_yields_before_any_child_learns_its_cell_from_the_marker_terminal() {
    use plakous::agent::ToolCallerLineageView;

    let mut projection = TurnProjection::<TestExtension>::default();
    // The interpreter had not minted the cell when the marker started.
    projection.open_compose("exec-1", "exec", None);
    let wait = ToolCallerLineageView::local_composition_marker(Some("cell-9"));
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection),
        None,
        "nothing is guessed before the host has observed the ownership"
    );

    projection.learn_marker_cell("exec-1", Some("cell-9"));
    assert_eq!(
        projection.cells["exec-1"].cell_id.as_deref(),
        Some("cell-9")
    );
    assert_eq!(
        continuation_parent_call_id("wait", &wait, &projection).as_deref(),
        Some("exec-1"),
        "the wait continuation folds under the cell's own row"
    );
    // A cell the host already knows is never overwritten by a later one.
    projection.learn_marker_cell("exec-1", Some("cell-other"));
    assert_eq!(
        projection.cells["exec-1"].cell_id.as_deref(),
        Some("cell-9")
    );
}

/// A cell yielded, a child dispatched during the wait put the marker's
/// persisted row back to `running`, and nothing wrote it terminal again, so
/// the `Completed` sweep rewrote a turn that succeeded to
/// `failed:observation-gap` — live and on resume.
/// A Compose head's line carries the cell's persisted start and, once the
/// head is terminal, the finish its duration measures to: the stamps a
/// group's elapsed span reads live, as the restored row has them.
#[test]
fn a_compose_update_carries_the_cell_stamps() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    session.upsert_tool_activity(sessions::ToolItem {
        call_id: "exec-1".into(),
        name: "exec".into(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("exec"),
        state: "running".into(),
        subject: None,
        detail: None,
        started_at_ms: Some(1_000),
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: Some("cell-1".into()),
        wait: None,
    });
    let (events, rx) = host_event_sink();
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    let stamps =
        |rx: &crate::surface_protocol::HostEventReceiver<TestExtension>| match rx.try_iter().last()
        {
            Some(HostEvent::ToolLine {
                started_at_ms,
                finished_at_ms,
                ..
            }) => (started_at_ms, finished_at_ms),
            other => panic!("a compose line expected, got {other:?}"),
        };
    emit_compose_update(
        &events,
        &session,
        &projection.cells["exec-1"],
        "exec-1",
        false,
        None,
    );
    assert_eq!(stamps(&rx), (Some(1_000), None), "in flight");
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    projection.observe_compose_terminal("exec-1", "done");
    emit_compose_update(
        &events,
        &session,
        &projection.cells["exec-1"],
        "exec-1",
        false,
        Some(4_500),
    );
    assert_eq!(stamps(&rx), (Some(1_000), Some(5_500)), "terminal");
}

#[test]
fn a_reopened_compose_marker_re_terminalises_and_the_completed_sweep_skips_it() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, rx) = host_event_sink();

    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            wait: None,
        },
    );
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    // The cell yields with every observed child terminal, so its own
    // terminal paints and persists.
    projection.observe_compose_terminal("exec-1", "done");
    assert_eq!(
        projection.cells["exec-1"].settled_persisted_state(),
        Some("succeeded")
    );
    observe_settled_compose_marker(
        &mut session,
        &projection.cells["exec-1"],
        false,
        "exec-1",
        2,
    );

    // A child dispatched during the `wait*` puts the cell back in flight.
    assert!(projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2)));
    assert_eq!(projection.cells["exec-1"].row_state(), "running");
    assert_eq!(
        projection.cells["exec-1"].settled_persisted_state(),
        None,
        "a cell with a running child has no terminal to persist"
    );
    let mut marker = session.transcript.tool_activity("exec-1").unwrap();
    "running".clone_into(&mut marker.state);
    marker.finished_at_ms = None;
    observe_tool_activity(&mut session, marker);

    // The wait-phase child closes: that is the cell's end.
    projection.observe_child_terminal("exec-1", Some("child-2"), false);
    observe_settled_compose_marker(
        &mut session,
        &projection.cells["exec-1"],
        false,
        "exec-1",
        9,
    );
    let settled = session.transcript.tool_activity("exec-1").unwrap();
    assert_eq!(settled.state, "succeeded");
    assert_eq!(settled.finished_at_ms, Some(9));
    assert_eq!(settled.subject.as_deref(), Some("2 tools"));

    // The `Completed` sweep rewrites persisted `running` rows only, so a
    // re-terminalised marker is not touched and emits no line.
    observe_terminal_tool_activity(
        &mut session,
        &events,
        "failed:observation-gap",
        Some(&projection),
    );
    let swept = session.transcript.tool_activity("exec-1").unwrap();
    assert_eq!(swept.state, "succeeded", "a cell that succeeded stays so");
    assert!(
        !rx.try_iter()
            .any(|event| matches!(event, HostEvent::ToolLine { .. })),
        "the sweep has nothing to rewrite"
    );
}

/// A cell that yields while a child is still in flight has
/// not ended, so its own terminal row stays `running` and a cancel in that
/// window reaches it — instead of leaving `✓ Compose` over `⊘` children.
#[test]
fn a_cancel_in_the_yield_window_sweeps_the_marker_and_states_the_remainder() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, rx) = host_event_sink();

    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose("exec-1", "exec", Some("cell-1"));
    projection.observe_child_start("exec-1", Some("child-1"), Some("cell-1"), Some(1));
    projection.observe_child_terminal("exec-1", Some("child-1"), false);
    projection.observe_child_start("exec-1", Some("child-2"), Some("cell-1"), Some(2));
    // The marker terminalises while child-2 runs: `row_state` is what the
    // line and the persisted row carry, not the local `done`.
    projection.observe_compose_terminal("exec-1", "done");
    assert_eq!(projection.cells["exec-1"].row_state(), "running");
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "running".into(),
            subject: Some("2 tools".into()),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: Some("cell-1".into()),
            wait: None,
        },
    );

    observe_cancelled_tool_activity(&mut session, &events, Some(&projection));
    let line = rx
        .try_iter()
        .find_map(|event| match event {
            HostEvent::ToolLine { call_id, state, .. } if call_id.as_deref() == Some("exec-1") => {
                Some(state)
            }
            _ => None,
        })
        .expect("the cancelled marker paints its own row");
    assert_eq!(line, "cancelled after 1 of 2");
}

/// The exec terminal persisted `3 tools`, two more children completed
/// under the `wait` continuation, the live row said `5 tools` and the
/// resumed one still `3 tools`.
#[test]
fn compose_counts_moved_by_wait_children_survive_resume() {
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "exec-1".into(),
            name: "exec".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("exec"),
            state: "succeeded".into(),
            subject: Some("3 tools".into()),
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: Some(2),
            parent_call_id: None,
            cell_id: Some("1".into()),
            wait: None,
        },
    );
    observe_compose_subject(&mut session, "exec-1", "5 tools".into());
    // A marker the host never persisted has nothing to move.
    observe_compose_subject(&mut session, "missing", "9 tools".into());

    let HostEvent::PresentationSnapshot(snapshot) =
        project_presentation_snapshot(&session.transcript)
    else {
        unreachable!("snapshot projection")
    };
    let tools: Vec<_> = snapshot
        .rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect();
    let compose = tools
        .iter()
        .find(|item| item.call_id == "exec-1")
        .expect("the Compose head row is reloaded");
    assert_eq!(compose.subject.as_deref(), Some("5 tools"));
    assert_eq!(
        crate::activity::tool_activity_kind(&compose.name),
        plakous_host_protocol::views::presentation::ActivityKind::Compose
    );
    assert_eq!(compose.cell_id.as_deref(), Some("1"));
    assert_eq!(
        tools.len(),
        1,
        "re-persisting the tally rewrites the row rather than adding one"
    );
}

/// The exact host activity classification table.
#[test]
fn activity_kind_mapping_table_is_host_authoritative() {
    use crate::activity::tool_activity_kind;
    use plakous_host_protocol::views::presentation::ActivityKind;

    for marker in ["exec", "exec_js", "exec_py"] {
        assert_eq!(
            tool_activity_kind(marker),
            ActivityKind::Compose,
            "{marker}"
        );
    }
    // `wait*` yields belong to an existing cell; they never open a Compose.
    for yielded in ["wait", "wait_js", "wait_py"] {
        assert_eq!(tool_activity_kind(yielded), ActivityKind::Tool, "{yielded}");
    }
    for topology in [
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
    ] {
        assert_eq!(
            tool_activity_kind(topology),
            ActivityKind::Agent,
            "{topology}"
        );
    }
    for process in [
        "process_session_start",
        "process-session-start",
        "plakous.process-session-start.v2",
        "plakous.process-session-start.v3",
        "exec_command",
        "run_process",
    ] {
        assert_eq!(
            tool_activity_kind(process),
            ActivityKind::Process,
            "{process}"
        );
    }
    for tool in ["read_file", "apply_patch", "grep", ""] {
        assert_eq!(tool_activity_kind(tool), ActivityKind::Tool, "{tool}");
    }
    assert_eq!(tool_activity_kind("compact"), ActivityKind::Compact);
    // Process rows normalize onto one label, host-side.
    let row = crate::activity::normalize_process_start(
        plakous_host_protocol::views::presentation::ActivityItem {
            parent_call_id: None,
            cell_id: None,
            id: "a".into(),
            correlation_id: None,
            kind: ActivityKind::Tool,
            label: "exec_command".into(),
            subject: None,
            detail: None,
            state: plakous_host_protocol::views::presentation::ActivityState::Running,
            started_at_ms: None,
            finished_at_ms: None,
            duration_ms: None,
        },
    );
    assert_eq!(row.kind, ActivityKind::Process);
    assert_eq!(row.label, "Shell");
}

/// One nested cell's Compose row: created on the cell's own start, tallied on
/// each child terminal, never re-parented.
#[test]
fn compose_row_opens_on_the_cell_and_counts_observed_children() {
    let parent = plakous::model::CallId::try_from(uuid::Uuid::from_u128(0x0104)).unwrap();
    let parent_id = parent.as_uuid().to_string();
    for marker in ["exec", "exec_js", "exec_py"] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.open_compose(&parent_id, marker, Some("cell-1"));
        for ordinal in 1..=3_u32 {
            let child =
                plakous::model::CallId::try_from(uuid::Uuid::from_u128(u128::from(ordinal) + 1))
                    .unwrap();
            let lineage = nested_lineage("cell-1", ordinal, parent, child);
            projection.observe_child_start(
                &parent_id,
                Some(&child.as_uuid().to_string()),
                lineage.cell_id(),
                lineage.nested_ordinal(),
            );
        }
        for ordinal in 1..=3_u32 {
            let child =
                plakous::model::CallId::try_from(uuid::Uuid::from_u128(u128::from(ordinal) + 1))
                    .unwrap();
            projection.observe_child_terminal(
                &parent_id,
                Some(&child.as_uuid().to_string()),
                ordinal == 2,
            );
        }
        let compose = projection.cells.get(&parent_id).unwrap();
        assert_eq!(compose.name, marker);
        assert_eq!(compose.cell_id.as_deref(), Some("cell-1"));
        assert_eq!(compose_counts_label(compose, false), "3 tools · 1 failed");
        assert_eq!(
            compose_counts_label(compose, true),
            "3 tools · 1 failed · counts partial after gap"
        );
    }
}

/// A cell that cancels states only the remainder it observed.
#[test]
fn cancelled_compose_states_only_the_observed_remainder() {
    let parent = "outer-call";
    let mut projection = TurnProjection::<TestExtension>::default();

    // Nothing observed: no ratio may be invented.
    projection.open_compose(parent, "exec_js", Some("cell-1"));
    assert_eq!(
        compose_cancelled_state(projection.cells.get(parent).unwrap()),
        "cancelled"
    );

    // Four accepted, two reached a terminal state.
    for ordinal in 1..=4_u32 {
        projection.observe_child_start(
            parent,
            Some(&format!("child-{ordinal}")),
            Some("cell-1"),
            Some(ordinal),
        );
    }
    for ordinal in 1..=2_u32 {
        projection.observe_child_terminal(parent, Some(&format!("child-{ordinal}")), false);
    }
    assert_eq!(
        compose_cancelled_state(projection.cells.get(parent).unwrap()),
        "cancelled after 2 of 4"
    );
}

/// A child whose parent the host never observed is never folded or guessed.
#[test]
fn an_unknown_parent_never_mints_a_compose() {
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_child_start("never-seen", Some("child-1"), Some("cell-9"), Some(1));
    assert!(
        projection
            .observe_child_terminal("never-seen", Some("child-1"), false)
            .is_none()
    );
    assert!(projection.cells.is_empty());
}

/// A yielded Compose keeps `running` while any observed child is not
/// terminal, and paints its terminal only when they all are.
///
/// The outer `exec*` completes when the cell *yields*, and children
/// dispatched during the later `wait*` still carry the exec's
/// `parent_call_id`. Spec 02 §5.4 puts the running glyph on the compose and on
/// the child that is running, and §5.1 forbids inventing state: a `✓ Compose`
/// over `⊘ cancelled` children is exactly the lie those two clauses name.
#[test]
fn a_yielded_compose_stays_running_until_every_observed_child_is_terminal() {
    let marker = "outer-exec";
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.open_compose(marker, "exec_js", Some("cell-7"));
    assert!(!projection.observe_child_start(marker, Some("child-1"), Some("cell-7"), Some(1)));
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // The cell yields with `child-1` still in flight: the marker has its own
    // observed terminal, but the row must not claim the cell is finished.
    projection.observe_compose_terminal(marker, "done");
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // `child-1` reaches its terminal: every observed child is terminal, so
    // the marker's own observed state is what the row now paints.
    let compose = projection
        .observe_child_terminal(marker, Some("child-1"), false)
        .unwrap();
    assert_eq!(compose.row_state(), "done");
    assert_eq!(compose_counts_label(compose, false), "1 tools");

    // A second child dispatched during the `wait*` puts the cell back in
    // flight, and `observe_child_start` says so, so the caller re-paints the
    // marker row and persists it back to `running`.
    assert!(projection.observe_child_start(marker, Some("child-2"), Some("cell-7"), Some(2)));
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");

    // A `wait*` continuation names the cell and no parent; the host
    // attributes it to the row that cell already opened.
    assert_eq!(
        projection.marker_for_cell(Some("cell-7")).as_deref(),
        Some(marker)
    );
    assert!(projection.marker_for_cell(Some("cell-unknown")).is_none());
    assert!(projection.marker_for_cell(None).is_none());

    // The `wait*` terminal alone does not close a row with a live child.
    projection.observe_compose_terminal(marker, "failed");
    assert_eq!(projection.cells.get(marker).unwrap().row_state(), "running");
    let compose = projection
        .observe_child_terminal(marker, Some("child-2"), true)
        .unwrap();
    assert_eq!(compose.row_state(), "failed");
    assert_eq!(compose_counts_label(compose, false), "2 tools · 1 failed");
}

/// Reasoning projects only under an admitting bound.
#[test]
fn reasoning_projects_only_under_an_admitting_visibility_bound() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

    let location = OutputLocation::new(0, 0);
    for visibility in [ReasoningVisibility::Visible, ReasoningVisibility::Summary] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, visibility));
        assert_eq!(
            projection
                .observe_reasoning_delta(&ReasoningDelta::new(location, b"thinking".to_vec())),
            Some("thinking".to_owned()),
            "{visibility:?}"
        );
        let sealed = projection
            .seal_reasoning(location)
            .expect("admitted reasoning seals");
        assert_eq!(sealed.location_key, "0:0", "{visibility:?}");
        assert_eq!(sealed.text, "thinking", "{visibility:?}");
    }
    for visibility in [
        ReasoningVisibility::Omitted,
        ReasoningVisibility::Redacted,
        ReasoningVisibility::OpaqueReplay,
    ] {
        let mut projection = TurnProjection::<TestExtension>::default();
        projection.observe_block_start(&reasoning_block(location, visibility));
        assert_eq!(
            projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"secret".to_vec())),
            None,
            "{visibility:?}"
        );
        assert_eq!(projection.seal_reasoning(location), None, "{visibility:?}");
        assert!(projection.seal_all_reasoning().is_empty(), "{visibility:?}");
    }
}

/// A replay that discards its response discards the reasoning that
/// response sealed: the session keeps only the replay's thinking.
#[test]
fn a_discarded_response_never_persists_its_sealed_thinking() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};

    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, _rx) = host_event_sink();
    let location = OutputLocation::new(0, 0);
    let mut projection = TurnProjection::<TestExtension>::default();
    for text in ["first try", "replayed"] {
        projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
        let _ = projection
            .observe_reasoning_delta(&ReasoningDelta::new(location, text.as_bytes().to_vec()));
        seal_reasoning_block(&mut projection, &events, location);
        if text == "first try" {
            discard_response_reasoning(&mut projection, &events);
        }
    }
    settle_live_thinking(&mut projection, &mut session, &events);
    let thinking = session
        .transcript
        .rows()
        .into_iter()
        .filter_map(|(line, _)| line.as_thinking().cloned())
        .map(|activity| activity.text)
        .collect::<Vec<_>>();
    assert_eq!(thinking, ["replayed"]);
}

#[test]
fn output_limit_discard_then_interrupted_retry_does_not_save_old_thinking() {
    use plakous::model::{OutputLocation, ReasoningDelta, ReasoningVisibility};
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let (events, _rx) = host_event_sink();
    let location = OutputLocation::new(0, 0);
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
    projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"discarded thought"));
    seal_reasoning_block(&mut projection, &events, location);
    discard_response_reasoning(&mut projection, &events);
    // The retry is interrupted before it emits anything. The ordinary
    // terminal flush must not resurrect the discarded response.
    settle_live_thinking(&mut projection, &mut session, &events);
    assert!(
        session
            .transcript
            .rows()
            .into_iter()
            .all(|(line, _)| line.as_thinking().is_none())
    );
}

/// A pending location's buffer is released only by an admitting bound.
#[test]
fn pending_reasoning_buffer_is_released_or_dropped_but_never_leaked() {
    use plakous::model::{
        OutputLocation, ReasoningDelta, ReasoningVisibility, ReasoningVisibilityBound,
    };

    let location = OutputLocation::new(1, 2);

    // Admitting bound releases exactly the buffered bytes, once.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec())),
        None
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Summary
        )),
        Some("held".to_owned())
    );
    let sealed = projection
        .seal_reasoning(location)
        .expect("released pending reasoning seals");
    assert_eq!(sealed.location_key, "1:2");
    assert_eq!(sealed.text, "held");

    // Non-admitting bound drops the buffer without emitting it.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let _ = projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Redacted
        )),
        None
    );
    assert_eq!(projection.seal_reasoning(location), None);

    // Overflow drops the buffer and the location stays non-displayable even
    // under a later admitting bound: a partial body is never emitted.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let oversized = vec![b'x'; MAX_PENDING_REASONING_BYTES + 1];
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, oversized)),
        None
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Visible
        )),
        None
    );
    // …and every delta after that bound stays suppressed. Re-admitting the
    // location would emit the tail of a body whose head was dropped, which
    // The transcript contract forbids for the remainder of the block.
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"tail".to_vec())),
        None
    );
    assert_eq!(projection.seal_reasoning(location), None);

    // A bound arriving after an already-emitting admitted block does not
    // reset it: the block still seals.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&reasoning_block(location, ReasoningVisibility::Visible));
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"shown".to_vec())),
        Some("shown".to_owned())
    );
    assert_eq!(
        projection.bind_reasoning_visibility(ReasoningVisibilityBound::new(
            location,
            ReasoningVisibility::Visible
        )),
        None
    );
    let sealed = projection
        .seal_reasoning(location)
        .expect("already-emitting admitted block still seals");
    assert_eq!(sealed.location_key, "1:2");
    assert_eq!(sealed.text, "shown");

    // An unbound pending location at response completion emits nothing.
    let mut projection = TurnProjection::<TestExtension>::default();
    projection.observe_block_start(&pending_reasoning_block(location));
    let _ = projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"held".to_vec()));
    assert_eq!(projection.seal_all_reasoning(), [] as [SealedReasoning; 0]);
    assert!(projection.reasoning.is_empty());

    // A delta for a location that never started is non-admitting.
    let mut projection = TurnProjection::<TestExtension>::default();
    assert_eq!(
        projection.observe_reasoning_delta(&ReasoningDelta::new(location, b"stray".to_vec())),
        None
    );
}

/// A live usage report is a snapshot, never a turn total.
#[test]
fn live_usage_projects_as_a_non_terminal_snapshot() {
    let update = plakous::model::UsageUpdate::new(plakous::model::Usage::new(11, 3, 2, 7, 0));
    assert_eq!(
        project_live_usage(&update),
        HostEvent::TurnUsage {
            input_tokens: 11,
            output_tokens: 3,
            reasoning_tokens: 2,
            cached_input_tokens: 7,
            cache_write_tokens: 0,
            // Coverage is unknown: a live report is one provider-reported
            // cumulative snapshot over an unknown number of operations, and
            // the host never fabricates a `1/1`.
            model_operations: None,
            reporting_model_operations: None,
            peak_prepared_input_tokens: None,
            terminal: false,
        }
    );
}

/// Spec 02 §5.4: a published notice in a persisted scope joins the
/// session's transcript in publication order, so a restore shows it as it
/// was shown live; settings, approval and clipped feedback stay live only,
/// a diagnostic is never persisted, a child's live rows never enter the
/// root transcript, a row its producer already stored is not kept twice,
/// a re-announced state note is never kept, and a background publisher's
/// notice for a session the host no longer owns is dropped.
#[test]
fn live_tool_observations_never_commit_product_records_or_views() {
    let root = tempfile::tempdir().unwrap();
    let store = crate::session_store::MemoryStore::new();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    store.save(&mut session).unwrap();
    let revision = session.store_revision;
    observe_tool_activity(
        &mut session,
        sessions::ToolItem {
            call_id: "live-call".into(),
            name: "read_file".into(),
            exposed_name: None,
            kind: crate::activity::tool_activity_kind("read_file"),
            state: "running".into(),
            subject: None,
            detail: None,
            started_at_ms: Some(1),
            finished_at_ms: None,
            parent_call_id: None,
            cell_id: None,
            wait: None,
        },
    );
    assert!(session.transcript.tool_activity("live-call").is_some());
    assert_eq!(session.store_revision, revision);
    assert_eq!(store.load(&session.id).unwrap().store_revision, revision);
    assert!(store.load_views(&session.id).unwrap().is_none());
    assert_eq!(
        store.load_intents(&session.id).unwrap().notices,
        [] as [sessions::projector::OperatorNotice; 0]
    );
}

#[test]
fn notice_intents_commit_by_scope_and_session_not_by_observation() {
    use crate::sessions::NoticeScope;
    let root = tempfile::tempdir().unwrap();
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let store = crate::session_store::MemoryStore::new();
    store.save(&mut session).unwrap();
    let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = Sender::over(tx);
    let scopes = [
        NoticeScope::Retry,
        NoticeScope::Clipped,
        NoticeScope::Turn,
        NoticeScope::Queue,
        NoticeScope::Recovery,
        NoticeScope::Tree,
        NoticeScope::Compaction,
        NoticeScope::Session,
        NoticeScope::Settings,
        NoticeScope::Approval,
        NoticeScope::Host,
        NoticeScope::Diagnostic,
    ];
    for scope in scopes {
        events.notice(TranscriptRow::warning(scope, format!("{scope:?}")));
    }
    events.send(HostEvent::ChildLive {
        agent_id: "c1".into(),
        turn_id: "t1".into(),
        op: Box::new(plakous_host_protocol::SessionObservation::Row(
            TranscriptRow::info(NoticeScope::Tree, "a child's row"),
        )),
    });
    events.send(HostEvent::Row(TranscriptRow::info(
        NoticeScope::Turn,
        "stored by its producer",
    )));
    events.send(HostEvent::Row(TranscriptRow::warning(
        NoticeScope::Queue,
        "re-announced on every open",
    )));
    events
        .pinned_to("a session the host has left")
        .notice(TranscriptRow::info(
            NoticeScope::Tree,
            "a stale background pass",
        ));
    let current = session.id.clone();
    events.pinned_to(&current).notice(TranscriptRow::info(
        NoticeScope::Tree,
        "this session's background pass",
    ));
    commit_notice_intents(
        &events,
        &store,
        &SessionHome::new(root.path().join("sessions")),
        None,
        &mut session,
    );
    let mut expected: Vec<_> = scopes
        .into_iter()
        .filter(|scope| scope.persisted())
        .map(|scope| TranscriptRow::warning(scope, format!("{scope:?}")))
        .collect();
    expected.push(TranscriptRow::info(
        NoticeScope::Tree,
        "this session's background pass",
    ));
    assert_eq!(store.load(&session.id).unwrap().transcript, expected);
    assert!(
        events.take_notice_intents().is_empty(),
        "each notice persists once"
    );
}

#[test]
fn row_observations_never_author_notice_intents() {
    let (events, receiver) = host_event_sink();
    let row = TranscriptRow::warning(NoticeScope::Turn, "live observation only");
    events.send(HostEvent::Row(row.clone()));
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        [HostEvent::Row(row)]
    );
    assert!(events.take_notice_intents().is_empty());
}

/// A definite provider failure during compaction is diagnosable from the log.
///
/// Five `compact.started`/`compact.finished`
/// pairs, three committed and two failed, and the failed records carried
/// only `kind`, `outcome`, `elapsed_ms`, `seq` and `ts`. Nothing said what
/// had failed, so nothing could be acted on.
#[test]
fn a_failed_compaction_records_and_reports_why_it_failed() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "9f9f9f9f-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(compact_finished_record(
        CompactionEventOutcome::Failed,
        4_210,
        None,
        Some("provider rejected the compaction request: context_capacity"),
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "compact.finished");
    assert_eq!(record["outcome"], "failed");
    assert_eq!(record["elapsed_ms"], 4_210);
    assert_eq!(
        record["detail"], "provider rejected the compaction request: context_capacity",
        "a failed compaction must say what failed"
    );

    let note = compact_finished_note(
        CompactionEventOutcome::Failed,
        None,
        Some("provider rejected the compaction request: context_capacity"),
    );
    assert!(
        note.contains("context_capacity"),
        "the operator note must carry the failure text too: {note}"
    );

    // An outcome with nothing to explain gains no empty field.
    log.append(compact_finished_record(
        CompactionEventOutcome::Committed,
        12,
        None,
        None,
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    assert!(
        !lines[1].contains("\"detail\""),
        "a committed compaction must not carry an empty detail: {}",
        lines[1]
    );
}

/// A truncated tool result is visible without reading the model's prose.
///
/// Until this existed the observation budget clipped a result silently: a
/// surface saw the tool succeed and had no way to know its output had been
/// cut, so the only trace was whatever the model said about it.
#[test]
fn a_clipped_observation_records_and_reports_what_was_cut() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "5c5c5c5c-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(
        SessionEvent::new("observation.clipped")
            .str("name", "read")
            .opt_str("call_id", Some("call-9"))
            .u64("bytes_kept", 4_096)
            .u64("bytes_dropped", 118_784)
            .u64("bound", 4_096),
    )
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "observation.clipped");
    assert_eq!(record["name"], "read");
    assert_eq!(record["call_id"], "call-9");
    assert_eq!(record["bytes_kept"], 4_096);
    assert_eq!(record["bytes_dropped"], 118_784);
    assert_eq!(record["bound"], 4_096);

    let note = observation_clipped_note("read", 4_096, 118_784, 4_096);
    for figure in ["read", "4096", "118784"] {
        assert!(
            note.contains(figure),
            "the note must carry {figure}: {note}"
        );
    }
    assert!(
        note.contains("narrow the call"),
        "the note must say what to do about it: {note}"
    );
}

#[test]
fn a_replaced_observation_records_its_distinct_canonical_counts() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "5c5c5c5c-1111-2222-3333-444444444445";
    let log = SessionLog::open(&home, id).unwrap();
    log.append(observation_replaced_record(Some("call-10"), 4_157, 161))
        .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "observation.replaced");
    assert_eq!(record["call_id"], "call-10");
    assert_eq!(record["bytes_before"], 4_157);
    assert_eq!(record["bytes_after"], 161);
    assert!(record.get("bytes_dropped").is_none());
}

/// A committed compaction says what it did, not merely that it committed.
///
/// "compact workflow committed" tells an operator nothing about whether the
/// compaction bought anything, and the fold's note said only that for as
/// long as the fold has existed. The figures come from the successor
/// checkpoint's own compaction provenance, so the line and the record agree
/// with what the checkpoint commits to.
#[test]
fn a_committed_compaction_records_and_reports_what_it_did() {
    let dir = tempfile::tempdir().unwrap();
    let home = SessionHome::new(dir.path().join("sessions"));
    home.ensure().unwrap();
    let id = "7a7a7a7a-1111-2222-3333-444444444444";
    let log = SessionLog::open(&home, id).unwrap();
    let summary = CompactionSummary {
        rung: CompactionRung::SemanticFold,
        segments: 14,
        input_tokens_before: 182_400,
        input_tokens_after: 61_200,
        occupancy_tokens_before: None,
        occupancy_tokens_after: None,
    };
    log.append(compact_finished_record(
        CompactionEventOutcome::Committed,
        3_120,
        Some(summary),
        None,
    ))
    .unwrap();
    log.flush().unwrap();
    let lines = crate::session_log::read_tail(&home, id, 10).unwrap();
    let record: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(record["kind"], "compact.finished");
    assert_eq!(record["outcome"], "committed");
    assert_eq!(record["rung"], "semantic_fold");
    assert_eq!(record["segments"], 14);
    assert_eq!(record["input_tokens_before"], 182_400);
    assert_eq!(record["input_tokens_after"], 61_200);

    let note = compact_finished_note(CompactionEventOutcome::Committed, Some(summary), None);
    for figure in ["folded", "14", "182400", "61200"] {
        assert!(
            note.contains(figure),
            "the note must carry {figure}: {note}"
        );
    }
    assert!(
        !note.contains("compact workflow committed"),
        "the figures replace the bare wording rather than trailing it: {note}"
    );

    // Each rung is named distinctly, so a reader can tell which one ran.
    for (rung, label, verb) in [
        (CompactionRung::NativeFacet, "native_facet", "natively"),
        (CompactionRung::SemanticFold, "semantic_fold", "folded"),
        (
            CompactionRung::ToolResultPlaceholders,
            "tool_result_placeholders",
            "replaced",
        ),
    ] {
        assert_eq!(compaction_rung_label(rung), label);
        assert!(rung.verb().contains(verb), "{rung:?}");
    }
}

/// On a live Grok hub the claimed call carries `exposed_name` =
/// `get_command_or_subagent_output` (or `kill_command_or_subagent`). A
/// call naming only a child must read as the await/interrupt over that
/// child, not as a process wait/stop over an `(unknown)` process id. The
/// restored transcript (exposed name absent) already reads it right; the
/// live row must match.
#[test]
fn live_grok_child_calls_do_not_read_as_process_calls() {
    use plakous::model::{Digest, DigestDomain};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    session.plakous_locator = Some(locator);
    let (events, _rx) = host_event_sink();
    let mut projection = TurnProjection::default();
    let mut stash = ToolSubjectStash::new();
    let mut assistant = String::new();
    let mut delta = String::new();
    let mut last_flush = Instant::now();
    let mut tree = TreeSnapshotPush::default();
    let mut compacted = false;
    let mut peak = 0;
    let uuid = uuid::Uuid::from_u128(1);
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    {
        let mut dispatch = |name: &str, call: u128, kind: &str, observation: serde_json::Value| {
            let identity = serde_json::json!({
                "canonical_name": name,
                "exposed_name": name,
                "exposure_map_digest": null,
                "registered_tool_id": format!("plak.topology.{name}.v1"),
                "executor_id": format!("plak.topology.{name}.executor.v1"),
                "definition_digest": digest,
                "schema_digest": digest,
                "effect_metadata_digest": digest,
                "argument_limits_digest": digest,
            });
            let event: AgentEvent = serde_json::from_value(serde_json::json!({
                "branch_sequence": 1,
                "event_id": uuid,
                "session_id": locator.session_id(),
                "branch_id": locator.branch_id(),
                "agent_id": uuid,
                "run_id": uuid,
                "turn_id": null,
                "attempt_id": null,
                "call_id": uuid::Uuid::from_u128(call),
                "timestamp": {"seconds": 1, "nanoseconds": 0},
                "causal_parent": null,
                "origin": "Tool",
                "classification": "Public",
                "event_schema": 3,
                "projection_revision": 6,
                "visibility": "Content",
                "scope": "Agent",
                "event_kind_version": 11,
                "kind": {kind: {
                    "identity": identity,
                    "lineage": plakous::agent::ToolCallerLineageView::direct(),
                    "observation": observation,
                }},
                "retained_bytes": 512,
                "metadata_retained_bytes": 512,
                "content_retained_bytes": 0,
            }))
            .unwrap();
            apply_event(
                &event,
                &roots,
                &rows,
                &store,
                &home,
                &mut session,
                &events,
                &mut assistant,
                &mut delta,
                &mut last_flush,
                &mut stash,
                &mut projection,
                None,
                "test",
                &mut tree,
                &mut compacted,
                &mut peak,
            );
        };
        let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
        dispatch(
            "get_command_or_subagent_output",
            21,
            "ToolStarted",
            started(serde_json::json!({"task_ids": ["c1"], "timeout_ms": 5000})),
        );
        dispatch(
            "kill_command_or_subagent",
            22,
            "ToolStarted",
            started(serde_json::json!({"task_id": "c1"})),
        );
    }
    for (call, slot) in [(21, "wait_agent"), (22, "interrupt_agent")] {
        let id = uuid::Uuid::from_u128(call).to_string();
        let row = session.transcript.tool_activity(&id).unwrap();
        assert_eq!(row.name, slot);
        let subject = row.subject.clone().unwrap_or_default();
        assert!(
            subject.contains("c1") && !subject.contains("process"),
            "{slot} row on a live Grok hub reads as a process call: {subject:?}"
        );
    }
    assert!(
        !projection
            .process_waits
            .contains_key(&uuid::Uuid::from_u128(21).to_string()),
        "a child await is recorded as a process wait"
    );
}

/// A bare pi `wait` on a live hub starts as the process wait (only its
/// run decides) and, once it settles with the tree's await receipt,
/// reads as the await: `wait_agent`, the children it listed, what woke it
/// and its 300000 ms ceiling, persisted as the restored row reads it. One
/// that ran the process tool's bare await stays the process wait.
#[test]
fn live_bare_pi_wait_that_ran_the_await_reads_as_the_await() {
    use plakous::model::{Digest, DigestDomain};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let locator: plakous::sessions::SessionLocator = "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003".parse().unwrap();
    session.plakous_locator = Some(locator);
    let (events, _rx) = host_event_sink();
    let mut projection = TurnProjection::default();
    let mut stash = ToolSubjectStash::new();
    let mut assistant = String::new();
    let mut delta = String::new();
    let mut last_flush = Instant::now();
    let mut tree = TreeSnapshotPush::default();
    let mut compacted = false;
    let mut peak = 0;
    let uuid = uuid::Uuid::from_u128(1);
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    {
        let mut dispatch = |name: &str, call: u128, kind: &str, observation: serde_json::Value| {
            let identity = serde_json::json!({
                "canonical_name": name,
                "exposed_name": name,
                "exposure_map_digest": null,
                "registered_tool_id": format!("plak.topology.{name}.v1"),
                "executor_id": format!("plak.topology.{name}.executor.v1"),
                "definition_digest": digest,
                "schema_digest": digest,
                "effect_metadata_digest": digest,
                "argument_limits_digest": digest,
            });
            let event: AgentEvent = serde_json::from_value(serde_json::json!({
                "branch_sequence": 1,
                "event_id": uuid,
                "session_id": locator.session_id(),
                "branch_id": locator.branch_id(),
                "agent_id": uuid,
                "run_id": uuid,
                "turn_id": null,
                "attempt_id": null,
                "call_id": uuid::Uuid::from_u128(call),
                "timestamp": {"seconds": 1, "nanoseconds": 0},
                "causal_parent": null,
                "origin": "Tool",
                "classification": "Public",
                "event_schema": 3,
                "projection_revision": 6,
                "visibility": "Content",
                "scope": "Agent",
                "event_kind_version": 11,
                "kind": {kind: {
                    "identity": identity,
                    "lineage": plakous::agent::ToolCallerLineageView::direct(),
                    "observation": observation,
                }},
                "retained_bytes": 512,
                "metadata_retained_bytes": 512,
                "content_retained_bytes": 0,
            }))
            .unwrap();
            apply_event(
                &event,
                &roots,
                &rows,
                &store,
                &home,
                &mut session,
                &events,
                &mut assistant,
                &mut delta,
                &mut last_flush,
                &mut stash,
                &mut projection,
                None,
                "test",
                &mut tree,
                &mut compacted,
                &mut peak,
            );
        };
        let started = |args: serde_json::Value| serde_json::json!({"subject":null,"arguments_preview":args.to_string()});
        let completed = |result: serde_json::Value| serde_json::json!({"succeeded":true,"preview":result.to_string()});
        let receipt = serde_json::json!({
            "mail": [{"message_id": "m1", "from": "c1", "purpose": "status", "body": "halfway"}],
            "woke_by": "mail",
            "agents": [{"agent_id": "c1", "status": "running"}],
            "timed_out": false,
        });
        for (call, result) in [
            (31, serde_json::json!({"result": receipt.to_string()})),
            (
                32,
                serde_json::json!({"tasks": [{"task_id": "p1", "status": "exited", "exit_code": 0}]}),
            ),
        ] {
            dispatch("wait", call, "ToolStarted", started(serde_json::json!({})));
            dispatch("wait", call, "ToolCompleted", completed(result));
        }
    }
    let row = session
        .transcript
        .tool_activity(&uuid::Uuid::from_u128(31).to_string())
        .unwrap();
    assert_eq!(row.name, "wait_agent", "{row:?}");
    assert_eq!(row.kind, crate::activity::tool_activity_kind("wait_agent"));
    assert_eq!(row.subject.as_deref(), Some("c1"), "{row:?}");
    let wait = row.wait.clone().unwrap();
    assert_eq!(wait.woke_by.as_deref(), Some("mail from c1"), "{row:?}");
    assert_eq!(wait.timeout_ms, Some(300_000), "{row:?}");
    assert!(
        !projection
            .process_waits
            .contains_key(&uuid::Uuid::from_u128(31).to_string())
    );
    let process = session
        .transcript
        .tool_activity(&uuid::Uuid::from_u128(32).to_string())
        .unwrap();
    assert_eq!(process.name, "wait", "{process:?}");
}
