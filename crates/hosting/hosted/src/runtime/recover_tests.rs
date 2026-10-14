//! A turn a live owner fenced recovers in place, on the next prompt or on
//! `/recover`, with one note and no second terminal result.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use plakous::agent::{AbandonedTurnClaim, AbandonmentCause, CommandId, TurnResultOutcome};
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
    OpenSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_offline_crash_harness::evidence::{EvidenceKind, read_evidence};
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding_with_cursor};

use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::{fresh_injected_attached_host, send_attached_action};
use crate::runtime::{TestHostInjection, spawn_host_worker};

type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
type UserAction = crate::surface_protocol::UserAction<TestExtension>;

/// The first round's stream is accepted, streams two deltas, and then reports
/// an outcome nobody can observe; every later round answers.
fn fenced_then_answering() -> Arc<ScriptPlan> {
    script(serde_json::json!([
        {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
        {"output": {"text": "the answer after recovery"}}
    ]))
}

pub(super) fn script(steps: serde_json::Value) -> Arc<ScriptPlan> {
    Arc::new(
        serde_json::from_value(serde_json::json!({"version": SCRIPT_VERSION, "steps": steps}))
            .unwrap(),
    )
}

fn dispatches(evidence: &std::path::Path) -> usize {
    read_evidence(evidence)
        .unwrap()
        .iter()
        .filter(|record| record.kind == EvidenceKind::ProviderDispatched)
        .count()
}

/// Reopening the fenced session in the same process abandons the turn the
/// live owner already ended: the claim is the model request, the cause is the
/// in-process fence, and the next prompt runs without replaying anything.
#[test]
fn an_in_process_fence_is_abandoned_by_the_next_open_of_the_same_session() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let evidence = root.path().join("evidence.jsonl");
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let controller =
                BoundaryController::quiet_from_evidence_path(CrashBoundary::None, evidence.clone())
                    .unwrap();
            let script = fenced_then_answering();
            let cursor = Arc::new(AtomicU64::new(1));
            let binding = || {
                scripted_runtime_binding_with_cursor(
                    &workspace,
                    controller.clone(),
                    Arc::clone(&script),
                    Arc::clone(&cursor),
                )
                .unwrap()
            };
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("recover:create").unwrap(),
                    binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("new session")
            };
            let fenced = session
                .agent()
                .start(CommandId::new("recover:first").unwrap(), "first")
                .await
                .unwrap()
                .into_turn()
                .unwrap()
                .result()
                .await
                .unwrap();
            assert!(
                matches!(fenced.outcome(), TurnResultOutcome::RecoveryRequired(_)),
                "the scripted fault fences the turn: {:?}",
                fenced.outcome()
            );
            let fenced_turn = fenced.turn_id();
            let locator = session.release().await.unwrap();

            let OpenSessionOutcome::Opened { session, .. } = sessions
                .open(OpenSessionRequest::new(
                    SessionOperationId::new("recover:reopen").unwrap(),
                    locator,
                    binding(),
                ))
                .await
                .unwrap()
            else {
                panic!("the fenced head is opened, not refused as recovery-only")
            };
            let report = session
                .agent()
                .abandoned_turn()
                .cloned()
                .expect("the reopen reports what it abandoned");
            assert_eq!(report.claim(), AbandonedTurnClaim::ModelRequest);
            assert_eq!(report.cause(), AbandonmentCause::Interrupted);
            let notes = session
                .agent()
                .conversation()
                .items()
                .into_iter()
                .filter(|item| {
                    format!("{:?}", item.blocks()).contains("interrupted before its outcome")
                })
                .count();
            assert_eq!(notes, 1, "exactly one harness note");
            assert_eq!(dispatches(&evidence), 1, "the reopen dispatched nothing");

            let next = session
                .agent()
                .start(CommandId::new("recover:second").unwrap(), "second")
                .await
                .unwrap()
                .into_turn()
                .unwrap()
                .result()
                .await
                .unwrap();
            assert!(matches!(next.outcome(), TurnResultOutcome::Completed(_)));
            assert_ne!(next.turn_id(), fenced_turn);
            assert_eq!(
                dispatches(&evidence),
                2,
                "the faulted round was never replayed"
            );
            session.release().await.unwrap();
        })
        .unwrap();
}

fn scripted_host(
    root: &std::path::Path,
) -> (
    crate::runtime::HostConfig<TestExtension>,
    std::path::PathBuf,
) {
    scripted_host_with(root, fenced_then_answering())
}

pub(super) fn scripted_host_with(
    root: &std::path::Path,
    script: Arc<ScriptPlan>,
) -> (
    crate::runtime::HostConfig<TestExtension>,
    std::path::PathBuf,
) {
    let (mut config, _) = fresh_injected_attached_host(root);
    let evidence = root.join("recover-evidence.jsonl");
    let controller =
        BoundaryController::quiet_from_evidence_path(CrashBoundary::None, evidence.clone())
            .unwrap();
    let cursor = Arc::new(AtomicU64::new(1));
    let workspace = config.cwd.clone();
    let target = scripted_runtime_binding_with_cursor(
        &workspace,
        controller.clone(),
        Arc::clone(&script),
        Arc::clone(&cursor),
    )
    .unwrap()
    .target_binding();
    config.test_injection = Some(Arc::new(TestHostInjection {
        runtime: Arc::new(move |_| {
            scripted_runtime_binding_with_cursor(
                &workspace,
                controller.clone(),
                Arc::clone(&script),
                Arc::clone(&cursor),
            )
            .map_err(|error| error.to_string())
        }),
        rebind_target: target,
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    }));
    (config, evidence)
}

pub(super) fn wait_for(
    events: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    seen: &mut Vec<HostEvent>,
    what: &str,
    matches: impl Fn(&HostEvent) -> bool,
) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(event) = events.recv_timeout(Duration::from_millis(50)) {
            let hit = matches(&event);
            seen.push(event);
            if hit {
                return;
            }
        }
    }
    panic!("timed out waiting for {what}: {seen:#?}");
}

pub(super) fn turn_completed(event: &HostEvent) -> bool {
    matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{lifecycle:?}").contains("Completed"))
}

fn recovery_required(event: &HostEvent) -> bool {
    matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{lifecycle:?}").contains("RecoveryRequired"))
}

fn turn_failed(event: &HostEvent) -> bool {
    matches!(event, HostEvent::TurnLifecycle(lifecycle) if format!("{lifecycle:?}").contains("Failed"))
}

/// The next prompt after a fence recovers the session and then runs: one
/// `Recovered` observation with the model-request claim, the operator line
/// once, and no dead end that asks for `/clear`.
#[test]
fn the_next_prompt_after_a_fence_recovers_in_place_and_runs() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host(root.path());
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the fenced terminal", recovery_required);
    send_attached_action(&actions, UserAction::Prompt("second".into()));
    wait_for(&events, &mut seen, "the next turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    seen.extend(events.try_iter());

    let recovered = seen
        .iter()
        .filter_map(|event| match event {
            HostEvent::Recovered { claim, cause, .. } => Some((claim.clone(), cause.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recovered,
        [(
            Some("model_request".to_owned()),
            Some("interrupted".to_owned())
        )],
        "{seen:#?}"
    );
    let lines = seen
        .iter()
        .filter(|event| {
            matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note.starts_with("plak: recovered an interrupted turn; its model request"))
        })
        .count();
    assert_eq!(lines, 1, "the operator line is printed once");
    assert!(
        !seen.iter().any(|event| matches!(event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text, .. })) if text.contains("/clear"))),
        "nothing sends the operator to /clear: {seen:#?}"
    );
    assert_eq!(
        seen.iter().filter(|event| recovery_required(event)).count(),
        1,
        "the fenced turn has exactly one terminal result"
    );
    assert_eq!(dispatches(&evidence), 2, "nothing was replayed");
}

/// The live OpenRouter Messages failure sequence is deterministic here: an
/// uncertain transport outcome fences one root, then the explicit recovery
/// prompt encounters a context limit. Neither failure is replayed, and only a
/// further explicit prompt starts the successful root on the same session.
#[test]
fn transport_uncertainty_then_context_limit_needs_another_explicit_prompt() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"fault": {"at": "in_band", "kind": "context_overflow"}},
            {"output": {"text": "the answer after explicit recovery"}}
        ])),
    );
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();

    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(
        &events,
        &mut seen,
        "the uncertain terminal",
        recovery_required,
    );
    assert_eq!(
        dispatches(&evidence),
        1,
        "the uncertain root dispatched once"
    );

    send_attached_action(&actions, UserAction::Prompt("recover and continue".into()));
    wait_for(
        &events,
        &mut seen,
        "the context-limit terminal",
        turn_failed,
    );
    assert_eq!(
        dispatches(&evidence),
        2,
        "recovery did not replay the uncertain request"
    );

    send_attached_action(&actions, UserAction::Prompt("try again explicitly".into()));
    wait_for(
        &events,
        &mut seen,
        "the successful terminal",
        turn_completed,
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    seen.extend(events.try_iter());

    assert_eq!(
        seen.iter().filter(|event| recovery_required(event)).count(),
        1,
        "the uncertain root has one terminal result: {seen:#?}"
    );
    assert_eq!(
        seen.iter().filter(|event| turn_failed(event)).count(),
        1,
        "the context-limit root has one terminal result: {seen:#?}"
    );
    assert_eq!(
        seen.iter().filter(|event| turn_completed(event)).count(),
        1,
        "only the third explicit root completes: {seen:#?}"
    );
    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("exceeded the model or route context limit"))),
        "the failed root must be the scripted context limit: {seen:#?}"
    );
    let recovered = seen
        .iter()
        .filter_map(|event| match event {
            HostEvent::Recovered { claim, cause, .. } => Some((claim.clone(), cause.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        recovered,
        [(
            Some("model_request".to_owned()),
            Some("interrupted".to_owned())
        )],
        "the same session is recovered exactly once: {seen:#?}"
    );
    assert_eq!(
        dispatches(&evidence),
        3,
        "no root was automatically replayed"
    );
}

/// `/recover` is the explicit control: it recovers at once, and says so when
/// there is nothing to recover.
#[test]
fn the_explicit_recover_control_recovers_once_and_then_has_nothing_to_do() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host(root.path());
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the fenced terminal", recovery_required);
    send_attached_action(&actions, UserAction::Recover);
    wait_for(&events, &mut seen, "the recovery", |event| {
        matches!(event, HostEvent::Recovered { .. })
    });
    send_attached_action(&actions, UserAction::Recover);
    wait_for(
        &events,
        &mut seen,
        "the second answer",
        |event| matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.starts_with("nothing to recover")),
    );
    send_attached_action(&actions, UserAction::Prompt("second".into()));
    wait_for(&events, &mut seen, "the next turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|event| matches!(event, HostEvent::Recovered { .. }))
            .count(),
        1
    );
}

/// `/recover` with no interrupted root turn runs the tree's recovery pass now
/// and tells the user which stranded child agent it settled.
#[test]
fn the_explicit_recover_control_reports_the_child_it_settled() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = fresh_injected_attached_host(root.path());
    let tree_id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    wait_for(
        &events,
        &mut seen,
        "the open",
        |event| matches!(event, HostEvent::Status { phase, .. } if phase == "idle"),
    );
    // The child is stranded after the open, so only `/recover` can settle it.
    let tokio = tokio::runtime::Runtime::new().unwrap();
    let fixture = crate::agent::RecoveryFixture::new(&tree_id);
    let id = tokio.block_on(fixture.stuck_child());
    send_attached_action(&actions, UserAction::Recover);
    let expected = format!("plak: recovered 1 child agent(s) ({id})");
    wait_for(
        &events,
        &mut seen,
        "the recovery report",
        |event| matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.starts_with(&expected)),
    );
    assert_eq!(
        fixture.status(&id),
        crate::agent_tree_protocol::NodeStatus::Failed
    );
    assert_eq!(
        fixture.failure_kind(&id).as_deref(),
        Some(crate::agent_tree::TURN_ABANDONED)
    );
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
}

/// A follow-up queued behind a fenced turn was written for a turn whose
/// outcome nobody saw, so recovery leaves it paused, says exactly how to
/// resume it, and the resume then delivers it.
#[test]
fn recovery_names_the_paused_queue_and_its_resume_delivers_it() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host_with(
        root.path(),
        script(serde_json::json!([
            {"stall_ms": 400,
             "fault": {"at": "in_band", "kind": "outcome_unknown", "after_events": 2}},
            {"output": {"text": "the queued follow-up's answer"}}
        ])),
    );
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the first turn", |event| {
        matches!(event, HostEvent::PromptStarted(_))
    });
    send_attached_action(&actions, UserAction::FollowUp("queued".into()));
    wait_for(&events, &mut seen, "the fenced terminal", recovery_required);
    send_attached_action(&actions, UserAction::Recover);
    wait_for(&events, &mut seen, "the paused-queue note", |event| {
        matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
            if note.contains("the queue is paused") && note.contains("resume_queue"))
    });
    assert!(
        seen.iter().any(|event| matches!(event,
            HostEvent::QueueSnapshot { items, drain }
                if items.len() == 1
                    && *drain == plakous_host_protocol::QueueDrainState::RestoredPaused)),
        "recovery republishes the paused queue: {seen:#?}"
    );
    assert!(
        !seen.iter().any(turn_completed),
        "nothing drained on its own: {seen:#?}"
    );
    send_attached_action(&actions, UserAction::ResumeQueueDrain);
    wait_for(&events, &mut seen, "the queued turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    assert_eq!(dispatches(&evidence), 2, "nothing was replayed");
}

/// A fork with no runtime admission is withdrawn once on open, leaving the
/// source session available to answer the next prompt.
#[test]
fn an_unadmitted_fork_is_withdrawn_at_open_and_the_source_answers() {
    let root = tempfile::tempdir().unwrap();
    let answers = || {
        script(serde_json::json!([
            {"output": {"text": "one"}},
            {"output": {"text": "two"}}
        ]))
    };
    let (config, _) = scripted_host_with(root.path(), answers());
    let store = Arc::clone(&config.store);
    let id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the first turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let mut session = store.load(&id).unwrap();
    let source = session.plakous_locator.unwrap();
    let request = crate::sessions::projector::WorkflowRequest {
        operation_id: "plak:fork:stranded".into(),
        source_locator: source,
        kind: crate::sessions::SessionWorkflowKind::Fork,
    };
    store
        .submit_workflow_request(&mut session, &request)
        .unwrap();

    let (mut config, _) = scripted_host_with(root.path(), answers());
    config.store = Arc::clone(&store);
    config.session = store.load(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("second".into()));
    wait_for(&events, &mut seen, "the next turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let notes = seen
        .iter()
        .filter(|event| {
            matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                if note == "fork did not start; run /fork again")
        })
        .count();
    assert_eq!(notes, 1, "{seen:#?}");
    let stored = store.load(&id).unwrap();
    assert!(
        store
            .load_intents(&id)
            .unwrap()
            .workflow_requests
            .is_empty()
    );
    assert_eq!(
        stored
            .transcript
            .iter()
            .filter(|row| { row.text() == Some("fork did not start; run /fork again") })
            .count(),
        1,
    );
    assert_eq!(
        stored
            .plakous_locator
            .map(plakous::sessions::SessionLocator::session_id),
        Some(source.session_id())
    );
}
