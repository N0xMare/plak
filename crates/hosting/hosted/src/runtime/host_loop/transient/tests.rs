#![allow(clippy::unwrap_used, reason = "unit tests")]

use super::super::*;
use super::{PendingResume, ResumeGate, TransientFailure};
use crate::runtime::test_kit::TestExtension;
use crate::runtime::test_support::fresh_injected_attached_host;
use crate::sessions::TranscriptRow;
use crate::surface_protocol::HostEvent;
use std::time::Duration;

fn plan(gate: ResumeGate, message: &str) -> crate::driver::transient::ResumePlan {
    crate::driver::transient::plan(
        gate,
        crate::driver::transient::ResumeOwner::Root {
            failed_turn: failure(gate).failed_turn,
        },
        message,
        0,
        1,
    )
    .unwrap()
}

fn failure(gate: ResumeGate) -> TransientFailure {
    TransientFailure {
        failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1)).unwrap(),
        gate,
    }
}

#[test]
fn queued_transcript_reads_do_not_cancel_a_transient_resume() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary connection failure")
    }));
    host.role.last_tree_observed_at = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    for _ in 0..2 {
        sender
            .send(UserAction::ResyncTranscript {
                scope: plakous_host_protocol::views::transcript::TranscriptScope::Root,
            })
            .unwrap();
    }
    let _ = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap();
    assert!(host.role.pending_resume.is_some());
}

#[tokio::test(start_paused = true)]
async fn root_transient_waits_for_exact_route_and_cancels_probe() {
    use plakous::model::{
        ModelNetworkFailure, ModelNetworkTransmission, ModelRetryPhase, ReachabilityProbe, RouteId,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let probe = ReachabilityProbe::new(RouteId::new("test:root:ready").unwrap(), {
        let calls = calls.clone();
        let entered = entered.clone();
        move |_, _| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            entered.notify_one();
            Box::pin(async move {
                if n == 0 {
                    Err(ModelNetworkFailure {
                        phase: ModelRetryPhase::Connect,
                        sent: ModelNetworkTransmission::NotSent,
                        timed_out: false,
                    })
                } else {
                    Ok(())
                }
            })
        }
    });
    let pending = PendingResume::spawn(
        &crate::TaskSet::new(),
        "session".into(),
        plan(ResumeGate::Network, "connection unavailable"),
        Some(probe),
    );
    entered.notified().await;
    assert!(!pending.ready.load(Ordering::Acquire));
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::advance(Duration::from_secs(1)).await;
    entered.notified().await;
    tokio::task::yield_now().await;
    assert!(pending.ready.load(Ordering::Acquire));
    let cancellation = pending.cancellation.clone();
    drop(pending);
    assert!(cancellation.is_cancelled());

    let started = Arc::new(tokio::sync::Notify::new());
    let (sent, received) = tokio::sync::oneshot::channel();
    let sent = std::sync::Mutex::new(Some(sent));
    let probe = ReachabilityProbe::new(RouteId::new("test:root:cancel").unwrap(), {
        let started = started.clone();
        move |cancel, _| {
            sent.lock().unwrap().take().unwrap().send(cancel).unwrap();
            started.notify_one();
            Box::pin(std::future::pending())
        }
    });
    let pending = PendingResume::spawn(
        &crate::TaskSet::new(),
        "session".into(),
        plan(ResumeGate::Network, "connection unavailable"),
        Some(probe),
    );
    started.notified().await;
    let probe_cancel = received.await.unwrap();
    drop(pending);
    tokio::task::yield_now().await;
    assert!(probe_cancel.is_cancelled());
}

#[tokio::test(start_paused = true)]
async fn root_transient_delay_and_no_probe_fallback_use_paused_time() {
    use std::sync::atomic::Ordering;
    for gate in [
        ResumeGate::Network,
        ResumeGate::Delay(Duration::from_secs(30)),
    ] {
        let pending = PendingResume::spawn(
            &crate::TaskSet::new(),
            "session".into(),
            plan(gate, "temporary failure"),
            None,
        );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(!pending.ready.load(Ordering::Acquire));
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(pending.ready.load(Ordering::Acquire));
    }
}

#[test]
fn placement_hold_root_ready_transient_retains_pending_instruction_until_placement_ready() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary failure")
    }));
    host.role
        .pending_resume
        .as_ref()
        .unwrap()
        .ready
        .store(true, Ordering::Release);
    let roots = host.role.config.state_roots.clone();
    let registry = plakous::runner::RunnerRegistry::default();
    registry.configure(plakous::runner::ConfiguredRunner {
        runner_id: "local".into(),
        names: vec!["local".into()],
    });
    host.role.config.state_roots = roots.clone().with_runner_readiness(registry, "local");
    assert!(matches!(
        host.block(async |host| host.transient_resume_prompt().await)
            .unwrap(),
        HostStep::Continue
    ));
    assert!(host.role.pending_resume.is_some());
    assert_eq!(host.role.config.session.pending_queue.len(), 0);
    assert!(!host.role.config.session.queue_commands.has_pending());
    host.role.config.state_roots = roots;
    assert!(matches!(
        host.block(async |host| host.transient_resume_prompt().await)
            .unwrap(),
        HostStep::Prompt { .. }
    ));
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn root_transient_ready_enqueues_one_instruction_without_user_history_or_mail() {
    use std::sync::atomic::Ordering;
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    let before = host.role.config.session.prompt_history.clone();
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary connection failure")
    }));
    assert!(matches!(
        host.block(async |host| host.transient_resume_prompt().await)
            .unwrap(),
        HostStep::Continue
    ));
    host.role
        .pending_resume
        .as_ref()
        .unwrap()
        .ready
        .store(true, Ordering::Release);
    let HostStep::Prompt {
        text,
        queued_delivery: Some(item),
        user_echoed: true,
    } = host
        .block(async |host| host.transient_resume_prompt().await)
        .unwrap()
    else {
        panic!("one host instruction");
    };
    let submission = host
        .role
        .config
        .session
        .queue_commands
        .pending_for(&item)
        .unwrap();
    assert_eq!(
        submission.command_id,
        format!(
            "{}{}",
            crate::driver::transient::ROOT_COMMAND_PREFIX,
            failure(ResumeGate::Network).failed_turn
        )
    );
    assert_eq!(submission.canonical_command, text);
    let bytes = serde_json::to_string(&serde_json::json!({
        "command_id": submission.command_id,
        "kind": submission.kind.as_str(),
        "target_turn_id": submission.target_turn_id,
        "canonical_command": submission.canonical_command,
    }))
    .unwrap();
    assert_eq!(
        bytes.as_bytes(),
        include_str!("fixtures/root-resume.json")
            .trim_end()
            .as_bytes()
    );
    assert!(text.contains("do not repeat completed actions"));
    assert_eq!(
        submission.kind,
        sessions::projector::SubmissionKind::HostWake
    );
    assert_eq!(host.role.config.session.prompt_history, before);
    assert!(host.role.pending_resume.is_none());
    assert!(matches!(
        host.block(async |host| host.transient_resume_prompt().await)
            .unwrap(),
        HostStep::Continue
    ));
    host.block(async |host| host.run_prompt(text, Some(item), true, None).await)
        .unwrap();
    assert_eq!(host.role.transient_resumes_used, 1);
    assert!(!host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "still unavailable")
    }));
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn root_transient_user_input_cancels_without_swallowing_prompt_steer_escape_or_quit() {
    use std::sync::atomic::Ordering;
    for action in [
        UserAction::Prompt("new prompt".into()),
        UserAction::Steer("new steer".into()),
        UserAction::Interrupt,
        UserAction::Quit,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
        assert!(host.in_runtime(|host| host.schedule_transient_resume(
            failure(ResumeGate::Network),
            "temporary connection failure"
        )));
        host.role
            .pending_resume
            .as_ref()
            .unwrap()
            .ready
            .store(true, Ordering::Release);
        let prompt = matches!(action, UserAction::Prompt(_));
        let quit = matches!(action, UserAction::Quit);
        sender.send(action).unwrap();
        assert!(matches!(
            host.block(async |host| host.transient_resume_prompt().await)
                .unwrap(),
            HostStep::Continue
        ));
        assert!(host.role.pending_resume.is_none());
        assert!(host.role.interrupt_state.borrow().has_deferred());
        let step = host
            .block(async |host| host.next_prompt(None).await)
            .unwrap();
        assert_eq!(matches!(step, HostStep::Prompt { .. }), prompt);
        assert_eq!(matches!(step, HostStep::Quit), quit);
        if prompt {
            assert!(matches!(step, HostStep::Prompt { text, .. } if text == "new prompt"));
        }
        assert!(observations.try_iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(row)) if row.text == "automatic continuation cancelled")));
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn root_transient_idle_escape_without_pending_work_is_a_noop() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    host.block(async |host| host.next_prompt(None).await)
        .unwrap();
    let before = host.role.config.session.clone();
    let _ = observations.try_iter().collect::<Vec<_>>();
    sender.send(UserAction::Interrupt).unwrap();
    assert!(matches!(
        host.block(async |host| host.next_prompt(None).await)
            .unwrap(),
        HostStep::Continue
    ));
    assert_eq!(host.role.config.session, before);
    assert!(
        !observations
            .try_iter()
            .any(|event| matches!(event, HostEvent::Status { .. } | HostEvent::Row(_)))
    );
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn root_transient_budget_resets_on_user_prompt_and_reload_drops_wait() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary connection failure")
    }));
    host.role.pending_resume = None;
    assert!(!host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "still unavailable")
    }));
    sender
        .send(UserAction::Prompt("new user task".into()))
        .unwrap();
    run_next_prompt(&mut host);
    assert_eq!(host.role.transient_resumes_used, 0);
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary connection failure")
    }));
    let store = host.role.config.store.clone();
    let id = host.role.config.session.id.clone();
    host.block(async |host| host.release().await).unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.session = store.load(&id).unwrap();
    config.store = store;
    let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut reopened =
        RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert!(reopened.role.pending_resume.is_none());
    reopened
        .block(async |reopened| reopened.release().await)
        .unwrap();
}

#[test]
fn root_transient_failure_announces_automatic_continue() {
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    config.store.save(&mut config.session).unwrap();
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    let _ = observations.try_iter().collect::<Vec<_>>();
    host.block(async |host| {
        host.publish_terminal(
            Ok(TurnHostOutcome::Failed {
                transient: Some(TransientFailure {
                    failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1))
                        .unwrap(),
                    gate: ResumeGate::Network,
                }),
                error: "connection temporarily unavailable".into(),
                exchange: crate::agent::TerminalExchange::Closed,
                remedy: None,
            }),
            Instant::now(),
            publication::TerminalErrorPresentation::Events,
        )
        .await
    });
    assert!(observations.try_iter().any(|event| matches!(event,
        HostEvent::Row(TranscriptRow::Notice(row))
            if row.text.contains("will continue automatically")
    )));
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn root_transient_uncancelled_product_full_pauses_without_submission_or_retry() {
    use crate::runtime::fault_store::{FaultStore, StoreCall, StoreFaults};
    use crate::runtime::oneshot::{OneShotOutcome, OneShotState, RunBounds, RunState};
    use crate::session_store::SessionRecordStore as _;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    struct FullSubmission(AtomicBool);
    impl StoreFaults for FullSubmission {
        fn before(&self, _: StoreCall, _: Option<&sessions::SessionRecord>) -> Result<(), String> {
            Ok(())
        }
        fn refusal(&self, call: StoreCall) -> Option<crate::error::StoreRefusal> {
            if call != StoreCall::SaveSubmission {
                return None;
            }
            self.0.store(true, Ordering::Release);
            Some(crate::error::StoreRefusal::StorageFull)
        }
        fn storage_unavailable(&self) -> bool {
            self.0.load(Ordering::Acquire)
        }
    }

    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let records = Arc::new(crate::session_store::MemoryStore::new());
    records.save(&mut config.session).unwrap();
    let id = config.session.id.clone();
    config.store = Arc::new(FaultStore::new(
        records.clone(),
        FullSubmission(AtomicBool::new(false)),
    ));
    let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    assert!(host.in_runtime(|host| {
        host.schedule_transient_resume(failure(ResumeGate::Network), "temporary failure")
    }));
    host.role
        .pending_resume
        .as_ref()
        .unwrap()
        .ready
        .store(true, Ordering::Release);
    assert!(!host.role.config.store.storage_wait_cancelled());
    let mut state = OneShotState {
        attempted: true,
        result: Some(Err(CliFailure::new("original failure"))),
        run: Some(RunState {
            bounds: RunBounds {
                max_turns: 100,
                leave_jobs_running: false,
                deadline: None,
            },
            deadline: Some(Instant::now() + Duration::from_secs(5)),
            turns: 1,
            timed_out: Arc::new(AtomicBool::new(false)),
        }),
    };
    host.block(async |host| host.drive(Some(&mut state)).await)
        .unwrap();
    assert!(!matches!(
        state.result,
        Some(Ok(OneShotOutcome::Bounded { .. }))
    ));
    assert!(host.role.pending_resume.is_none());
    assert_eq!(
        host.role.config.session.queue_drain_state,
        sessions::QueueDrainState::RestoredPaused
    );
    assert!(!host.role.config.session.queue_commands.has_pending());
    assert_eq!(records.load_intents(&id).unwrap().submissions, []);
    assert!(matches!(
        host.block(async |host| host.transient_resume_prompt().await)
            .unwrap(),
        HostStep::Continue
    ));
    let notices: Vec<_> = observations
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::Row(TranscriptRow::Notice(row)) => Some(row.text),
            _ => None,
        })
        .collect();
    let cancelled = notices
        .iter()
        .position(|text| text == "automatic continuation cancelled")
        .unwrap();
    let unavailable = notices
        .iter()
        .position(|text| text.contains("storage write unavailable"))
        .unwrap();
    assert!(cancelled < unavailable);
    assert_eq!(
        notices
            .iter()
            .filter(|text| text.contains("storage write unavailable"))
            .count(),
        1
    );
    host.block(async |host| host.release().await).unwrap();
}

#[test]
fn root_transient_run_bounds_cancel_the_pending_wait() {
    use crate::runtime::oneshot::{OneShotOutcome, OneShotState, RunBounds, RunState};
    use std::sync::Arc;
    for timeout in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
        assert!(host.in_runtime(|host| host.schedule_transient_resume(
            failure(ResumeGate::Network),
            "temporary connection failure"
        )));
        let deadline = timeout.then(Instant::now);
        let mut state = OneShotState {
            attempted: true,
            result: Some(Err(CliFailure::new("original failure"))),
            run: Some(RunState {
                bounds: RunBounds {
                    max_turns: 1,
                    leave_jobs_running: false,
                    deadline,
                },
                deadline,
                turns: 1,
                timed_out: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
        };
        assert!(matches!(
            host.block(async |host| host.next_run_prompt(&mut state).await)
                .unwrap(),
            HostStep::Quit
        ));
        assert!(host.role.pending_resume.is_none());
        assert!(
            matches!(state.result, Some(Ok(OneShotOutcome::Bounded { bound, .. })) if bound == if timeout { "timeout" } else { "max-turns 1" })
        );
        host.block(async |host| host.release().await).unwrap();
    }
}

#[test]
fn root_transient_canonical_failure_continues_in_a_new_turn() {
    use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
    use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding_with_cursor};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    let root = tempfile::tempdir().unwrap();
    let (mut config, _) = fresh_injected_attached_host(root.path());
    let workspace = config.cwd.clone();
    let controller = BoundaryController::quiet_from_evidence_path(
        plakous_offline_crash_harness::protocol::CrashBoundary::None,
        root.path().join("root-transient-events.jsonl"),
    )
    .unwrap();
    let script: Arc<ScriptPlan> = Arc::new(serde_json::from_value(serde_json::json!({
        "version": SCRIPT_VERSION,
        "steps": [
            {"fault": {"at": "pre_stream", "kind": "transport", "phase": "Connect", "retryability": "Exhausted", "message": "route unavailable"}, "output": {"text": "unused"}},
            {"output": {"text": "continued task"}}
        ]
    })).unwrap());
    let cursor = Arc::new(AtomicU64::new(1));
    let binding = scripted_runtime_binding_with_cursor(
        &workspace,
        controller.clone(),
        script.clone(),
        cursor.clone(),
    )
    .unwrap();
    config.runtime_factory = Arc::new(crate::runtime::ScriptedRuntimeFactory {
        rebind_target: binding.target_binding(),
        runtime: Arc::new(move |_| {
            scripted_runtime_binding_with_cursor(
                &workspace,
                controller.clone(),
                script.clone(),
                cursor.clone(),
            )
            .map_err(|error| error.to_string())
        }),
        observe: Arc::new(|_, _| {}),
        child_runtime: None,
        profile_route: None,
        rebind_builds: std::sync::Mutex::new(Vec::new()),
    });
    let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
    let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
    let mut host = RootActor::open_blocking(config, &actions, Sender::over(events), None).unwrap();
    sender
        .send(UserAction::Prompt("continue my task".into()))
        .unwrap();
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap()
    else {
        panic!("user prompt");
    };
    host.block(async |host| {
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .await
    })
    .unwrap();
    assert_eq!(host.role.config.session.root_turn_count, 1);
    let pending = host
        .role
        .pending_resume
        .as_ref()
        .expect("classified canonical failure");
    let failed_turn = pending
        .plan
        .command_id
        .as_str()
        .strip_prefix(crate::driver::transient::ROOT_COMMAND_PREFIX)
        .unwrap()
        .to_owned();
    pending.ready.store(true, Ordering::Release);
    run_next_prompt(&mut host);
    assert_eq!(host.role.config.session.root_turn_count, 2);
    assert_eq!(
        host.role.config.session.prompt_history,
        vec!["continue my task"]
    );
    let lifecycle = observations
        .try_iter()
        .filter_map(|event| match event {
            HostEvent::TurnLifecycle(value) => Some(value),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(lifecycle.iter().any(|value| value.turn_id == failed_turn
        && value.phase == plakous_host_protocol::TurnPhase::Failed));
    assert!(lifecycle.iter().any(|value| value.turn_id != failed_turn
        && value.phase == plakous_host_protocol::TurnPhase::Completed));
    assert!(host.role.pending_resume.is_none());
    assert_eq!(host.role.transient_resumes_used, 1);
    host.block(async |host| host.release().await).unwrap();
}

/// Takes the next prompt the idle loop selects and runs it.
fn run_next_prompt(host: &mut RootActor<'_, TestExtension>) {
    let HostStep::Prompt {
        text,
        queued_delivery,
        user_echoed,
    } = host
        .block(async |host| host.next_prompt(None).await)
        .unwrap()
    else {
        panic!("the idle loop selects a prompt");
    };
    host.block(async |host| {
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .await
    })
    .unwrap();
}
