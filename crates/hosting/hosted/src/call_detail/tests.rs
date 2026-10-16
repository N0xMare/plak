#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{
    CallDetailRequest, CanonicalRead, NOT_IN_CHILD, NOT_IN_SESSION, Target, child_locator, resolve,
};
use crate::agent_tree::ProductAgentTree;
use crate::agent_tree_protocol::{SpawnAgentRequest, WriterMode};
use crate::runtime::test_kit::{MemoryRows, TestExtension};
use crate::sessions::{SessionRecord, ToolItem};
use plakous::sessions::{
    AgentSessions, CompactReason, CompactSessionOutcome, CompactSessionRequest,
    CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionHistoryPageOutcome,
    SessionLocator, SessionOperationId,
};
use plakous_host_protocol::views::{CallDetailOutcome, CallDetailSource};
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

const CALL: &str = "00000000-0000-4000-8000-0000000000ca";
const SECRET: &str = "sk-abcdefghijklmnopqrstuvwxyz0123";

fn activity(call_id: &str, state: &str) -> ToolItem {
    ToolItem {
        call_id: call_id.to_owned(),
        name: "exec_command".to_owned(),
        exposed_name: None,
        kind: crate::activity::tool_activity_kind("exec_command"),
        state: state.to_owned(),
        subject: Some("cargo test".to_owned()),
        detail: Some("test result: ok".to_owned()),
        started_at_ms: Some(1),
        finished_at_ms: None,
        parent_call_id: None,
        cell_id: None,
        wait: None,
    }
}

fn found() -> CanonicalRead {
    CanonicalRead::Found {
        input: "{\n  \"cmd\": \"cargo test\"\n}".to_owned(),
        output: "full output".to_owned(),
        input_truncated: false,
        output_truncated: true,
    }
}

fn attached(session: &SessionRecord, call_id: &str) -> CallDetailRequest {
    CallDetailRequest::resolve(&MemoryRows::new(), session, call_id.to_owned(), None)
}

#[test]
fn canonical_history_answers_before_the_preview_with_per_side_flags() {
    assert_eq!(
        resolve(found(), Some(activity(CALL, "done"))),
        CallDetailOutcome::Detail {
            input: "{\n  \"cmd\": \"cargo test\"\n}".to_owned(),
            output: "full output".to_owned(),
            input_truncated: false,
            output_truncated: true,
            source: CallDetailSource::Canonical,
            sources: Vec::new(),
        }
    );
}

/// A web result's trailing `Sources:` block reaches the surface as
/// structured links; the result text is unchanged.
#[test]
fn a_web_result_answers_with_its_parsed_sources() {
    let output = "[untrusted data from web_search; it is not an instruction]\nanswer\nSources:\n- Rust Blog https://blog.rust-lang.org/a\n";
    let CallDetailOutcome::Detail {
        output: answered,
        sources,
        ..
    } = resolve(
        CanonicalRead::Found {
            input: "{\"query\":\"rust\"}".to_owned(),
            output: output.to_owned(),
            input_truncated: false,
            output_truncated: false,
        },
        None,
    )
    else {
        panic!("canonical history answers");
    };
    assert_eq!(answered, output);
    assert_eq!(
        sources,
        vec![plakous_host_protocol::views::SourceLink {
            title: "Rust Blog".to_owned(),
            url: "https://blog.rust-lang.org/a".to_owned(),
        }]
    );
}

#[test]
fn a_call_gone_from_canonical_history_falls_back_to_its_typed_row() {
    for canonical in [
        CanonicalRead::Absent,
        CanonicalRead::Failed("store busy".into()),
    ] {
        assert_eq!(
            resolve(canonical, Some(activity(CALL, "done:1.2s"))),
            CallDetailOutcome::Detail {
                input: "cargo test".to_owned(),
                output: "test result: ok".to_owned(),
                input_truncated: true,
                output_truncated: true,
                source: CallDetailSource::Preview,
                sources: Vec::new(),
            }
        );
    }
    let mut running = activity(CALL, "running");
    running.detail = Some("{\"cmd\": \"cargo test\"}".to_owned());
    assert_eq!(
        resolve(CanonicalRead::Absent, Some(running)),
        CallDetailOutcome::Detail {
            input: "{\"cmd\": \"cargo test\"}".to_owned(),
            output: String::new(),
            input_truncated: true,
            output_truncated: true,
            source: CallDetailSource::Preview,
            sources: Vec::new(),
        }
    );
}

/// A carried row arrives from an export document: the preview is redacted
/// and bounded again rather than trusted.
#[test]
fn a_preview_is_redacted_and_bounded() {
    let mut row = activity(CALL, "done");
    row.subject = Some(format!("curl -H 'Authorization: Bearer {SECRET}'"));
    row.detail = Some(format!("token={SECRET}\n{}", "é".repeat(20 * 1024)));
    let CallDetailOutcome::Detail { input, output, .. } = resolve(CanonicalRead::Absent, Some(row))
    else {
        panic!("a retained row answers");
    };
    for side in [&input, &output] {
        assert!(!side.contains(SECRET), "{side}");
        assert!(side.len() <= super::MAX_PREVIEW_SIDE_BYTES);
    }
    assert!(output.starts_with("token=[redacted]"));
}

/// A finished await row answers with its stored subject when the result
/// preview is no longer the receipt. The subject is not re-derived here.
#[test]
fn a_finished_await_preview_uses_the_stored_subject_as_its_input() {
    let mut row = activity(CALL, "done");
    row.name = "wait_agent".to_owned();
    row.subject = Some("c1,c2".to_owned());
    row.detail = Some(r#"{"result":"{ "#.trim().to_owned());
    let CallDetailOutcome::Detail {
        input,
        source: CallDetailSource::Preview,
        ..
    } = resolve(CanonicalRead::Absent, Some(row))
    else {
        panic!("a retained row answers from the preview");
    };
    assert_eq!(input, "c1,c2");
}

#[test]
fn a_call_in_neither_store_is_refused() {
    assert_eq!(
        resolve(CanonicalRead::Absent, None),
        CallDetailOutcome::Unavailable {
            reason: NOT_IN_SESSION.to_owned()
        }
    );
    assert_eq!(
        resolve(CanonicalRead::Failed("store busy".into()), None),
        CallDetailOutcome::Unavailable {
            reason: "store busy".to_owned()
        }
    );
}

#[test]
fn canonical_read_debug_never_prints_the_text() {
    let debug = format!("{:?}", found());
    assert!(!debug.contains("cargo test") && !debug.contains("full output"));
    assert!(debug.contains("input_bytes"));
}

#[tokio::test]
async fn only_the_attached_sessions_rows_answer_without_a_runtime() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    assert!(session.transcript.upsert_tool(&activity(CALL, "done")));
    // A host-minted row id (a compact row) is not a canonical call id; only
    // the session's own row can answer it.
    assert!(
        session
            .transcript
            .upsert_tool(&activity("compact:local", "done"))
    );
    for call_id in [CALL, "compact:local"] {
        assert!(matches!(
            attached(&session, call_id).read(None).await.outcome,
            CallDetailOutcome::Detail {
                source: CallDetailSource::Preview,
                ..
            }
        ));
    }
    // Another session's (or a child's) call has no row here; without a
    // runtime its canonical read fails, and nothing else answers.
    let other = "00000000-0000-4000-8000-0000000000cb";
    assert_eq!(
        attached(&session, other).read(None).await.outcome,
        CallDetailOutcome::Unavailable {
            reason: "canonical history is unavailable".to_owned()
        }
    );
    assert_eq!(
        attached(&session, "not-a-call-id").read(None).await.outcome,
        CallDetailOutcome::Unavailable {
            reason: NOT_IN_SESSION.to_owned()
        }
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawned_read_posts_exactly_one_answer_naming_the_request() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.transcript.upsert_tool(&activity(CALL, "done"));
    let session_id = session.id.clone();
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    // A private gate: `spawn` uses the process-wide one, which host-loop tests
    // also take when they note the attached session.
    attached(&session, CALL).spawn_on(
        None,
        crate::runtime::Sender::over(sender),
        super::ReadGate::new(),
    );
    let answer = tokio::task::spawn_blocking(move || {
        let first = receiver.recv_timeout(std::time::Duration::from_secs(10));
        let second = receiver.recv_timeout(std::time::Duration::from_millis(100));
        (first, second.is_err())
    })
    .await
    .unwrap();
    let Ok(crate::runtime::HostEvent::CallDetail(detail)) = answer.0 else {
        panic!("one call detail observation");
    };
    assert_eq!(
        (detail.call_id.as_str(), detail.agent_id, detail.session_id),
        (CALL, None, session_id)
    );
    assert!(answer.1, "exactly one answer");
}

fn tree(tree_id: &str, root: &Path) -> (ProductAgentTree, String) {
    let mut tree = ProductAgentTree::bootstrap_root_for_test(
        tree_id,
        "root",
        "owner",
        root.to_path_buf(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let child = tree
        .spawn_agent("root", &SpawnAgentRequest::test_review(), Some("op-spawn"))
        .unwrap()
        .agent_id;
    (tree, child)
}

#[test]
fn only_a_bound_non_root_node_resolves_to_a_child_session() {
    let root = tempfile::tempdir().unwrap();
    let (mut tree, child) = tree("cd-child-locator", root.path());
    let unbound = child_locator(&tree, &child).unwrap_err();
    assert!(unbound.contains("no bound session"), "{unbound}");
    assert!(child_locator(&tree, "nope").is_err());
    let root_refusal = child_locator(&tree, "root").unwrap_err();
    assert!(root_refusal.contains("tree root"), "{root_refusal}");
    tree.bind_session_locator(&child, "not-a-locator").unwrap();
    assert!(child_locator(&tree, &child).is_err());
}

#[test]
fn a_child_request_without_an_attached_tree_is_refused() {
    let session = SessionRecord::new(PathBuf::from("/work"));
    let request = CallDetailRequest::resolve(
        &MemoryRows::new(),
        &session,
        CALL.to_owned(),
        Some("c1".to_owned()),
    );
    assert!(matches!(request.target, Target::Refused(_)));
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn script(steps: serde_json::Value, identity_namespace: u128) -> Arc<ScriptPlan> {
    Arc::new(
        serde_json::from_value(serde_json::json!({
            "version": SCRIPT_VERSION,
            "steps": steps,
            "identity_namespace": identity_namespace,
        }))
        .unwrap(),
    )
}

/// Creates one session that runs `prompts` against `steps`, and returns it
/// with what it needs to compact.
async fn session_with_turns(
    sessions: &AgentSessions,
    workspace: &Path,
    name: &str,
    steps: serde_json::Value,
    prompts: &[&str],
) -> TestResult<(
    plakous::sessions::OpenedSession,
    BoundaryController,
    Arc<ScriptPlan>,
)> {
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.join(format!("{name}-evidence.jsonl")),
    )?;
    // Scripted call ids share an ordinal when digest JSON widths match, which
    // prompt length does not prevent. Each session name owns a namespace, so
    // the ids differ in the high 64 bits for every wire this binding admits.
    let identity_namespace = match name {
        "root" => 1,
        "child" => 2,
        other => return Err(format!("no scripted call namespace for {other}").into()),
    };
    let plan = script(steps, identity_namespace);
    let binding = plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
        workspace,
        controller.clone(),
        Arc::clone(&plan),
        Arc::new(AtomicU64::new(1)),
    )?;
    let CreateSessionOutcome::Created { session, .. } = sessions
        .create(CreateSessionRequest::new(
            SessionOperationId::new(format!("{name}:create"))?,
            binding,
        ))
        .await?
    else {
        return Err("not created".into());
    };
    for prompt in prompts {
        let result = session.agent().prompt(*prompt).await?.result().await?;
        assert!(
            matches!(
                result.outcome(),
                plakous::agent::TurnResultOutcome::Completed(_)
            ),
            "{name}: {:?}",
            result.outcome()
        );
    }
    Ok((session, controller, plan))
}

async fn first_call_id(sessions: &AgentSessions, locator: SessionLocator) -> TestResult<String> {
    let SessionHistoryPageOutcome::Page { items, .. } = sessions
        .history_page(crate::topology_navigator::initial_history_page_request(
            locator,
        ))
        .await?
    else {
        return Err("no history page".into());
    };
    items
        .iter()
        .flat_map(plakous::sessions::SessionHistoryItem::tool_blocks)
        .map(|block| block.call_id().to_string())
        .next()
        .ok_or_else(|| "no tool call committed".into())
}

const TOOL_ROUND: &str = "loopback_effect";

/// Over a real `AgentSessions`: a call compacted out of the attached
/// session's canonical history answers from its retained row; a child
/// session's call is refused unless the request names that child, and the
/// child path never answers from the attached session's rows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_compacted_call_answers_from_its_row_and_a_childs_call_only_by_agent_id() -> TestResult {
    let directory = tempfile::tempdir()?;
    let workspace = directory.path();
    let sessions = AgentSessions::new(SessionBackend::sqlite(workspace.join("runtime.sqlite"))?)?;
    let tool_then_text = |text: &str| {
        serde_json::json!([
            {"output": {"tool": {"name": TOOL_ROUND, "arguments": {}}}, "finish": "tool_calls"},
            {"output": {"text": text}},
            {"output": {"text": "latest answer"}}
        ])
    };
    let (root, controller, plan) = session_with_turns(
        &sessions,
        workspace,
        "root",
        tool_then_text("first done"),
        &["older intent", "latest intent"],
    )
    .await?;
    let (child, _, _) = session_with_turns(
        &sessions,
        workspace,
        "child",
        tool_then_text("child done"),
        &["child intent that is deliberately longer than the root prompt"],
    )
    .await?;
    let root_call = first_call_id(&sessions, root.locator()).await?;
    let child_call = first_call_id(&sessions, child.locator()).await?;
    assert_ne!(root_call, child_call);

    let tree_id = format!("cd-{}", uuid::Uuid::new_v4().simple());
    let (mut tree, child_agent) = tree(&tree_id, workspace);
    tree.bind_session_locator(&child_agent, child.locator().to_string())
        .unwrap();
    crate::topology_runtime::insert_root(tree_id.clone(), tree, None).unwrap();
    let mut session = SessionRecord::new(workspace.to_path_buf());
    session.id.clone_from(&tree_id);
    session.plakous_locator = Some(root.locator());
    let mut row = activity(&root_call, "done");
    row.detail = Some(format!("token={SECRET}"));
    session.transcript.upsert_tool(&row);
    let read = |call_id: &str, agent_id: Option<&str>| {
        CallDetailRequest::resolve(
            &MemoryRows::new(),
            &session,
            call_id.to_owned(),
            agent_id.map(str::to_owned),
        )
    };

    let before = read(&root_call, None).read(Some(&sessions)).await;
    assert!(
        matches!(
            before.outcome,
            CallDetailOutcome::Detail {
                source: CallDetailSource::Canonical,
                input_truncated: false,
                output_truncated: false,
                ..
            }
        ),
        "{before:?}"
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
            SessionOperationId::new("root:compact")?,
            root.locator(),
            CompactReason::Manual,
            compaction,
        ))
        .await?
    else {
        return Err("not compacted".into());
    };
    session.plakous_locator = Some(target);
    let read = |call_id: &str, agent_id: Option<&str>| {
        CallDetailRequest::resolve(
            &MemoryRows::new(),
            &session,
            call_id.to_owned(),
            agent_id.map(str::to_owned),
        )
    };
    let after = read(&root_call, None).read(Some(&sessions)).await;
    let CallDetailOutcome::Detail {
        source: CallDetailSource::Preview,
        output,
        input_truncated: true,
        output_truncated: true,
        ..
    } = after.outcome
    else {
        return Err(format!("compacted call answers from its row: {after:?}").into());
    };
    assert_eq!(output, "token=[redacted]");

    assert_eq!(
        read(&child_call, None).read(Some(&sessions)).await.outcome,
        CallDetailOutcome::Unavailable {
            reason: NOT_IN_SESSION.to_owned()
        }
    );
    let by_child = read(&child_call, Some(&child_agent))
        .read(Some(&sessions))
        .await;
    assert_eq!(by_child.agent_id.as_deref(), Some(child_agent.as_str()));
    assert!(
        matches!(
            by_child.outcome,
            CallDetailOutcome::Detail {
                source: CallDetailSource::Canonical,
                ..
            }
        ),
        "{by_child:?}"
    );
    assert_eq!(
        read(&root_call, Some(&child_agent))
            .read(Some(&sessions))
            .await
            .outcome,
        CallDetailOutcome::Unavailable {
            reason: NOT_IN_CHILD.to_owned()
        }
    );
    for refused in ["root", "c404"] {
        assert!(matches!(
            read(&root_call, Some(refused))
                .read(Some(&sessions))
                .await
                .outcome,
            CallDetailOutcome::Unavailable { .. }
        ));
    }
    root.release().await?;
    child.release().await?;
    Ok(())
}

#[test]
fn a_request_for_another_session_is_not_read() {
    let session = SessionRecord::new(PathBuf::from("/work"));
    assert!(
        super::prepare(
            &MemoryRows::new(),
            &session,
            CALL.to_owned(),
            None,
            &session.id,
        )
        .is_some()
    );
    assert!(
        super::prepare(
            &MemoryRows::new(),
            &session,
            CALL.to_owned(),
            None,
            "other-session",
        )
        .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_in_flight_read_answers_once_and_the_wait_queue_caps_at_busy() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.transcript.upsert_tool(&activity(CALL, "done"));
    let gate = super::ReadGate::new();
    let permit = gate.reads.try_acquire().expect("hold the loader");
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let events = crate::runtime::Sender::over(sender);
    attached(&session, CALL).spawn_on(None, events.clone(), Arc::clone(&gate));
    // The same call is already in flight: no second answer.
    attached(&session, CALL).spawn_on(None, events.clone(), Arc::clone(&gate));
    for index in 0..super::MAX_WAITING_READS {
        let call_id = format!("00000000-0000-4000-8000-{index:012x}");
        CallDetailRequest::resolve(&MemoryRows::new(), &session, call_id, None).spawn_on(
            None,
            events.clone(),
            Arc::clone(&gate),
        );
    }
    let overflow = format!(
        "00000000-0000-4000-8000-{:012x}",
        super::MAX_WAITING_READS + 8
    );
    CallDetailRequest::resolve(&MemoryRows::new(), &session, overflow.clone(), None).spawn_on(
        None,
        events,
        Arc::clone(&gate),
    );
    let crate::runtime::HostEvent::CallDetail(detail) = receiver.try_recv().expect("busy answer")
    else {
        panic!("busy answer");
    };
    assert_eq!(detail.call_id, overflow);
    assert_eq!(detail.session_id, session.id);
    assert!(
        matches!(detail.outcome, CallDetailOutcome::Unavailable { ref reason } if reason == "busy")
    );
    assert!(receiver.try_recv().is_err(), "only the busy answer so far");
    drop(permit);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_read_is_dropped_when_its_session_is_no_longer_attached() {
    let mut session = SessionRecord::new(PathBuf::from("/work"));
    session.transcript.upsert_tool(&activity(CALL, "done"));
    let gate = super::ReadGate::new();
    gate.note(&session.id);
    let permit = gate.reads.try_acquire().expect("hold the loader");
    let (sender, receiver) = crate::surface_protocol::host_event_channel::<TestExtension>();
    attached(&session, CALL).spawn_on(
        None,
        crate::runtime::Sender::over(sender),
        Arc::clone(&gate),
    );
    gate.note("other-session");
    drop(permit);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while gate.inflight_len() > 0 && tokio::time::Instant::now() < deadline {
        tokio::task::yield_now().await;
    }
    assert_eq!(gate.inflight_len(), 0, "the queued read did not finish");
    assert!(
        receiver.try_recv().is_err(),
        "a read whose session was left must not answer"
    );
}
