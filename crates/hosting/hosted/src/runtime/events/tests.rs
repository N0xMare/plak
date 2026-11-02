#![allow(
    clippy::too_many_lines,
    reason = "Event projection scenarios drive complete response and tool lifecycles against the host's live row accumulators."
)]
#![allow(clippy::unwrap_used, reason = "unit tests")]

use crate::session_store::SessionRecordStore;
use crate::sessions::{NoticeScope, live_tool_subject};
use crate::surface_protocol::ContentEvent;

use crate::runtime::test_kit::TestExtension;
type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type Sender = crate::runtime::Sender<TestExtension>;

use crate::runtime::events::{apply_event, commit_notice_intents};
use crate::runtime::test_support::host_event_sink;
use crate::runtime::usage::record_operation_prepared_input;
use crate::runtime::{ToolSubjectStash, TreeSnapshotPush, TurnProjection, test_kit};
use crate::session_home::SessionHome;
use crate::sessions;
use crate::sessions::{SessionRecord, TranscriptRow};
use plakous::agent::AgentEvent;
use std::time::{Duration, Instant};

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

fn agent_event(kind: serde_json::Value) -> AgentEvent {
    let uuid = uuid::Uuid::from_u128(1);
    serde_json::from_value(serde_json::json!({
        "branch_sequence": 1, "event_id": uuid, "session_id": uuid, "branch_id": uuid,
        "agent_id": uuid, "run_id": uuid, "turn_id": null, "attempt_id": null, "call_id": null,
        "timestamp": {"seconds": 1, "nanoseconds": 0}, "causal_parent": null,
        "origin": "Model", "classification": "Public", "event_schema": 3,
        "projection_revision": 6, "visibility": "Content", "scope": "Agent", "event_kind_version": 11,
        "kind": kind,
        "retained_bytes": 512, "metadata_retained_bytes": 512, "content_retained_bytes": 0
    }))
    .unwrap()
}

fn apply_kind(
    kind: serde_json::Value,
    assistant: &mut String,
    projection: &mut TurnProjection<TestExtension>,
) -> Vec<HostEvent> {
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    let (events, rx) = host_event_sink();
    let event = agent_event(kind);
    apply_event(
        &event,
        &roots,
        &rows,
        &store,
        &home,
        &mut session,
        &events,
        assistant,
        &mut String::new(),
        &mut Instant::now(),
        &mut ToolSubjectStash::new(),
        projection,
        None,
        "test",
        &mut TreeSnapshotPush::default(),
        &mut false,
        &mut 0,
    );
    rx.try_iter().collect()
}

#[test]
fn uncommitted_tails_are_discarded_on_output_limit_settle_and_overflow_fold() {
    let workflow: Vec<u8> = std::iter::repeat_n(0x5a, 32).collect();
    let cases = [
        serde_json::json!({"OutputLimitRetry": {"bound": 128}}),
        serde_json::json!({"ResponseSettled": {"discardable": true}}),
        serde_json::json!({"CompactionStarted": {"workflow_id": workflow, "reason": "scheduled"}}),
    ];
    for kind in cases {
        let mut assistant = String::from("kept tail");
        let mut projection = TurnProjection::<TestExtension> {
            settled_assistant_len: 4,
            ..TurnProjection::default()
        };
        let events = apply_kind(kind, &mut assistant, &mut projection);
        assert_eq!(assistant, "kept");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, HostEvent::OutputDiscarded { dim: true })),
            "{events:?}"
        );
    }
    let mut assistant = String::from("kept tail");
    let mut projection = TurnProjection::<TestExtension> {
        settled_assistant_len: 4,
        ..TurnProjection::default()
    };
    let workflow: Vec<u8> = std::iter::repeat_n(0x5a, 32).collect();
    let events = apply_kind(
        serde_json::json!({"CompactionStarted": {"workflow_id": workflow, "reason": "manual"}}),
        &mut assistant,
        &mut projection,
    );
    assert_eq!(assistant, "kept tail");
    assert!(
        events
            .iter()
            .all(|event| !matches!(event, HostEvent::OutputDiscarded { .. }))
    );
}

fn model_kind(event: plakous::model::ModelEvent) -> serde_json::Value {
    serde_json::to_value(plakous::agent::AgentEventKind::model(event)).unwrap()
}

fn text_delta(text: &str) -> serde_json::Value {
    model_kind(plakous::model::ModelEvent::TextDelta(
        plakous::model::TextDelta::new(plakous::model::OutputLocation::new(0, 0), text.as_bytes()),
    ))
}

fn response_completed() -> serde_json::Value {
    use plakous::model::{Digest, DigestDomain, FinishDetails, FinishReason, ModelResponse};
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        Digest::sha256(DigestDomain::RequestPlan, 1, &[b"plan"]).unwrap(),
    );
    model_kind(plakous::model::ModelEvent::ResponseCompleted(response))
}

/// `ResponseCompleted` must not mark the tail settled. A later discardable
/// settle still drops it; a kept settle does not.
#[test]
fn discardable_settle_drops_the_tail_a_completed_response_streamed() {
    for discardable in [true, false] {
        let mut assistant = String::from("kept");
        let mut projection = TurnProjection::<TestExtension> {
            settled_assistant_len: 4,
            ..TurnProjection::default()
        };
        let _ = apply_kind(text_delta(" tail"), &mut assistant, &mut projection);
        assert_eq!(assistant, "kept tail");
        let completed = apply_kind(response_completed(), &mut assistant, &mut projection);
        assert_eq!(projection.settled_assistant_len, 4);
        assert!(
            completed
                .iter()
                .all(|event| !matches!(event, HostEvent::OutputDiscarded { .. }))
        );
        let settled = apply_kind(
            serde_json::json!({"ResponseSettled": {"discardable": discardable}}),
            &mut assistant,
            &mut projection,
        );
        if discardable {
            assert_eq!(assistant, "kept");
            assert!(
                settled
                    .iter()
                    .any(|event| matches!(event, HostEvent::OutputDiscarded { dim: true })),
                "{settled:?}"
            );
        } else {
            assert_eq!(assistant, "kept tail");
            assert!(
                settled
                    .iter()
                    .all(|event| !matches!(event, HostEvent::OutputDiscarded { .. }))
            );
        }
    }
}

fn apply_with(
    kind: serde_json::Value,
    assistant: &mut String,
    delta_buf: &mut String,
    last_flush: &mut Instant,
    projection: &mut TurnProjection<TestExtension>,
    events: &Sender,
) {
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_path_buf(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let store = crate::session_store::MemoryStore::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_path_buf());
    apply_event(
        &agent_event(kind),
        &roots,
        &rows,
        &store,
        &home,
        &mut session,
        events,
        assistant,
        delta_buf,
        last_flush,
        &mut ToolSubjectStash::new(),
        projection,
        None,
        "test",
        &mut TreeSnapshotPush::default(),
        &mut false,
        &mut 0,
    );
}

/// A scheduled compaction after a kept response drops nothing, so the answer
/// row the transcript already holds survives.
#[test]
fn scheduled_compaction_after_a_kept_response_sends_no_discard() {
    use plakous_host_protocol::views::PresentationSnapshot;
    use plakous_host_protocol::views::transcript::TranscriptScope;
    let (events, rx) = host_event_sink();
    assert!(events.replace_transcript(PresentationSnapshot {
        scope: TranscriptScope::Root,
        ..PresentationSnapshot::default()
    }));
    assert!(events.send(ContentEvent::ModelRequestStarted));
    let mut assistant = String::new();
    let mut delta_buf = String::new();
    let mut last_flush = Instant::now();
    let mut projection = TurnProjection::<TestExtension>::default();
    apply_with(
        text_delta("answer"),
        &mut assistant,
        &mut delta_buf,
        &mut last_flush,
        &mut projection,
        &events,
    );
    apply_with(
        serde_json::json!({"ResponseSettled": {"discardable": false}}),
        &mut assistant,
        &mut delta_buf,
        &mut last_flush,
        &mut projection,
        &events,
    );
    assert_eq!(assistant, "answer");
    assert_eq!(projection.settled_assistant_len, assistant.len());
    let workflow: Vec<u8> = std::iter::repeat_n(0x5a, 32).collect();
    apply_with(
        serde_json::json!({"CompactionStarted": {"workflow_id": workflow, "reason": "scheduled"}}),
        &mut assistant,
        &mut delta_buf,
        &mut last_flush,
        &mut projection,
        &events,
    );
    assert_eq!(assistant, "answer");
    let observed: Vec<_> = rx.try_iter().collect();
    assert!(
        observed
            .iter()
            .all(|event| !matches!(event, HostEvent::OutputDiscarded { .. })),
        "{observed:?}"
    );
    let published: Vec<String> = observed
        .iter()
        .filter_map(|event| match event {
            HostEvent::TranscriptPatch(patch) => Some(format!("{patch:?}")),
            HostEvent::PresentationSnapshot(snapshot) => Some(format!("{snapshot:?}")),
            _ => None,
        })
        .collect();
    assert!(
        published.iter().any(|text| text.contains("answer")),
        "{published:?}"
    );
}

#[test]
fn live_tool_subject_redaction_is_idempotent() {
    let subject = live_tool_subject("GITHUB_TOKEN=ghp_abcdefghijklmnopqrst gh pr list");
    assert_eq!(subject, "GITHUB_TOKEN=[redacted] gh pr list");
    assert_eq!(live_tool_subject(&subject), subject);
}

fn completed_adopt_event(operation: uuid::Uuid) -> AgentEvent {
    use plakous::model::{Digest, DigestDomain};
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    serde_json::from_value(serde_json::json!({
        "branch_sequence": 1, "event_id": operation, "session_id": operation,
        "branch_id": operation, "agent_id": operation, "run_id": operation,
        "turn_id": null, "attempt_id": null, "call_id": operation,
        "timestamp": {"seconds": 1, "nanoseconds": 0}, "causal_parent": null,
        "origin": "Tool", "classification": "Public", "event_schema": 3,
        "projection_revision": 6, "visibility": "Content", "scope": "Agent", "event_kind_version": 11,
        "kind": {"ToolCompleted": {
            "identity": {"canonical_name":"adopt_candidate", "exposed_name":"adopt_candidate",
                "exposure_map_digest":null, "registered_tool_id":"plak.topology.adopt_candidate.v1",
                "executor_id":"plak.topology.adopt_candidate.executor.v1", "definition_digest":digest,
                "schema_digest":digest, "effect_metadata_digest":digest, "argument_limits_digest":digest},
            "lineage":plakous::agent::ToolCallerLineageView::direct(),
            "observation":{"succeeded":false,"preview":"merge conflict"}
        }}, "retained_bytes":512,"metadata_retained_bytes":512,"content_retained_bytes":0
    })).unwrap()
}

#[tokio::test]
async fn conflict_notices_use_operation_ids_with_reused_absent_and_empty_display_ids() {
    use crate::agent_tree_protocol::{AdoptCandidateRequest, TopologyErrorCode};
    use crate::occupancy_vcs::{AdoptFailure, ConflictPath};
    let dir = tempfile::tempdir().unwrap();
    let roots = crate::agent::ProductStateRoots::with_doubles(
        dir.path().to_owned(),
        dir.path().join("state"),
    );
    let rows = test_kit::MemoryRows::new();
    let home = SessionHome::new(dir.path().join("sessions"));
    let mut session = SessionRecord::new(dir.path().to_owned());
    let tree_id = session.id.clone();
    let (tree, vcs, candidate) =
        crate::runtime::test_support::finished_fork_tree(dir.path(), &tree_id);
    let runtime = crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let (events, received) = host_event_sink();
    let mut projection = TurnProjection::default();
    let mut push = TreeSnapshotPush::default();
    let request = serde_json::to_value(AdoptCandidateRequest {
        candidate_id: candidate.clone(),
        revert: false,
        accept_conflicts: false,
        expected_target: None,
        evidence: None,
    })
    .unwrap();
    let mut expected = Vec::new();
    for (index, display) in [Some("reused"), Some("reused"), None, Some("")]
        .into_iter()
        .enumerate()
    {
        let operation = uuid::Uuid::new_v4();
        let path = format!("src/conflict-{index}, draft.rs");
        vcs.adopt_next_with(Err(AdoptFailure::Conflict {
            paths: vec![ConflictPath {
                path: path.clone(),
                kind: "contents".into(),
            }],
            candidate: None,
        }));
        let _outcome = crate::agent_tree_tools::invoke_granted_topology_tool_on(
            runtime.driver().clone(),
            "root".into(),
            "adopt_candidate",
            &request,
            operation.to_string(),
            crate::agent_tree_tools::TopologyHostPorts::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            runtime
                .read(|tree| tree.operations()[&operation.to_string()].terminal_code)
                .unwrap(),
            Some(TopologyErrorCode::MergeConflict)
        );
        let event = completed_adopt_event(operation);
        super::EventApplier {
            event: &event,
            roots: &roots,
            rows: &rows,
            store: rows.records(),
            home: &home,
            session: &mut session,
            events: &events,
            assistant: &mut String::new(),
            delta_buf: &mut String::new(),
            last_flush: &mut Instant::now(),
            tool_subjects: &mut ToolSubjectStash::new(),
            projection: &mut projection,
            log: None,
            observed_session_id: "test",
            last_tree_revision: &mut push,
            compacted_mid_turn: &mut false,
            peak_prepared_input_tokens: &mut 0,
            call_id_str: display.map(str::to_owned),
        }
        .tool_completed();
        expected.push((
            format!("tree:{tree_id}:adopt-conflict:{candidate}:{operation}"),
            path,
        ));
    }
    let notices = rows.records().load_intents(&session.id).unwrap().notices;
    for (id, path) in expected {
        let matching: Vec<_> = notices
            .iter()
            .filter(|notice| notice.notice_id == id)
            .collect();
        assert_eq!(matching.len(), 1);
        let TranscriptRow::Milestone(row) = &matching[0].row else {
            panic!("conflict milestone")
        };
        assert_eq!(
            row.event,
            plakous_host_protocol::views::transcript::MilestoneEvent::AdoptConflict
        );
        assert_eq!(row.paths, [path]);
    }
    assert_eq!(received.try_iter().filter(|event| matches!(event,
        HostEvent::Row(TranscriptRow::Milestone(row)) if row.event == plakous_host_protocol::views::transcript::MilestoneEvent::AdoptConflict)).count(), 4);
    crate::topology_runtime::force_release(&tree_id).await;
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
    events.observe_agent(
        "c1",
        &plakous_host_protocol::SessionObservation::Row(TranscriptRow::info(
            NoticeScope::Tree,
            "a child's row",
        )),
    );
    events.send(ContentEvent::Row(TranscriptRow::info(
        NoticeScope::Turn,
        "stored by its producer",
    )));
    events.send(ContentEvent::Row(TranscriptRow::warning(
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
    events.send(ContentEvent::Row(row.clone()));
    assert_eq!(
        receiver.try_iter().collect::<Vec<_>>(),
        [HostEvent::Row(row)]
    );
    assert!(events.take_notice_intents().is_empty());
}

#[test]
fn storage_notice_operator_path_never_reaches_session_log_or_exportable_transcript() {
    let root = tempfile::tempdir().unwrap();
    let home = SessionHome::new(root.path().join("sessions"));
    let mut session = SessionRecord::new(root.path().to_path_buf());
    let store = crate::session_store::MemoryStore::new();
    store.save(&mut session).unwrap();
    let log = crate::session_log::SessionLog::open(&home, &session.id).unwrap();
    let (events, receiver) = host_event_sink();
    let private_path = "/private/operator-home/storage-notice-only";
    events.notice(TranscriptRow::info(NoticeScope::Settings,
        format!("storage unavailable · {private_path}; existing work retained; retrying writes every 5 s")));
    assert!(format!("{:?}", receiver.try_iter().collect::<Vec<_>>()).contains(private_path));
    commit_notice_intents(&events, &store, &home, Some(&log), &mut session);
    let stored = store.load(&session.id).unwrap();
    let exported_rows =
        serde_json::to_string(&stored.transcript.iter().collect::<Vec<_>>()).unwrap();
    assert!(!exported_rows.contains(private_path));
    assert!(
        !format!(
            "{:?}",
            crate::session_log::read_tail(&home, &session.id, 10).unwrap()
        )
        .contains(private_path)
    );
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
