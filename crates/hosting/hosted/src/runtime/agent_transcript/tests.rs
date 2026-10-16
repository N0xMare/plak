#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{Item, Part, bound_rows, read_rows, read_rows_within, rows_from_items};
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous::model::{Actor, ReasoningVisibility};
use plakous::sessions::{
    AgentSessions, CompactReason, CompactSessionOutcome, CompactSessionRequest,
    CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionLocator, SessionOperationId,
};
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

const SECRET: &str = "sk-abcdefghijklmnopqrstuvwxyz0123";
const CALL: &str = "00000000-0000-4000-8000-0000000000ca";

#[test]
fn unlinked_nested_calls_have_neither_a_parent_nor_a_cell_row_identity() {
    use plakous::agent::ToolCallerLineageView;
    use plakous::model::{NamespaceId, ToolCallerLineage};
    let id = CALL.parse().unwrap();
    let local = ToolCallerLineageView::from_lineage(
        &ToolCallerLineage::local_programmatic(41, 7, "cell-7", 1, id, 1).unwrap(),
        None,
    );
    let hosted = ToolCallerLineageView::from_lineage(
        &ToolCallerLineage::provider_programmatic(
            NamespaceId::new("hosted.program").unwrap(),
            "cell-7",
        )
        .unwrap(),
        None,
    );
    for lineage in [&local, &hosted] {
        let rows = rows_from_items(
            &[
                item(
                    Actor::Assistant,
                    vec![Part::Call {
                        call_id: CALL.into(),
                        canonical: "read_file",
                        exposed: "read_file",
                        observation: None,
                        lineage: Some(lineage),
                        composition_owner: None,
                    }],
                ),
                item(
                    Actor::Tool,
                    vec![Part::Result {
                        call_id: CALL.into(),
                        succeeded: true,
                        preview: "done",
                        subject: None,
                        lineage: Some(lineage),
                    }],
                ),
            ],
            locator(),
            &[],
            false,
        );
        let tool = rows[0].as_tool().unwrap();
        assert_eq!(tool.parent_call_id, None);
        assert_eq!(tool.cell_id, None);
        assert_eq!(tool.state, "succeeded");
    }
}

#[test]
fn explicit_owners_win_over_history_order_and_reused_cell_ids() {
    use plakous::agent::ToolCallerLineageView;
    use plakous::model::ToolCallerLineage;
    let other = "00000000-0000-4000-8000-0000000000d1";
    let child = "00000000-0000-4000-8000-0000000000d2";
    let wait = "00000000-0000-4000-8000-0000000000d3";
    let marker = ToolCallerLineageView::local_composition_marker(Some("cell-7"));
    let nested = ToolCallerLineageView::from_lineage(
        &ToolCallerLineage::local_programmatic(41, 7, "cell-7", 1, child.parse().unwrap(), 1)
            .unwrap(),
        Some(CALL.parse().unwrap()),
    );
    let rows = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![
                Part::Call {
                    call_id: CALL.into(),
                    canonical: "exec",
                    exposed: "exec",
                    observation: None,
                    lineage: Some(&marker),
                    composition_owner: None,
                },
                Part::Call {
                    call_id: other.into(),
                    canonical: "exec",
                    exposed: "exec",
                    observation: None,
                    lineage: Some(&marker),
                    composition_owner: None,
                },
                Part::Call {
                    call_id: child.into(),
                    canonical: "read_file",
                    exposed: "read_file",
                    observation: None,
                    lineage: Some(&nested),
                    composition_owner: None,
                },
                Part::Call {
                    call_id: wait.into(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: None,
                    lineage: Some(&marker),
                    composition_owner: Some(CALL.into()),
                },
            ],
        )],
        locator(),
        &[],
        false,
    );
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect::<Vec<_>>();
    assert_eq!(tools[2].parent_call_id.as_deref(), Some(CALL));
    assert_eq!(tools[3].parent_call_id.as_deref(), Some(CALL));
}

#[test]
fn finished_native_and_local_cells_restore_three_nested_calls_and_one_failure() {
    use plakous::agent::ToolCallerLineageView;
    use plakous::model::ToolCallerLineage;
    let parent = CALL.parse().unwrap();
    let marker = ToolCallerLineageView::local_composition_marker(Some("cell-7"));
    let ids = [
        "00000000-0000-4000-8000-0000000000d1",
        "00000000-0000-4000-8000-0000000000d2",
        "00000000-0000-4000-8000-0000000000d3",
    ];
    let nested = ids
        .iter()
        .enumerate()
        .map(|(index, id)| {
            ToolCallerLineageView::from_lineage(
                &ToolCallerLineage::local_programmatic(
                    41,
                    7,
                    "cell-7",
                    u32::try_from(index + 1).unwrap(),
                    id.parse().unwrap(),
                    1,
                )
                .unwrap(),
                Some(parent),
            )
        })
        .collect::<Vec<_>>();
    for name in ["exec", "exec_js"] {
        let observation = call_observation(serde_json::json!({"path": "src/main.rs"}));
        let mut calls = vec![Part::Call {
            composition_owner: None,
            call_id: CALL.into(),
            canonical: name,
            exposed: name,
            observation: None,
            lineage: Some(&marker),
        }];
        calls.extend(ids.iter().zip(&nested).map(|(id, lineage)| Part::Call {
            composition_owner: None,
            call_id: (*id).into(),
            canonical: "read_file",
            exposed: "read_file",
            observation: Some(&observation),
            lineage: Some(lineage),
        }));
        let mut results = ids
            .iter()
            .zip(&nested)
            .enumerate()
            .map(|(index, (id, lineage))| Part::Result {
                call_id: (*id).into(),
                succeeded: index != 1,
                preview: "read result",
                subject: None,
                lineage: Some(lineage),
            })
            .collect::<Vec<_>>();
        results.push(Part::Result {
            call_id: CALL.into(),
            succeeded: true,
            preview: if name == "exec" {
                "Script running with cell ID cell-7"
            } else {
                "Script completed"
            },
            subject: None,
            lineage: Some(&marker),
        });
        let mut history = vec![item(Actor::Assistant, calls), item(Actor::Tool, results)];
        if name == "exec" {
            let wait = "00000000-0000-4000-8000-0000000000d4";
            history.push(item(
                Actor::Assistant,
                vec![Part::Call {
                    call_id: wait.into(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: None,
                    lineage: Some(&marker),
                    composition_owner: Some(CALL.into()),
                }],
            ));
            history.push(item(
                Actor::Tool,
                vec![Part::Result {
                    call_id: wait.into(),
                    succeeded: true,
                    preview: "Script completed",
                    subject: None,
                    lineage: Some(&marker),
                }],
            ));
        }
        let rows = rows_from_items(&history, locator(), &[], false);
        let tools = rows
            .iter()
            .filter_map(TranscriptRow::as_tool)
            .collect::<Vec<_>>();
        let expected_rows = if name == "exec" { 5 } else { 4 };
        assert_eq!(tools.len(), expected_rows);
        assert_eq!(tools[0].state, "succeeded");
        assert_eq!(tools[0].cell_id.as_deref(), Some("cell-7"));
        assert!(
            tools[1..4]
                .iter()
                .all(|tool| tool.parent_call_id.as_deref() == Some(CALL))
        );
        assert!(
            tools[1..4]
                .iter()
                .all(|tool| tool.cell_id.as_deref() == Some("cell-7"))
        );
        assert!(
            tools[1..4]
                .iter()
                .all(|tool| tool.subject.as_deref() == Some("src/main.rs"))
        );
        assert_eq!(
            tools[1..4]
                .iter()
                .filter(|tool| tool.state == "failed")
                .count(),
            1
        );
        let bounded = bound_rows(rows, false, expected_rows, usize::MAX);
        assert!(matches!(bounded.first(), Some(TranscriptRow::Tool(_))));
        let bounded = bound_rows(bounded, false, 3, usize::MAX);
        assert!(matches!(bounded.first(), Some(TranscriptRow::Truncated)));
        assert!(
            bounded
                .iter()
                .filter_map(TranscriptRow::as_tool)
                .all(|tool| tool.parent_call_id.is_none() && tool.cell_id.is_none())
        );
    }
}

fn locator() -> SessionLocator {
    "00000000-0000-0000-0000-000000000001/00000000-0000-0000-0000-000000000002/00000000-0000-0000-0000-000000000003"
        .parse()
        .unwrap()
}

fn item(actor: Actor, parts: Vec<Part<'_>>) -> Item<'_> {
    Item {
        actor,
        harness_authored: false,
        parts,
    }
}

#[test]
fn mail_is_a_tree_notice_and_harness_notes_and_hidden_reasoning_have_no_row() {
    let mail = serde_json::json!({
        "purpose": "result", "from": "root-c39", "attempt_id": "c39-a2",
        "output": "all green\nmore"
    })
    .to_string();
    let mut harness = item(Actor::User, vec![Part::Text("[plak: wrap up]")]);
    harness.harness_authored = true;
    let rows = rows_from_items(
        &[
            item(Actor::User, vec![Part::Text("do the task")]),
            harness,
            item(
                Actor::Assistant,
                vec![
                    Part::Reasoning {
                        visibility: ReasoningVisibility::Omitted,
                        text: "hidden",
                    },
                    Part::Reasoning {
                        visibility: ReasoningVisibility::Summary,
                        text: "weighing it",
                    },
                    Part::Text("done"),
                ],
            ),
            item(Actor::User, vec![Part::Text(&mail)]),
        ],
        locator(),
        &[],
        false,
    );
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0], TranscriptRow::user("do the task"));
    let thinking = rows[1].as_thinking().unwrap();
    assert_eq!(thinking.text, "weighing it");
    assert_eq!(rows[2], TranscriptRow::assistant("done"));
    assert_eq!(
        rows[3],
        TranscriptRow::info(NoticeScope::Tree, "result · #c39 · a2 · all green")
    );
}

#[test]
fn a_call_joins_its_result_by_call_id_and_a_credential_in_the_result_is_redacted() {
    let preview = format!("token={SECRET}\nok");
    let rows = rows_from_items(
        &[
            item(
                Actor::Assistant,
                vec![
                    Part::Call {
                        composition_owner: None,
                        lineage: None,
                        call_id: CALL.to_owned(),
                        canonical: "read_file",
                        exposed: "read_file",
                        observation: None,
                    },
                    Part::Call {
                        composition_owner: None,
                        lineage: None,
                        call_id: "unsettled".to_owned(),
                        canonical: "read_file",
                        exposed: "read_file",
                        observation: None,
                    },
                ],
            ),
            item(
                Actor::Tool,
                vec![
                    Part::Result {
                        lineage: None,
                        call_id: CALL.to_owned(),
                        succeeded: true,
                        preview: &preview,
                        subject: None,
                    },
                    Part::Result {
                        lineage: None,
                        call_id: "never-called".to_owned(),
                        succeeded: false,
                        preview: "x",
                        subject: None,
                    },
                ],
            ),
            item(Actor::Assistant, vec![Part::Compaction]),
        ],
        locator(),
        &[],
        false,
    );
    assert_eq!(rows.len(), 3, "{rows:?}");
    let tool = rows[0].as_tool().unwrap();
    assert_eq!(
        (tool.call_id.as_str(), tool.name.as_str()),
        (CALL, "read_file")
    );
    assert_eq!(tool.state, "succeeded");
    let detail = tool.detail.as_deref().unwrap();
    assert!(!detail.contains(SECRET), "{detail}");
    assert!(detail.contains("[redacted]"), "{detail}");
    assert_eq!(rows[1].as_tool().unwrap().state, "interrupted");
    let compact = rows[2].as_tool().unwrap();
    assert_eq!(compact.name, "compact");
    assert!(rows[2].ends_group());
}

#[test]
fn rows_past_the_bound_keep_the_newest_behind_one_truncated_row() {
    let rows = (0..10)
        .map(|index| TranscriptRow::user(format!("prompt {index}")))
        .collect::<Vec<_>>();
    let kept = bound_rows(rows.clone(), false, 4, usize::MAX);
    assert_eq!(kept[0], TranscriptRow::Truncated);
    assert_eq!(&kept[1..], &rows[7..]);
    assert_eq!(bound_rows(rows.clone(), false, 64, usize::MAX), rows);
    let mut marked = vec![TranscriptRow::Truncated];
    marked.extend(rows.iter().cloned());
    assert_eq!(bound_rows(rows.clone(), true, 64, usize::MAX), marked);
    let big = vec![TranscriptRow::Tool(crate::sessions::ToolItem {
        call_id: CALL.to_owned(),
        name: "exec_command".to_owned(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("exec_command"),
        state: "succeeded".to_owned(),
        subject: Some("s".repeat(4096)),
        detail: Some("d".repeat(64 * 1024)),
        started_at_ms: None,
        finished_at_ms: None,
        parent_call_id: Some("gone".to_owned()),
        cell_id: Some("cell".to_owned()),
        wait: None,
    })];
    let bounded = bound_rows(big, false, 64, usize::MAX);
    let tool = bounded[0].as_tool().unwrap();
    assert!(tool.subject.as_ref().unwrap().len() <= 240);
    assert!(tool.detail.as_ref().unwrap().len() <= 2 * 1024);
    assert_eq!(
        (tool.parent_call_id.as_ref(), tool.cell_id.as_ref()),
        (None, None)
    );
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn script(steps: serde_json::Value) -> Arc<ScriptPlan> {
    Arc::new(
        serde_json::from_value(serde_json::json!({"version": SCRIPT_VERSION, "steps": steps}))
            .unwrap(),
    )
}

/// Over a real `AgentSessions`: a finished session's committed history maps
/// to the root's row types in order, its compaction is the `compact` row, and
/// a tight bound keeps the newest rows behind `Truncated`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_sessions_history_reads_as_root_rows() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    let sessions = AgentSessions::new(SessionBackend::sqlite(workspace.join("runtime.sqlite"))?)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("evidence.jsonl"),
    )?;
    let plan = script(serde_json::json!([
        {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
        {"output": {"text": "child done"}},
        {"output": {"text": "second answer"}}
    ]));
    let binding = plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
        workspace,
        controller.clone(),
        Arc::clone(&plan),
        Arc::new(AtomicU64::new(1)),
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("agent-transcript:create")?,
            binding,
        ))
        .await?
    else {
        return Err("not created".into());
    };
    for prompt in ["child intent", "second intent"] {
        session.agent().prompt(prompt).await?.result().await?;
    }
    let created = session.locator();
    // The tree keeps the locator it bound; a head that moved is followed.
    let rows = read_rows(&sessions, created, &[]).await?;
    let position = |wanted: &dyn Fn(&TranscriptRow) -> bool| {
        rows.iter()
            .position(wanted)
            .unwrap_or_else(|| panic!("missing row in {rows:?}"))
    };
    let user = position(&|row| *row == TranscriptRow::user("child intent"));
    let tool = position(&|row| {
        row.as_tool()
            .is_some_and(|tool| tool.name == "loopback_effect")
    });
    let answer = position(&|row| *row == TranscriptRow::assistant("child done"));
    assert!(user < tool && tool < answer, "{rows:?}");
    let call = rows[tool].as_tool().unwrap();
    assert!(uuid::Uuid::parse_str(&call.call_id).is_ok(), "{call:?}");
    assert_eq!(call.state, "succeeded");
    assert!(
        call.detail
            .as_deref()
            .is_some_and(|detail| detail.contains("loopback effect committed")),
        "{call:?}"
    );
    assert_eq!(
        rows.last(),
        Some(&TranscriptRow::assistant("second answer"))
    );
    assert!(!rows.contains(&TranscriptRow::Truncated));

    let tight = read_rows_within(&sessions, created, &[], 2, usize::MAX).await?;
    assert_eq!(
        tight,
        vec![
            TranscriptRow::Truncated,
            TranscriptRow::assistant("second answer")
        ]
    );

    let compaction = plakous_offline_crash_harness::scripted_compaction_runtime_binding(
        workspace,
        controller,
        &plan,
        None,
        None,
        Vec::new(),
    )?;
    let CompactSessionOutcome::Compacted { target, .. } = sessions
        .compact(CompactSessionRequest::new(
            SessionOperationId::new("agent-transcript:compact")?,
            session.locator(),
            CompactReason::Manual,
            compaction,
        ))
        .await?
    else {
        return Err("not compacted".into());
    };
    let compacted = read_rows(&sessions, target, &[]).await?;
    assert!(
        compacted
            .iter()
            .any(|row| row.as_tool().is_some_and(|tool| tool.name == "compact")),
        "{compacted:?}"
    );
    session.release().await?;
    Ok(())
}

/// Rows that exactly fill `max_rows` fit; nothing is dropped
/// and no `Truncated` row is invented.
#[test]
fn rows_that_exactly_fit_the_row_bound_are_all_kept() {
    let rows = vec![TranscriptRow::user("a"), TranscriptRow::assistant("b")];
    assert_eq!(bound_rows(rows.clone(), false, 2, usize::MAX), rows);
}

/// A settled process wait reads like the root's live row
/// (`Waited on process 7 · exited 0`), not like a wait still in flight.
#[test]
fn a_settled_process_wait_reads_like_the_live_row() {
    let observation: plakous::agent::ToolCallObservation =
        serde_json::from_value(serde_json::json!({
            "subject": null,
            "arguments_preview": serde_json::to_string_pretty(
                &serde_json::json!({"session_id": 7, "timeout_ms": 1000})
            ).unwrap(),
        }))
        .unwrap();
    let result = serde_json::json!({"exit_code": 0}).to_string();
    let rows = rows_from_items(
        &[
            item(
                Actor::Assistant,
                vec![Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CALL.to_owned(),
                    canonical: "process_session_poll",
                    exposed: "process_session_poll",
                    observation: Some(&observation),
                }],
            ),
            item(
                Actor::Tool,
                vec![Part::Result {
                    lineage: None,
                    call_id: CALL.to_owned(),
                    succeeded: true,
                    preview: &result,
                    subject: None,
                }],
            ),
        ],
        locator(),
        &[],
        false,
    );
    let tool = rows[0].as_tool().unwrap();
    assert_eq!(
        tool.subject.as_deref(),
        Some("Waited on process 7 · exited 0"),
        "{tool:?}"
    );
}

fn call_observation(args: serde_json::Value) -> plakous::agent::ToolCallObservation {
    serde_json::from_value(serde_json::json!({
        "subject": null,
        "arguments_preview": serde_json::to_string_pretty(&args).unwrap(),
    }))
    .unwrap()
}

/// A wait names the command an earlier start in the same history ran, and
/// its wait facts read the outcome for the id its arguments named.
#[test]
fn a_process_wait_names_the_command_an_earlier_start_ran() {
    const WAIT: &str = "00000000-0000-4000-8000-0000000000cb";
    let start = call_observation(serde_json::json!({"cmd": "cargo test"}));
    let wait = call_observation(serde_json::json!({"session_id": 7, "timeout_ms": 1000}));
    let started = serde_json::json!({"session_id": 7, "status": "running"}).to_string();
    let exited = serde_json::json!({"exit_code": 0}).to_string();
    let rows = rows_from_items(
        &[
            item(
                Actor::Assistant,
                vec![Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CALL.to_owned(),
                    canonical: "exec_command",
                    exposed: "exec_command",
                    observation: Some(&start),
                }],
            ),
            item(
                Actor::Tool,
                vec![Part::Result {
                    lineage: None,
                    call_id: CALL.to_owned(),
                    succeeded: true,
                    preview: &started,
                    subject: None,
                }],
            ),
            item(
                Actor::Assistant,
                vec![Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: WAIT.to_owned(),
                    canonical: "process_session_poll",
                    exposed: "process_session_poll",
                    observation: Some(&wait),
                }],
            ),
            item(
                Actor::Tool,
                vec![Part::Result {
                    lineage: None,
                    call_id: WAIT.to_owned(),
                    succeeded: true,
                    preview: &exited,
                    subject: None,
                }],
            ),
        ],
        locator(),
        &[],
        false,
    );
    let tool = rows[1].as_tool().unwrap();
    assert_eq!(
        tool.subject.as_deref(),
        Some("Waited on process 7 · cargo test · exited 0"),
        "{tool:?}"
    );
    assert_eq!(
        tool.wait.as_ref().and_then(|wait| wait.woke_by.as_deref()),
        Some("exited 0"),
        "{tool:?}"
    );
}

/// Returns one large process-shaped JSON result: exit code 1 beside an
/// output that encodes past the history read's exact bound.
struct LargeFailedProcess;

impl plakous::tools::ToolExecutor for LargeFailedProcess {
    fn execute(
        self: Arc<Self>,
        _: plakous::tools::ToolInvocation,
        _: plakous::tools::ToolContext,
    ) -> plakous::tools::ToolFuture {
        Box::pin(async move {
            let output = "\u{1b}[31merr\u{1b}[0m\n".repeat(40_000 / 13);
            let text = serde_json::json!({"exit_code": 1, "output": output}).to_string();
            assert!(text.len() > 64 * 1024, "{}", text.len());
            Ok(plakous::tools::ToolOutcome::success_text(text).unwrap())
        })
    }
}

/// A process call whose committed result is past the history read's exact
/// bound still reads its exit code: the row is `failed:exit 1`, as live.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_failed_process_result_reads_its_exit_code() -> TestResult {
    use plakous::model::{
        ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
        ToolOutputContract,
    };
    use plakous::tools::{
        RegisteredToolId, ToolExecutorBindingIdentity, ToolExecutorId, ToolRegistration,
    };
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    let sessions = AgentSessions::new(SessionBackend::sqlite(workspace.join("runtime.sqlite"))?)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("evidence.jsonl"),
    )?;
    let definition = ToolDefinition::new(
        ToolName::new("exec_command")?,
        "1.0.0",
        "Run a command.",
        ToolInputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object", "properties": {"cmd": {"type": "string"}},
            "required": [], "additionalProperties": false
        }))?,
        ToolOutputContract::model_content(1, 1 << 20)?,
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let tools = vec![ToolRegistration::new(
        RegisteredToolId::new("large-process.registration")?,
        ToolExecutorId::new("large-process.executor")?,
        ToolExecutorBindingIdentity::new(1, &[b"large-process.executor"])?,
        definition,
        Arc::new(LargeFailedProcess),
    )];
    let plan = script(serde_json::json!([
        {"output": {"tool": {"name": "exec_command", "arguments": {"cmd": "make"}}}, "finish": "tool_calls"},
        {"output": {"text": "it failed"}}
    ]));
    let binding = plakous_offline_crash_harness::scripted_topology_runtime_binding(
        workspace,
        controller,
        plan,
        Arc::new(AtomicU64::new(1)),
        tools,
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("agent-transcript:large-process")?,
            binding,
        ))
        .await?
    else {
        return Err("not created".into());
    };
    session.agent().prompt("build it").await?.result().await?;
    let rows = read_rows(&sessions, session.locator(), &[]).await?;
    let tool = rows
        .iter()
        .find_map(TranscriptRow::as_tool)
        .ok_or_else(|| format!("no tool row in {rows:?}"))?;
    assert_eq!(tool.name, "exec_command", "{tool:?}");
    assert_eq!(tool.state, "failed:exit 1", "{tool:?}");
    session.release().await?;
    Ok(())
}

/// A Grok output call naming a child reads as the await it is and a kill
/// naming a child as the interrupt; a process-only output call keeps its own
/// name (presentation only: rows, kinds and subjects).
#[test]
fn grok_calls_naming_a_child_read_as_the_await_and_the_interrupt() {
    const KILL: &str = "00000000-0000-4000-8000-0000000000cc";
    const PROCESS: &str = "00000000-0000-4000-8000-0000000000cd";
    let await_child = call_observation(serde_json::json!({"task_ids": ["c1", "p2"]}));
    let kill_child = call_observation(serde_json::json!({"task_id": "c1"}));
    let process_only = call_observation(serde_json::json!({"task_ids": ["p2"]}));
    let rows = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CALL.to_owned(),
                    canonical: "get_command_or_subagent_output",
                    exposed: "get_command_or_subagent_output",
                    observation: Some(&await_child),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: KILL.to_owned(),
                    canonical: "kill_command_or_subagent",
                    exposed: "kill_command_or_subagent",
                    observation: Some(&kill_child),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: PROCESS.to_owned(),
                    canonical: "get_command_or_subagent_output",
                    exposed: "get_command_or_subagent_output",
                    observation: Some(&process_only),
                },
            ],
        )],
        locator(),
        &[],
        true,
    );
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect::<Vec<_>>();
    assert_eq!(
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>(),
        [
            "wait_agent",
            "interrupt_agent",
            "get_command_or_subagent_output"
        ]
    );
    let leaf = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![Part::Call {
                composition_owner: None,
                lineage: None,
                call_id: CALL.to_owned(),
                canonical: "get_command_or_subagent_output",
                exposed: "get_command_or_subagent_output",
                observation: Some(&await_child),
            }],
        )],
        locator(),
        &[],
        false,
    );
    assert_eq!(
        leaf.iter()
            .find_map(TranscriptRow::as_tool)
            .map(|tool| tool.name.as_str()),
        Some("get_command_or_subagent_output"),
        "a refused Grok leaf call must not restore as wait_agent: {leaf:?}"
    );
    assert_eq!(
        tools[0].kind,
        crate::activity::tool_activity_kind("wait_agent")
    );
    assert!(
        tools[0]
            .subject
            .as_deref()
            .is_some_and(|subject| subject.contains("c1")),
        "{:?}",
        tools[0]
    );
    assert!(
        tools[1]
            .subject
            .as_deref()
            .is_some_and(|subject| subject.contains("c1")),
        "{:?}",
        tools[1]
    );
}

/// On a node granted agent tools pi's `wait` and `stop` are the tree's own
/// slots (canonical and exposed names agree): a `wait` naming processes is
/// the process wait (a bare one too, 300000 ms by default), `stop {id}` names
/// its process, and a `wait` naming a child reads as the await.
#[test]
fn pi_wait_and_stop_history_rows_read_as_process_calls() {
    const BARE: &str = "00000000-0000-4000-8000-0000000000ce";
    const STOP: &str = "00000000-0000-4000-8000-0000000000cf";
    const CHILD: &str = "00000000-0000-4000-8000-0000000000d0";
    let wait = call_observation(serde_json::json!({"ids": ["p7"], "timeout_ms": 5000}));
    let bare = call_observation(serde_json::json!({}));
    let stop = call_observation(serde_json::json!({"id": "p7"}));
    let child = call_observation(serde_json::json!({"ids": ["c1"]}));
    let rows = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CALL.to_owned(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: Some(&wait),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: BARE.to_owned(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: Some(&bare),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: STOP.to_owned(),
                    canonical: "stop",
                    exposed: "stop",
                    observation: Some(&stop),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CHILD.to_owned(),
                    canonical: "wait",
                    exposed: "wait",
                    observation: Some(&child),
                },
            ],
        )],
        locator(),
        &[],
        true,
    );
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect::<Vec<_>>();
    assert!(
        tools[0]
            .subject
            .as_deref()
            .is_some_and(|subject| subject.contains('7')),
        "{:?}",
        tools[0]
    );
    assert_eq!(
        tools[0].wait.as_ref().and_then(|wait| wait.timeout_ms),
        Some(5000)
    );
    assert!(tools[1].wait.is_some(), "{:?}", tools[1]);
    assert!(
        tools[2]
            .subject
            .as_deref()
            .is_some_and(|subject| subject.contains('7')),
        "{:?}",
        tools[2]
    );
    assert_eq!(tools[3].name, "wait_agent");
    assert!(
        tools[3]
            .subject
            .as_deref()
            .is_some_and(|subject| subject.contains("c1")),
        "{:?}",
        tools[3]
    );
    let leaf = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![Part::Call {
                composition_owner: None,
                lineage: None,
                call_id: CHILD.to_owned(),
                canonical: "process_session_poll",
                exposed: "wait",
                observation: Some(&child),
            }],
        )],
        locator(),
        &[],
        false,
    );
    assert_eq!(
        leaf.iter()
            .find_map(TranscriptRow::as_tool)
            .map(|tool| (tool.name.as_str(), tool.label())),
        Some(("process_session_poll", "wait")),
        "a refused pi leaf wait must not restore as wait_agent: {leaf:?}"
    );
}

/// A bare pi `wait` on a node granted agent tools restores as the await when
/// its result is the tree's await receipt (the topology envelope of a Direct
/// call, or the raw receipt in a cell): `wait_agent`, the children it listed,
/// what woke it and the 300000 ms ceiling, as it read live. One that ran the
/// process tool's bare await keeps reading as the process wait, and a leaf's
/// never reads as the await.
#[test]
fn a_bare_pi_wait_that_ran_the_await_restores_as_the_await() {
    const RAW: &str = "00000000-0000-4000-8000-0000000000d1";
    const TASKS: &str = "00000000-0000-4000-8000-0000000000d2";
    let receipt = serde_json::json!({
        "mail": [{"message_id": "m1", "from": "c1", "purpose": "status", "body": "halfway"}],
        "woke_by": "mail",
        "agents": [{"agent_id": "c1", "status": "running"}, {"agent_id": "c2", "status": "running"}],
        "timed_out": false,
    });
    let envelope = serde_json::json!({"result": receipt.to_string()}).to_string();
    let raw = receipt.to_string();
    let tasks =
        serde_json::json!({"tasks": [{"task_id": "p1", "status": "exited", "exit_code": 0}]})
            .to_string();
    let bare = call_observation(serde_json::json!({}));
    let history = |task_tools: bool| {
        let calls = [(CALL, &envelope), (RAW, &raw), (TASKS, &tasks)];
        let items = calls
            .iter()
            .flat_map(|(call_id, preview)| {
                [
                    item(
                        Actor::Assistant,
                        vec![Part::Call {
                            composition_owner: None,
                            lineage: None,
                            call_id: (*call_id).to_owned(),
                            canonical: "wait",
                            exposed: "wait",
                            observation: Some(&bare),
                        }],
                    ),
                    item(
                        Actor::Tool,
                        vec![Part::Result {
                            lineage: None,
                            call_id: (*call_id).to_owned(),
                            succeeded: true,
                            preview: preview.as_str(),
                            subject: None,
                        }],
                    ),
                ]
            })
            .collect::<Vec<_>>();
        rows_from_items(&items, locator(), &[], task_tools)
    };
    let rows = history(true);
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .collect::<Vec<_>>();
    for tool in &tools[..2] {
        assert_eq!(tool.name, "wait_agent", "{tool:?}");
        assert_eq!(tool.kind, crate::activity::tool_activity_kind("wait_agent"));
        assert_eq!(tool.subject.as_deref(), Some("c1,c2"), "{tool:?}");
        let wait = tool.wait.as_ref().unwrap();
        assert_eq!(wait.woke_by.as_deref(), Some("mail from c1"), "{tool:?}");
        assert_eq!(wait.timeout_ms, Some(300_000), "{tool:?}");
    }
    assert_eq!(tools[2].name, "wait", "{:?}", tools[2]);
    let leaf = history(false);
    assert!(
        leaf.iter()
            .filter_map(TranscriptRow::as_tool)
            .all(|tool| tool.name == "wait"),
        "a leaf's pi wait never restores as the await: {leaf:?}"
    );
}

/// A preview that is still the topology envelope, but whose receipt no longer
/// parses, does not carry the children. The observation subject does.
#[test]
fn a_restored_bare_wait_keeps_the_observation_subject_when_the_preview_drops_it() {
    let bare = call_observation(serde_json::json!({}));
    let preview = serde_json::json!({"result": "{"}).to_string();
    assert!(
        serde_json::from_str::<serde_json::Value>(
            serde_json::from_str::<serde_json::Value>(&preview).unwrap()["result"]
                .as_str()
                .unwrap()
        )
        .is_err()
    );
    let row = |subject: Option<&str>| {
        let rows = rows_from_items(
            &[
                item(
                    Actor::Assistant,
                    vec![Part::Call {
                        composition_owner: None,
                        lineage: None,
                        call_id: CALL.to_owned(),
                        canonical: "wait",
                        exposed: "wait",
                        observation: Some(&bare),
                    }],
                ),
                item(
                    Actor::Tool,
                    vec![Part::Result {
                        lineage: None,
                        call_id: CALL.to_owned(),
                        succeeded: true,
                        preview: preview.as_str(),
                        subject,
                    }],
                ),
            ],
            locator(),
            &[],
            true,
        );
        rows.into_iter()
            .find_map(|row| match row {
                TranscriptRow::Tool(tool) => Some(tool),
                _ => None,
            })
            .unwrap()
    };
    let dropped = row(None);
    assert_eq!(dropped.name, "wait_agent", "{dropped:?}");
    assert_eq!(dropped.subject, None, "{dropped:?}");
    let kept = row(Some("c1,c2"));
    assert_eq!(kept.name, "wait_agent", "{kept:?}");
    assert_eq!(kept.subject.as_deref(), Some("c1,c2"), "{kept:?}");
}

/// pi's `edit` and `write` over one patch tool restore as two rows labelled by
/// the names the model called, each classified by the canonical name, as
/// their live rows are.
#[test]
fn aliased_history_calls_are_labelled_by_exposed_name_and_classified_by_canonical() {
    const WRITE: &str = "00000000-0000-4000-8000-0000000000e1";
    let edit = call_observation(serde_json::json!({"path": "src/main.rs"}));
    let write = call_observation(serde_json::json!({"path": "src/lib.rs"}));
    let rows = rows_from_items(
        &[item(
            Actor::Assistant,
            vec![
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: CALL.to_owned(),
                    canonical: "apply_patch",
                    exposed: "edit",
                    observation: Some(&edit),
                },
                Part::Call {
                    composition_owner: None,
                    lineage: None,
                    call_id: WRITE.to_owned(),
                    canonical: "apply_patch",
                    exposed: "write",
                    observation: Some(&write),
                },
            ],
        )],
        locator(),
        &[],
        false,
    );
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .map(|tool| {
            (
                tool.name.as_str(),
                tool.exposed_name.as_deref(),
                tool.label(),
                tool.kind,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        tools,
        [
            (
                "apply_patch",
                Some("edit"),
                "edit",
                crate::activity::tool_activity_kind("apply_patch")
            ),
            (
                "apply_patch",
                Some("write"),
                "write",
                crate::activity::tool_activity_kind("apply_patch")
            ),
        ]
    );
}

/// The same pair from a real committed history: pi's `edit` and `write` in one
/// round of a Pi-presented session read back as two rows labelled by the
/// names the model called, each classified by the canonical `apply_patch`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn committed_pi_aliases_read_back_labelled_by_exposed_name() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    std::fs::write(workspace.join("edited.txt"), "before\n")?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(workspace.join("runtime.sqlite"))?)?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join("evidence.jsonl"),
    )?;
    let plan = script(serde_json::json!([
        {"output": {"tools": [
            {"name": "edit", "arguments": {
                "path": "edited.txt",
                "edits": [{"oldText": "before", "newText": "after"}]
            }},
            {"name": "write", "arguments": {"path": "written.txt", "content": "written\n"}}
        ]}, "finish": "tool_calls"},
        {"output": {"text": "patched"}}
    ]));
    let binding =
        plakous_offline_crash_harness::scripted_pi_runtime_binding(workspace, controller, &plan)?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new("agent-transcript:pi-aliases")?,
            binding,
        ))
        .await?
    else {
        return Err("not created".into());
    };
    session.agent().prompt("patch").await?.result().await?;
    let rows = read_rows(&sessions, session.locator(), &[]).await?;
    let tools = rows
        .iter()
        .filter_map(TranscriptRow::as_tool)
        .map(|tool| {
            (
                tool.name.as_str(),
                tool.exposed_name.as_deref(),
                tool.label(),
                tool.kind,
                tool.state.as_str(),
            )
        })
        .collect::<Vec<_>>();
    let kind = crate::activity::tool_activity_kind("apply_patch");
    assert_eq!(
        tools,
        [
            ("apply_patch", Some("edit"), "edit", kind, "succeeded"),
            ("apply_patch", Some("write"), "write", kind, "succeeded"),
        ],
        "{rows:?}"
    );
    assert_eq!(rows.last(), Some(&TranscriptRow::assistant("patched")));
    Ok(())
}

#[test]
fn child_mail_riding_a_prompt_is_one_tree_row_per_child_not_part_of_the_user_row() {
    let frame = crate::runtime::result_mail::CHILD_MAIL_FRAME;
    let command = format!(
        "do the task\n\n{frame}\n\n<child_message message_id=\"m1\" from=\"tree-c7\" purpose=\"finding\">\nsaw it\n</child_message>"
    );
    let rows = rows_from_items(
        &[item(Actor::User, vec![Part::Text(&command)])],
        locator(),
        &[],
        false,
    );
    assert_eq!(
        rows,
        vec![
            TranscriptRow::user("do the task"),
            TranscriptRow::info(NoticeScope::Tree, "finding · #c7 · saw it"),
        ]
    );
}
