//! A turn a live owner fenced recovers in place, on the next prompt or on
//! `/recover`, with one note and no second terminal result.

#![allow(
    clippy::too_many_lines,
    reason = "In-process recovery scenarios abandon an owner fence and reopen the same session before checking terminal reconciliation."
)]

use plakous_host_protocol::status::HostPhase;
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
use crate::runtime::{ScriptedRuntimeFactory, spawn_host_worker};

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
    config.runtime_factory = Arc::new(ScriptedRuntimeFactory {
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
        profile_route: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
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

/// A fenced root recovers in place at once, with no action: one `Recovered`
/// observation with the model-request claim, the operator line once, no
/// dead end that asks for `/new`, and the next prompt runs. The projector
/// derives the line into the saved transcript once, and the session log
/// carries the claim once.
#[test]
fn a_fence_recovers_in_place_at_once_and_the_next_prompt_runs() {
    let root = tempfile::tempdir().unwrap();
    let (config, evidence) = scripted_host(root.path());
    let session = config.session.id.clone();
    let home = config.home.clone();
    let store = Arc::clone(&config.store);
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the fenced terminal", recovery_required);
    wait_for(&events, &mut seen, "the eager recovery", |event| {
        matches!(event, HostEvent::Recovered { .. })
    });
    let recovered =
        crate::session_log::read_tail_for_kind(&home, &session, 8, Some("session.recovered"))
            .unwrap();
    assert!(
        recovered.iter().any(|line| line.contains("eager")),
        "{recovered:?}"
    );
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
            HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text, .. })) if text.contains("/new"))),
        "nothing sends the operator to /new: {seen:#?}"
    );
    assert_eq!(
        seen.iter().filter(|event| recovery_required(event)).count(),
        1,
        "the fenced turn has exactly one terminal result"
    );
    assert_eq!(dispatches(&evidence), 2, "nothing was replayed");
    let views = store.load_views(&session).unwrap().unwrap();
    assert_eq!(
        views
            .rows
            .values()
            .filter(|row| matches!(row,
                plakous_host_protocol::views::transcript::TranscriptRow::Notice(notice)
                    if notice.text.starts_with("plak: recovered an interrupted turn; its model request")))
            .count(),
        1,
        "the saved transcript derives the line once: {:#?}",
        views.rows
    );
    let claims =
        crate::session_log::read_tail_for_kind(&home, &session, 8, Some("turn.abandoned")).unwrap();
    assert_eq!(claims.len(), 1, "{claims:?}");
    assert!(claims[0].contains("model_request"), "{claims:?}");
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

/// `/recover` is the explicit control; after the eager reopen it says there
/// is nothing to recover.
#[test]
fn the_explicit_recover_control_recovers_once_and_then_has_nothing_to_do() {
    let root = tempfile::tempdir().unwrap();
    let (config, _) = scripted_host(root.path());
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the fenced terminal", recovery_required);
    wait_for(&events, &mut seen, "the eager recovery", |event| {
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
        |event| matches!(event, HostEvent::Status { phase, .. } if matches!(phase, HostPhase::Idle)),
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
        crate::agent_tree_protocol::NodeStatus::Interrupted
    );
    assert_eq!(fixture.failure_kind(&id), None);
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
    wait_for(
        &events,
        &mut seen,
        "the paused-queue note",
        |event| matches!(event, HostEvent::RestoredPause(pause) if pause.queue),
    );
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
                if note == "fork did not start; request a fork again explicitly")
        })
        .count();
    assert_eq!(notes, 1, "{seen:#?}");
    let stored = store.load(&id).unwrap();
    assert_eq!(
        store.load_intents(&id).unwrap().workflow_requests,
        [] as [crate::sessions::projector::WorkflowRequest; 0]
    );
    assert_eq!(
        stored
            .transcript
            .iter()
            .filter(|row| {
                row.text() == Some("fork did not start; request a fork again explicitly")
            })
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

/// What a fork seeded before its runtime fork committed goes with the fork
/// request reopen withdraws: the genesis tree under the fork's id, and the
/// custody record of a seed that died before writing its rows. A tree under
/// such an id that is not a fork genesis is left alone.
#[test]
fn a_withdrawn_fork_leaves_no_seeded_tree_or_custody_record() {
    use crate::agent_tree::{NodeLineId, ProductAgentTree};
    use crate::agent_tree_protocol::WriterMode;
    use crate::runtime::TreeRows as _;
    let root = tempfile::tempdir().unwrap();
    let answers = || {
        script(serde_json::json!([
            {"output": {"text": "one"}},
            {"output": {"text": "two"}}
        ]))
    };
    let (config, _) = scripted_host_with(root.path(), answers());
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let id = config.session.id.clone();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the first turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let mut session = store.load(&id).unwrap();
    let source = session.plakous_locator.unwrap();
    let request = |operation: &str| crate::sessions::projector::WorkflowRequest {
        operation_id: operation.into(),
        source_locator: source,
        kind: crate::sessions::SessionWorkflowKind::Fork,
    };
    let (seeded, unwritten, foreign) = (
        request("plak:fork:seeded"),
        request("plak:fork:unwritten"),
        request("plak:fork:foreign"),
    );
    for request in [&seeded, &unwritten, &foreign] {
        store
            .submit_workflow_request(&mut session, request)
            .unwrap();
    }
    let rows = crate::runtime::HostRows::new(&state_roots, store.as_ref());
    let source_tree = ProductAgentTree::bootstrap_root_for_test(
        id.clone(),
        "root",
        "owner",
        session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    let seeded_id = seeded.successor_product_id();
    let mut genesis = crate::agent_tree::block_on_for_test(ProductAgentTree::fork_genesis(
        &source_tree,
        &seeded_id,
        session.workspace_root.clone(),
        NodeLineId::parse("0190c3a1-5b7e-7c2d-9f00-0123456789ab").unwrap(),
        source.branch_id().to_string(),
    ))
    .unwrap();
    rows.save_tree(&mut genesis).unwrap();
    // A seed that died after taking custody and before its rows.
    let trees = state_roots.tree_store().unwrap();
    let unwritten_id = unwritten.successor_product_id();
    std::mem::forget(
        crate::agent_tree::acquire_custody(trees.custody_port(), &unwritten_id).unwrap(),
    );
    // A tree under a withdrawn fork's id that is not a fork genesis.
    let foreign_id = foreign.successor_product_id();
    let mut unrelated = ProductAgentTree::bootstrap_root_for_test(
        foreign_id.clone(),
        "root",
        "owner",
        session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    rows.save_tree(&mut unrelated).unwrap();
    assert!(
        trees
            .custody_port()
            .custody(&unwritten_id)
            .unwrap()
            .is_some()
    );

    let (mut config, _) = scripted_host_with(root.path(), answers());
    config.store = Arc::clone(&store);
    config.state_roots = state_roots.clone();
    config.session = store.load(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("second".into()));
    wait_for(&events, &mut seen, "the next turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    assert_eq!(
        store.load_intents(&id).unwrap().workflow_requests,
        [] as [crate::sessions::projector::WorkflowRequest; 0]
    );
    assert!(
        rows.tree(&seeded_id).unwrap().is_none(),
        "the seeded tree stayed"
    );
    assert!(trees.custody_port().custody(&seeded_id).unwrap().is_none());
    assert!(
        trees
            .custody_port()
            .custody(&unwritten_id)
            .unwrap()
            .is_none(),
        "the unwritten seed's custody record stayed"
    );
    assert!(rows.tree(&foreign_id).unwrap().is_some());
}

/// A fork that published keeps its tree: the source's next open withdraws
/// nothing of it, its genesis tree and custody record stay, and the fork
/// opens on them.
#[test]
fn reopening_a_source_after_its_fork_published_keeps_the_forks_tree() {
    use crate::agent_tree::ProductAgentTree;
    use crate::agent_tree_protocol::WriterMode;
    use crate::runtime::TreeRows as _;
    let root = tempfile::tempdir().unwrap();
    let answers = || {
        script(serde_json::json!([
            {"output": {"text": "one"}},
            {"output": {"text": "two"}},
            {"output": {"text": "three"}}
        ]))
    };
    let (config, _) = scripted_host_with(root.path(), answers());
    let store = Arc::clone(&config.store);
    let state_roots = config.state_roots.clone();
    let id = config.session.id.clone();
    let rows = crate::runtime::HostRows::new(&state_roots, store.as_ref());
    let mut source_tree = ProductAgentTree::bootstrap_root_for_test(
        id.clone(),
        "root",
        "owner",
        config.session.workspace_root.clone(),
        "selected-tuple",
        WriterMode::SingleLease,
    )
    .unwrap();
    rows.save_tree(&mut source_tree).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("first".into()));
    wait_for(&events, &mut seen, "the first turn", turn_completed);
    send_attached_action(
        &actions,
        UserAction::ForkSession {
            account: None,
            compact: false,
        },
    );
    wait_for(&events, &mut seen, "the published fork", |event| {
        matches!(event, HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text, .. }))
            if text.contains("forked session"))
    });
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();

    let request = store.load_intents(&id).unwrap().workflow_requests[0].clone();
    assert_eq!(request.kind, crate::sessions::SessionWorkflowKind::Fork);
    let fork_id = request.successor_product_id();
    store.load(&fork_id).expect("the fork published its record");
    let trees = state_roots.tree_store().unwrap();
    let seeded = rows
        .tree(&fork_id)
        .unwrap()
        .expect("the fork's seeded tree");
    assert!(seeded.forked_from().is_some());
    assert!(trees.custody_port().custody(&fork_id).unwrap().is_some());

    // The source reopens and answers.
    let (mut config, _) = scripted_host_with(root.path(), answers());
    config.store = Arc::clone(&store);
    config.state_roots = state_roots.clone();
    config.session = store.load(&id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("on the source".into()));
    wait_for(&events, &mut seen, "the source's turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let kept = rows
        .tree(&fork_id)
        .unwrap()
        .expect("the source's reopen kept the fork's tree");
    assert_eq!(kept.stored_revision, seeded.stored_revision);
    assert!(trees.custody_port().custody(&fork_id).unwrap().is_some());

    // The fork opens on its own tree.
    let (mut config, _) = scripted_host_with(root.path(), answers());
    config.store = Arc::clone(&store);
    config.state_roots = state_roots.clone();
    config.session = store.load(&fork_id).unwrap();
    let (actions, events, worker) = spawn_host_worker(config).unwrap();
    let mut seen = Vec::new();
    send_attached_action(&actions, UserAction::Prompt("on the fork".into()));
    wait_for(&events, &mut seen, "the fork's turn", turn_completed);
    send_attached_action(&actions, UserAction::Quit);
    worker.join().unwrap().unwrap();
    let opened = rows.tree(&fork_id).unwrap().expect("the fork's tree");
    assert!(opened.forked_from().is_some());
    assert!(trees.custody_port().custody(&fork_id).unwrap().is_some());
}
