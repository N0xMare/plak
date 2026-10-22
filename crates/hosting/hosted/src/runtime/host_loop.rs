//! The attached host control loop.

use crate::sessions::queue::{drop_last_mutable, edit_mutable, move_mutable, remove_mutable};

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use plakous::application::ApplicationExecutor;
use plakous::sessions::ContextQuery;

use crate::context::{
    ContextDisposition, occupancy_requires_scheduled_compact, scheduled_compaction_disabled_note,
    scheduled_compaction_parks_when_disabled, should_schedule_compaction,
};
use crate::interrupt::{InterruptWatch, InterruptWatchState};
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{
    self, QueueDrainState as SessionQueueDrainState, QueueItem as SessionQueueItem,
};
use crate::{CliFailure, CliResult};

use super::admission::{
    admit_idle_immediate_prompt, commit_session_update, emit_prompt_history, emit_queue_snapshot,
    emit_restored_paused_note, emit_unavailable_context, queue_edit_applied, queue_edit_refused,
    queue_persist_failed, queue_refusal, recovery_blocked_action, refuse_recovery_blocked_action,
    reject_draft, reject_stale_queue_action, restored_paused_status_suffix,
};
use super::clear::{clear_session_rebuild, delete_unpublished_runtime, resume_session_switch};
use super::compact::{
    ScheduledPrompt, compact_may_resume_queue, context_capacity_recovery_guidance, handle_compact,
    observe_compact, run_scheduled_compaction_until_fits,
};
use super::fork::handle_fork;
use super::info::{
    TerminalPhase, emit_agent_plan_notes, emit_terminal_status, send_binding, send_session_info,
};
use super::logging::{
    elapsed_ms_since, log_ev, log_host_attached, log_host_ready, log_session_ready,
};
use super::models::native_compact_facet;
use super::rebind::{Rebound, durable_tree_id, handle_rebind};
use super::reconcile::{
    commit_definite_failure_projection, commit_exceptional_terminal_projection, persist,
};
use super::resume::handle_resume_picker;
use super::shell::refresh_credential_readiness;
use super::startup::build_session_from_config;
#[cfg(any(test, feature = "test-support"))]
use super::startup::observe_test_boundary;
use super::surface;
use super::tree::{
    dispatch_invoke_tool_sync, dispatch_topology_navigator_sync,
    emit_tree_snapshot_if_revision_advanced, record_emitted_tree_revision,
};
use super::turn::{host_outcome_blocks_continuation, publish_recovered_turn_result};
use super::usage::{
    compact_reason_for_disposition, emit_context_projection, last_enrolled_source_bytes,
    query_context, report_context,
};
use super::{
    APPROVAL_REFUSED_NOTE, ExceptionalTerminalProjection, HeldUsage, HostConfig, HostEvent,
    IDLE_OBSERVE_POLL_INTERVAL, Sender, SessionLedger, SubscriptionUsageRefresh,
    TREE_OBSERVE_INTERVAL, TreeSnapshotPush, TurnHostOutcome, UserAction, drive_one_turn, queue,
    topology_state,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow};

pub(super) fn run_host_loop<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
    actions: &crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
) -> CliResult<()> {
    run(config, actions, events, startup, None)
}

fn prompt_requires_scheduled_compaction(projection: crate::context::ContextProjection) -> bool {
    occupancy_requires_scheduled_compact(projection)
        || projection.observation_headroom_requires_compaction()
}

pub(super) fn run<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
    actions: &crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    one_shot: Option<&mut super::oneshot::OneShotState>,
) -> CliResult<()> {
    HostLoop::open(config, actions, events, startup)?.run(one_shot)
}

/// Sole owner of the attachment, durable session record, queue, and publication.
struct HostLoop<'a, P: plakous_host_protocol::ProtocolExtension> {
    config: HostConfig<P>,
    actions: &'a crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    opened: crate::agent::AttachedCodingSession,
    quit_started_at: Option<Instant>,
    quit_abandoned_turn: bool,
    quit_job_stops: Option<tokio::task::JoinHandle<Result<(), String>>>,
    executor: ApplicationExecutor,
    log: Option<SessionLog>,
    stderr_buf: Vec<u8>,
    interrupt_state: RefCell<InterruptWatchState<P>>,
    pending_start: Option<SessionQueueItem>,
    pending_resume: Option<transient::PendingResume>,
    transient_resumes_used: u8,
    last_tree_revision: TreeSnapshotPush,
    job_events: super::job_events::JobEvents,
    last_tree_observed_at: Instant,
    child_wake: wake::ChildWake,
    pending_track: pending::PendingTrack,
    recovery_blocked: bool,
    /// This attachment's own open closed an unfinished turn and said so.
    recovered_at_open: bool,
    held_usage: HeldUsage,
    session_ledger: SessionLedger,
    pending_usage: std::sync::Arc<std::sync::Mutex<super::projection::presentation::ProcessUsage>>,
    subscription_usage_refresh: SubscriptionUsageRefresh<P>,
    /// The session's projector task, spawned on the first tick.
    live: Option<super::projection::live::LiveProjector>,
    projection_finished: bool,
    #[cfg(any(test, feature = "test-support"))]
    _injected_child_runtime: Option<crate::agent::injected_child_binding::Guard>,
}

enum HostStep {
    Continue,
    Quit,
    Prompt {
        text: String,
        queued_delivery: Option<SessionQueueItem>,
        user_echoed: bool,
    },
}

mod idle;
mod open;
mod pending;
mod prompt;
pub(super) mod publication;
mod recover;
mod release;
mod run_until_idle;
pub(in crate::runtime) mod transient;
pub(in crate::runtime) mod wake;

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    /// What the live projector follows now: the attached session's feed.
    fn live_target(&self) -> super::projection::live::LiveTarget {
        let rows = self.config.topology_rows();
        let events = self.events.clone();
        self.pending_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target = Some(self.opened.locator().session_id());
        super::projection::live::LiveTarget {
            sessions: self.opened.shared_sessions(),
            store: self.config.store.clone(),
            roots: self.config.state_roots.clone(),
            product_id: self.config.session.id.clone(),
            followed: self.opened.locator(),
            tree_id: durable_tree_id(&rows, &self.config.session),
            emit_settled: std::sync::Arc::new(move |row| {
                let _ = events.send(HostEvent::Row(row));
            }),
        }
    }

    /// The live projector tick (spec 02 §2.6): asks the session's projector
    /// task for a pass and returns at once (it also follows every commit the
    /// runtime acknowledges). A refused pass is projection debt; the next
    /// pass applies it.
    ///
    /// A session the product has not stored yet still gets its task: the
    /// first prompt stores it mid-turn, and the passes before that refuse
    /// on the missing session row and change nothing.
    fn project(&mut self) {
        let target = self.live_target();
        if self.live.is_none() {
            self.live = self
                .executor
                .block_on(async { super::projection::live::LiveProjector::spawn() })
                .ok();
        }
        if let Some(live) = self.live.as_mut() {
            live.tick(target.clone());
            if let Some(error) = live.take_refusal()
                && self.config.session.store_revision != 0
            {
                self.log_projector_debt(&error);
            }
        }
        if self.config.session.store_revision != 0
            && let Ok(Err(error)) = self.executor.block_on(target.tree_pass())
        {
            self.log_projector_debt(&error);
        }
        if self.config.session.queue_commands.has_pending()
            || self.config.session.queue_projection_debt
        {
            let joined = self
                .config
                .store
                .load_intents(&self.config.session.id)
                .and_then(|intents| {
                    self.config
                        .store
                        .load_views(&self.config.session.id)
                        .map(|views| sessions::QueueCommands::from_join(&intents, views.as_ref()))
                });
            if let Ok(commands) = joined {
                let changed = self.config.session.queue_commands != commands;
                self.config.session.queue_commands = commands;
                self.config.session.queue_projection_debt =
                    self.config.session.queue_projection_debt
                        && self.config.session.queue_commands.has_pending();
                self.config
                    .session
                    .pending_queue
                    .retain(|item| !self.config.session.queue_commands.is_consumed(item));
                if changed {
                    emit_queue_snapshot(&self.events, &self.config.session);
                }
                if self.pending_start.is_none()
                    && !self.recovery_blocked
                    && self.config.session.queue_drain_state
                        == SessionQueueDrainState::LiveAutoDrain
                {
                    self.pending_start = queue::next_drainable(&self.config.session);
                }
            }
        }
        self.publish_projected_usage();
    }

    /// A projector pass to the tail that the caller waits for: a reopen's
    /// step 2, whose rows the next step reads.
    fn project_now(&mut self) {
        if self.config.session.store_revision == 0 {
            return;
        }
        let target = self.live_target();
        let passed = self.executor.block_on(async {
            target.pass().await?;
            target.tree_pass().await
        });
        if matches!(&passed, Ok(Ok(()))) {
            self.config.session.queue_projection_debt = false;
        }
        if let Ok(Err(error)) = passed {
            self.log_projector_debt(&error);
        }
        self.publish_projected_usage();
    }

    /// Replace settled previews with the committed projector values before
    /// announcing idle. This also includes provider-hosted calls, which have
    /// no local ToolStarted/ToolCompleted observations.
    fn publish_projected_transcript(&mut self, one_shot: bool) {
        self.commit_notice_intents();
        let target = self.live_target();
        let deadline = self.projection_deadline(one_shot);
        let projected = self.executor.block_on(async {
            let pass = async {
                if let Some(live) = self.live.take()
                    && let Some(error) = live.finish(target.clone(), None).await
                {
                    return Err(error);
                }
                target.pass().await
            };
            if let Some(deadline) = deadline {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), pass)
                    .await
                    .unwrap_or_else(|_| {
                        Err("the terminal projector pass exceeded its bound".to_owned())
                    })
            } else {
                pass.await
            }
        });
        match projected {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                self.log_projector_debt(&error);
                return;
            }
            Err(_) => {
                self.log_projector_debt("the terminal projector pass could not run");
                return;
            }
        }
        self.publish_projected_usage();
        let Ok(stored) = self.config.store.load(&self.config.session.id) else {
            return;
        };
        let unsaved = self.config.session.transcript.pending_notices().to_vec();
        self.config.session.transcript = stored.transcript;
        for notice in unsaved {
            self.config.session.record_notice(notice);
        }
        self.config.session.root_turn_count = stored.root_turn_count;
        self.config.session.tool_call_count = stored.tool_call_count;
        self.config.session.compaction_count = stored.compaction_count;
        self.config.session.plakous_locator = stored.plakous_locator;
        self.config.session.pending_queue = stored.pending_queue;
        self.config.session.queue_commands = stored.queue_commands;
        self.config.session.queue_projection_debt = self.config.session.queue_projection_debt
            && self.config.session.queue_commands.has_pending();
        let _ = self.events.send(super::logging::project_session_snapshot(
            &self.config.session,
        ));
    }

    fn publish_projected_usage(&self) {
        for terminal in take_pending_usage(&self.pending_usage, self.opened.locator().session_id())
        {
            super::usage::emit_projected_turn_usage(
                &self.events,
                &self.config.session.id,
                terminal.turn_id,
                terminal.usage,
                self.log.as_ref(),
                &self.session_ledger,
            );
        }
    }

    fn projection_deadline(&self, one_shot: bool) -> Option<Instant> {
        self.quit_started_at
            .map(|started| started + Duration::from_secs(1))
            .or_else(|| (!one_shot).then(|| Instant::now() + FINAL_PROJECTION_BOUND))
    }

    /// The final pass before release (spec 02 §2.6, §4). The interactive
    /// faces bound it: debt it leaves is safe, because the cursor is CAS'd
    /// and the next open catches up. The one-shot face exits only after the
    /// projector applied its turn's terminal, so it waits for the pass to
    /// complete and a refused pass fails the command.
    fn finish_projection(&mut self, one_shot: bool) -> CliResult<()> {
        self.projection_finished = true;
        if self.config.session.store_revision == 0 {
            return Ok(());
        }
        let target = self.live_target();
        let deadline = self.projection_deadline(one_shot);
        let live = self.live.take();
        let finished = self.executor.block_on(async {
            let pass = async {
                let refused = match live {
                    Some(live) => live.finish(target.clone(), None).await,
                    None => target.pass().await.err(),
                };
                (refused, target.tree_pass().await.err())
            };
            if let Some(deadline) = deadline {
                tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), pass)
                    .await
                    .unwrap_or_else(|_| {
                        (
                            Some("the final projector pass exceeded its bound".to_owned()),
                            None,
                        )
                    })
            } else {
                pass.await
            }
        });
        let refused = match finished {
            Ok((refused, tree_refused)) => {
                if let Some(error) = tree_refused {
                    self.log_projector_debt(&error);
                }
                refused
            }
            Err(_) => Some("the final projector pass could not run".to_owned()),
        };
        self.publish_projected_usage();
        match refused {
            Some(error) if one_shot => Err(CliFailure::classified(
                crate::error::CliFailureKind::Internal,
                format!("the session's final projection failed: {error}"),
            )),
            Some(error) => {
                self.log_projector_debt(&error);
                Ok(())
            }
            None => Ok(()),
        }
    }

    fn log_projector_debt(&self, error: &str) {
        log_ev(
            self.log.as_ref(),
            &self.config,
            SessionEvent::new("projector.deferred").str("error", error),
        );
    }

    fn commit_notice_intents(&mut self) {
        super::events::commit_notice_intents(
            &self.events,
            &*self.config.store,
            &self.config.home,
            self.log.as_ref(),
            &mut self.config.session,
        );
    }

    fn update_background_process_count(&mut self) {
        for failure in self
            .config
            .state_roots
            .job_registry()
            .take_user_stop_failures()
        {
            let _ = self.events.notice(sessions::TranscriptRow::error(
                sessions::NoticeScope::Session,
                failure,
            ));
        }
        self.actions
            .set_background_processes(self.config.state_roots.job_registry().running_count());
        for row in self
            .job_events
            .emit(self.config.state_roots.job_registry(), &self.events)
        {
            // Live only: the projector derives the stored process row.
            self.config.session.append_transcript(row.clone());
            let _ = self.events.send(HostEvent::Row(row));
        }
        self.actions
            .set_processes(self.config.state_roots.job_registry().snapshot());
    }

    fn run(mut self, mut one_shot: Option<&mut super::oneshot::OneShotState>) -> CliResult<()> {
        let driven = self.drive(one_shot.as_deref_mut());
        let disowned = if one_shot
            .as_ref()
            .and_then(|state| state.run.as_ref())
            .is_some_and(|run| run.bounds.leave_jobs_running)
        {
            self.executor
                .block_on(self.config.state_roots.job_registry().disown_leftovers())
                .map_err(|_| CliFailure::new("job disown execution failed"))
                .and_then(|result| result.map_err(CliFailure::new))
        } else {
            Ok(())
        };
        let released = self.release().and(disowned);
        match (driven, released) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(error), Err(release)) => Err(error.with_release_failure(release)),
        }
    }

    fn drive(&mut self, mut one_shot: Option<&mut super::oneshot::OneShotState>) -> CliResult<()> {
        let mut passive_checkpoint_scheduled = false;
        if let Some(state) = one_shot.as_deref_mut() {
            let tree_id = durable_tree_id(&self.config.topology_rows(), &self.config.session);
            let resumed = if state.resume_tree {
                topology_state::resume_tree(&tree_id)
                    .map_err(CliFailure::new)
                    .and_then(|()| {
                        self.executor
                            .block_on(crate::agent::rejoin_inflight_after_resume(&tree_id))
                            .map_err(|_| CliFailure::new("application execution failed"))
                    })
                    .inspect(|()| self.spawn_tree_recovery())
            } else if crate::agent::topology_tree_is_restored_paused(&tree_id) {
                Err(CliFailure::new(crate::agent::RESTORED_PAUSED_RESUME_HINT))
            } else {
                Ok(())
            };
            if let Err(error) = resumed {
                state.result = Some(Err(error));
                state.attempted = true;
            }
        }

        loop {
            self.commit_notice_intents();
            self.project();
            if !passive_checkpoint_scheduled && self.actions.is_promptable() {
                self.executor
                    .block_on(async { self.opened.schedule_passive_checkpoint() })
                    .map_err(|_| CliFailure::new("application execution failed"))?;
                passive_checkpoint_scheduled = true;
            }
            if let Some(state) = one_shot.as_deref_mut() {
                if state.attempted && state.run.is_none() {
                    break;
                }
            }
            let (raw_text, queued_delivery, user_echoed) = match if let Some(state) =
                one_shot.as_deref_mut()
                && state.attempted
            {
                self.next_run_prompt(state)?
            } else {
                self.next_prompt(one_shot.as_deref_mut())?
            } {
                HostStep::Continue => continue,
                HostStep::Quit => break,
                HostStep::Prompt {
                    text,
                    queued_delivery,
                    user_echoed,
                } => (text, queued_delivery, user_echoed),
            };
            let before = self.config.session.root_turn_count;
            let step = self.run_prompt(
                raw_text,
                queued_delivery,
                user_echoed,
                one_shot.as_deref_mut(),
            )?;
            if let Some(run) = one_shot.as_deref_mut().and_then(|state| state.run.as_mut()) {
                run.turns = run
                    .turns
                    .saturating_add(u8::from(self.config.session.root_turn_count > before));
            }
            if matches!(step, HostStep::Quit) {
                break;
            }
        }

        self.commit_notice_intents();
        self.finish_projection(one_shot.is_some())
    }
}

fn take_pending_usage(
    pending: &std::sync::Mutex<super::projection::presentation::ProcessUsage>,
    session: plakous::model::SessionId,
) -> Vec<sessions::projector::ProjectedTurnUsage> {
    let mut usage = pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    std::mem::take(&mut usage.pending)
        .into_iter()
        .filter(|terminal| terminal.session_id == session)
        .collect()
}

/// How long an interactive release waits for the final projector pass outside quit.
pub(in crate::runtime) const FINAL_PROJECTION_BOUND: Duration = Duration::from_millis(400);

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::*;

    use crate::interrupt::{InterruptWatch, InterruptWatchState, run_watched};
    use crate::runtime::test_support::*;
    use plakous::application::ApplicationExecutor;
    use std::cell::RefCell;
    use std::time::Duration;

    #[test]
    fn unavailable_session_workflows_never_guess_or_mutate() {
        let (events, received) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(events);
        emit_unavailable_context(&events);
        assert!(matches!(
            received.recv_timeout(Duration::from_secs(1)).unwrap(),
            HostEvent::Usage(usage)
                if usage.prepared_input.is_none()
                    && usage.admitted_ceiling.is_none()
                    && usage.status == "unavailable"
        ));
    }

    /// An action the operator already sent outranks the durable queue, so
    /// the drive loop asks before it starts a queued turn.
    #[test]
    fn a_deferred_action_is_visible_before_the_queue_opens_a_turn() {
        let (actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&receiver, &state);
        assert!(!state.borrow().has_deferred());
        actions.send(UserAction::Quit).unwrap();
        watch.poll();
        assert!(state.borrow().has_deferred());
        assert!(matches!(
            state.borrow_mut().take_deferred().as_deref(),
            Some(UserAction::Quit)
        ));
        assert!(!state.borrow().has_deferred());
    }

    #[test]
    fn run_watched_observes_an_interrupt_within_one_round_trip() {
        let executor = ApplicationExecutor::new().unwrap();
        let (actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        watch.begin();

        let sender = actions_tx;
        let round_trip = async move {
            tokio::time::sleep(Duration::from_millis(40)).await;
            sender.send(UserAction::Prompt("deferred".into())).unwrap();
            sender.send(UserAction::Interrupt).unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
            "receipt"
        };
        let receipt = run_watched(&executor, watch, round_trip).unwrap();

        // The round-trip still reached its own receipt; only the next step is
        // skipped by the caller.
        assert_eq!(receipt, "receipt");
        assert!(watch.interrupted());
        // Ordinary product work drained on the way to the urgent lane is
        // handed back to the host loop instead of being swallowed.
        let deferred = interrupt_state.borrow_mut().take_deferred();
        assert_eq!(
            deferred.map(|delivery| delivery.into_parts().0),
            Some(UserAction::Prompt("deferred".into()))
        );
    }

    #[test]
    fn uninterrupted_compact_and_fork_still_run_their_host_workflows() {
        let root = tempfile::tempdir().unwrap();
        // This injected compact can fail without a fact. Fork success must
        // not depend on discarding that other workflow's unresolved request.
        let mut events = run_attached_actions(
            no_effort_openrouter_injected_host(&root.path().join("compact")),
            vec![UserAction::Compact],
        );
        assert!(
            events.iter().any(|event| matches!(event,
                HostEvent::ToolLine { name, state, .. } if name == "compact" && state == "running"
            )),
            "an uninterrupted compact still dispatches: {events:?}"
        );
        events.extend(run_attached_actions(
            no_effort_openrouter_injected_host(&root.path().join("fork")),
            vec![UserAction::ForkSession {
                account: None,
                compact: false,
            }],
        ));

        assert!(
            events.iter().any(|event| matches!(
                event,
                HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("forked session")
            )),
            "an uninterrupted fork still commits: {events:?}"
        );
        assert!(
            events.iter().all(|event| !matches!(
                event,
                HostEvent::Row(plakous_host_protocol::views::transcript::TranscriptRow::Notice(plakous_host_protocol::views::transcript::NoticeRow { text: note, .. })) if note.contains("before its canonical dispatch")
            )),
            "no interrupt checkpoint may fire without an interrupt: {events:?}"
        );
    }
}
