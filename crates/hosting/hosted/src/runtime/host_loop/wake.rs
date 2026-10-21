//! Idle interactive continuation uses ordinary root admission and its journal.

use super::{CliFailure, CliResult, HostLoop, HostStep, Instant, SessionQueueItem};
use crate::sessions::{NoticeScope, TranscriptRow};
use std::time::Duration;

const QUIET_WINDOW: Duration = Duration::from_secs(2);
/// Consecutive transient store refusals a wake retries on a doubling delay,
/// starting at [`TRANSIENT_FIRST_DELAY`], before it reports itself paused.
const TRANSIENT_RETRIES: u32 = 5;
const TRANSIENT_FIRST_DELAY: Duration = Duration::from_secs(1);
/// A wake paused by transient refusals is retried at this interval, so the
/// pause ends on its own once the store answers again.
const TRANSIENT_REARM: Duration = Duration::from_secs(60);
pub(in crate::runtime) const WAKE_COMMAND_PREFIX: &str = "plak:child-wake:";
pub(in crate::runtime) const WAKE_PROMPT: &str = "Background processes finished or sub-agent results arrived while you were idle; continue the current task, or reply briefly if nothing remains.";

#[derive(Default)]
pub(super) struct ChildWake {
    session_id: String,
    message_ids: Vec<String>,
    quiet_since: Option<Instant>,
    suppression_pending: bool,
    pause_pending: Option<String>,
    retry: Option<TransientRetry>,
    pub(super) prepared_context: Option<crate::context::ContextProjection>,
}

/// A wake whose reads the session store refused transiently. It is neither
/// suppressed nor parked: the observed batch stays, and the next attempt
/// waits for `next_at`.
struct TransientRetry {
    session_id: String,
    failures: u32,
    next_at: Instant,
}

impl ChildWake {
    fn observe(&mut self, session_id: &str, ids: Vec<String>, now: Instant) -> bool {
        if self.session_id != session_id {
            *self = Self {
                session_id: session_id.to_owned(),
                ..Self::default()
            };
        }
        if self.message_ids != ids {
            self.message_ids = ids;
            self.quiet_since = Some(now);
        }
        !self.message_ids.is_empty()
            && self
                .quiet_since
                .is_some_and(|since| now.duration_since(since) >= QUIET_WINDOW)
    }

    fn park(&mut self) {
        self.message_ids.clear();
        self.quiet_since = None;
    }

    fn retry_waiting(&self, session_id: &str, now: Instant) -> bool {
        self.retry
            .as_ref()
            .is_some_and(|retry| retry.session_id == session_id && now < retry.next_at)
    }

    /// Records one more transient refusal and returns the consecutive count
    /// and the delay before the next attempt.
    fn defer(&mut self, session_id: &str, now: Instant) -> (u32, Duration) {
        let failures = self
            .retry
            .as_ref()
            .filter(|retry| retry.session_id == session_id)
            .map_or(0, |retry| retry.failures)
            .saturating_add(1);
        let delay = if failures <= TRANSIENT_RETRIES {
            TRANSIENT_FIRST_DELAY * 2u32.pow(failures - 1)
        } else {
            TRANSIENT_REARM
        };
        self.retry = Some(TransientRetry {
            session_id: session_id.to_owned(),
            failures,
            next_at: now + delay,
        });
        (failures, delay)
    }
}

pub(in crate::runtime) fn is_wake_delivery(
    session: &crate::sessions::SessionRecord,
    item: &SessionQueueItem,
) -> bool {
    session
        .queue_commands
        .pending_for(item)
        .is_some_and(|submission| {
            submission.kind == crate::sessions::projector::SubmissionKind::HostWake
        })
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn suppress_child_wake(&mut self, reason: Option<String>) -> CliResult<()> {
        self.config.session.child_wake_suppressed = true;
        self.child_wake.suppression_pending = true;
        self.child_wake.pause_pending = reason.clone();
        if self.config.session.store_revision == 0 {
            self.config
                .store
                .save(&mut self.config.session)
                .map_err(CliFailure::new)?;
        }
        let pause =
            reason.map(
                |reason| crate::sessions::projector::HostPauseCause::WakePreflightFailed { reason },
            );
        self.config
            .store
            .write_host_controls(&self.config.session.id, true, pause.clone())
            .map_err(CliFailure::new)?;
        self.child_wake.suppression_pending = false;
        self.child_wake.pause_pending = None;
        if pause.is_some() {
            self.config.session.queue_drain_state = super::SessionQueueDrainState::RestoredPaused;
        }
        Ok(())
    }

    pub(super) fn pause_unrecorded(&mut self, reason: String) -> CliResult<()> {
        if self.config.session.store_revision == 0 {
            self.config
                .store
                .save(&mut self.config.session)
                .map_err(CliFailure::new)?;
        }
        self.config
            .store
            .write_host_controls(
                &self.config.session.id,
                false,
                Some(crate::sessions::projector::HostPauseCause::UnrecordedRefusal { reason }),
            )
            .map_err(CliFailure::new)?;
        self.config.session.queue_drain_state = super::SessionQueueDrainState::RestoredPaused;
        Ok(())
    }

    pub(super) fn child_wake_prompt(&mut self, now: Instant) -> CliResult<HostStep> {
        if self.config.interactive
            && let Some(surface) = self.config.surface.as_deref()
            && let Some(enabled) = surface.child_wake_enabled()
        {
            self.config.auto_continue = enabled;
        }
        if self.child_wake.suppression_pending
            && self
                .suppress_child_wake(self.child_wake.pause_pending.clone())
                .is_err()
        {
            return Ok(HostStep::Continue);
        }
        if self.child_wake.retry_waiting(&self.config.session.id, now) {
            return self.wake_deferred();
        }
        let suppressed = self.config.session.child_wake_suppressed;
        let step = match self.try_child_wake_prompt(now) {
            Ok(step) => {
                self.child_wake.retry = None;
                Ok(step)
            }
            Err(error) => {
                if let Some(refusal) = error.store_refusal() {
                    self.defer_child_wake(refusal, now);
                    return self.wake_deferred();
                }
                tracing::warn!(
                    target: "plak.wake",
                    cause = "refused",
                    "sub-agent auto-continue paused until the next prompt"
                );
                self.child_wake.retry = None;
                let _ = self.events.notice(TranscriptRow::error(
                    NoticeScope::Tree,
                    format!(
                        "Sub-agent auto-continue paused until the next prompt: {}",
                        error.message()
                    ),
                ));
                let _ = self.suppress_child_wake(Some(error.message().to_owned()));
                self.child_wake.park();
                Ok(HostStep::Continue)
            }
        };
        if !suppressed && self.config.session.child_wake_suppressed {
            self.auto_continue_pause_reported();
        }
        self.publish_pending_continuation();
        step
    }

    fn wake_deferred(&mut self) -> CliResult<HostStep> {
        self.publish_pending_continuation();
        Ok(HostStep::Continue)
    }

    /// A transient store refusal neither suppresses nor parks the wake: it is
    /// retried on a doubling delay, then every [`TRANSIENT_REARM`] once the
    /// retries are spent, which is when the pause is reported.
    fn defer_child_wake(&mut self, refusal: crate::error::StoreRefusal, now: Instant) {
        let (failures, delay) = self.child_wake.defer(&self.config.session.id, now);
        tracing::warn!(
            target: "plak.wake",
            cause = refusal.as_str(),
            failures,
            retry_in_secs = delay.as_secs(),
            "the session store refused a sub-agent wake read; retrying"
        );
        if failures == TRANSIENT_RETRIES + 1 {
            let _ = self.events.notice(TranscriptRow::warning(
                NoticeScope::Tree,
                format!(
                    "Sub-agent auto-continue paused: the session store is {}; it retries every {} s",
                    refusal.as_str(),
                    TRANSIENT_REARM.as_secs()
                ),
            ));
        }
    }

    fn try_child_wake_prompt(&mut self, now: Instant) -> CliResult<HostStep> {
        let jobs = self
            .opened
            .agent()
            .map_or_else(Vec::new, plakous::agent::Agent::jobs);
        self.try_child_wake_with_jobs(now, jobs)
    }

    fn try_child_wake_with_jobs(
        &mut self,
        now: Instant,
        jobs: Vec<plakous::tools::JobView>,
    ) -> CliResult<HostStep> {
        let views = self
            .config
            .store
            .load_views(&self.config.session.id)
            .map_err(CliFailure::new)?
            .unwrap_or_else(crate::sessions::projector::SessionViews::empty);
        let intents = self
            .config
            .store
            .load_intents(&self.config.session.id)
            .map_err(CliFailure::new)?;
        self.config.session.child_wake_suppressed = views.wake_suppressed(&intents);
        if views.pause_cause(&intents).is_some() {
            self.child_wake.park();
            return Ok(HostStep::Continue);
        }
        self.config
            .session
            .pending_queue
            .retain(|item| !self.config.session.queue_commands.is_consumed(item));
        let session = &self.config.session;
        if !self.config.prompt_host_facts().host_wakes
            || self.recovery_blocked
            || session.child_wake_suppressed
            || self.pending_start.is_some()
            || !session.pending_queue.is_empty()
            || session.queue_commands.has_pending()
        {
            self.child_wake.park();
            return Ok(HostStep::Continue);
        }
        let rows = self.config.topology_rows();
        let runtime = self.last_tree_revision.runtime(&rows, session);
        self.executor
            .block_on(super::super::projection::project_tree(
                self.opened.sessions(),
                &rows,
                &super::durable_tree_id(&rows, session),
                self.opened.locator().session_id(),
            ))
            .map_err(|_| CliFailure::new("application execution failed"))??;
        let mails = if let Some(runtime) = runtime {
            runtime
                .read(|tree| {
                    tree.pending_mail(&tree.root_agent_id)
                        .into_iter()
                        .filter(|mail| {
                            matches!(
                                mail.purpose,
                                crate::agent_tree_protocol::MessagePurpose::Result
                                    | crate::agent_tree_protocol::MessagePurpose::Question
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .map_err(|error| CliFailure::new(error.message))?
        } else {
            Vec::new()
        };
        let notices: Vec<_> = jobs
            .into_iter()
            .filter(|job| job.status != plakous::tools::JobStatus::Running)
            .collect();
        let ids: Vec<_> = mails
            .iter()
            .map(|mail| mail.message_id.clone())
            .chain(notices.iter().map(|job| format!("p{}", job.number)))
            .collect();
        if !self.child_wake.observe(&session.id, ids.clone(), now) {
            return Ok(HostStep::Continue);
        }
        // Recheck ingress after observing mail: user input and shutdown outrank a wake.
        let watch = super::InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.poll();
        if self.interrupt_state.borrow().has_deferred() || watch.interrupted() {
            return Ok(HostStep::Continue);
        }
        let (command, batch) = super::super::result_mail::select(&rows, session, WAKE_PROMPT)
            .map_err(CliFailure::new)?;
        if batch.is_none() && notices.is_empty() {
            self.child_wake.park();
            return Ok(HostStep::Continue);
        }
        // Compaction requires an idle journal; budget the complete mail batch
        // before preparing its immutable delivery receipt.
        if !self.preflight_child_wake(&command)? {
            return Ok(HostStep::Continue);
        }
        let watch = super::InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.poll();
        if self.interrupt_state.borrow().has_deferred() || watch.interrupted() {
            return Ok(HostStep::Continue);
        }
        let mut candidate = self.config.session.clone();
        let item = candidate
            .enqueue_live(WAKE_PROMPT)
            .map_err(|_| CliFailure::new("child wake queue admission was refused"))?;
        // Queue identity and the exact instruction batch publish atomically;
        // an unresolved wake cannot be edited or exported as a user prompt.
        self.config.session = super::super::result_mail::prepare_instruction(
            &self.config.topology_rows(),
            self.config.store.as_ref(),
            &candidate,
            &item,
            &command,
            batch,
            &ids,
        )
        .map_err(CliFailure::from)?;
        self.child_wake.park();
        Ok(HostStep::Prompt {
            text: item.text.clone(),
            queued_delivery: Some(item),
            user_echoed: true,
        })
    }

    fn preflight_child_wake(&mut self, command: &str) -> CliResult<bool> {
        let outcome = self.fit_child_wake(command)?;
        if outcome {
            return Ok(true);
        }
        self.suppress_child_wake(Some(
            "Sub-agent auto-continue preflight did not fit".to_owned(),
        ))?;

        let _ = self.events.notice(TranscriptRow::warning(
            NoticeScope::Tree,
            "Sub-agent auto-continue preflight paused until the next prompt",
        ));
        self.child_wake.park();
        Ok(false)
    }

    pub(super) fn fit_child_wake(&mut self, command: &str) -> CliResult<bool> {
        self.refresh_native_authority_before_prompt()?;
        let command = plakous::agent::UserInput::from(command.to_owned());
        let context = super::query_context(
            &self.executor,
            &self.opened,
            super::ContextQuery::WithPendingUserInput(command.clone()),
        )?;
        if !super::prompt_requires_scheduled_compaction(context) {
            self.child_wake.prepared_context = Some(context);
            return Ok(true);
        }
        if !self.config.compaction_enabled {
            if super::scheduled_compaction_parks_when_disabled(context.disposition()) {
                return Err(CliFailure::new(super::scheduled_compaction_disabled_note(
                    context.disposition(),
                )));
            }
            self.child_wake.prepared_context = Some(context);
            return Ok(true);
        }
        self.config.session.queue_drain_state = super::SessionQueueDrainState::LiveAutoDrain;
        super::observe_compact(&self.events, 0, true);
        let outcome = super::run_scheduled_compaction_until_fits(
            &self.executor,
            super::InterruptWatch::new(self.actions, &self.interrupt_state),
            &mut self.config,
            &self.opened,
            &mut self.stderr_buf,
            &self.events,
            self.log.as_ref(),
            &command,
            None,
        );
        super::observe_compact(&self.events, 0, false);
        // A wake that does not fit would be withheld by its turn after its
        // admission acknowledged the mail it carries; it waits instead. A
        // wake has no bare prompt to fall back to: its batch is the point.
        let sends = matches!(
            outcome?,
            super::ScheduledPrompt::Send {
                over_window: false,
                ..
            }
        );
        if sends {
            self.child_wake.prepared_context = Some(super::query_context(
                &self.executor,
                &self.opened,
                super::ContextQuery::WithPendingUserInput(command),
            )?);
        }
        Ok(sends)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline host assertions")]
mod tests {
    use super::super::HostEvent;
    use super::*;
    use crate::agent_tree::{MailDurableState, ProductAgentTree};
    use crate::agent_tree_protocol::{
        MessagePurpose, SendAgentMessageRequest, SpawnAgentRequest, SubmitResultRequest, WriterMode,
    };
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::sessions::QueueDrainState;
    use crate::surface_protocol::{HostEventReceiver, UserAction, UserActionSender};
    use std::sync::Arc;

    fn submissions<P: plakous_host_protocol::ProtocolExtension>(
        host: &HostLoop<'_, P>,
    ) -> Vec<crate::sessions::projector::Submission> {
        host.config
            .store
            .load_intents(&host.config.session.id)
            .unwrap()
            .submissions
    }

    fn root_batch<P: plakous_host_protocol::ProtocolExtension>(
        host: &HostLoop<'_, P>,
        submission: &crate::sessions::projector::Submission,
    ) -> Option<crate::agent_tree::AgentCommand> {
        let rows = host.config.topology_rows();
        let id = super::super::durable_tree_id(&rows, &host.config.session);
        crate::topology_runtime::lookup(&id).and_then(|runtime| {
            runtime
                .read(|tree| tree.command(&submission.command_id).cloned())
                .unwrap()
        })
    }

    fn accepted<P: plakous_host_protocol::ProtocolExtension>(
        host: &HostLoop<'_, P>,
        submission: &crate::sessions::projector::Submission,
    ) -> bool {
        host.config
            .store
            .load_views(&host.config.session.id)
            .unwrap()
            .unwrap()
            .state
            .deliveries
            .get(&submission.command_id)
            == Some(&crate::sessions::projector::Delivery::Accepted)
    }

    #[test]
    fn notices_only_wake_without_a_tree_carries_ids_not_notice_text() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(dir.path());
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host =
            HostLoop::open(config, &actions, crate::runtime::Sender::over(events), None).unwrap();
        let job = plakous::tools::JobView {
            number: 3,
            status: plakous::tools::JobStatus::Exited(Some(0)),
            output: "private notice tail".into(),
            command: "npm run dev".into(),
            started_at: plakous::model::Timestamp::new(1_791_115_203, 0).unwrap(),
            owner: "root".into(),
            elapsed: Duration::from_secs(1),
            session_id: [1; 16],
            owner_generation: 1,
        };
        let now = Instant::now();
        host.config.auto_continue = false;
        assert!(matches!(
            host.try_child_wake_with_jobs(now, vec![job.clone()])
                .unwrap(),
            HostStep::Continue
        ));
        assert_eq!(
            submissions(&host),
            [] as [crate::sessions::projector::Submission; 0]
        );
        host.config.auto_continue = true;
        host.config.session.child_wake_suppressed = true;
        assert!(matches!(
            host.try_child_wake_with_jobs(now, vec![job.clone()])
                .unwrap(),
            HostStep::Continue
        ));
        assert_eq!(
            submissions(&host),
            [] as [crate::sessions::projector::Submission; 0]
        );
        host.config.session.child_wake_suppressed = false;
        let mut second = job.clone();
        second.number = 4;
        assert!(matches!(
            host.try_child_wake_with_jobs(now, vec![job.clone(), second.clone()])
                .unwrap(),
            HostStep::Continue
        ));
        let (text, _, _) = take_prompt(
            host.try_child_wake_with_jobs(now + QUIET_WINDOW, vec![job, second])
                .unwrap(),
        );
        assert_eq!(
            text,
            "Background processes finished or sub-agent results arrived while you were idle; continue the current task, or reply briefly if nothing remains."
        );
        let intents = host
            .config
            .store
            .load_intents(&host.config.session.id)
            .unwrap();
        let intent = intents.submissions.last().unwrap();
        assert!(intent.command_id.contains("p3"));
        assert!(intent.command_id.contains("p4"));
        assert!(!intent.canonical_command.contains("private notice tail"));
        host.release().unwrap();
    }

    #[test]
    fn idle_escape_suppresses_wake_until_user_prompt() {
        with_host(1, |host, _, actions, _| {
            actions.send(UserAction::Interrupt).unwrap();
            assert!(matches!(
                host.next_prompt(None).unwrap(),
                HostStep::Continue
            ));
            assert!(host.config.session.child_wake_suppressed);
            let now = Instant::now();
            for at in [now, now + QUIET_WINDOW] {
                assert!(matches!(
                    host.child_wake_prompt(at).unwrap(),
                    HostStep::Continue
                ));
            }
            actions
                .send(UserAction::Prompt("resume work".into()))
                .unwrap();
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            assert!(!host.config.session.child_wake_suppressed);
        });
    }

    fn completed_run(max_turns: u8) -> super::super::super::oneshot::OneShotState {
        super::super::super::oneshot::OneShotState {
            attempted: true,
            resume_tree: false,
            result: Some(Ok(crate::runtime::OneShotOutcome::Completed {
                assistant: "first answer".into(),
            })),
            run: Some(super::super::super::oneshot::RunState {
                bounds: crate::runtime::RunBounds {
                    max_turns,
                    deadline: None,
                },
                deadline: None,
                turns: 1,
                timed_out: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }),
        }
    }

    #[test]
    fn completed_final_run_is_not_bounded_and_drain_is_process_local() {
        with_host(0, |host, _, controls, _| {
            host.config.session.queue_drain_state = QueueDrainState::LiveAutoDrain;
            let mut state = completed_run(25);
            state
                .run
                .as_mut()
                .unwrap()
                .timed_out
                .store(true, std::sync::atomic::Ordering::Relaxed);
            controls.send(UserAction::Interrupt).unwrap();
            assert!(matches!(
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
            assert!(matches!(
                state.result,
                Some(Ok(crate::runtime::OneShotOutcome::Completed { .. }))
            ));
            assert_eq!(
                host.config.session.queue_drain_state,
                QueueDrainState::RestoredPaused
            );
            let stored = host.config.store.load(&host.config.session.id).unwrap();
            assert_eq!(stored.queue_drain_state, QueueDrainState::LiveAutoDrain);
        });
    }

    #[test]
    fn run_continuation_ignores_auto_continue_and_admits_its_own_terminal() {
        with_host(1, |host, _, _, observed| {
            host.config.auto_continue = false;
            host.config.interactive = false;
            let mut state = completed_run(100);
            let (text, delivery, echoed) = take_prompt(host.next_run_prompt(&mut state).unwrap());
            assert!(observed.try_iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note == "plak: continuing (turn 2: c1 result)")));
            assert!(matches!(
                host.run_prompt(text, delivery, echoed, Some(&mut state))
                    .unwrap(),
                HostStep::Continue
            ));
            assert!(matches!(
                state.result,
                Some(Ok(crate::runtime::OneShotOutcome::Completed { .. }))
            ));
            assert_eq!(host.config.session.root_turn_count, 1);
            assert!(matches!(
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
        });
    }

    #[test]
    fn run_max_turns_stops_before_admitting_a_continuation() {
        with_host(1, |host, _, _, observed| {
            let mut state = completed_run(1);
            assert!(matches!(
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
            assert!(matches!(
                state.result,
                Some(Ok(crate::runtime::OneShotOutcome::Bounded { .. }))
            ));
            assert_eq!(host.config.session.root_turn_count, 0);
            assert!(observed.try_iter().any(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note == "plak: run stopped at max-turns 1")));
        });
    }

    #[test]
    fn run_waits_without_a_turn_and_timeout_interrupts_resumable_children() {
        with_host(0, |host, runtime, _, observed| {
            runtime
                .mutate_then_emit(false, |tree| {
                    let child = tree
                        .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .map_err(|error| error.message)?;
                    tree.set_active_turn(&child.agent_id, true)
                        .map_err(|error| error.message)?;
                    Ok(())
                })
                .unwrap();
            let mut state = completed_run(25);
            state.run.as_mut().unwrap().deadline = Some(Instant::now() + Duration::from_millis(40));
            assert!(matches!(
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
            let notes = observed
                .try_iter()
                .filter_map(|event| {
                    if let HostEvent::Row(TranscriptRow::Notice(
                        plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
                    )) = event
                    {
                        Some(note)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            assert_eq!(
                notes
                    .iter()
                    .filter(|note| *note == "plak: waiting for c1; Ctrl-C to stop")
                    .count(),
                1
            );
            assert!(
                notes
                    .iter()
                    .any(|note| note == "plak: run stopped at timeout; interrupted c1")
            );
            assert_eq!(host.config.session.root_turn_count, 0);
            assert!(
                runtime
                    .read(|tree| tree.nodes()["c1"].status
                        == crate::agent_tree_protocol::NodeStatus::Interrupted)
                    .unwrap()
            );
        });
    }

    #[test]
    fn run_sigint_while_waiting_keeps_the_last_answer_and_interrupts() {
        with_host(0, |host, _, actions, _| {
            let mut state = completed_run(25);
            actions.send(UserAction::Interrupt).unwrap();
            assert!(matches!(
                host.next_run_prompt(&mut state).unwrap(),
                HostStep::Quit
            ));
            assert_eq!(
                state.result.unwrap().unwrap(),
                crate::runtime::OneShotOutcome::Interrupted {
                    assistant: "first answer".into()
                }
            );
        });
    }

    #[test]
    fn run_bounds_and_interrupt_join_a_real_child_and_allow_reopen() {
        use plakous::sessions::{
            AgentSessions, CreateSessionOutcome, CreateSessionRequest, OpenSessionOutcome,
            OpenSessionRequest, SessionBackend, SessionOperationId,
        };
        use plakous_offline_crash_harness::protocol::CrashBoundary;
        use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding};
        for stop in ["max-turns", "timeout", "sigint"] {
            with_configured_fixture(
                0,
                false,
                |config, _| {
                    let injection = config.test_injection.as_ref().unwrap();
                    config.test_injection = Some(Arc::new(crate::runtime::TestHostInjection {
                        runtime: injection.runtime.clone(),
                        rebind_target: injection.rebind_target.clone(),
                        observe: injection.observe.clone(),
                        child_runtime: Some(Arc::new(|_, path, _| {
                            let workspace = std::path::Path::new(path);
                            let controller = BoundaryController::quiet_from_evidence_path(
                                CrashBoundary::None,
                                workspace.join("run-child-evidence.jsonl"),
                            )
                            .unwrap();
                            let plan = Arc::new(serde_json::from_value(serde_json::json!({"version": 1, "steps": [{"stream": [{"text": "child working"}, {"stall": 60_000}]}]})).unwrap());
                            scripted_runtime_binding(workspace, controller, plan)
                                .map_err(|error| error.to_string())
                        })),
                        rebind_builds: std::sync::Mutex::new(Vec::new()),
                    }));
                },
                |host, runtime, controls, _| {
                    let child = runtime
                        .mutate_then_emit(false, |tree| {
                            Ok(tree
                                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                                .unwrap()
                                .agent_id)
                        })
                        .unwrap();
                    let path = runtime
                        .read(|tree| tree.node(&child).unwrap().path.clone())
                        .unwrap();
                    let factory = host
                        .config
                        .test_injection
                        .as_ref()
                        .unwrap()
                        .child_runtime
                        .as_ref()
                        .unwrap();
                    let sessions = Arc::new(
                        AgentSessions::new(
                            SessionBackend::sqlite(path.join("run-child.sqlite3")).unwrap(),
                        )
                        .unwrap(),
                    );
                    let binding = factory(
                        &child,
                        path.to_str().unwrap(),
                        &crate::agent::ProductStateRoots::with_doubles(
                            path.clone(),
                            path.join("state"),
                        ),
                    )
                    .unwrap();
                    let CreateSessionOutcome::Created { session, .. } = host
                        .executor
                        .block_on(sessions.create(CreateSessionRequest::new(
                            SessionOperationId::new("run-child:create").unwrap(),
                            binding,
                        )))
                        .unwrap()
                        .unwrap()
                    else {
                        panic!("created child required")
                    };
                    let agent = session.agent().clone();
                    let submission = host
                        .executor
                        .block_on(agent.start_instruction(
                            plakous::agent::CommandId::new("run-child:start").unwrap(),
                            "work",
                        ))
                        .unwrap()
                        .unwrap();
                    let turn_id = submission.acknowledgement().turn_id().unwrap();
                    let turn = submission.turn().unwrap();
                    let result = submission.result().unwrap().clone();
                    runtime
                        .mutate_then_emit(false, |tree| {
                            tree.bind_session_locator(&child, session.locator().to_string())
                                .unwrap();
                            tree.bind_attempt_turn(&child, turn_id.to_string()).unwrap();
                            tree.set_active_turn(&child, true).unwrap();
                            Ok(())
                        })
                        .unwrap();
                    let attached = crate::agent::AttachedCodingSession::opened_for_host_tests(
                        sessions.clone(),
                        session,
                    );
                    assert!(
                        host.executor
                            .block_on(runtime.try_occupy_live(
                                child.clone(),
                                Box::new(crate::agent::live::ProductLiveChild::new(
                                    attached,
                                    turn_id,
                                    turn.control()
                                ))
                            ))
                            .unwrap()
                            .is_ok()
                    );
                    let mut state = completed_run(if stop == "max-turns" { 1 } else { 25 });
                    if stop == "timeout" {
                        state
                            .run
                            .as_ref()
                            .unwrap()
                            .timed_out
                            .store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                    if stop == "sigint" {
                        controls.send(UserAction::Interrupt).unwrap();
                    }
                    assert!(matches!(
                        host.next_run_prompt(&mut state).unwrap(),
                        HostStep::Quit
                    ));
                    assert!(
                        host.executor
                            .block_on(async { runtime.live.lock().await.is_empty() })
                            .unwrap(),
                        "{stop} left a live child owner"
                    );
                    let terminal = host.executor.block_on(result.result()).unwrap().unwrap();
                    assert!(matches!(
                        terminal.outcome(),
                        plakous::agent::TurnResultOutcome::Cancelled(_)
                    ));
                    let locator = runtime
                        .read(|tree| {
                            assert_eq!(
                                tree.node(&child).unwrap().status,
                                crate::agent_tree_protocol::NodeStatus::Interrupted
                            );
                            tree.node(&child).unwrap().session_locator.clone().unwrap()
                        })
                        .unwrap()
                        .parse()
                        .unwrap();
                    let workspace = path.clone();
                    let resumed_binding = scripted_runtime_binding(&workspace, BoundaryController::quiet_from_evidence_path(CrashBoundary::None, workspace.join("resume-evidence.jsonl")).unwrap(), Arc::new(serde_json::from_value(serde_json::json!({"version": 1, "steps": [{"output": {"text": "resumed child"}}]})).unwrap())).unwrap();
                    let OpenSessionOutcome::Opened { session, .. } = host
                        .executor
                        .block_on(sessions.open(OpenSessionRequest::new(
                            SessionOperationId::new("run-child:resume").unwrap(),
                            locator,
                            resumed_binding,
                        )))
                        .unwrap()
                        .unwrap()
                    else {
                        panic!("resumable child required")
                    };
                    let resumed = host
                        .executor
                        .block_on(async {
                            session
                                .agent()
                                .prompt("continue")
                                .await
                                .unwrap()
                                .result()
                                .await
                                .unwrap()
                        })
                        .unwrap();
                    assert!(matches!(
                        resumed.outcome(),
                        plakous::agent::TurnResultOutcome::Completed(_)
                    ));
                    host.executor.block_on(session.release()).unwrap().unwrap();
                },
            );
        }
    }

    fn with_host(
        results: usize,
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        with_fixture(results, false, test);
    }

    fn with_fixture(
        results: usize,
        overflow: bool,
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        with_configured_fixture(results, overflow, |_, _| {}, test);
    }

    fn with_configured_fixture(
        results: usize,
        overflow: bool,
        configure: impl FnOnce(
            &mut crate::runtime::HostConfig<TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
        ),
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (mut config, _) = if overflow {
            crate::runtime::test_support::overflow_injected_attached_host(
                dir.path(),
                plakous::subscriptions::NativeProvider::Xai,
            )
        } else {
            fresh_injected_attached_host(dir.path())
        };
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &config.session.id,
            "root",
            "owner",
            config.session.workspace_root.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        for _ in 0..results {
            let child = tree
                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                .unwrap();
            tree.submit_result(
                &child.agent_id,
                &SubmitResultRequest {
                    turn_token: 1,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            tree.record_turn_completed(&child.agent_id).unwrap();
        }
        tree.settle_for_test();
        let runtime =
            crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
        configure(&mut config, &runtime);
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        observed.try_iter().for_each(drop);
        test(&mut host, &runtime, &actions, &observed);
        host.release().unwrap();
    }

    fn take_prompt(step: HostStep) -> (String, Option<SessionQueueItem>, bool) {
        let HostStep::Prompt {
            text,
            queued_delivery,
            user_echoed,
        } = step
        else {
            panic!("expected a wake prompt")
        };
        (text, queued_delivery, user_echoed)
    }

    struct CutAfterWait {
        runtime: Arc<crate::topology_runtime::ProductTopologyRuntime>,
        database: std::path::PathBuf,
        armed: std::sync::atomic::AtomicBool,
    }
    impl plakous::model::Clock for CutAfterWait {
        fn now(&self) -> plakous::model::Timestamp {
            if self
                .runtime
                .read(|tree| tree.pending_mail("root").is_empty())
                .unwrap()
                && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                // Refuse every publication after the wait side effect so
                // recovery sees the same durable head as a process cut.
                rusqlite::Connection::open(&self.database)
                    .unwrap()
                    .execute_batch(
                        "CREATE TRIGGER cut_wait_publication BEFORE INSERT ON checkpoints
                     BEGIN SELECT RAISE(FAIL, 'publication cut after wait'); END;",
                    )
                    .unwrap();
            }
            plakous::model::Timestamp::new(10, 0).unwrap()
        }
    }

    fn configure_uncommitted_wait(
        config: &mut crate::runtime::HostConfig<TestExtension>,
        runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    ) {
        use plakous_offline_crash_harness::{
            BoundaryController, scripted_topology_runtime_binding_with_clock,
        };
        let mut tools = crate::agent_tree_tools::topology_tool_registrations_on(
            runtime.driver().clone(),
            "root",
            crate::agent_tree_tools::TopologyHostPorts::default(),
        )
        .unwrap();
        tools.retain(|tool| tool.name().as_str() == "wait_agent");
        let script = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": 1,
                "steps": [{
                    "output": {"tool": {
                        "name": "wait_agent", "arguments": {"agent_ids": ["c1"], "timeout_ms": 1}
                    }},
                    "finish": "tool_calls"
                }]
            }))
            .unwrap(),
        );
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            config.cwd.join("wait-cut-evidence.jsonl"),
        )
        .unwrap();
        let clock: Arc<dyn plakous::model::Clock> = Arc::new(CutAfterWait {
            runtime: Arc::clone(runtime),
            database: config.product_home.join("runtime.sqlite3"),
            armed: std::sync::atomic::AtomicBool::new(true),
        });
        let workspace = config.cwd.clone();
        let cursor = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .runtime = Arc::new(move |_| {
            scripted_topology_runtime_binding_with_clock(
                &workspace,
                controller.clone(),
                &script,
                Arc::clone(&cursor),
                tools.clone(),
                Arc::clone(&clock),
            )
            .map_err(|error| error.to_string())
        });
    }

    fn configure_fanout_wait(
        config: &mut crate::runtime::HostConfig<TestExtension>,
        runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    ) {
        use plakous_offline_crash_harness::{
            BoundaryController, scripted_topology_runtime_binding,
        };
        let mut tools = crate::agent_tree_tools::topology_tool_registrations_on(
            runtime.driver().clone(),
            "root",
            crate::agent_tree_tools::TopologyHostPorts::default(),
        )
        .unwrap();
        tools.retain(|tool| tool.name().as_str() == "wait_agent");
        // A full fan-out (the direct child budget) of settled children whose
        // results are each a few hundred bytes.
        let mut request = SpawnAgentRequest::test_review();
        request.output_schema_json = r#"{"$schema":"https://json-schema.org/draft/2020-12/schema","type":"object","properties":{"summary":{"type":"string"}},"required":["summary"],"additionalProperties":false}"#.into();
        runtime
            .mutate_then_emit(false, |tree| {
                for _ in 0..8 {
                    let child = tree
                        .spawn_agent("root", &request, None)
                        .map_err(|error| error.message)?;
                    tree.submit_result(
                        &child.agent_id,
                        &SubmitResultRequest {
                            turn_token: 1,
                            output_json: format!(r#"{{"summary":"{}"}}"#, "done ".repeat(60)),
                        },
                        None,
                    )
                    .map_err(|error| error.message)?;
                    tree.record_turn_completed(&child.agent_id)
                        .map_err(|error| error.message)?;
                }
                Ok(())
            })
            .unwrap();
        let agent_ids: Vec<String> = runtime
            .read(|tree| {
                tree.nodes()
                    .keys()
                    .filter(|id| **id != tree.root_agent_id)
                    .cloned()
                    .collect()
            })
            .unwrap();
        let script = Arc::new(
            serde_json::from_value(serde_json::json!({
                "version": 1,
                "steps": [
                    {
                        "output": {"tool": {
                            "name": "wait_agent",
                            "arguments": {"agent_ids": agent_ids, "timeout_ms": 1_000}
                        }},
                        "finish": "tool_calls"
                    },
                    {"output": {"text": "all settled"}}
                ]
            }))
            .unwrap(),
        );
        let controller = BoundaryController::quiet_from_evidence_path(
            plakous_offline_crash_harness::protocol::CrashBoundary::None,
            config.cwd.join("fanout-wait-evidence.jsonl"),
        )
        .unwrap();
        let workspace = config.cwd.clone();
        let cursor = Arc::new(std::sync::atomic::AtomicU64::new(0));
        Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .runtime = Arc::new(move |_| {
            scripted_topology_runtime_binding(
                &workspace,
                controller.clone(),
                Arc::clone(&script),
                Arc::clone(&cursor),
                tools.clone(),
            )
            .map_err(|error| error.to_string())
        });
    }

    /// A `wait_agent` over a full fan-out of settled agents (the direct
    /// child budget, eight) returns more than the bounded observation preview
    /// holds, yet its wait facts (what woke it, the settled agent) reach the
    /// live tool line and the persisted row alike: the host reads them from
    /// the whole result its tool left, not from the preview.
    #[test]
    fn a_fanout_wait_keeps_its_wait_facts_live_and_restored() {
        with_configured_fixture(
            0,
            false,
            configure_fanout_wait,
            |host, runtime, actions, observed| {
                actions
                    .send(UserAction::Prompt("wait for every child".into()))
                    .unwrap();
                let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
                host.run_prompt(text, item, echoed, None).unwrap();
                // The committed root turn retired its await receipt.
                assert_eq!(
                    runtime
                        .read(|tree| tree.retained_wait_receipts(&tree.root_agent_id))
                        .unwrap(),
                    [] as [String; 0]
                );
                let live = observed
                    .try_iter()
                    .filter_map(|event| match event {
                        HostEvent::ToolLine {
                            name, state, wait, ..
                        } if name == "wait_agent" && state != "running" => wait,
                        _ => None,
                    })
                    .last()
                    .expect("a settled wait_agent line with wait facts");
                assert!(live.woke_by.is_some(), "{live:?}");
                assert!(live.settled_agent.is_some(), "{live:?}");
                assert!(live.timeout_ms.is_some(), "{live:?}");
                host.project_now();
                let restored = host.config.store.load(&host.config.session.id).unwrap();
                let persisted = restored
                    .transcript
                    .iter()
                    .find_map(|row| row.as_tool().filter(|tool| tool.name == "wait_agent"))
                    .and_then(|tool| tool.wait.clone())
                    .expect("the persisted wait row carries its facts");
                // The persisted wait time is the effect's committed dispatch
                // and settle stamps (spec 02 §2.6), not the live measurement.
                assert_eq!(
                    (
                        persisted.timeout_ms,
                        &persisted.woke_by,
                        &persisted.settled_agent
                    ),
                    (live.timeout_ms, &live.woke_by, &live.settled_agent)
                );
                assert!(persisted.waited_ms.is_some());
                let whole = host
                    .opened
                    .agent()
                    .unwrap()
                    .conversation()
                    .items()
                    .iter()
                    .flat_map(plakous::model::ConversationItem::blocks)
                    .find_map(|block| match block {
                        plakous::model::ContentBlock::ToolResult(result) => {
                            result.parts().iter().find_map(|part| match part {
                                plakous::model::ToolResultPart::Text(text)
                                    if text.text().contains("woke_by") =>
                                {
                                    Some(text.text().len())
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .unwrap();
                assert!(
                    whole > 2 * 1024,
                    "the wait result ({whole} bytes) must outgrow the 2 KiB preview"
                );
            },
        );
    }

    fn assert_wait_is_unread(
        host: &HostLoop<'_, TestExtension>,
        runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    ) {
        let report = host.opened.agent().unwrap().abandoned_turn().unwrap();
        assert_eq!(
            report
                .uncommitted_tool_calls()
                .iter()
                .filter(|(_, name)| name.as_str() == "wait_agent")
                .count(),
            1
        );
        let wait_id = report
            .uncommitted_tool_calls()
            .iter()
            .find(|(_, name)| name.as_str() == "wait_agent")
            .unwrap()
            .0;
        assert!(
            host.opened
                .agent()
                .unwrap()
                .conversation()
                .items()
                .iter()
                .all(|item| {
                    item.blocks().iter().all(|block| {
                        !matches!(
                            block, plakous::model::ContentBlock::ToolResult(result)
                                if result.call_id() == wait_id
                        )
                    })
                })
        );
        assert_eq!(
            runtime
                .read(|tree| tree.pending_mail("root").len())
                .unwrap(),
            1
        );
        assert!(
            runtime
                .read(|tree| tree.retained_wait_receipts("root").is_empty())
                .unwrap()
        );
        assert!(
            runtime
                .read(|tree| tree
                    .messages()
                    .values()
                    .all(|mail| mail.durable == MailDurableState::Accepted))
                .unwrap()
        );
        // Repeating settlement is harmless and does not consume the mail.
        host.executor
            .block_on(super::super::super::result_mail::settle_waits(
                &host.config.topology_rows(),
                &host.config.session,
                &host.opened,
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            runtime
                .read(|tree| tree.pending_mail("root").len())
                .unwrap(),
            1
        );
    }

    #[test]
    fn abandoned_uncommitted_wait_returns_its_acknowledged_mail_to_unread() {
        for in_place in [false, true] {
            with_configured_fixture(
                1,
                false,
                configure_uncommitted_wait,
                |host, runtime, _, _| {
                    host.executor
                        .block_on(async {
                            let turn = host
                                .opened
                                .agent()
                                .unwrap()
                                .start(
                                    plakous::agent::CommandId::new("test:uncommitted-wait")
                                        .unwrap(),
                                    "wait before the actor is interrupted",
                                )
                                .await
                                .unwrap()
                                .into_turn()
                                .unwrap();
                            let result = turn.result().await.unwrap();
                            assert!(
                                matches!(
                                    result.outcome(),
                                    plakous::agent::TurnResultOutcome::RecoveryRequired(_)
                                ),
                                "unexpected cut result: {:?}",
                                result.outcome()
                            );
                        })
                        .unwrap();
                    assert!(
                        runtime
                            .read(|tree| tree.pending_mail("root").is_empty())
                            .unwrap(),
                        "wait operations: {:?}",
                        runtime.read(|tree| tree.operations().clone()).unwrap()
                    );
                    rusqlite::Connection::open(host.config.product_home.join("runtime.sqlite3"))
                        .unwrap()
                        .execute_batch("DROP TRIGGER cut_wait_publication")
                        .unwrap();
                    if in_place {
                        host.reopen_in_place().unwrap().abandoned.unwrap();
                        assert_wait_is_unread(host, runtime);
                    } else {
                        host.config
                            .session
                            .bind_plakous_locator(host.opened.locator())
                            .unwrap();
                        host.config.store.save(&mut host.config.session).unwrap();
                        reopen_at_cut(host, |reopened| assert_wait_is_unread(reopened, runtime));
                    }
                },
            );
        }
    }

    /// The crash window of spec 03 section 4.4: a process loss after the
    /// canonical abandonment was published but before the tree's settlement
    /// was saved. The next open sees no abandonment, yet the receipt's call is
    /// absent from committed history, so its mail still returns to unread.
    #[test]
    fn a_loss_between_canonical_abandonment_and_tree_settlement_still_restores_mail() {
        with_configured_fixture(
            1,
            false,
            configure_uncommitted_wait,
            |host, runtime, _, _| {
                host.executor
                    .block_on(async {
                        let turn = host
                            .opened
                            .agent()
                            .unwrap()
                            .start(
                                plakous::agent::CommandId::new("test:lost-settlement").unwrap(),
                                "wait before the actor is interrupted",
                            )
                            .await
                            .unwrap()
                            .into_turn()
                            .unwrap();
                        turn.result().await.unwrap();
                    })
                    .unwrap();
                rusqlite::Connection::open(host.config.product_home.join("runtime.sqlite3"))
                    .unwrap()
                    .execute_batch("DROP TRIGGER cut_wait_publication")
                    .unwrap();
                let before = runtime.read(Clone::clone).unwrap();
                assert_eq!(
                    before.pending_mail("root"),
                    [] as [plakous_recursive_subagents::tree::MailRecord; 0]
                );
                assert_eq!(before.retained_wait_receipts("root").len(), 1);
                host.config
                    .session
                    .bind_plakous_locator(host.opened.locator())
                    .unwrap();
                host.config.store.save(&mut host.config.session).unwrap();
                reopen_at_cut(host, |reopened| assert_wait_is_unread(reopened, runtime));
                // The settled tree save is lost: the store goes back to the
                // tree as it was before that open, over its current revision.
                runtime
                    .mutate_then_emit(false, |tree| {
                        let revision = tree.stored_revision;
                        *tree = before.clone();
                        tree.stored_revision = revision;
                        Ok(())
                    })
                    .unwrap();
                assert!(
                    runtime
                        .read(|tree| tree.pending_mail("root").is_empty())
                        .unwrap()
                );
                reopen_at_cut(host, |reopened| {
                    assert!(reopened.opened.agent().unwrap().abandoned_turn().is_none());
                    let (pending, retained) = runtime
                        .read(|tree| {
                            (
                                tree.pending_mail("root").len(),
                                tree.retained_wait_receipts("root").len(),
                            )
                        })
                        .unwrap();
                    assert_eq!((pending, retained), (1, 0));
                });
            },
        );
    }

    /// Root admission acknowledges the mail a prompt carries before its turn
    /// runs, so a prompt its turn withholds at the context limit must not
    /// carry mail. A scheduled pass with nothing eligible (an empty
    /// conversation) sends an over-window prompt to its turn; that prompt goes
    /// without the mail, which stays unread and rides the next prompt that
    /// fits.
    #[test]
    fn a_withheld_over_window_prompt_leaves_child_mail_unread() {
        with_scripted_mail_fixture(
            1,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [{"output": {"text": "answered"}}]
            }),
            |host, runtime, _, observed| {
                let pending = || {
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap()
                };
                assert_eq!(pending(), 1);
                observed.try_iter().for_each(drop);

                // Prompts reach a turn through the queue, which is what a
                // scheduled pass measures.
                let send = |host: &mut HostLoop<'_, TestExtension>, text: String| {
                    let item = host.config.session.enqueue_live(text.clone()).unwrap();
                    host.config.store.save(&mut host.config.session).unwrap();
                    host.run_prompt(text, Some(item), false, None).unwrap();
                };
                send(host, "p".repeat(30_000));
                let events: Vec<_> = observed.try_iter().collect();
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, HostEvent::DraftRestored(_))),
                    "the over-window prompt is withheld: {events:?}"
                );
                assert_eq!(pending(), 1, "withheld prompt consumed child mail");
                assert!(
                    submissions(host)
                        .iter()
                        .all(|intent| root_batch(host, intent).is_none())
                );

                send(host, "carry on".to_owned());
                assert_eq!(pending(), 0, "the next fitting prompt carries the mail");
                assert!(
                    submissions(host)
                        .iter()
                        .any(|intent| root_batch(host, intent).is_some())
                );
            },
        );
    }

    /// The scripted fixture of
    /// `a_withheld_over_window_prompt_leaves_child_mail_unread`, with a caller
    /// script: `results` finished children, compaction enabled, and the
    /// script driving both turns and compaction.
    fn with_scripted_mail_fixture(
        results: usize,
        script: serde_json::Value,
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &UserActionSender<TestExtension>,
            &HostEventReceiver<TestExtension>,
        ),
    ) {
        use plakous_offline_crash_harness::script::ScriptPlan;
        let evidence_root = tempfile::tempdir().unwrap();
        let script: ScriptPlan = serde_json::from_value(script).unwrap();
        let script = Arc::new(script);
        with_configured_fixture(
            results,
            false,
            |config, _| {
                let controller =
                    plakous_offline_crash_harness::BoundaryController::quiet_from_evidence_path(
                        plakous_offline_crash_harness::protocol::CrashBoundary::None,
                        evidence_root.path().join("evidence.jsonl"),
                    )
                    .unwrap();
                let workspace = config.cwd.clone();
                let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
                let script = Arc::clone(&script);
                config.test_injection = Some(Arc::new(crate::runtime::TestHostInjection {
                    rebind_target: config
                        .test_injection
                        .as_ref()
                        .unwrap()
                        .rebind_target
                        .clone(),
                    runtime: Arc::new(move |role| {
                        if role == crate::runtime::TestRuntimeRole::Compact {
                            plakous_offline_crash_harness::scripted_compaction_runtime_binding(
                                &workspace,
                                controller.clone(),
                                &script,
                                None,
                                None,
                                Vec::new(),
                            )
                        } else {
                            plakous_offline_crash_harness::scripted_runtime_binding_with_cursor(
                                &workspace,
                                controller.clone(),
                                Arc::clone(&script),
                                Arc::clone(&cursor),
                            )
                        }
                        .map_err(|error| error.to_string())
                    }),
                    observe: Arc::new(|_, _| {}),
                    child_runtime: None,
                    rebind_builds: std::sync::Mutex::new(Vec::new()),
                }));
                config.compaction_enabled = true;
            },
            test,
        );
    }

    /// Queues a user prompt and runs it, as the drain does.
    fn send_queued(host: &mut HostLoop<'_, TestExtension>, text: String) {
        let item = host.config.session.enqueue_live(text.clone()).unwrap();
        host.config.store.save(&mut host.config.session).unwrap();
        host.run_prompt(text, Some(item), false, None).unwrap();
    }

    fn notice_texts(observed: &HostEventReceiver<TestExtension>) -> Vec<String> {
        observed
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow { text, .. },
                )) => Some(text),
                _ => None,
            })
            .collect()
    }

    /// A user prompt that fits, with five child findings that only together
    /// with it overflow the window, and the bare prompt's text.
    fn prompt_whose_mail_alone_overflows(
        host: &HostLoop<'_, TestExtension>,
        runtime: &Arc<crate::topology_runtime::ProductTopologyRuntime>,
    ) -> String {
        let child = runtime
            .mutate_then_emit(false, |tree| {
                tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)
            })
            .unwrap();
        for index in 0..5 {
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.send_agent_message(
                        &child.agent_id,
                        &SendAgentMessageRequest {
                            agent_id: "root".into(),
                            message: format!("{index}{}", "f".repeat(1_900)),
                            purpose: Some(MessagePurpose::Finding),
                            priority: None,
                            in_reply_to: None,
                            output_schema_json: None,
                        },
                        None,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
        }
        let project = |text: &str| {
            super::super::query_context(
                &host.executor,
                &host.opened,
                super::super::ContextQuery::WithPendingUserInput(text.to_owned().into()),
            )
            .unwrap()
        };
        let fits =
            |text: &str| project(text).disposition() == crate::context::ContextDisposition::Fits;
        // The longest bare prompt that fits.
        let (mut low, mut high) = (1usize, 40_000usize);
        assert!(fits("q") && !fits(&"q".repeat(high)));
        while high - low > 1 {
            let mid = low.midpoint(high);
            if fits(&"q".repeat(mid)) {
                low = mid;
            } else {
                high = mid;
            }
        }
        // Clear of the observation reserve, so the bare prompt needs no
        // compaction at all.
        let text = "q".repeat(low.saturating_sub(5_000));
        assert!(
            !super::super::prompt_requires_scheduled_compaction(project(&text)),
            "precondition: the bare prompt needs no compaction"
        );
        let (with_mail, batch) = super::super::super::result_mail::select(
            &host.config.topology_rows(),
            &host.config.session,
            &text,
        )
        .unwrap();
        assert_eq!(batch.map(|batch| batch.message_ids.len()), Some(5));
        assert!(!fits(&with_mail), "precondition: the mail batch overflows");
        text
    }

    /// A fitting prompt is never parked behind its own child-mail batch: the
    /// scheduled pass measures the mail-bearing command, and when no fold
    /// makes room for the batch the prompt is sent bare and the batch stays
    /// unread (review E6b-2).
    #[test]
    fn a_fitting_prompt_is_sent_without_mail_that_alone_overflows() {
        with_scripted_mail_fixture(
            0,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [{"output": {"text": "answered"}}]
            }),
            |host, runtime, _, observed| {
                let pending = || {
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap()
                };
                let text = prompt_whose_mail_alone_overflows(host, runtime);
                assert_eq!(pending(), 5);
                observed.try_iter().for_each(drop);
                send_queued(host, text);
                let notes = notice_texts(observed);
                assert!(
                    !notes
                        .iter()
                        .any(|note| note.contains("prompt remains queued")
                            || note.contains("successor still meets")
                            || note.contains("drain is paused")),
                    "a fitting prompt parked behind its own mail: {notes:?}"
                );
                assert_eq!(
                    host.config.session.queue_drain_state,
                    QueueDrainState::LiveAutoDrain
                );
                assert!(host.config.session.pending_queue.is_empty(), "it ran");
                assert!(
                    submissions(host)
                        .iter()
                        .all(|intent| root_batch(host, intent).is_none())
                );
                assert_eq!(pending(), 5, "the mail stays unread");
            },
        );
    }

    /// With compaction disabled the same prompt is sent bare too, instead of
    /// parking as an overflow.
    #[test]
    fn a_fitting_prompt_is_sent_without_overflowing_mail_when_compaction_is_disabled() {
        with_scripted_mail_fixture(
            0,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [{"output": {"text": "answered"}}]
            }),
            |host, runtime, _, observed| {
                host.config.compaction_enabled = false;
                let text = prompt_whose_mail_alone_overflows(host, runtime);
                observed.try_iter().for_each(drop);
                send_queued(host, text);
                let notes = notice_texts(observed);
                assert!(
                    !notes.iter().any(|note| note.contains("disabled")),
                    "{notes:?}"
                );
                assert_eq!(
                    host.config.session.queue_drain_state,
                    QueueDrainState::LiveAutoDrain
                );
                assert!(host.config.session.pending_queue.is_empty(), "it ran");
                assert_eq!(
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap(),
                    5
                );
            },
        );
    }

    /// The prompt and its mail fit by local measure, but the provider refuses
    /// the request as over its window, overflow recovery has nothing to fold
    /// and the turn withholds the prompt. Admission had acknowledged the
    /// batch; the model never saw it, so it returns to unread, the next
    /// prompt carries it exactly once, and nothing selects it again.
    #[test]
    fn a_withheld_prompt_returns_its_acknowledged_child_mail_once() {
        with_scripted_mail_fixture(
            1,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [
                    {"fault": {"at": "pre_stream", "kind": "context_overflow"}},
                    {"output": {"text": "answered"}}
                ]
            }),
            |host, runtime, _, observed| {
                let pending = || {
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap()
                };
                assert_eq!(pending(), 1);
                observed.try_iter().for_each(drop);
                send_queued(host, "carry on".to_owned());
                let events: Vec<_> = observed.try_iter().collect();
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, HostEvent::DraftRestored(_))),
                    "precondition: the prompt is withheld: {events:?}"
                );
                let withheld = submissions(host).last().unwrap().clone();
                assert!(root_batch(host, &withheld).is_some());
                assert_eq!(pending(), 1, "the withheld prompt's mail is unread again");
                assert!(
                    runtime
                        .read(|tree| tree.admitted_mail_returned(&withheld.command_id))
                        .unwrap()
                );
                // Reconciling the withheld admission leaves it unread.
                host.project_now();
                assert_eq!(pending(), 1);

                send_queued(host, "carry on again".to_owned());
                assert_eq!(pending(), 0, "the next prompt carries it");
                let carried = submissions(host).last().unwrap().clone().clone();
                assert_ne!(carried.command_id, withheld.command_id);
                assert_eq!(
                    root_batch(host, &carried).unwrap().message_ids,
                    root_batch(host, &withheld).unwrap().message_ids
                );
                assert!(accepted(host, &carried));
                assert!(
                    !runtime
                        .read(|tree| tree.admitted_mail_returned(&withheld.command_id))
                        .unwrap(),
                    "acknowledging the carrying admission retires the returned receipt"
                );

                send_queued(host, "and once more".to_owned());
                let last = submissions(host).last().unwrap().clone();
                assert!(root_batch(host, &last).is_none(), "never delivered twice");
                assert_eq!(pending(), 0);
            },
        );
    }

    /// A wake withheld the same way returns its batch too, and the next
    /// admission carries it.
    #[test]
    fn a_withheld_wake_returns_its_acknowledged_child_mail() {
        with_scripted_mail_fixture(
            1,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [
                    {"fault": {"at": "pre_stream", "kind": "context_overflow"}},
                    {"output": {"text": "answered"}}
                ]
            }),
            |host, runtime, _, _| {
                let pending = || {
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap()
                };
                assert_eq!(pending(), 1);
                let now = Instant::now();
                host.child_wake_prompt(now).unwrap();
                let HostStep::Prompt {
                    text,
                    queued_delivery,
                    user_echoed,
                } = host.child_wake_prompt(now + QUIET_WINDOW).unwrap()
                else {
                    panic!("the result wakes the root");
                };
                host.run_prompt(text, queued_delivery, user_echoed, None)
                    .unwrap();
                let wake = submissions(host).last().unwrap().clone();
                assert!(root_batch(host, &wake).is_some());
                assert_eq!(pending(), 1, "the withheld wake's mail is unread again");
                send_queued(host, "carry on".to_owned());
                assert_eq!(pending(), 0);
                let carried = submissions(host).last().unwrap().clone();
                assert_eq!(
                    root_batch(host, &carried).unwrap().message_ids,
                    root_batch(host, &wake).unwrap().message_ids
                );
            },
        );
    }

    /// A wake the provider refuses as over the window pauses auto-continue
    /// as a failed wake preflight does: its returned batch waits for the
    /// next operator prompt instead of waking the root again every quiet
    /// window, and the host's wake text never lands in the composer.
    #[test]
    fn a_withheld_wake_does_not_wake_again_on_its_returned_batch() {
        with_scripted_mail_fixture(
            1,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [
                    {"fault": {"at": "pre_stream", "kind": "context_overflow"}}
                ]
            }),
            |host, runtime, _, observed| {
                let pending = || {
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap()
                };
                assert_eq!(pending(), 1);
                let mut now = Instant::now();
                let mut wakes = 0;
                for _ in 0..4 {
                    host.child_wake_prompt(now).unwrap();
                    now += QUIET_WINDOW;
                    if let HostStep::Prompt {
                        text,
                        queued_delivery,
                        user_echoed,
                    } = host.child_wake_prompt(now).unwrap()
                    {
                        wakes += 1;
                        host.run_prompt(text, queued_delivery, user_echoed, None)
                            .unwrap();
                        assert_eq!(pending(), 1, "the withheld wake's mail is unread again");
                    }
                    now += QUIET_WINDOW;
                }
                assert_eq!(
                    wakes, 1,
                    "a withheld wake re-woke on its own returned batch {wakes} times"
                );
                let persisted = host.config.store.load(&host.config.session.id).unwrap();
                assert!(persisted.child_wake_suppressed);
                let pause_lines = host
                    .config
                    .session
                    .transcript
                    .rows()
                    .iter()
                    .filter(|(row, _)| {
                        matches!(
                            row,
                            TranscriptRow::Notice(notice)
                                if notice.scope == NoticeScope::Tree
                                    && notice.tone == crate::sessions::NoticeTone::Warning
                                    && notice.text
                                        == super::super::pending::AUTO_CONTINUE_PAUSE_TEXT
                        )
                    })
                    .count();
                assert_eq!(
                    pause_lines, 1,
                    "a withheld wake tells the operator auto-continue is paused"
                );
                assert!(
                    !observed
                        .try_iter()
                        .any(|event| matches!(event, HostEvent::DraftRestored(_))),
                    "the host's wake prompt is not the operator's draft"
                );
            },
        );
    }

    #[test]
    fn idle_results_coalesce_and_admit_one_harness_turn_with_exact_journal() {
        with_host(2, |host, runtime, _, observed| {
            let now = Instant::now();
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            assert!(matches!(
                host.child_wake_prompt(now + Duration::from_millis(1999))
                    .unwrap(),
                HostStep::Continue
            ));
            let (text, item, echoed) =
                take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
            assert!(is_wake_delivery(
                &host.config.session,
                item.as_ref().unwrap()
            ));
            host.run_prompt(text, item, echoed, None).unwrap();
            let published = observed.try_iter().collect::<Vec<_>>();
            // A wake leaves no transcript row: neither published nor persisted.
            assert!(
                !published
                    .iter()
                    .any(|event| matches!(event, HostEvent::Row(TranscriptRow::AutoContinue(_)))),
                "{published:?}"
            );
            assert_eq!(host.config.session.root_turn_count, 1);
            let intents = submissions(host);
            assert_eq!(intents.len(), 1);
            assert_eq!(root_batch(host, &intents[0]).unwrap().message_ids.len(), 2);
            assert!(accepted(host, &intents[0]));
            assert_eq!(host.config.session.prompt_history, [] as [String; 0]);
            assert!(
                !host
                    .config
                    .session
                    .transcript
                    .iter()
                    .any(|line| matches!(line, TranscriptRow::AutoContinue(_)))
            );
            assert!(
                !published
                    .iter()
                    .any(|event| matches!(event, HostEvent::UserMessage { .. }))
            );
            assert!(
                runtime
                    .read(|tree| tree.pending_mail("root").is_empty())
                    .unwrap()
            );
            let locator = host.opened.locator();
            let sessions = host.config.state_roots.runtime_sessions().unwrap();
            let history = host
                .executor
                .block_on(
                    sessions.history_page(plakous::sessions::SessionHistoryPageRequest::new(
                        locator,
                        locator.checkpoint_id(),
                        plakous::sessions::SessionHistoryVisibility::Content,
                        plakous::sessions::SESSION_HISTORY_VISIBILITY_REVISION,
                        plakous::sessions::SessionHistoryDirection::Forward,
                        128,
                        64 * 1024,
                        None,
                    )),
                )
                .unwrap()
                .unwrap();
            let plakous::sessions::SessionHistoryPageOutcome::Page { items, .. } = history else {
                panic!("expected exact canonical history")
            };
            assert!(
                items
                    .iter()
                    .any(plakous::sessions::SessionHistoryItem::is_harness_authored)
            );
            // Reloading the committed record and reconciling cannot re-select this batch.
            host.config.session = host.config.store.load(&host.config.session.id).unwrap();
            host.child_wake = ChildWake::default();
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW * 2).unwrap(),
                HostStep::Continue
            ));
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW * 3).unwrap(),
                HostStep::Continue
            ));
            assert!(
                runtime
                    .read(|tree| tree
                        .messages()
                        .values()
                        .all(|mail| mail.durable == MailDurableState::Acknowledged))
                    .unwrap()
            );
        });
    }

    /// A withheld wake whose acceptance the product never recorded reopens
    /// through stale-head duplicate reconciliation. The duplicate's withheld
    /// terminal returns the batch to unread there too, and auto-continue
    /// pauses until the next prompt.
    #[test]
    fn a_withheld_wake_reconciled_as_a_duplicate_on_open_returns_its_mail() {
        with_scripted_mail_fixture(
            1,
            serde_json::json!({
                "version": plakous_offline_crash_harness::script::SCRIPT_VERSION,
                "maximum_projected_input": 20_000,
                "content_sensitive_projection": true,
                "repeat_last_step": true,
                "steps": [
                    {"fault": {"at": "pre_stream", "kind": "context_overflow"}}
                ]
            }),
            |host, runtime, _, _| {
                let now = Instant::now();
                host.child_wake_prompt(now).unwrap();
                let _ = take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
                let intent = submissions(host)[0].clone();
                host.executor
                    .block_on(async {
                        let submission = host
                            .opened
                            .agent()
                            .unwrap()
                            .start_instruction(
                                plakous::agent::CommandId::new(&intent.command_id).unwrap(),
                                intent.canonical_command.clone(),
                            )
                            .await
                            .unwrap();
                        submission.into_turn().unwrap().result().await.unwrap();
                    })
                    .unwrap();

                host.config.store.save(&mut host.config.session).unwrap();
                reopen_at_cut(host, |reopened| {
                    assert!(accepted(reopened, &submissions(reopened)[0]));
                    assert_eq!(
                        runtime
                            .read(|tree| tree.pending_mail("root").len())
                            .unwrap(),
                        1,
                        "the withheld duplicate's mail is unread again"
                    );
                    let persisted = reopened
                        .config
                        .store
                        .load(&reopened.config.session.id)
                        .unwrap();
                    assert!(persisted.child_wake_suppressed);
                });
            },
        );
    }

    fn reopen_at_cut(
        host: &mut HostLoop<'_, TestExtension>,
        test: impl FnOnce(&mut HostLoop<'_, TestExtension>),
    ) {
        host.executor
            .block_on(host.opened.detach())
            .unwrap()
            .unwrap();
        let root = host.config.product_home.parent().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root);
        config.state_roots = host.config.state_roots.clone();
        config.store = host.config.store.clone();
        config.session = config.store.load(&host.config.session.id).unwrap();
        config.test_injection = host.config.test_injection.clone();
        let mut reopened = HostLoop::open(config, host.actions, host.events.clone(), None).unwrap();
        test(&mut reopened);
        // Keep the fixture's old host at the same released head and CAS revision.
        reopened
            .executor
            .block_on(reopened.opened.detach())
            .unwrap()
            .unwrap();
        host.opened = reopened.opened;
        host.config.session = reopened.config.session;
    }

    #[test]
    fn accepted_wake_reopens_with_duplicate_instruction_and_acknowledges_once() {
        with_host(1, |host, runtime, _, _| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let _ = take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
            let intent = submissions(host)[0].clone();
            host.executor
                .block_on(async {
                    let submission = host
                        .opened
                        .agent()
                        .unwrap()
                        .start_instruction(
                            plakous::agent::CommandId::new(&intent.command_id).unwrap(),
                            intent.canonical_command.clone(),
                        )
                        .await
                        .unwrap();
                    assert_eq!(
                        submission.acknowledgement().disposition(),
                        plakous::agent::CommandAckDisposition::Accepted
                    );
                    submission.into_turn().unwrap().result().await.unwrap();
                })
                .unwrap();

            host.config.store.save(&mut host.config.session).unwrap();
            assert_eq!(
                runtime
                    .read(|tree| tree.pending_mail("root").len())
                    .unwrap(),
                1
            );
            reopen_at_cut(host, |reopened| {
                assert_eq!(submissions(reopened).len(), 1);
                assert!(accepted(reopened, &submissions(reopened)[0]));
                assert_eq!(
                    reopened.config.session.pending_queue,
                    [] as [crate::sessions::queue::QueueItem; 0]
                );
                assert!(
                    runtime
                        .read(|tree| tree.pending_mail("root").is_empty())
                        .unwrap()
                );
                let revision = runtime.read(|tree| tree.revision).unwrap();
                reopened.project_now();
                assert_eq!(runtime.read(|tree| tree.revision).unwrap(), revision);
            });
        });
    }

    #[test]
    fn prepared_wake_reopen_discards_the_delivery_and_user_prompt_carries_unread_mail() {
        with_host(1, |host, runtime, actions, _| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let _ = take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
            let command_id = submissions(host)[0].command_id.clone();
            reopen_at_cut(host, |reopened| {
                assert!(
                    runtime
                        .read(|tree| tree.command(&command_id).unwrap().withdrawn)
                        .unwrap()
                );
                assert!(
                    reopened
                        .config
                        .store
                        .load_intents(&reopened.config.session.id)
                        .unwrap()
                        .submission(&command_id)
                        .is_none()
                );
                assert_eq!(
                    submissions(reopened),
                    [] as [crate::sessions::projector::Submission; 0]
                );
                assert_eq!(
                    reopened.config.session.pending_queue,
                    [] as [crate::sessions::queue::QueueItem; 0]
                );
                assert_eq!(
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap(),
                    1
                );
                actions
                    .send(UserAction::Prompt("human next".into()))
                    .unwrap();
                let (text, item, echoed) = take_prompt(reopened.next_prompt(None).unwrap());
                assert_eq!(text, "human next");
                reopened.run_prompt(text, item, echoed, None).unwrap();
                assert_eq!(reopened.config.session.root_turn_count, 1);
                assert_eq!(submissions(reopened).len(), 1);
                assert_eq!(
                    root_batch(reopened, &submissions(reopened)[0])
                        .unwrap()
                        .message_ids
                        .len(),
                    1
                );
                assert!(
                    runtime
                        .read(|tree| tree.pending_mail("root").is_empty())
                        .unwrap()
                );
            });
        });
    }

    #[test]
    fn pre_admission_wake_failure_is_disposable_without_pausing_user_work() {
        with_host(1, |host, runtime, actions, observed| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let (text, item, echoed) =
                take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
            host.child_wake.prepared_context = None;
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );
            assert_eq!(
                host.config.session.pending_queue,
                [] as [crate::sessions::queue::QueueItem; 0]
            );
            assert!(!host.recovery_blocked);
            assert_eq!(
                host.config.session.queue_drain_state,
                QueueDrainState::LiveAutoDrain
            );
            assert_eq!(
                runtime
                    .read(|tree| tree.pending_mail("root").len())
                    .unwrap(),
                1
            );
            assert!(observed.try_iter().any(|event| matches!(
                event,
                HostEvent::Row(TranscriptRow::Notice(
                    plakous_host_protocol::views::transcript::NoticeRow {
                        tone: plakous_host_protocol::views::transcript::NoticeTone::Error,
                        ..
                    }
                ))
            )));
            actions
                .send(UserAction::Prompt("human after failure".into()))
                .unwrap();
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(host.config.session.root_turn_count, 1);
            assert!(
                runtime
                    .read(|tree| tree.pending_mail("root").is_empty())
                    .unwrap()
            );
        });
    }

    #[test]
    fn wake_store_failure_emits_error_and_stays_attached_even_when_suppression_save_fails() {
        with_host(1, |host, runtime, actions, observed| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let store = host.config.store.clone();
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::refusing(
                "injected store busy",
            ));
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            assert!(host.config.session.child_wake_suppressed);
            assert_eq!(host.child_wake.message_ids, [] as [String; 0]);
            assert_eq!(
                host.config.session.pending_queue,
                [] as [crate::sessions::queue::QueueItem; 0]
            );
            assert_eq!(
                store
                    .load_intents(&host.config.session.id)
                    .unwrap()
                    .submissions,
                [] as [crate::sessions::projector::Submission; 0]
            );
            assert_eq!(
                runtime
                    .read(|tree| tree.pending_mail("root").len())
                    .unwrap(),
                1
            );
            assert_eq!(observed.try_iter().filter(|event| matches!(event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("injected store busy"))).count(), 1);
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW * 2).unwrap(),
                HostStep::Continue
            ));
            assert!(observed.try_iter().next().is_none());
            host.config.store = store;
            host.child_wake_prompt(now + QUIET_WINDOW * 3).unwrap();
            assert!(
                host.config
                    .store
                    .load(&host.config.session.id)
                    .unwrap()
                    .child_wake_suppressed
            );
            actions.send(UserAction::Quit).unwrap();
            assert!(matches!(host.next_prompt(None).unwrap(), HostStep::Quit));
        });
    }

    #[test]
    fn transient_wake_refusal_discards_queue_and_intent_without_consuming_mail() {
        use plakous::agent::{CommandAckDisposition, ControlErrorKind};

        for kind in [ControlErrorKind::Backpressure, ControlErrorKind::QueueFull] {
            with_host(1, |host, runtime, actions, _| {
                let now = Instant::now();
                host.child_wake_prompt(now).unwrap();
                let (text, item, echoed) =
                    take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
                crate::runtime::turn_drive::stub_start_refusal(
                    format!(
                        "plak:deliver:{}:{}",
                        submissions(host)[0].queue_item_id,
                        submissions(host)[0].queue_item_revision
                    ),
                    CommandAckDisposition::Rejected,
                    kind,
                );
                host.run_prompt(text, item, echoed, None).unwrap();

                assert!(host.config.session.pending_queue.is_empty(), "{kind:?}");
                assert_eq!(
                    submissions(host),
                    [] as [crate::sessions::projector::Submission; 0]
                );
                assert!(host.config.session.child_wake_suppressed);
                assert!(!host.recovery_blocked);
                assert_eq!(
                    host.config.session.queue_drain_state,
                    QueueDrainState::LiveAutoDrain
                );
                let saved = host.config.store.load(&host.config.session.id).unwrap();
                assert_eq!(
                    saved.pending_queue,
                    [] as [crate::sessions::queue::QueueItem; 0]
                );
                assert_eq!(
                    host.config
                        .store
                        .load_intents(&saved.id)
                        .unwrap()
                        .submissions,
                    [] as [crate::sessions::projector::Submission; 0]
                );
                assert_eq!(
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap(),
                    1
                );

                actions
                    .send(UserAction::Prompt("human after refusal".into()))
                    .unwrap();
                let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
                host.run_prompt(text, item, echoed, None).unwrap();
                assert_eq!(host.config.session.root_turn_count, 1);
                assert!(
                    runtime
                        .read(|tree| tree.pending_mail("root").is_empty())
                        .unwrap()
                );
            });
        }
    }

    /// Captures `plak.wake` WARN events, the lines `plak.log` would carry.
    #[derive(Clone, Default)]
    struct WakeWarnings(Arc<std::sync::Mutex<Vec<String>>>);

    impl WakeWarnings {
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    impl tracing::Subscriber for WakeWarnings {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write as _;
                    let _ = write!(self.0, "{}={value:?} ", field.name());
                }
            }
            let metadata = event.metadata();
            if *metadata.level() == tracing::Level::WARN
                && matches!(metadata.target(), "plak.wake" | "plak.context")
            {
                let mut fields = Fields(String::new());
                event.record(&mut fields);
                self.0.lock().unwrap().push(fields.0);
            }
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    fn with_wake_warnings(
        results: usize,
        test: impl FnOnce(
            &mut HostLoop<'_, TestExtension>,
            &Arc<crate::topology_runtime::ProductTopologyRuntime>,
            &HostEventReceiver<TestExtension>,
            &WakeWarnings,
        ),
    ) {
        let warnings = WakeWarnings::default();
        let captured = warnings.clone();
        tracing::subscriber::with_default(warnings.clone(), || {
            with_host(results, |host, runtime, _, observed| {
                test(host, runtime, observed, &captured);
            });
        });
    }

    #[test]
    fn busy_context_meter_refresh_is_quiet_and_unavailable() {
        use crate::error::{StoreRefusal, read_faults};
        with_wake_warnings(1, |host, _, observed, warnings| {
            let _ = observed.try_iter().count();
            let _ = warnings.take();
            read_faults::inject(
                super::super::super::usage::CONTEXT_QUERY_FAILED,
                Some(StoreRefusal::Busy),
                1,
            );
            let result = super::super::report_context(
                &host.executor,
                &host.opened,
                &host.events,
                "after turn publication",
                false,
                host.config.session.context_counters(),
            )
            .unwrap();
            assert_eq!(
                result.disposition(),
                crate::context::ContextDisposition::Unavailable
            );
            let events: Vec<_> = observed.try_iter().collect();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, HostEvent::Usage(_)))
            );
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, HostEvent::Row(_))),
                "{events:?}"
            );
            let logged = warnings.take();
            assert_eq!(logged.len(), 1, "{logged:?}");
            assert!(
                logged[0].contains("refusal=\"busy\"")
                    && logged[0].contains("stage=\"after turn publication\""),
                "{logged:?}"
            );
        });
    }

    #[test]
    fn nontransient_context_meter_failure_keeps_a_stage_specific_error() {
        use crate::error::read_faults;
        with_host(1, |host, _, _, _| {
            read_faults::inject(super::super::super::usage::CONTEXT_QUERY_FAILED, None, 1);
            let error = super::super::report_context(
                &host.executor,
                &host.opened,
                &host.events,
                "after recovery",
                false,
                host.config.session.context_counters(),
            )
            .unwrap_err();
            assert!(error.message().contains("context meter after recovery:"));
        });
    }

    #[test]
    fn a_transient_tree_feed_refusal_retries_and_wakes_exactly_once() {
        use crate::error::{StoreRefusal, read_faults};

        for refusal in [StoreRefusal::Busy, StoreRefusal::Unavailable] {
            with_wake_warnings(1, |host, runtime, observed, warnings| {
                let now = Instant::now();
                host.child_wake_prompt(now).unwrap();
                read_faults::inject(crate::agent::TREE_FEED_READ_FAILED, Some(refusal), 1);
                let ready = now + QUIET_WINDOW;
                assert!(matches!(
                    host.child_wake_prompt(ready).unwrap(),
                    HostStep::Continue
                ));
                assert!(!host.config.session.child_wake_suppressed);
                assert_eq!(notice_texts(observed), [] as [String; 0], "{refusal:?}");
                let logged = warnings.take();
                assert_eq!(logged.len(), 1, "{logged:?}");
                assert!(
                    logged[0].contains(&format!("cause=\"{}\"", refusal.as_str())),
                    "{logged:?}"
                );
                assert_eq!(
                    runtime
                        .read(|tree| tree.pending_mail("root").len())
                        .unwrap(),
                    1
                );

                // The backoff holds the next attempt; a retry then wakes once.
                assert!(matches!(
                    host.child_wake_prompt(ready + TRANSIENT_FIRST_DELAY / 2)
                        .unwrap(),
                    HostStep::Continue
                ));
                let (text, item, echoed) = take_prompt(
                    host.child_wake_prompt(ready + TRANSIENT_FIRST_DELAY)
                        .unwrap(),
                );
                assert_eq!(submissions(host).len(), 1);
                host.run_prompt(text, item, echoed, None).unwrap();
                assert!(accepted(host, &submissions(host)[0]));
                assert!(
                    runtime
                        .read(|tree| tree.pending_mail("root").is_empty())
                        .unwrap()
                );
                let later = ready + TRANSIENT_REARM;
                for at in [later, later + QUIET_WINDOW] {
                    assert!(matches!(
                        host.child_wake_prompt(at).unwrap(),
                        HostStep::Continue
                    ));
                }
                assert_eq!(submissions(host).len(), 1);
                assert_eq!(warnings.take(), [] as [String; 0]);
            });
        }
    }

    /// Refuses as many submissions as it counts, as a product store another
    /// writer holds would.
    struct BusySubmissions(std::sync::atomic::AtomicUsize);

    impl crate::runtime::fault_store::StoreFaults for BusySubmissions {
        fn before(
            &self,
            _: crate::runtime::fault_store::StoreCall,
            _: Option<&crate::sessions::SessionRecord>,
        ) -> Result<(), String> {
            Ok(())
        }

        fn refusal(
            &self,
            call: crate::runtime::fault_store::StoreCall,
        ) -> Option<crate::error::StoreRefusal> {
            use std::sync::atomic::Ordering;
            let refused = call == crate::runtime::fault_store::StoreCall::SaveSubmission
                && self
                    .0
                    .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                        left.checked_sub(1)
                    })
                    .is_ok();
            refused.then_some(crate::error::StoreRefusal::Busy)
        }
    }

    #[test]
    fn a_busy_product_store_submission_retries_and_wakes_exactly_once() {
        with_wake_warnings(1, |host, runtime, observed, warnings| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                host.config.store.clone(),
                BusySubmissions(std::sync::atomic::AtomicUsize::new(1)),
            ));
            let ready = now + QUIET_WINDOW;
            assert!(matches!(
                host.child_wake_prompt(ready).unwrap(),
                HostStep::Continue
            ));
            assert!(!host.config.session.child_wake_suppressed);
            assert_eq!(notice_texts(observed), [] as [String; 0]);
            let logged = warnings.take();
            assert_eq!(logged.len(), 1, "{logged:?}");
            assert!(logged[0].contains("cause=\"busy\""), "{logged:?}");
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );
            // The refused submission's batch is withdrawn with its mail unread.
            let (unresolved, unread) = runtime
                .read(|tree| {
                    (
                        tree.commands()
                            .values()
                            .filter(|command| command.unresolved())
                            .count(),
                        tree.pending_mail("root").len(),
                    )
                })
                .unwrap();
            assert_eq!((unresolved, unread), (0, 1));

            let (text, item, echoed) = take_prompt(
                host.child_wake_prompt(ready + TRANSIENT_FIRST_DELAY)
                    .unwrap(),
            );
            assert_eq!(submissions(host).len(), 1);
            host.run_prompt(text, item, echoed, None).unwrap();
            assert!(accepted(host, &submissions(host)[0]));
            assert!(
                runtime
                    .read(|tree| tree.pending_mail("root").is_empty())
                    .unwrap()
            );
            let later = ready + TRANSIENT_REARM;
            for at in [later, later + QUIET_WINDOW] {
                assert!(matches!(
                    host.child_wake_prompt(at).unwrap(),
                    HostStep::Continue
                ));
            }
            assert_eq!(submissions(host).len(), 1);
            assert_eq!(warnings.take(), [] as [String; 0]);
        });
    }

    #[test]
    fn spent_transient_retries_pause_with_a_rearm_that_later_wakes() {
        use crate::error::{StoreRefusal, read_faults};

        with_wake_warnings(1, |host, runtime, observed, warnings| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let site = super::super::super::usage::CONTEXT_QUERY_FAILED;
            let attempts = TRANSIENT_RETRIES + 2;
            read_faults::inject(
                site,
                Some(StoreRefusal::Unavailable),
                usize::try_from(attempts).unwrap(),
            );
            let mut at = now + QUIET_WINDOW;
            for attempt in 1..=attempts {
                assert!(matches!(
                    host.child_wake_prompt(at).unwrap(),
                    HostStep::Continue
                ));
                let notices = notice_texts(observed);
                if attempt == TRANSIENT_RETRIES + 1 {
                    assert_eq!(notices.len(), 1, "{notices:?}");
                    assert!(
                        notices[0].contains("paused")
                            && notices[0].contains("unavailable")
                            && !notices[0].contains("until the next prompt"),
                        "{notices:?}"
                    );
                } else {
                    assert_eq!(notices, [] as [String; 0], "attempt {attempt}");
                }
                assert_eq!(warnings.take().len(), 1, "attempt {attempt}");
                let delay = if attempt <= TRANSIENT_RETRIES {
                    TRANSIENT_FIRST_DELAY * 2u32.pow(attempt - 1)
                } else {
                    TRANSIENT_REARM
                };
                // Nothing retries before the delay is over.
                assert!(matches!(
                    host.child_wake_prompt(at + delay.saturating_sub(Duration::from_millis(1)))
                        .unwrap(),
                    HostStep::Continue
                ));
                at += delay;
            }
            assert!(!host.config.session.child_wake_suppressed);
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );

            // The re-arm finds the store answering again and wakes once.
            let (text, item, echoed) = take_prompt(host.child_wake_prompt(at).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(submissions(host).len(), 1);
            assert!(
                runtime
                    .read(|tree| tree.pending_mail("root").is_empty())
                    .unwrap()
            );
        });
    }

    #[test]
    fn a_permanent_read_refusal_still_pauses_until_the_next_prompt() {
        use crate::error::read_faults;

        with_wake_warnings(1, |host, runtime, observed, warnings| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            read_faults::inject(crate::agent::TREE_FEED_READ_FAILED, None, 1);
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            assert!(host.config.session.child_wake_suppressed);
            let notices = notice_texts(observed);
            assert!(
                notices
                    .iter()
                    .any(|note| note.contains("paused until the next prompt")
                        && note.contains("integrity")),
                "{notices:?}"
            );
            let logged = warnings.take();
            assert_eq!(logged.len(), 1, "{logged:?}");
            assert!(logged[0].contains("cause=\"refused\""), "{logged:?}");
            for at in [now + TRANSIENT_REARM, now + TRANSIENT_REARM * 2] {
                assert!(matches!(
                    host.child_wake_prompt(at).unwrap(),
                    HostStep::Continue
                ));
            }
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );
            assert_eq!(
                runtime
                    .read(|tree| tree.pending_mail("root").len())
                    .unwrap(),
                1
            );
        });
    }

    #[test]
    fn staggered_results_reset_the_quiet_window_and_coalesce() {
        let now = Instant::now();
        let mut wake = ChildWake::default();
        assert!(!wake.observe("session", vec!["m1".into()], now));
        let later = now + Duration::from_secs(1);
        let batch = vec!["m1".into(), "m2".into()];
        assert!(!wake.observe("session", batch.clone(), later));
        assert!(!wake.observe("session", batch.clone(), now + QUIET_WINDOW));
        assert!(wake.observe("session", batch, later + QUIET_WINDOW));
    }

    #[test]
    fn wake_preflight_compaction_error_parks_instead_of_deferring() {
        with_fixture(1, true, |host, runtime, _, observed| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );
            assert_eq!(
                host.config.session.pending_queue,
                [] as [crate::sessions::queue::QueueItem; 0]
            );
            assert!(host.config.session.child_wake_suppressed);
            assert!(host.child_wake.retry.is_none(), "compaction errors must park, not defer");
            let events: Vec<_> = observed.try_iter().collect();
            assert!(
                events.iter().any(|event| matches!(
                    event, HostEvent::ToolLine { name, state, .. } if name == "compact" && state == "running"
                )),
                "events: {events:?}"
            );
            assert!(!events.iter().any(|event| matches!(
                event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.contains("no unresolved product work")
            )));
            assert!(events.iter().any(|event| matches!(
                event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note.starts_with("Sub-agent auto-continue")
            )));
            assert!(!events.iter().any(|event| matches!(
                event, HostEvent::Row(TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. }))
                    if note == super::super::pending::AUTO_CONTINUE_PAUSE_TEXT
            )), "a reported wake pause wrote a second line: {events:?}");
            assert_eq!(
                runtime
                    .read(|tree| tree.pending_mail("root").len())
                    .unwrap(),
                1
            );
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW * 4).unwrap(),
                HostStep::Continue
            ));
            assert!(!observed.try_iter().any(|event| matches!(
                event, HostEvent::ToolLine { name, state, .. } if name == "compact" && state == "running"
            )));
            let persisted = host.config.store.load(&host.config.session.id).unwrap();
            assert!(persisted.child_wake_suppressed);
        });
    }

    #[test]
    fn reopen_with_unread_result_admits_once_and_keeps_the_delivery_journal() {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(dir.path());
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &config.session.id,
            "root",
            "owner",
            config.session.workspace_root.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let runtime =
            crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events.clone()),
            None,
        )
        .unwrap();
        let store = host.config.store.clone();
        let roots = host.config.state_roots.clone();
        let id = host.config.session.id.clone();
        host.config.session.touch();
        store.save(&mut host.config.session).unwrap();
        actions.send(UserAction::Quit).unwrap();
        assert!(matches!(host.next_prompt(None).unwrap(), HostStep::Quit));
        assert_eq!(
            host.config.session.pending_queue,
            [] as [crate::sessions::queue::QueueItem; 0]
        );
        assert_eq!(
            host.config.session.queue_drain_state,
            QueueDrainState::RestoredPaused
        );
        host.release().unwrap();
        observed.try_iter().for_each(drop);

        let (mut config, _) = fresh_injected_attached_host(dir.path());
        config.store = store.clone();
        config.state_roots = roots.clone();
        config.session = store.load(&id).unwrap();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events.clone()),
            None,
        )
        .unwrap();
        let now = Instant::now();
        assert!(matches!(
            host.child_wake_prompt(now).unwrap(),
            HostStep::Continue
        ));
        let (text, item, echoed) = take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
        host.run_prompt(text, item, echoed, None).unwrap();
        assert_eq!(host.config.session.root_turn_count, 1);
        let journal = submissions(&host)[0].clone();
        assert!(accepted(&host, &journal));
        host.release().unwrap();

        let (mut config, _) = fresh_injected_attached_host(dir.path());
        config.store = store.clone();
        config.state_roots = roots;
        config.session = store.load(&id).unwrap();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        assert!(matches!(
            host.child_wake_prompt(now + QUIET_WINDOW * 2).unwrap(),
            HostStep::Continue
        ));
        assert!(matches!(
            host.child_wake_prompt(now + QUIET_WINDOW * 3).unwrap(),
            HostStep::Continue
        ));
        assert_eq!(host.config.session.root_turn_count, 1);
        // Reopen preserves the exact immutable submission, including its bytes.
        assert_eq!(submissions(&host)[0], journal);
        assert!(
            runtime
                .read(|tree| tree.pending_mail("root").is_empty())
                .unwrap()
        );
        host.release().unwrap();
    }

    #[test]
    fn finding_does_not_wake_but_question_does_and_resets_quiet_window() {
        with_host(0, |host, runtime, _, _| {
            let child = runtime
                .mutate_then_emit(false, |tree| {
                    tree.spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                        .map_err(|error| error.message)
                })
                .unwrap();
            let mail = |purpose, body: &str| SendAgentMessageRequest {
                agent_id: "root".into(),
                message: body.into(),
                purpose: Some(purpose),
                priority: None,
                in_reply_to: None,
                output_schema_json: None,
            };
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.send_agent_message(
                        &child.agent_id,
                        &mail(MessagePurpose::Finding, "finding"),
                        None,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
            let now = Instant::now();
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            runtime
                .mutate_then_emit(false, |tree| {
                    tree.send_agent_message(
                        &child.agent_id,
                        &mail(MessagePurpose::Question, "question"),
                        None,
                    )
                    .map_err(|error| error.message)
                })
                .unwrap();
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            let (text, item, echoed) =
                take_prompt(host.child_wake_prompt(now + QUIET_WINDOW * 2).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(
                root_batch(host, &submissions(host)[0])
                    .unwrap()
                    .message_ids
                    .len(),
                2
            );
        });
    }

    #[test]
    fn a_noninteractive_face_never_reenables_wakes_from_home_configuration() {
        struct EnabledSurface;
        impl crate::runtime::SurfaceHandler<TestExtension> for EnabledSurface {
            fn child_wake_enabled(&self) -> Option<bool> {
                Some(true)
            }

            fn dispatch(
                &self,
                _: crate::runtime::test_kit::ProbeIntent,
                _: crate::runtime::SurfaceFacts<'_>,
                _: &crate::runtime::Sender<TestExtension>,
            ) -> crate::runtime::SurfaceEffect {
                crate::runtime::SurfaceEffect::default()
            }

            fn publish_template_readiness(
                &self,
                _: &[plakous_host_protocol::views::ProviderSection],
                _: &crate::runtime::Sender<TestExtension>,
            ) {
            }

            fn refresh_readiness(
                &self,
            ) -> Result<
                (
                    Vec<plakous_host_protocol::views::ProviderDef>,
                    Vec<plakous_host_protocol::views::ProviderSection>,
                ),
                String,
            > {
                Ok((Vec::new(), Vec::new()))
            }
        }
        with_host(1, |host, _, _, _| {
            host.config.interactive = false;
            host.config.auto_continue = false;
            host.config.surface = Some(Box::new(EnabledSurface));
            let now = Instant::now();
            assert!(!host.config.prompt_host_facts().host_wakes);
            for at in [now, now + QUIET_WINDOW] {
                assert!(matches!(
                    host.child_wake_prompt(at).unwrap(),
                    HostStep::Continue
                ));
            }
            assert!(!host.config.auto_continue);
            assert_eq!(
                submissions(host),
                [] as [crate::sessions::projector::Submission; 0]
            );
            host.config.interactive = true;
            host.child_wake_prompt(now).unwrap();
            assert!(host.config.auto_continue);
            assert!(host.config.prompt_host_facts().host_wakes);
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Prompt { .. }
            ));
        });
    }

    #[test]
    fn disabled_interrupt_recovery_paused_queue_and_user_ingress_prevent_wake() {
        with_host(1, |host, _, actions, _| {
            let now = Instant::now();
            host.config.auto_continue = false;
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            host.config.auto_continue = true;
            let store = host.config.store.clone();
            host.suppress_child_wake(None).unwrap();
            host.config.session = store.load(&host.config.session.id).unwrap();
            assert!(host.config.session.child_wake_suppressed);
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            actions
                .send(UserAction::Prompt("reset suppression".into()))
                .unwrap();
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            host.run_prompt(text, item, echoed, None).unwrap();
            host.recovery_blocked = true;
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            host.recovery_blocked = false;
            host.config.session.queue_drain_state = QueueDrainState::RestoredPaused;
            host.config
                .session
                .pending_queue
                .push(SessionQueueItem::new("restored prompt"));
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            host.config.session.pending_queue.clear();
            host.config.session.queue_drain_state = QueueDrainState::LiveAutoDrain;
            assert!(matches!(
                host.child_wake_prompt(now).unwrap(),
                HostStep::Continue
            ));
            actions
                .send(UserAction::Prompt("human prompt".into()))
                .unwrap();
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            assert_eq!(text, "human prompt");
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(host.config.session.root_turn_count, 2);
            assert!(
                !submissions(host)
                    .last()
                    .unwrap()
                    .command_id
                    .starts_with(WAKE_COMMAND_PREFIX)
            );
            assert_eq!(
                root_batch(host, &submissions(host)[0])
                    .unwrap()
                    .message_ids
                    .len(),
                1
            );
            assert!(root_batch(host, submissions(host).last().unwrap()).is_none());
        });
    }

    #[derive(Default)]
    struct WakeJobFixture(std::sync::Mutex<Option<plakous::tools::JoinedProcessSessions>>);

    impl plakous::tools::ToolExecutor for WakeJobFixture {
        fn execute(
            self: Arc<Self>,
            _: plakous::tools::ToolInvocation,
            context: plakous::tools::ToolContext,
        ) -> plakous::tools::ToolFuture {
            Box::pin(async move {
                use plakous::tools::*;
                let sessions = context.environment().joined_process_sessions().unwrap();
                let shell =
                    JoinedProcessShellProfile::new("/bin/sh", [String::from("-c")]).unwrap();
                let request = JoinedProcessStartRequest::new(
                    JoinedProcessCommand::shell(shell, "read line; exit 0").unwrap(),
                    ".",
                    Vec::new(),
                    true,
                    JoinedProcessTerminalMode::Pipes,
                )
                .unwrap();
                let id = sessions
                    .start(
                        request,
                        ExecutionOperationControl::new(context.cancellation().clone()),
                    )
                    .unwrap();
                plakous_tools::internal::promote(&sessions, id).unwrap();
                *self.0.lock().unwrap() = Some(sessions.clone());
                Ok(plakous_tools::internal::process_session_active(
                    ToolOutcome::success_text("background job running").unwrap(),
                    &sessions,
                    id,
                ))
            })
        }
    }

    impl WakeJobFixture {
        fn complete(&self, host: &HostLoop<'_, TestExtension>, number: u32) {
            let sessions = self.0.lock().unwrap().clone().unwrap();
            let id = sessions.session_for_number(number).unwrap();
            sessions
                .write_stdin(
                    id,
                    b"done\n",
                    plakous::tools::JoinedProcessInputDisposition::KeepOpen,
                )
                .unwrap();
            host.executor
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while sessions.joined_terminal_status(id).is_none() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                })
                .unwrap();
            sessions.join(id).unwrap();
            assert_eq!(
                sessions
                    .jobs()
                    .iter()
                    .find(|job| job.number == number)
                    .unwrap()
                    .status,
                plakous::tools::JobStatus::Exited(Some(0))
            );
        }

        fn configure(
            self: &Arc<Self>,
            config: &mut crate::runtime::HostConfig<TestExtension>,
            jobs_per_turn: &[usize],
        ) {
            use plakous::model::*;
            use plakous::tools::*;
            use plakous_offline_crash_harness::{
                BoundaryController, scripted_topology_runtime_binding,
            };
            let definition = ToolDefinition::new(
                ToolName::new("wake_job").unwrap(),
                "1.0.0",
                "Start a controlled offline process.",
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
                RegisteredToolId::new("wake-job.registration").unwrap(),
                ToolExecutorId::new("wake-job.executor").unwrap(),
                ToolExecutorBindingIdentity::new(1, &[b"wake-job.executor"]).unwrap(),
                definition,
                self.clone(),
            )];
            let mut steps = Vec::new();
            for count in jobs_per_turn {
                for _ in 0..*count {
                    steps.push(serde_json::json!({
                        "output": {"tool": {"name": "wake_job", "arguments": {}}},
                        "finish": "tool_calls"
                    }));
                }
                steps.push(serde_json::json!({"output": {"text": "waiting for completion"}}));
            }
            let script = Arc::new(
                serde_json::from_value::<plakous_offline_crash_harness::script::ScriptPlan>(
                    serde_json::json!({
                        "version": 1, "steps": steps
                    }),
                )
                .unwrap(),
            );
            let workspace = config.cwd.clone();
            let controller = BoundaryController::quiet_from_evidence_path(
                plakous_offline_crash_harness::protocol::CrashBoundary::None,
                workspace.join("wake-evidence.jsonl"),
            )
            .unwrap();
            let cursor = Arc::new(std::sync::atomic::AtomicU64::new(1));
            Arc::get_mut(config.test_injection.as_mut().unwrap())
                .unwrap()
                .runtime = Arc::new(move |_| {
                scripted_topology_runtime_binding(
                    &workspace,
                    controller.clone(),
                    script.clone(),
                    cursor.clone(),
                    tools.clone(),
                )
                .map_err(|error| error.to_string())
            });
        }
    }

    fn post_child_result(runtime: &crate::topology_runtime::ProductTopologyRuntime) {
        runtime
            .mutate_then_emit(false, |tree| {
                let child = tree
                    .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                    .map_err(|error| error.message)?;
                tree.submit_result(
                    &child.agent_id,
                    &SubmitResultRequest {
                        turn_token: 1,
                        output_json: r#"{"ok":true}"#.into(),
                    },
                    None,
                )
                .map_err(|error| error.message)?;
                tree.record_turn_completed(&child.agent_id)
                    .map_err(|error| error.message)
            })
            .unwrap();
    }

    #[test]
    fn thirty_consecutive_wakes_admit_fresh_completions_without_a_user_prompt() {
        let jobs = Arc::new(WakeJobFixture::default());
        with_configured_fixture(
            1,
            false,
            |config, _| jobs.configure(config, &[1; 30]),
            |host, runtime, _, _| {
                for cycle in 1..=30 {
                    let mail_ids = runtime
                        .read(|tree| {
                            tree.pending_mail("root")
                                .into_iter()
                                .map(|mail| mail.message_id)
                                .collect::<Vec<_>>()
                        })
                        .unwrap();
                    assert_eq!(mail_ids.len(), 1);
                    let now = Instant::now();
                    assert!(matches!(
                        host.child_wake_prompt(now).unwrap(),
                        HostStep::Continue
                    ));
                    let step = host.child_wake_prompt(now + QUIET_WINDOW).unwrap();
                    assert!(
                        matches!(step, HostStep::Prompt { .. }),
                        "wake cycle {cycle} was refused"
                    );
                    let (text, item, echoed) = take_prompt(step);
                    assert!(is_wake_delivery(
                        &host.config.session,
                        item.as_ref().unwrap()
                    ));
                    let intents = host
                        .config
                        .store
                        .load_intents(&host.config.session.id)
                        .unwrap();
                    let intent = intents.submissions.last().unwrap();
                    assert_eq!(
                        runtime
                            .read(|tree| tree
                                .command(&intent.command_id)
                                .unwrap()
                                .message_ids
                                .clone())
                            .unwrap(),
                        mail_ids
                    );
                    let notice_ids = if cycle == 1 {
                        String::new()
                    } else {
                        format!("p{}", cycle - 1)
                    };
                    assert_eq!(intent.command_id.rsplit(':').next().unwrap(), notice_ids);
                    host.run_prompt(text, item, echoed, None).unwrap();
                    host.project_now();
                    assert!(
                        runtime
                            .read(|tree| tree.pending_mail("root").is_empty())
                            .unwrap()
                    );
                    let running = host.opened.agent().unwrap().jobs();
                    assert_eq!(running.len(), 1);
                    assert_eq!(running[0].number, cycle);
                    assert_eq!(running[0].status, plakous::tools::JobStatus::Running);
                    if cycle < 30 {
                        jobs.complete(host, cycle);
                        post_child_result(runtime);
                    }
                }
            },
        );
    }

    #[test]
    fn post_terminal_jobs_and_child_result_wake_beyond_twenty_four_turns() {
        for coalesce in [false, true] {
            let jobs = Arc::new(WakeJobFixture::default());
            with_configured_fixture(
                1,
                false,
                |config, _| jobs.configure(config, &[2, 0, 0, 0, 0]),
                |host, runtime, _, _| {
                    let child = runtime
                        .mutate_then_emit(false, |tree| {
                            let child = tree
                                .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
                                .map_err(|error| error.message)?;
                            tree.set_active_turn(&child.agent_id, true)
                                .map_err(|error| error.message)?;
                            Ok(child)
                        })
                        .unwrap();
                    let now = Instant::now();
                    host.child_wake_prompt(now).unwrap();
                    let (text, item, echoed) =
                        take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
                    host.run_prompt(text, item, echoed, None).unwrap();
                    assert!(accepted(host, submissions(host).last().unwrap()));
                    assert_eq!(host.opened.agent().unwrap().jobs().len(), 2);
                    assert!(
                        runtime
                            .read(|tree| tree.nodes()[&child.agent_id].active_turn)
                            .unwrap()
                    );
                    let first = now + Duration::from_secs(10);
                    let second = first + Duration::from_secs(10);
                    for (number, at) in [(1, first), (2, second)] {
                        jobs.complete(host, number);
                        assert!(matches!(
                            host.child_wake_prompt(at).unwrap(),
                            HostStep::Continue
                        ));
                        if !coalesce || number == 1 {
                            let (text, item, echoed) =
                                take_prompt(host.child_wake_prompt(at + QUIET_WINDOW).unwrap());
                            assert_eq!(
                                submissions(host)
                                    .last()
                                    .unwrap()
                                    .command_id
                                    .rsplit(':')
                                    .next()
                                    .unwrap(),
                                format!("p{number}")
                            );
                            host.run_prompt(text, item, echoed, None).unwrap();
                        }
                    }
                    let arrived = second + Duration::from_secs(1);
                    runtime
                        .mutate_then_emit(false, |tree| {
                            tree.submit_result(
                                &child.agent_id,
                                &SubmitResultRequest {
                                    turn_token: 1,
                                    output_json: r#"{"ok":true}"#.into(),
                                },
                                None,
                            )
                            .map_err(|error| error.message)?;
                            tree.record_turn_completed(&child.agent_id)
                                .map_err(|error| error.message)
                        })
                        .unwrap();
                    let mail_ids = runtime
                        .read(|tree| {
                            tree.pending_mail("root")
                                .into_iter()
                                .map(|mail| mail.message_id)
                                .collect::<Vec<_>>()
                        })
                        .unwrap();
                    assert!(matches!(
                        host.child_wake_prompt(arrived).unwrap(),
                        HostStep::Continue
                    ));
                    assert!(matches!(
                        host.child_wake_prompt(
                            arrived + QUIET_WINDOW.checked_sub(Duration::from_millis(1)).unwrap()
                        )
                        .unwrap(),
                        HostStep::Continue
                    ));
                    let (text, item, echoed) =
                        take_prompt(host.child_wake_prompt(arrived + QUIET_WINDOW).unwrap());
                    let intents = host
                        .config
                        .store
                        .load_intents(&host.config.session.id)
                        .unwrap();
                    let intent = intents.submissions.last().unwrap();
                    assert_eq!(
                        runtime
                            .read(|tree| tree
                                .command(&intent.command_id)
                                .unwrap()
                                .message_ids
                                .clone())
                            .unwrap(),
                        mail_ids
                    );
                    assert_eq!(
                        intent.command_id.rsplit(':').next().unwrap(),
                        if coalesce { "p2" } else { "" }
                    );
                    host.run_prompt(text, item, echoed, None).unwrap();
                    assert_eq!(
                        host.opened.agent().unwrap().jobs(),
                        [] as [plakous_tools::JobView; 0]
                    );
                    assert!(
                        runtime
                            .read(|tree| tree.pending_mail("root").is_empty())
                            .unwrap()
                    );
                    assert!(matches!(
                        host.child_wake_prompt(arrived + QUIET_WINDOW * 2).unwrap(),
                        HostStep::Continue
                    ));
                    let item = host
                        .config
                        .session
                        .enqueue_live("next user prompt")
                        .unwrap();
                    host.config.store.save(&mut host.config.session).unwrap();
                    host.run_prompt(item.text.clone(), Some(item), false, None)
                        .unwrap();
                    assert!(root_batch(host, submissions(host).last().unwrap()).is_none());
                    assert_eq!(
                        host.opened.agent().unwrap().jobs(),
                        [] as [plakous_tools::JobView; 0]
                    );
                },
            );
        }
    }

    #[test]
    fn process_exit_raises_the_pending_indicator_and_a_wake_clears_it() {
        let jobs = Arc::new(WakeJobFixture::default());
        with_configured_fixture(
            0,
            false,
            |config, _| jobs.configure(config, &[1, 0]),
            |host, _, actions, observed| {
                actions.send(UserAction::Prompt("start".into())).unwrap();
                let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
                host.run_prompt(text, item, echoed, None).unwrap();
                let _ = observed.try_iter().count();
                jobs.complete(host, 1);
                host.publish_pending_continuation();
                assert!(observed.try_iter().any(|event| matches!(
                    event,
                    HostEvent::PendingContinuation {
                        process_exits: 1,
                        results: 0,
                        questions: 0,
                        ..
                    }
                )));
                let now = Instant::now();
                host.child_wake_prompt(now).unwrap();
                let (text, item, echoed) =
                    take_prompt(host.child_wake_prompt(now + QUIET_WINDOW).unwrap());
                host.run_prompt(text, item, echoed, None).unwrap();
                assert!(observed.try_iter().any(|event| matches!(
                    event,
                    HostEvent::PendingContinuation {
                        process_exits: 0,
                        ..
                    }
                )));
            },
        );
    }

    #[test]
    fn a_terminal_unseen_process_exit_survives_reopen_in_the_indicator() {
        let jobs = Arc::new(WakeJobFixture::default());
        with_configured_fixture(
            0,
            false,
            |config, _| jobs.configure(config, &[1, 0]),
            |host, _, actions, observed| {
                actions.send(UserAction::Prompt("start".into())).unwrap();
                let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
                host.run_prompt(text, item, echoed, None).unwrap();
                jobs.complete(host, 1);
                let _ = observed.try_iter().count();
                reopen_at_cut(host, |_| {
                    assert!(observed.try_iter().any(|event| matches!(
                        event,
                        HostEvent::PendingContinuation {
                            process_exits: 1,
                            ..
                        }
                    )));
                });
            },
        );
    }

    #[test]
    fn queued_user_prompt_and_shutdown_outrank_a_ready_wake() {
        with_host(1, |host, _, _, _| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            let item = host
                .config
                .session
                .enqueue_live("queued user prompt")
                .unwrap();
            host.config.store.save(&mut host.config.session).unwrap();
            host.pending_start = Some(item);
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            let (text, item, echoed) = take_prompt(host.next_prompt(None).unwrap());
            assert_eq!(text, "queued user prompt");
            host.run_prompt(text, item, echoed, None).unwrap();
            assert_eq!(
                root_batch(host, &submissions(host)[0])
                    .unwrap()
                    .message_ids
                    .len(),
                1
            );
        });
        with_host(1, |host, _, actions, _| {
            let now = Instant::now();
            host.child_wake_prompt(now).unwrap();
            actions.send(UserAction::Quit).unwrap();
            assert!(matches!(
                host.child_wake_prompt(now + QUIET_WINDOW).unwrap(),
                HostStep::Continue
            ));
            assert!(matches!(host.next_prompt(None).unwrap(), HostStep::Quit));
            assert_eq!(host.config.session.root_turn_count, 0);
        });
    }
}
