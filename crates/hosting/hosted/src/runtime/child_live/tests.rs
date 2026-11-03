#![allow(
    clippy::too_many_lines,
    reason = "Live child transcript scenarios compare streamed rows against committed history through yields, gaps, and subsequent turns."
)]
use crate::runtime::test_kit::BlockOn as _;

use super::*;
use crate::runtime::test_support::empty_transcript_call;
use crate::surface_protocol::ContentEvent;
use plakous::model::UsageUpdate;

#[test]
fn child_success_uses_the_root_done_vocabulary() {
    let lineage = plakous::agent::ToolCallerLineageView::direct();
    let event = tool_lifecycle_event(
        "read_file",
        &uuid::Uuid::from_u128(1).to_string(),
        "ToolCompleted",
        &lineage,
        serde_json::json!({"succeeded":true,"preview":"read result","subject":null}),
    );
    let ops = transcript_ops(&event, &mut TurnProjection::default());
    assert!(matches!(&ops[0], HostEvent::ToolLine {state,..} if state=="done"));
}

#[test]
fn child_subject_capacity_evicts_the_oldest_started_call() {
    let lineage = plakous::agent::ToolCallerLineageView::direct();
    let mut projection = TurnProjection::default();
    for index in (1..=513).rev() {
        let event = tool_lifecycle_event(
            "read_file",
            &uuid::Uuid::from_u128(index).to_string(),
            "ToolStarted",
            &lineage,
            serde_json::json!({"arguments_preview":"{}","subject":"subject"}),
        );
        transcript_ops(&event, &mut projection);
    }
    assert_eq!(projection.progress.subjects.len(), 512);
    for (index, retained) in [(513, false), (512, true), (1, true)] {
        let id = uuid::Uuid::from_u128(index).to_string();
        assert_eq!(
            projection
                .progress
                .subjects
                .iter()
                .any(|(call, _)| call == &id),
            retained,
            "call {index}"
        );
    }
}

#[tokio::test]
async fn real_finished_cells_restore_nested_rows_and_failure_tally_from_the_ledger() {
    use crate::sessions::TranscriptRow;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
    use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
    use std::sync::atomic::AtomicU64;
    for javascript in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let sessions = AgentSessions::new(
            SessionBackend::sqlite(directory.path().join("runtime.sqlite")).unwrap(),
        )
        .unwrap();
        let source = "text(await tools.exec_command({cmd:'printf one'})); text(await tools.exec_command({cmd:'exit 17'})); text(await tools.exec_command({cmd:'printf three'}));";
        let steps = if javascript {
            serde_json::json!([{ "output": {"tool":{"name":"exec_js","arguments":{"source":source}}},"finish":"tool_calls"},{"output":{"text":"done"}}])
        } else {
            serde_json::json!([{ "output": {"cell":format!("// @exec: {{\"yield_time_ms\":1}}\n{}",source.replace("printf one","sleep 0.1; printf one"))},"finish":"tool_calls"},{"output":{"wait":{"cell_id":"all","yield_time_ms":10000}},"finish":"tool_calls"},{"output":{"text":"done"}}])
        };
        let script: ScriptPlan = serde_json::from_value(serde_json::json!({"version":SCRIPT_VERSION,"code_mode":true,"js_ptc":javascript,"steps":steps})).unwrap();
        let controller = BoundaryController::quiet_from_evidence_path(
            CrashBoundary::None,
            directory.path().join("evidence.jsonl"),
        )
        .unwrap();
        let sidecar = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("plakous-js-host");
        let binding = plakous_offline_crash_harness::scripted_code_mode_runtime_binding(
            directory.path(),
            controller,
            Arc::new(script),
            Arc::new(AtomicU64::new(1)),
            &sidecar,
            None,
        )
        .unwrap();
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("real-cell-transcript").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("create");
        };
        let mut turn = session.agent().prompt("run the cell").await.unwrap();
        let mut events = turn.take_events().unwrap();
        let mut projection = TurnProjection::default();
        let mut live = BTreeMap::new();
        loop {
            let event = events.recv().await.unwrap();
            for op in transcript_ops(&event, &mut projection) {
                if let HostEvent::ToolLine {
                    call_id: Some(id),
                    parent_call_id,
                    cell_id,
                    name,
                    state,
                    subject,
                    detail,
                    ..
                } = op
                {
                    let row =
                        live.entry(id)
                            .or_insert((name, None, None, String::new(), None, None));
                    if parent_call_id.is_some() {
                        row.1 = parent_call_id;
                    }
                    if cell_id.is_some() {
                        row.2 = cell_id;
                    }
                    row.3 = state;
                    if subject.is_some() {
                        row.4 = subject;
                    }
                    if let Some(detail) = detail {
                        row.5 = crate::activity::tool_detail_text(row.5.as_deref(), Some(&detail));
                    }
                }
            }
            if matches!(
                event.kind(),
                AgentEventKind::TurnCompleted { .. } | AgentEventKind::TurnFailed { .. }
            ) {
                break;
            }
        }
        turn.result().await.unwrap();
        let source = sessions.transcript_source(session.locator()).await.unwrap();
        crate::runtime::agent_transcript::assert_full_and_narrow_source_keys(&source);
        let rows = crate::runtime::agent_transcript::rows_from_source(&source, &[], false);
        let tools = rows
            .iter()
            .filter_map(TranscriptRow::as_tool)
            .collect::<Vec<_>>();
        let nested = tools
            .iter()
            .filter(|tool| tool.parent_call_id.is_some() && tool.label() == "exec_command")
            .collect::<Vec<_>>();
        assert_eq!(nested.len(), 3, "{rows:?}");
        assert_eq!(
            nested
                .iter()
                .filter(|tool| tool.state.starts_with("failed"))
                .count(),
            1,
            "{rows:?}"
        );
        for tool in tools {
            let observed = live.get(&tool.call_id).unwrap();
            assert_eq!(tool.label(), observed.0);
            assert_eq!(tool.parent_call_id, observed.1);
            assert_eq!(tool.cell_id, observed.2);
            if tool.state == "succeeded" {
                assert_eq!(observed.3, "done", "{}", tool.name);
            } else {
                assert_eq!(tool.state, observed.3, "{}", tool.name);
            }
            assert_eq!(tool.subject, observed.4, "{}", tool.name);
            assert_eq!(tool.detail, observed.5, "{}", tool.name);
        }
        session.release().await.unwrap();
    }
}

pub(in crate::runtime) fn tool_lifecycle_event(
    name: &str,
    call_id: &str,
    kind: &str,
    lineage: &plakous::agent::ToolCallerLineageView,
    observation: serde_json::Value,
) -> AgentEvent {
    use plakous::model::{Digest, DigestDomain};
    let digest = Digest::sha256(DigestDomain::ToolDefinition, 1, &[b"test"]).unwrap();
    let uuid = uuid::Uuid::from_u128(99);
    serde_json::from_value::<AgentEvent>(serde_json::json!({
        "branch_sequence": 1, "event_id": uuid, "session_id": uuid, "branch_id": uuid,
        "agent_id": uuid, "run_id": uuid, "turn_id": null, "attempt_id": null,
        "call_id": call_id, "timestamp": {"seconds": 1, "nanoseconds": 0},
        "causal_parent": null, "origin": "Tool", "classification": "Public",
        "event_schema": 3, "projection_revision": 6, "visibility": "Content", "scope": "Agent", "event_kind_version": 11,
        "kind": {kind: {"identity": {
            "canonical_name": name, "exposed_name": null, "exposure_map_digest": null,
            "registered_tool_id": "test.tool", "executor_id": "test.tool",
            "definition_digest": digest, "schema_digest": digest,
            "effect_metadata_digest": digest, "argument_limits_digest": digest,
        }, "lineage": lineage, "observation": observation}},
        "retained_bytes": 512, "metadata_retained_bytes": 512, "content_retained_bytes": 0,
    })).unwrap()
}

#[test]
fn live_and_rebuilt_marker_and_continuation_rows_match_after_a_yield() {
    use crate::runtime::agent_transcript::rows_from_items;
    use crate::sessions::projector::{TranscriptPart, TranscriptRead};
    use plakous::agent::ToolCallerLineageView;
    use plakous::model::Actor;
    let parent = uuid::Uuid::from_u128(1).to_string();
    let wait = uuid::Uuid::from_u128(3).to_string();
    let starting = ToolCallerLineageView::local_composition_marker(None);
    let marker = ToolCallerLineageView::local_composition_marker(Some("cell-7"));
    let mut projection = TurnProjection::default();
    transcript_ops(
        &tool_lifecycle_event(
            "exec",
            &parent,
            "ToolStarted",
            &starting,
            serde_json::Value::Null,
        ),
        &mut projection,
    );
    let marker_ops = transcript_ops(
        &tool_lifecycle_event(
            "exec",
            &parent,
            "ToolCompleted",
            &marker,
            serde_json::json!({"succeeded":true,"preview":"Script running with cell ID cell-7","subject":null}),
        ),
        &mut projection,
    );
    let wait_ops = transcript_ops(
        &tool_lifecycle_event(
            "wait",
            &wait,
            "ToolStarted",
            &marker,
            serde_json::Value::Null,
        ),
        &mut projection,
    );
    let end_ops = transcript_ops(
        &tool_lifecycle_event(
            "wait",
            &wait,
            "ToolCompleted",
            &marker,
            serde_json::json!({"succeeded":true,"preview":"Script completed","subject":null}),
        ),
        &mut projection,
    );
    let rows = rows_from_items(
        &[
            TranscriptRead {
                position: 0,
                part_offset: 0,
                actor: Actor::Assistant,
                harness_authored: false,
                parts: vec![TranscriptPart::Call {
                    call_id: parent.clone(),
                    canonical: "exec",
                    exposed: "exec",
                    observation: empty_transcript_call(),
                    lineage: &marker,
                    composition_owner: None,
                }],
            },
            TranscriptRead {
                position: 0,
                part_offset: 0,
                actor: Actor::Tool,
                harness_authored: false,
                parts: vec![TranscriptPart::Result {
                    call_id: parent.clone(),
                    succeeded: true,
                    preview: "Script running with cell ID cell-7",
                    subject: None,
                    lineage: &marker,
                }],
            },
            TranscriptRead {
                position: 0,
                part_offset: 0,
                actor: Actor::Assistant,
                harness_authored: false,
                parts: vec![TranscriptPart::Call {
                    call_id: wait.clone(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: empty_transcript_call(),
                    lineage: &marker,
                    composition_owner: Some(parent.clone()),
                }],
            },
            TranscriptRead {
                position: 0,
                part_offset: 0,
                actor: Actor::Tool,
                harness_authored: false,
                parts: vec![TranscriptPart::Result {
                    call_id: wait,
                    succeeded: true,
                    preview: "Script completed",
                    subject: None,
                    lineage: &marker,
                }],
            },
        ],
        format!("{0}/{0}/{0}", uuid::Uuid::from_u128(99))
            .parse()
            .unwrap(),
        &[],
        false,
    );
    for (rebuilt, op) in [
        (rows[0].as_tool().unwrap(), &marker_ops[0]),
        (rows[1].as_tool().unwrap(), &wait_ops[0]),
        (rows[1].as_tool().unwrap(), &end_ops[0]),
    ] {
        let HostEvent::ToolLine {
            call_id,
            parent_call_id,
            cell_id,
            name,
            kind,
            ..
        } = op
        else {
            panic!("tool row");
        };
        assert_eq!(Some(&rebuilt.call_id), call_id.as_ref());
        assert_eq!(&rebuilt.parent_call_id, parent_call_id);
        assert_eq!(&rebuilt.cell_id, cell_id);
        assert_eq!(&rebuilt.name, name);
        assert_eq!(&rebuilt.kind, kind);
    }
}

#[test]
fn live_and_rebuilt_nested_rows_match_for_the_same_tool_history() {
    use crate::runtime::agent_transcript::rows_from_items;
    use crate::sessions::projector::{TranscriptPart, TranscriptRead};
    use plakous::agent::{ToolCallObservation, ToolCallerLineageView};
    use plakous::model::{Actor, ToolCallerLineage};
    let parent = uuid::Uuid::from_u128(1).to_string();
    let marker = ToolCallerLineageView::local_composition_marker(Some("cell-7"));
    let call_id = uuid::Uuid::from_u128(2).to_string();
    let lineage = ToolCallerLineageView::from_lineage(
        &ToolCallerLineage::local_programmatic(41, 7, "cell-7", 1, call_id.parse().unwrap(), 1)
            .unwrap(),
        Some(parent.parse().unwrap()),
    );
    let started: ToolCallObservation = serde_json::from_value(serde_json::json!({
        "subject": "src/main.rs", "arguments_preview": "{\"path\":\"src/main.rs\"}",
    }))
    .unwrap();
    for succeeded in [true, false] {
        let completed = serde_json::json!({
            "succeeded": succeeded, "preview": "read result", "subject": null,
        });
        let mut projection = TurnProjection::default();
        let start_ops = transcript_ops(
            &tool_lifecycle_event(
                "read_file",
                &call_id,
                "ToolStarted",
                &lineage,
                serde_json::to_value(&started).unwrap(),
            ),
            &mut projection,
        );
        let end_ops = transcript_ops(
            &tool_lifecycle_event(
                "read_file",
                &call_id,
                "ToolCompleted",
                &lineage,
                completed.clone(),
            ),
            &mut projection,
        );
        let HostEvent::ToolLine {
            parent_call_id,
            cell_id,
            name,
            kind,
            subject,
            detail: start_detail,
            ..
        } = &start_ops[0]
        else {
            panic!("start row");
        };
        let HostEvent::ToolLine {
            state,
            detail: end_detail,
            ..
        } = &end_ops[0]
        else {
            panic!("end row");
        };
        let rows = rows_from_items(
            &[
                TranscriptRead {
                    position: 0,
                    part_offset: 0,
                    actor: Actor::Assistant,
                    harness_authored: false,
                    parts: vec![
                        TranscriptPart::Call {
                            call_id: parent.clone(),
                            canonical: "exec",
                            exposed: "exec",
                            observation: empty_transcript_call(),
                            lineage: &marker,
                            composition_owner: None,
                        },
                        TranscriptPart::Call {
                            call_id: call_id.clone(),
                            canonical: "read_file",
                            exposed: "read_file",
                            observation: &started,
                            lineage: &lineage,
                            composition_owner: None,
                        },
                    ],
                },
                TranscriptRead {
                    position: 0,
                    part_offset: 0,
                    actor: Actor::Tool,
                    harness_authored: false,
                    parts: vec![TranscriptPart::Result {
                        call_id: call_id.clone(),
                        succeeded,
                        preview: "read result",
                        subject: None,
                        lineage: &lineage,
                    }],
                },
            ],
            format!("{0}/{0}/{0}", uuid::Uuid::from_u128(99))
                .parse()
                .unwrap(),
            &[],
            false,
        );
        let rebuilt = rows[1].as_tool().unwrap();
        assert_eq!(&rebuilt.parent_call_id, parent_call_id);
        assert_eq!(&rebuilt.cell_id, cell_id);
        assert_eq!(&rebuilt.name, name);
        assert_eq!(&rebuilt.kind, kind);
        assert_eq!(&rebuilt.subject, subject);
        assert_eq!(
            rebuilt.state,
            if state == "failed" {
                "failed"
            } else {
                "succeeded"
            }
        );
        assert_eq!(
            rebuilt.detail,
            crate::activity::tool_detail_text(start_detail.as_deref(), end_detail.as_deref())
        );
    }
}

#[tokio::test]
async fn cached_child_progress_does_not_need_a_core_subscriber() {
    let (_root, _sessions, attached, turn, live) = fixture().await;
    let agent = attached.agent().clone();
    let mut held = Vec::new();
    loop {
        match agent.attach(ViewRequest::content()).await {
            Ok(attachment) => {
                held.push(attachment);
                assert!(held.len() <= 4096, "subscriber admission must be bounded");
            }
            Err(error) => {
                assert_eq!(error, plakous::agent::ViewError::SubscriberLimit);
                break;
            }
        }
    }
    let mut observations = ChildObservers::default();
    for _ in 0..3 {
        observations.sync_children(vec![(
            "c1".into(),
            turn.id(),
            agent.clone(),
            live.owner.observation.clone(),
        )]);
        assert!(Arc::ptr_eq(
            &observations.children["c1"].observation,
            &live.owner.observation
        ));
    }
    drop(held);
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    attached.release().await.unwrap();
}

#[test]
fn child_progress_usage_updates_and_terminal_clears_activity() {
    let mut progress = Progress::default();
    progress.observe(
        &AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
            11, 3, 2, 7, 0,
        )))),
        None,
    );
    assert_eq!(progress.view.input_tokens, 11);
    assert_eq!(progress.view.output_tokens, 3);
    progress.view.activity = Some("writing".into());
    progress.observe(
        &AgentEventKind::TurnCompleted {
            usage: plakous::agent::TurnUsage::default(),
        },
        None,
    );
    assert!(progress.view.activity.is_none());
}

#[tokio::test]
async fn child_progress_subscriptions_are_bounded_to_eight() {
    let (_root, _sessions, attached, turn, live) = fixture().await;
    let agent = attached.agent().clone();
    let mut observations = ChildObservers::default();
    observations.sync_children(
        (0..9)
            .map(|id| {
                (
                    format!("c{id}"),
                    turn.id(),
                    agent.clone(),
                    live.owner.observation.clone(),
                )
            })
            .collect(),
    );
    assert_eq!(observations.children.len(), 8);
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    drop(observations);
    attached.release().await.unwrap();
}

#[test]
fn child_progress_usage_snapshots_do_not_double_count() {
    let mut progress = Progress::default();
    for output in [2, 3] {
        progress.observe(
            &AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                11, output, 1, 7, 0,
            )))),
            None,
        );
    }
    assert_eq!(progress.view.output_tokens, 3);
    progress.observe(&AgentEventKind::ModelDispatchStarted, None);
    progress.observe(
        &AgentEventKind::model(ModelEvent::Usage(UsageUpdate::new(Usage::new(
            17, 5, 2, 9, 0,
        )))),
        None,
    );
    assert_eq!(progress.view.input_tokens, 28);
    assert_eq!(progress.view.output_tokens, 8);
}

/// An agent view opened after the child's first text was emitted: that
/// uncommitted block is in neither the committed rows nor the tail, so a gap
/// notice leads the ops. An unread, saturated face is presentation debt, never
/// a reason for the child's cancellation or result to wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_view_marks_an_uncommitted_prefix_and_never_blocks_the_child() {
    let (_root, sessions, attached, turn, mut live) = fixture().await;
    let agent = attached.agent().clone();
    let admission = &mut live.admission;
    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let event = admission.recv().await.unwrap();
            if matches!(event.kind(), AgentEventKind::Model { event: Some(model), .. } if matches!(model.as_ref(), ModelEvent::TextDelta(_))) {
                break event;
            }
        }
    }).await.unwrap();
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c1",
        attached.locator(),
        Arc::clone(&sessions),
        Some((turn.id(), agent.clone(), live.owner.observation.clone())),
        &sender,
    );
    let mut ops = Vec::new();
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let first = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(ops.is_empty(), "{ops:?}");
    assert!(
        first
            .rows
            .contains(&TranscriptRow::assistant("committed prefix")),
        "{:?}",
        first.rows
    );
    let gap = next_event(&received).await;
    assert!(
        matches!(&gap, HostEvent::TranscriptPatch(patch) if patch.scope == TranscriptScope::Agent("c1".into()) && carries_text(&gap, GAP)),
        "{gap:?}"
    );
    let ops = transcript_ops(&event, &mut TurnProjection::default());
    assert!(
        ops.iter()
            .any(|op| matches!(op, HostEvent::AssistantDelta(text) if text == "live text"))
    );
    for _ in 0..1024 {
        sender.send(ContentEvent::AssistantDelta("x".repeat(8192)));
    }
    turn.control().cancel().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), turn.result())
        .await
        .unwrap()
        .unwrap();
    drop(observations);
    attached.release().await.unwrap();
}

pub(in crate::runtime) struct LiveFixture {
    pub(in crate::runtime) owner: super::super::child_projection::ChildTurnProjection,
    admission: plakous::agent::EventSubscription,
}

pub(in crate::runtime) fn start_attach_probe(
    sessions: Arc<AgentSessions>,
    session: &plakous::sessions::OpenedSession,
    turn: TurnId,
    observation: Arc<ChildObservation>,
) -> (ChildObservers, crate::surface_protocol::HostEventReceiver) {
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observers = ChildObservers::default();
    observers.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "attach-order",
        session.locator(),
        sessions,
        Some((turn, session.agent().clone(), observation)),
        &sender,
    );
    (observers, received)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lagged_face_emits_one_resync_gap_and_converges_to_committed_history() {
    use plakous_offline_crash_harness::gate::GateHandle;
    let release_burst = GateHandle::new();
    let finish = GateHandle::new();
    let batches: Vec<_> = (0..16).map(|_| GateHandle::new()).collect();
    let mut stream = vec![serde_json::json!({"gate":release_burst.script()})];
    for batch in &batches {
        stream.extend((0..16).map(|_| serde_json::json!({"text":"burst"})));
        stream.push(serde_json::json!({"gate":batch.script()}));
    }
    stream.push(serde_json::json!({"gate":finish.script()}));
    let (_root, sessions, attached, turn, live) = fixture_with(serde_json::json!([
        {"output":{"text":"prefix"}}, {"stream":stream, "output":{"text":"committed end"}}
    ]))
    .await;
    release_burst.entered().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    let notified = entered.clone();
    let resumed = resume.clone();
    hooks::set_before_follow(
        "lagged-face",
        Arc::new(move || {
            let entered = notified.clone();
            let resume = resumed.clone();
            Box::pin(async move {
                entered.notify_one();
                resume.notified().await;
            })
        }),
    );
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observers = ChildObservers::default();
    observers.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "lagged-face",
        attached.locator(),
        sessions,
        Some((
            turn.id(),
            attached.agent().clone(),
            live.owner.observation.clone(),
        )),
        &sender,
    );
    entered.notified().await;
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let mut ops = Vec::new();
    next_agent_transcript(&received, &mut state, &mut ops).await;
    // Discard the honest attach-prefix notice; only the later lag is counted.
    for event in received.try_iter() {
        if let HostEvent::TranscriptPatch(patch) = event {
            state.apply(patch).unwrap();
        }
    }
    let before = live.owner.observation.snapshot().cursor;
    release_burst.release();
    // Lag the face's 128-update buffer, not the core event feed that the
    // shared reducer itself follows. Every batch must be reduced before
    // another is emitted, even when that reducer is descheduled under load.
    for (index, batch) in batches.iter().enumerate() {
        batch.entered().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while live.owner.observation.snapshot().cursor < before + ((index + 1) * 16) as u64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        batch.release();
    }
    finish.entered().await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while live.owner.observation.snapshot().cursor < before + 256 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    resume.notify_one();
    let gap = next_event(&received).await;
    assert!(carries_text(&gap, GAP), "{gap:?}");
    if let HostEvent::TranscriptPatch(patch) = gap {
        state.apply(patch).unwrap();
    }
    finish.release();
    turn.result().await.unwrap();
    let ended = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        ended
            .rows
            .contains(&TranscriptRow::assistant("committed end")),
        "{ended:?}"
    );
    assert!(
        !ops.iter().any(|op| carries_text(op, GAP)),
        "a single lag produces one gap notice"
    );
    drop(observers);
    attached.release().await.unwrap();
}

async fn fixture() -> (
    tempfile::TempDir,
    Arc<AgentSessions>,
    plakous::sessions::OpenedSession,
    plakous::agent::Turn,
    LiveFixture,
) {
    fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"},"finish":"stop"},
        {"stream":[{"text":"live text"},{"stall":30000}],"finish":"stop"}
    ]))
    .await
}

pub(in crate::runtime) async fn fixture_with(
    steps: serde_json::Value,
) -> (
    tempfile::TempDir,
    Arc<AgentSessions>,
    plakous::sessions::OpenedSession,
    plakous::agent::Turn,
    LiveFixture,
) {
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_offline_crash_harness::{
        BoundaryController, protocol::CrashBoundary, scripted_runtime_binding_with_cursor,
    };
    let root = tempfile::tempdir().unwrap();
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": steps
        }))
        .unwrap(),
    );
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("evidence.jsonl"),
    )
    .unwrap();
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap())
            .unwrap(),
    );
    let binding = scripted_runtime_binding_with_cursor(
        root.path(),
        controller,
        plan,
        Arc::new(std::sync::atomic::AtomicU64::new(1)),
    )
    .unwrap();
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("child-observe:create").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("session required");
    };
    let agent = session.agent().clone();
    agent
        .prompt("prefix")
        .await
        .unwrap()
        .result()
        .await
        .unwrap();
    let (_, admission) = agent
        .attach(ViewRequest::content())
        .await
        .unwrap()
        .into_parts();
    let mut turn = agent.prompt("live").await.unwrap();
    let owner = super::super::child_projection::ChildTurnProjection::admitted(
        &crate::TaskSet::new(),
        &mut turn,
        &agent,
    );
    (
        root,
        sessions,
        session,
        turn,
        LiveFixture { owner, admission },
    )
}

#[tokio::test]
async fn child_observer_selection_keeps_existing_children() {
    let (_root, _sessions, attached, turn, live) = fixture().await;
    let agent = attached.agent().clone();
    let mut observations = ChildObservers::default();
    observations.sync_children(
        (0..9)
            .map(|id| {
                (
                    format!("c{id}"),
                    turn.id(),
                    agent.clone(),
                    live.owner.observation.clone(),
                )
            })
            .collect(),
    );
    let initial = observations.children.keys().cloned().collect::<Vec<_>>();
    observations.sync_children(
        (0..9)
            .rev()
            .map(|id| {
                (
                    format!("c{id}"),
                    turn.id(),
                    agent.clone(),
                    live.owner.observation.clone(),
                )
            })
            .collect(),
    );
    assert_eq!(
        observations.children.keys().cloned().collect::<Vec<_>>(),
        initial
    );
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    drop(observations);
    attached.release().await.unwrap();
}

pub(in crate::runtime) async fn next_event(
    received: &crate::surface_protocol::HostEventReceiver,
) -> HostEvent {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(event) = received.try_recv() {
                return event;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("an observation")
}

#[derive(Debug)]
struct TestView {
    agent_id: String,
    rows: Vec<TranscriptRow>,
    note: Option<String>,
}

async fn next_agent_transcript(
    received: &crate::surface_protocol::HostEventReceiver,
    state: &mut plakous_host_protocol::views::transcript::TranscriptState,
    ops: &mut Vec<HostEvent>,
) -> TestView {
    loop {
        let event = next_event(received).await;
        let settled = match &event {
            HostEvent::PresentationSnapshot(snapshot) => {
                state.replace(snapshot.clone()).unwrap();
                true
            }
            HostEvent::TranscriptPatch(patch) => {
                state.apply(patch.clone()).unwrap();
                !patch.deletes.is_empty()
                    || patch.upserts.is_empty()
                    || patch
                        .upserts
                        .iter()
                        .any(|item| item.key.anchor < 1_u64 << 63)
            }
            _ => false,
        };
        if settled {
            let TranscriptScope::Agent(agent_id) = state.scope() else {
                panic!("agent scope");
            };
            let mut rows: Vec<_> = state.items().values().cloned().collect();
            let note = rows
                .iter()
                .position(|row| {
                    row.text()
                        .is_some_and(|text| text.starts_with("transcript unavailable:"))
                })
                .map(|index| rows.remove(index).text().unwrap().to_owned());
            return TestView {
                agent_id: agent_id.clone(),
                rows,
                note,
            };
        }
        ops.push(event);
    }
}

fn carries_text(op: &HostEvent, text: &str) -> bool {
    match op {
        HostEvent::AssistantDelta(delta) => delta.contains(text),
        HostEvent::TranscriptStream {
            delta:
                plakous_host_protocol::views::transcript::TranscriptStreamDelta::Assistant(delta)
                | plakous_host_protocol::views::transcript::TranscriptStreamDelta::Reasoning {
                    text: delta,
                    ..
                },
            ..
        } => delta.contains(text),
        HostEvent::Row(row) => row.text().is_some_and(|value| value.contains(text)),
        HostEvent::TranscriptPatch(patch) => patch
            .upserts
            .iter()
            .any(|item| item.row.text().is_some_and(|value| value.contains(text))),
        _ => false,
    }
}

fn test_runtime(root: &std::path::Path) -> (String, Arc<ProductTopologyRuntime>) {
    let tree_id = format!("agent-view-{}", uuid::Uuid::new_v4());
    let tree = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        tree_id.clone(),
        "root",
        "owner",
        root.to_path_buf(),
        "tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None)
        .block()
        .unwrap();
    let runtime = crate::topology_runtime::lookup(&tree_id).unwrap();
    (tree_id, runtime)
}

/// A running agent's view: committed rows first, naming the live turn, then
/// only that turn's ops, none of them a committed item again. A tree update
/// for the same turn changes nothing; the turn's end re-sends committed rows;
/// a follow-up turn re-sends and streams; close stops every op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_view_streams_its_live_turn_and_follows_the_next_one() {
    let (root, sessions, attached, turn, live) = fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"},"finish":"stop"},
        {"stream":[{"text":"live text"},{"stall":30000}],"finish":"stop"},
        {"stream":[{"text":"second turn"},{"stall":30000}],"finish":"stop"}
    ]))
    .await;
    let agent = attached.agent().clone();
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c1",
        attached.locator(),
        Arc::clone(&sessions),
        Some((turn.id(), agent.clone(), live.owner.observation.clone())),
        &sender,
    );
    let mut ops = Vec::new();
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let first = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(ops.is_empty(), "{ops:?}");
    assert_eq!(first.agent_id, "c1");
    assert_eq!(first.note, None);
    assert!(
        first.rows.contains(&TranscriptRow::user("prefix")),
        "{:?}",
        first.rows
    );
    assert!(
        first
            .rows
            .contains(&TranscriptRow::assistant("committed prefix"))
    );

    let turn_id = turn.id();
    // A tree revision for the same turn neither resets nor re-sends.
    observations.sync_children(vec![(
        "c1".to_owned(),
        turn_id,
        agent.clone(),
        live.owner.observation.clone(),
    )]);
    tokio::time::sleep(Duration::from_millis(100)).await;
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    let ended = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        ended
            .rows
            .contains(&TranscriptRow::assistant("committed prefix"))
    );
    assert_ne!(ops, [] as [plakous_host_protocol::SessionObservation; 0]);
    for op in &ops {
        let scope = match op {
            HostEvent::TranscriptPatch(patch) => &patch.scope,
            HostEvent::TranscriptStream { scope, .. } => scope,
            _ => panic!("scoped agent content: {op:?}"),
        };
        assert_eq!(*scope, TranscriptScope::Agent("c1".to_owned()));
        assert!(!carries_text(op, "committed prefix"), "{op:?}");
    }
    assert!(received.try_recv().is_err(), "the turn's end re-sends once");

    let mut follow_up = agent.prompt("again").await.unwrap();
    let next_projection = super::super::child_projection::ChildTurnProjection::admitted(
        &crate::TaskSet::new(),
        &mut follow_up,
        &agent,
    );
    observations.sync_children(vec![(
        "c1".to_owned(),
        follow_up.id(),
        agent.clone(),
        next_projection.observation.clone(),
    )]);
    ops.clear();
    let next = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        next.rows.contains(&TranscriptRow::user("prefix")),
        "{next:?}"
    );

    let (tree_id, runtime) = test_runtime(root.path());
    observations
        .action(
            &crate::TaskSet::new(),
            &runtime,
            Some(Arc::clone(&sessions)),
            "s1",
            &UserAction::CloseAgentTranscript,
            &sender,
        )
        .await;
    assert!(observations.agent_view.is_none());
    received.try_iter().for_each(drop);
    follow_up.control().cancel().await.unwrap();
    follow_up.result().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        received.try_iter().all(|event| !matches!(
            event,
            HostEvent::TranscriptPatch(_)
                | HostEvent::PresentationSnapshot(_)
                | HostEvent::TranscriptStream { .. }
        )),
        "nothing arrives for a closed view"
    );
    crate::topology_runtime::force_release(&tree_id).await;
    attached.release().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_session_drops_the_agent_view() {
    let (_root, sessions, attached, turn, _live) = fixture().await;
    let (wire, _received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c1",
        attached.locator(),
        sessions,
        None,
        &sender,
    );
    observations.retain_session("s1");
    assert!(observations.agent_view.is_some());
    observations.retain_session("s2");
    assert!(observations.agent_view.is_none());
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    attached.release().await.unwrap();
}

/// Only a bound non-root node of the attached tree is served; everything
/// else answers at once with empty rows and a note, and opens nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_root_unbound_and_another_trees_agents_answer_with_a_note() {
    use crate::agent_tree_protocol::SpawnAgentRequest;
    let (root, sessions, attached, turn, _live) = fixture().await;
    let (tree_id, runtime) = test_runtime(root.path());
    let unbound = runtime
        .mutate_then_emit(false, |tree| {
            tree.spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-a"))
                .map(|spawned| spawned.agent_id)
                .map_err(|error| format!("{error:?}"))
        })
        .await
        .unwrap();
    let other_tree = format!("agent-view-other-{}", uuid::Uuid::new_v4());
    let mut other = crate::agent_tree::ProductAgentTree::bootstrap_root_for_test(
        other_tree.clone(),
        "root",
        "owner",
        root.path().to_path_buf(),
        "tuple",
        crate::agent_tree_protocol::WriterMode::SingleLease,
    )
    .unwrap();
    let mut foreign = String::new();
    for operation in ["op-b1", "op-b2"] {
        foreign = other
            .spawn_agent("root", &SpawnAgentRequest::test_review(), Some(operation))
            .unwrap()
            .agent_id;
    }
    other
        .bind_session_locator(&foreign, attached.locator().to_string())
        .unwrap();
    assert!(runtime.read(|tree| tree.node(&foreign).is_err()).unwrap());
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    for agent_id in ["nope", "root", unbound.as_str(), foreign.as_str()] {
        observations
            .action(
                &crate::TaskSet::new(),
                &runtime,
                Some(Arc::clone(&sessions)),
                "s1",
                &UserAction::OpenAgentTranscript {
                    agent_id: agent_id.to_owned(),
                },
                &sender,
            )
            .await;
        let HostEvent::PresentationSnapshot(view) = next_event(&received).await else {
            panic!("an agent transcript answers");
        };
        assert_eq!(view.scope, TranscriptScope::Agent(agent_id.to_owned()));
        assert!(
            view.items
                .iter()
                .any(|item| matches!(item.row, TranscriptRow::Notice(_))),
            "{agent_id}"
        );
        assert!(observations.agent_view.is_none());
    }
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    crate::topology_runtime::force_release(&tree_id).await;
    attached.release().await.unwrap();
}

/// When an open agent's live turn completes normally, the
/// re-sent transcript already holds that turn's prompt and committed answer,
/// so the view converges without waiting for another turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turn_end_resend_holds_the_finished_turn() {
    let (_root, sessions, attached, turn, live) = fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"},"finish":"stop"},
        {"stream":[{"text":"live answer"},{"stall":400}],"finish":"stop"}
    ]))
    .await;
    let agent = attached.agent().clone();
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c1",
        attached.locator(),
        Arc::clone(&sessions),
        Some((turn.id(), agent.clone(), live.owner.observation.clone())),
        &sender,
    );
    let mut ops = Vec::new();
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let first = next_agent_transcript(&received, &mut state, &mut ops).await;
    turn.result().await.unwrap();
    let ended = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        first
            .rows
            .contains(&TranscriptRow::assistant("committed prefix"))
    );
    assert!(
        ended.rows.contains(&TranscriptRow::user("live")),
        "{:?}",
        ended.rows
    );
    assert!(
        ended
            .rows
            // The scripted harness commits its canned answer for a streamed step.
            .contains(&TranscriptRow::assistant("loopback turn committed")),
        "{:?}",
        ended.rows
    );
    drop(observations);
    attached.release().await.unwrap();
}

fn has_tool_result(agent: &Agent) -> bool {
    let view = agent.conversation();
    let items = view.items();
    (0..items.len()).any(|index| {
        items[index]
            .blocks()
            .iter()
            .any(|block| matches!(block, plakous::model::ContentBlock::ToolResult(_)))
    })
}

/// An item committed while the view opens lands in its rows: the rows and
/// the ops are partitioned by one atomic attach, so nothing committed before
/// the attach's cursor is missing from both.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commit_while_the_view_opens_is_in_its_rows() {
    let (_root, sessions, attached, turn, live) = fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"},"finish":"stop"},
        {"output":{"tool":{"name":"loopback_effect","arguments":{}}},"finish":"tool_calls","stall_ms":500},
        {"stream":[{"text":"after the tool"},{"stall":30000}],"finish":"stop"}
    ]))
    .await;
    let agent = attached.agent().clone();
    let probe = agent.clone();
    hooks::set_before_attach(
        "c-commit",
        Arc::new(move || {
            let agent = probe.clone();
            Box::pin(async move {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !has_tool_result(&agent) {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("the tool result commits");
            })
        }),
    );
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c-commit",
        attached.locator(),
        Arc::clone(&sessions),
        Some((turn.id(), agent.clone(), live.owner.observation.clone())),
        &sender,
    );
    let mut ops = Vec::new();
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let first = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert!(
        first.rows.iter().any(|row| row
            .as_tool()
            .is_some_and(|tool| tool.name == "loopback_effect" && tool.state == "succeeded")),
        "{:?}",
        first.rows
    );
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    drop(observations);
    attached.release().await.unwrap();
}

/// A failed read re-sends the view's last good rows beside its note rather
/// than emptying the view; a send under a stale generation never lands and
/// never replaces the last good rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_read_keeps_the_last_good_rows_and_stale_serves_send_nothing() {
    let (_root, sessions, attached, turn, _live) = fixture().await;
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let gate = Arc::new(ViewGate::default());
    let context = ServeContext {
        agent_id: "c1".to_owned(),
        locator: attached.locator(),
        sessions: Arc::clone(&sessions),
        jobs: None,
        events: sender.clone(),
        gate: Arc::clone(&gate),
        generation: gate.bump(),
        reads: Arc::new(tokio::sync::Semaphore::new(1)),
        last_good: Arc::new(Mutex::new(None)),
        task_tools: false,
    };
    let rows = vec![TranscriptRow::user("kept")];
    let items = vec![TranscriptItem {
        key: crate::sessions::projector::RowKey::new(
            1,
            crate::sessions::projector::Lane::Runtime,
            0,
        ),
        row: rows[0].clone(),
    }];
    let mut ops = Vec::new();
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    assert!(context.send_transcript(Ok(items.clone()), true));
    let good = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert_eq!((good.rows.clone(), good.note.clone()), (rows.clone(), None));
    assert!(context.send_transcript(Err("transcript unavailable: gone".to_owned()), true));
    let failed = next_agent_transcript(&received, &mut state, &mut ops).await;
    assert_eq!(failed.rows, rows);
    assert_eq!(failed.note.as_deref(), Some("transcript unavailable: gone"));

    gate.bump();
    let retired = vec![TranscriptItem {
        row: TranscriptRow::user("stale"),
        ..items[0].clone()
    }];
    assert!(!context.send_transcript(Ok(retired), true));
    assert!(!(context.emitter())(HostEvent::AssistantDone));
    assert!(received.try_recv().is_err(), "a stale serve sends nothing");
    assert_eq!(
        *context.last_good.lock().unwrap(),
        Some(items),
        "a stale serve's rows are not the view's last good rows"
    );
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    attached.release().await.unwrap();
}

/// Close and a session switch bump the generation, so a serve started
/// before them cannot send after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_and_session_switch_retire_the_views_generation() {
    let (_root, sessions, attached, turn, live) = fixture().await;
    let (wire, _received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c1",
        attached.locator(),
        Arc::clone(&sessions),
        Some((
            turn.id(),
            attached.agent().clone(),
            live.owner.observation.clone(),
        )),
        &sender,
    );
    let open = *observations.gate.lock();
    assert!(
        observations.serves("s1", "c1", turn.id()),
        "a reopen is a no-op"
    );
    assert!(!observations.serves("s1", "c2", turn.id()));
    observations.retain_session("s1");
    assert_eq!(*observations.gate.lock(), open, "the same session keeps it");
    observations.retain_session("s2");
    assert!(observations.agent_view.is_none());
    assert!(!observations.gate.send_if_current(open, || true));
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s2",
        "c1",
        attached.locator(),
        Arc::clone(&sessions),
        None,
        &sender,
    );
    let reopened = *observations.gate.lock();
    observations.close_agent_view();
    assert!(!observations.gate.send_if_current(reopened, || true));
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    attached.release().await.unwrap();
}

/// A tool that reports its start and finishes when released.
struct HeldTool {
    started: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl plakous::tools::ToolExecutor for HeldTool {
    fn execute(
        self: Arc<Self>,
        _: plakous::tools::ToolInvocation,
        _: plakous::tools::ToolContext,
    ) -> plakous::tools::ToolFuture {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            Ok(plakous::tools::ToolOutcome::success_text("held tool done").unwrap())
        })
    }
}

fn has_tool_call(agent: &Agent) -> bool {
    let view = agent.conversation();
    let items = view.items();
    (0..items.len()).any(|index| {
        items[index]
            .blocks()
            .iter()
            .any(|block| matches!(block, plakous::model::ContentBlock::ToolCall(_)))
    })
}

async fn wait_for_held(
    received: &crate::surface_protocol::HostEventReceiver,
    state: &mut plakous_host_protocol::views::transcript::TranscriptState,
    expected: plakous_host_protocol::views::presentation::ActivityState,
) -> plakous_host_protocol::views::transcript::RowKey {
    loop {
        match next_event(received).await {
            HostEvent::PresentationSnapshot(snapshot) => state.replace(snapshot).unwrap(),
            HostEvent::TranscriptPatch(patch) => state.apply(patch).unwrap(),
            _ => continue,
        }
        if let Some((key, _)) = state.items().iter().find(|(_, row)| {
            row.as_tool()
                .is_some_and(|tool| tool.name == "held_tool" && tool.activity_state() == expected)
        }) {
            return *key;
        }
    }
}

/// A late face sees an in-flight call from the admission-owned reducer,
/// then its result while the model's next response is still running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_in_flight_call_attaches_running_and_settles_before_turn_end() {
    use plakous::model::{
        ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract,
    };
    use plakous::sessions::{
        CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
    };
    use plakous::tools::{
        RegisteredToolId, ToolExecutorBindingIdentity, ToolExecutorId, ToolRegistration,
    };
    use plakous_host_protocol::views::presentation::ActivityState;
    use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
    let root = tempfile::tempdir().unwrap();
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let definition = ToolDefinition::new(
        ToolName::new("held_tool").unwrap(),
        "1.0.0",
        "Wait until released.",
        ToolInputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object", "properties": {}, "required": [], "additionalProperties": false
        }))
        .unwrap(),
        ToolOutputContract::model_content(1, 1024).unwrap(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )
    .unwrap();
    let tools = vec![ToolRegistration::new(
        RegisteredToolId::new("held-tool.registration").unwrap(),
        ToolExecutorId::new("held-tool.executor").unwrap(),
        ToolExecutorBindingIdentity::new(1, &[b"held-tool.executor"]).unwrap(),
        definition,
        Arc::new(HeldTool {
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }),
    )];
    let plan = Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
            "steps": [
                {"output":{"tool":{"name":"held_tool","arguments":{}}},"finish":"tool_calls"},
                {"stream":[{"text":"after the tool"},{"stall":30000}],"finish":"stop"}
            ]
        }))
        .unwrap(),
    );
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        root.path().join("evidence.jsonl"),
    )
    .unwrap();
    let sessions = Arc::new(
        AgentSessions::new(SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap())
            .unwrap(),
    );
    let binding = plakous_offline_crash_harness::scripted_topology_runtime_binding(
        root.path(),
        controller,
        plan,
        Arc::new(std::sync::atomic::AtomicU64::new(1)),
        tools,
    )
    .unwrap();
    let CreateSessionOutcome::Created {
        session: attached, ..
    } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("child-observe:held").unwrap(),
            binding,
        ))
        .await
        .unwrap()
    else {
        panic!("session required");
    };
    let agent = attached.agent().clone();
    let mut turn = agent.prompt("live").await.unwrap();
    let projection = super::super::child_projection::ChildTurnProjection::admitted(
        &crate::TaskSet::new(),
        &mut turn,
        &agent,
    );
    tokio::time::timeout(Duration::from_secs(10), started.notified())
        .await
        .expect("the tool starts");
    assert!(!has_tool_call(&agent), "the exchange commits whole");
    tokio::time::timeout(Duration::from_secs(10), async {
        while projection.observation.snapshot().progress.tool_calls == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the admission-owned reducer sees the start before attach");
    let (wire, received) = crate::surface_protocol::host_event_channel();
    let sender: Sender = Sender::over(wire);
    let mut observations = ChildObservers::default();
    observations.start_agent_view(
        &crate::TaskSet::new(),
        false,
        "s1",
        "c-held",
        attached.locator(),
        Arc::clone(&sessions),
        Some((turn.id(), agent.clone(), projection.observation.clone())),
        &sender,
    );
    let mut state = plakous_host_protocol::views::transcript::TranscriptState::default();
    let running_key = wait_for_held(&received, &mut state, ActivityState::Running).await;
    release.notify_one();
    let settled_key = wait_for_held(&received, &mut state, ActivityState::Succeeded).await;
    let settled: Vec<_> = state
        .items()
        .iter()
        .filter(|(_, row)| row.as_tool().is_some_and(|tool| tool.name == "held_tool"))
        .collect();
    assert_eq!(settled.len(), 1);
    assert_eq!(settled_key, running_key);
    let (snapshot, _) = agent
        .attach(ViewRequest::content())
        .await
        .unwrap()
        .into_parts();
    assert!(
        snapshot
            .turns()
            .iter()
            .any(|view| view.turn_id() == turn.id() && view.state() == TurnViewState::Running),
        "the result row must precede the turn terminal"
    );
    turn.control().cancel().await.unwrap();
    turn.result().await.unwrap();
    drop(observations);
    attached.release().await.unwrap();
}

/// An attach names the checkpoint its history belongs to, read in the same
/// instant: after a turn advanced the head, the head it is now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_attach_names_the_checkpoint_of_its_history() {
    let (_root, _sessions, attached, turn, _live) = fixture_with(serde_json::json!([
        {"output":{"text":"committed prefix"},"finish":"stop"},
        {"output":{"text":"second answer"},"finish":"stop"}
    ]))
    .await;
    turn.result().await.unwrap();
    let after = attached.locator();
    let (_, source) =
        SessionTranscriptSource::attach(attached.agent(), ViewRequest::content()).unwrap();
    assert_eq!(source.locator(), after);
    attached.release().await.unwrap();
}
