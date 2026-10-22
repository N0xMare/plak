//! Root transient continuation owns no turn while waiting.

use super::{CliFailure, CliResult, HostLoop, HostStep, InterruptWatch};
use crate::agent::transient::ResumeGate;
use crate::runtime::TransientFailure;
use crate::sessions::{NoticeScope, TranscriptRow};
use plakous::model::{CancellationToken, ReachabilityProbe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub(in crate::runtime) const COMMAND_PREFIX: &str = "plak:resume-transient:root:";

pub(super) struct PendingResume {
    session_id: String,
    failed_turn: plakous::model::TurnId,
    error: String,
    ready: Arc<AtomicBool>,
    cancellation: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl PendingResume {
    fn spawn(
        session_id: String,
        failure: TransientFailure,
        error: String,
        probe: Option<ReachabilityProbe>,
    ) -> Self {
        let ready = Arc::new(AtomicBool::new(false));
        let cancellation = CancellationToken::new();
        let signal = ready.clone();
        let cancel = cancellation.clone();
        let task = tokio::spawn(async move {
            let (probe, fallback) = match failure.gate {
                ResumeGate::Network => (probe, Duration::from_secs(30)),
                ResumeGate::Delay(delay) => (None, delay),
            };
            let reached = crate::runtime::network_watch::NetworkWatch::shared()
                .wait(probe, fallback, &cancel)
                .await;
            signal.store(reached, Ordering::Release);
        });
        Self {
            session_id,
            failed_turn: failure.failed_turn,
            error,
            ready,
            cancellation,
            task,
        }
    }
}

impl Drop for PendingResume {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn schedule_transient_resume(
        &mut self,
        failure: TransientFailure,
        error: &str,
    ) -> bool {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.defer_input();
        if self.recovery_blocked
            || self.quit_started_at.is_some()
            || watch.interrupted()
            || self.interrupt_state.borrow().has_deferred()
            || self.pending_start.is_some()
            || !self.config.session.pending_queue.is_empty()
            || self.config.session.queue_commands.has_pending()
        {
            return false;
        }
        let budget = self
            .config
            .state_roots
            .roster_source()
            .roster(
                self.config.session.account.as_deref().unwrap_or_default(),
                &self.config.model_id,
                self.config.session.effort.as_deref(),
            )
            .map_or(0, |roster| roster.transient_resumes.min(2));
        if self.transient_resumes_used >= budget {
            return false;
        }
        let human = error.to_owned();
        let wait = match failure.gate {
            ResumeGate::Network => "when the network returns".to_owned(),
            ResumeGate::Delay(delay) => format!("after {}s", delay.as_secs()),
        };
        let session_id = self.config.session.id.clone();
        let probe = self.opened.reachability_probe();
        let Ok(pending) = self
            .executor
            .block_on(async { PendingResume::spawn(session_id, failure, human.clone(), probe) })
        else {
            return false;
        };
        self.pending_resume = Some(pending);
        self.transient_resumes_used += 1;
        let _ = self.events.notice(TranscriptRow::info(
            NoticeScope::Turn,
            format!(
                "{human}; will continue automatically {wait} ({}/{budget}) \u{b7} Esc cancels",
                self.transient_resumes_used,
            ),
        ));
        true
    }

    pub(super) fn cancel_transient_resume(&mut self) {
        if self.pending_resume.take().is_some() {
            let _ = self.events.notice(TranscriptRow::info(
                NoticeScope::Turn,
                "automatic continuation cancelled",
            ));
        }
    }

    pub(super) fn transient_resume_prompt(&mut self) -> CliResult<HostStep> {
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.defer_input();
        if watch.interrupted() || self.interrupt_state.borrow().has_deferred() {
            self.cancel_transient_resume();
            return Ok(HostStep::Continue);
        }
        let Some(pending) = self.pending_resume.as_ref() else {
            return Ok(HostStep::Continue);
        };
        if pending.session_id != self.config.session.id
            || self.recovery_blocked
            || self.pending_start.is_some()
            || !self.config.session.pending_queue.is_empty()
            || self.config.session.queue_commands.has_pending()
        {
            self.cancel_transient_resume();
            return Ok(HostStep::Continue);
        }
        if !pending.ready.load(Ordering::Acquire) {
            return Ok(HostStep::Continue);
        }
        let command = format!("{COMMAND_PREFIX}{}", pending.failed_turn);
        let text = format!(
            "The previous turn failed transiently: {}. Continue the current task from the last completed step; do not repeat completed actions.",
            pending.error,
        );
        self.pending_resume = None;
        let mut candidate = self.config.session.clone();
        let item = candidate
            .enqueue_live(text.clone())
            .map_err(|_| CliFailure::new("transient continuation queue admission was refused"))?;
        self.config.session = super::super::result_mail::prepare_with_command(
            &self.config.topology_rows(),
            self.config.store.as_ref(),
            &candidate,
            &item,
            &text,
            None,
            &command,
        )
        .map_err(CliFailure::new)?;
        Ok(HostStep::Prompt {
            text,
            queued_delivery: Some(item),
            user_echoed: true,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::super::*;
    use super::{PendingResume, ResumeGate, TransientFailure};
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::sessions::TranscriptRow;

    fn failure(gate: ResumeGate) -> TransientFailure {
        TransientFailure {
            failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1)).unwrap(),
            gate,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn root_transient_waits_for_exact_route_and_cancels_probe() {
        use plakous::model::{
            ModelNetworkFailure, ModelNetworkTransmission, ModelRetryPhase, ReachabilityProbe,
            RouteId,
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
            "session".into(),
            failure(ResumeGate::Network),
            "connection unavailable".into(),
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
            "session".into(),
            failure(ResumeGate::Network),
            "connection unavailable".into(),
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
                "session".into(),
                failure(gate),
                "temporary failure".into(),
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
    fn root_transient_ready_enqueues_one_instruction_without_user_history_or_mail() {
        use std::sync::atomic::Ordering;
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        let before = host.config.session.prompt_history.clone();
        assert!(host.schedule_transient_resume(
            failure(ResumeGate::Network),
            "temporary connection failure"
        ));
        assert!(matches!(
            host.transient_resume_prompt().unwrap(),
            HostStep::Continue
        ));
        host.pending_resume
            .as_ref()
            .unwrap()
            .ready
            .store(true, Ordering::Release);
        let HostStep::Prompt {
            text,
            queued_delivery: Some(item),
            user_echoed: true,
        } = host.transient_resume_prompt().unwrap()
        else {
            panic!("one host instruction");
        };
        let submission = host
            .config
            .session
            .queue_commands
            .pending_for(&item)
            .unwrap();
        assert_eq!(
            submission.command_id,
            format!(
                "{}{}",
                super::COMMAND_PREFIX,
                failure(ResumeGate::Network).failed_turn
            )
        );
        assert_eq!(submission.canonical_command, text);
        assert!(text.contains("do not repeat completed actions"));
        assert_eq!(
            submission.kind,
            sessions::projector::SubmissionKind::HostWake
        );
        assert_eq!(host.config.session.prompt_history, before);
        assert!(host.pending_resume.is_none());
        assert!(matches!(
            host.transient_resume_prompt().unwrap(),
            HostStep::Continue
        ));
        host.run_prompt(text, Some(item), true, None).unwrap();
        assert_eq!(host.transient_resumes_used, 1);
        assert!(!host.schedule_transient_resume(failure(ResumeGate::Network), "still unavailable"));
        host.release().unwrap();
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
            let (events, observations) =
                crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
            assert!(host.schedule_transient_resume(
                failure(ResumeGate::Network),
                "temporary connection failure"
            ));
            host.pending_resume
                .as_ref()
                .unwrap()
                .ready
                .store(true, Ordering::Release);
            let prompt = matches!(action, UserAction::Prompt(_));
            let quit = matches!(action, UserAction::Quit);
            sender.send(action).unwrap();
            assert!(matches!(
                host.transient_resume_prompt().unwrap(),
                HostStep::Continue
            ));
            assert!(host.pending_resume.is_none());
            assert!(host.interrupt_state.borrow().has_deferred());
            let step = host.next_prompt(None).unwrap();
            assert_eq!(matches!(step, HostStep::Prompt { .. }), prompt);
            assert_eq!(matches!(step, HostStep::Quit), quit);
            if prompt {
                assert!(matches!(step, HostStep::Prompt { text, .. } if text == "new prompt"));
            }
            assert!(observations.try_iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(row)) if row.text == "automatic continuation cancelled")));
            host.release().unwrap();
        }
    }

    #[test]
    fn root_transient_idle_escape_without_pending_work_is_a_noop() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        host.next_prompt(None).unwrap();
        let before = host.config.session.clone();
        let _ = observations.try_iter().collect::<Vec<_>>();
        sender.send(UserAction::Interrupt).unwrap();
        assert!(matches!(
            host.next_prompt(None).unwrap(),
            HostStep::Continue
        ));
        assert_eq!(host.config.session, before);
        assert!(
            !observations
                .try_iter()
                .any(|event| matches!(event, HostEvent::Status { .. } | HostEvent::Row(_)))
        );
        host.release().unwrap();
    }

    #[test]
    fn root_transient_budget_resets_on_user_prompt_and_reload_drops_wait() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        assert!(host.schedule_transient_resume(
            failure(ResumeGate::Network),
            "temporary connection failure"
        ));
        host.pending_resume = None;
        assert!(!host.schedule_transient_resume(failure(ResumeGate::Network), "still unavailable"));
        sender
            .send(UserAction::Prompt("new user task".into()))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("user prompt");
        };
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert_eq!(host.transient_resumes_used, 0);
        assert!(host.schedule_transient_resume(
            failure(ResumeGate::Network),
            "temporary connection failure"
        ));
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        host.release().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session = store.load(&id).unwrap();
        config.store = store;
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        assert!(reopened.pending_resume.is_none());
        reopened.release().unwrap();
    }

    #[test]
    fn root_transient_failure_announces_automatic_continue() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store.save(&mut config.session).unwrap();
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        let _ = observations.try_iter().collect::<Vec<_>>();
        host.publish_terminal(
            Ok(TurnHostOutcome::Failed {
                transient: Some(TransientFailure {
                    failed_turn: plakous::model::TurnId::try_from(uuid::Uuid::from_u128(1))
                        .unwrap(),
                    gate: ResumeGate::Network,
                }),
                error: "connection temporarily unavailable".into(),
                exchange: crate::agent::TerminalExchange::Closed,
            }),
            Instant::now(),
            publication::TerminalErrorPresentation::Events,
        );
        assert!(observations.try_iter().any(|event| matches!(event,
            HostEvent::Row(TranscriptRow::Notice(row))
                if row.text.contains("will continue automatically")
        )));
        host.release().unwrap();
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
            let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
            assert!(host.schedule_transient_resume(
                failure(ResumeGate::Network),
                "temporary connection failure"
            ));
            let deadline = timeout.then(Instant::now);
            let mut state = OneShotState {
                attempted: true,
                resume_tree: false,
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
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
            assert!(host.pending_resume.is_none());
            assert!(
                matches!(state.result, Some(Ok(OneShotOutcome::Bounded { bound, .. })) if bound == if timeout { "timeout" } else { "max-turns 1" })
            );
            host.release().unwrap();
        }
    }

    #[test]
    fn root_transient_canonical_failure_continues_in_a_new_turn() {
        use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
        use plakous_offline_crash_harness::{
            BoundaryController, scripted_runtime_binding_with_cursor,
        };
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
        config.test_injection = Some(Arc::new(crate::runtime::TestHostInjection {
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
            rebind_builds: std::sync::Mutex::new(Vec::new()),
        }));
        let (sender, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observations) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        sender
            .send(UserAction::Prompt("continue my task".into()))
            .unwrap();
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("user prompt");
        };
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert_eq!(host.config.session.root_turn_count, 1);
        let pending = host
            .pending_resume
            .as_ref()
            .expect("classified canonical failure");
        let failed_turn = pending.failed_turn;
        pending.ready.store(true, Ordering::Release);
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = host.next_prompt(None).unwrap()
        else {
            panic!("root continuation");
        };
        host.run_prompt(text, queued_delivery, user_echoed, None)
            .unwrap();
        assert_eq!(host.config.session.root_turn_count, 2);
        assert_eq!(host.config.session.prompt_history, vec!["continue my task"]);
        let lifecycle = observations
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::TurnLifecycle(value) => Some(value),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            lifecycle
                .iter()
                .any(|value| value.turn_id == failed_turn.to_string()
                    && value.phase == plakous_host_protocol::TurnPhase::Failed)
        );
        assert!(
            lifecycle
                .iter()
                .any(|value| value.turn_id != failed_turn.to_string()
                    && value.phase == plakous_host_protocol::TurnPhase::Completed)
        );
        assert!(host.pending_resume.is_none());
        assert_eq!(host.transient_resumes_used, 1);
        host.release().unwrap();
    }
}
