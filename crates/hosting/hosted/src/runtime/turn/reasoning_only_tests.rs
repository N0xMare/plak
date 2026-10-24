#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::{TurnHostOutcome, terminal_turn_outcome};
use std::sync::Arc;

#[test]
fn terminal_overflow_uses_the_canonical_remedy_even_without_a_capacity_tuple() {
    use plakous::agent::ContextOverflowRemedy;
    use plakous::model::ModelErrorKind;
    use plakous::sessions::{
        AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
        SessionOperationId,
    };
    use plakous_host_protocol::ContextCapacityRecovery;
    use plakous_offline_crash_harness::{
        BoundaryController, projected_terminal_failure_runtime_binding,
    };

    let root = tempfile::tempdir().unwrap();
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("overflow.jsonl"),
    )
    .unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    executor
        .block_on(async {
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("overflow:create").unwrap(),
                    projected_terminal_failure_runtime_binding(
                        root.path(),
                        controller,
                        ModelErrorKind::ContextLimit,
                    )
                    .unwrap(),
                ))
                .await
                .unwrap()
            else {
                panic!("new session");
            };
            let result = session
                .agent()
                .prompt("withheld provider-overflow prompt")
                .await
                .unwrap()
                .result()
                .await
                .unwrap();
            assert!(result.context_capacity().is_none());
            assert_eq!(
                result.context_overflow_remedy(),
                Some(ContextOverflowRemedy::ResendShorter)
            );
            let TurnHostOutcome::ContextCapacity { recovery, .. } = terminal_turn_outcome(&result)
            else {
                panic!("overflow must not become a generic failure");
            };
            assert_eq!(recovery, ContextCapacityRecovery::ResendShorter);
            session.release().await.unwrap();
        })
        .unwrap();
}

/// The peak is populated on a route that reports usage only at the end.
///
/// 2026-09-18, live attempt 3: `peak_prepared_input_tokens` was null on
/// every position on every route. The peak was read only from a mid-stream
/// `ModelEvent::Usage`, and several routes never emit one — they report an
/// operation's counts on its terminal response, which never reaches a
/// Content view the host could observe. The growth loopback has exactly
/// that shape, and this drives it through a real multi-operation turn.
#[test]
fn a_terminal_only_usage_route_still_reports_a_per_operation_peak() {
    let root = tempfile::tempdir().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let usage = executor
        .block_on(async {
            use plakous::sessions::{
                AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                SessionOperationId,
            };
            use plakous_offline_crash_harness::{BoundaryController, placeholder_runtime_binding};
            let workspace = root.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("peak-events.jsonl"),
            )
            .unwrap();
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session: live, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("peak:create").unwrap(),
                    placeholder_runtime_binding(&workspace, controller).unwrap(),
                ))
                .await
                .unwrap()
            else {
                panic!("new session")
            };
            let result = live
                .agent()
                .start(
                    plakous::agent::CommandId::new("peak:turn").unwrap(),
                    "read the big file",
                )
                .await
                .unwrap()
                .into_turn()
                .unwrap()
                .result()
                .await
                .unwrap();
            let usage = result.usage();
            live.release().await.unwrap();
            usage
        })
        .unwrap();
    assert!(
        usage.model_operations_reporting_usage() > 1,
        "a multi-operation turn is what makes the peak distinct from the sum"
    );
    let peak = usage
        .peak_input_tokens()
        .expect("a turn with reporting operations must publish a peak");
    assert!(peak > 0, "the peak must not be null on this route");
    assert!(
        peak < usage.model().input_tokens(),
        "the peak is one operation's prompt, never the sum: {peak} vs {}",
        usage.model().input_tokens()
    );
}

/// A reasoning-only Length round must fail after its bounded retry.
#[test]
fn a_budget_exhausted_completion_arrives_as_typed_failure() {
    let root = tempfile::tempdir().unwrap();
    let executor = plakous::application::ApplicationExecutor::new().unwrap();
    let result = executor
        .block_on(async {
            use plakous::sessions::{
                AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend,
                SessionOperationId,
            };
            use plakous_offline_crash_harness::{
                BoundaryController, script::ScriptPlan, scripted_runtime_binding,
            };
            let workspace = root.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                root.path().join("reasoning-only-events.jsonl"),
            )
            .unwrap();
            let sessions = AgentSessions::new(
                SessionBackend::sqlite(root.path().join("runtime.sqlite3")).unwrap(),
            )
            .unwrap();
            let CreateSessionOutcome::Created { session: live, .. } = sessions
                .create(CreateSessionRequest::new(
                    SessionOperationId::new("reasoning-only:create").unwrap(),
                    // Empty prefix and empty terminal: the model thinks and
                    // emits nothing visible.
                    scripted_runtime_binding(
                        &workspace,
                        controller.clone(),
                        Arc::new(
                            serde_json::from_value::<ScriptPlan>(serde_json::json!({
                                "version": 1,
                                "steps": [
                                    {"output": "reasoning_only", "finish": "length"},
                                    {"output": "reasoning_only", "finish": "length"}
                                ]
                            }))
                            .unwrap(),
                        ),
                    )
                    .unwrap(),
                ))
                .await
                .unwrap()
            else {
                panic!("new session")
            };
            let result = live
                .agent()
                .start(
                    plakous::agent::CommandId::new("reasoning-only:turn").unwrap(),
                    "think about it",
                )
                .await
                .unwrap()
                .into_turn()
                .unwrap()
                .result()
                .await
                .unwrap();
            live.release().await.unwrap();
            result
        })
        .unwrap();
    assert_eq!(
        result.error().unwrap().model_kind(),
        Some(plakous::model::ModelErrorKind::OutputLimit)
    );
    let events = std::fs::read_to_string(root.path().join("reasoning-only-events.jsonl")).unwrap();
    assert_eq!(
        events
            .lines()
            .filter(|line| line.contains("provider_dispatched"))
            .count(),
        2
    );
    let lifecycle = super::turn_lifecycle(&result);
    assert_eq!(lifecycle.model_error_kind.as_deref(), Some("OutputLimit"));
}
