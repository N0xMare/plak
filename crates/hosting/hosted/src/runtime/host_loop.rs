//! The attached host control loop.

use crate::sessions::queue::{drop_last_mutable, edit_mutable, move_mutable, remove_mutable};
use crate::surface_protocol::ContentEvent;

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use plakous::sessions::ContextQuery;

use crate::context::{ContextDisposition, scheduled_compaction_disabled_note};
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
use super::factory::observe_boundary;
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
use super::startup::build_session_from_config;
use super::surface;
use super::tree::{
    dispatch_invoke_tool, dispatch_topology_navigator, emit_tree_snapshot_if_revision_advanced,
    record_emitted_tree_revision,
};
use super::turn::host_outcome_blocks_continuation;
use super::usage::{
    compact_reason_for_disposition, emit_context_projection, last_enrolled_source_bytes,
    query_context, report_context,
};
use super::{
    APPROVAL_REFUSED_NOTE, ExceptionalTerminalProjection, HeldUsage, HostConfig,
    IDLE_OBSERVE_POLL_INTERVAL, Sender, SessionLedger, SubscriptionUsageRefresh,
    TREE_OBSERVE_INTERVAL, TreeSnapshotPush, TurnHostOutcome, UserAction, drive_one_turn, queue,
    topology_state,
};
use super::{HostBoundary, HostWorkflow};

pub(super) fn run_host_loop<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
    actions: &crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
) -> CliResult<()> {
    run(config, actions, events, startup, None)
}

/// Runs the attached root as one async actor: the single future of one
/// `block_on` on the attachment's thread, from open through release. Nothing
/// it awaits blocks on an executor of its own. The actor is dropped outside
/// the executor, before the session executor that ends its tasks.
pub(super) fn run<P: plakous_host_protocol::ProtocolExtension>(
    config: HostConfig<P>,
    actions: &crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    one_shot: Option<&mut super::oneshot::OneShotState>,
) -> CliResult<()> {
    let executor = std::rc::Rc::new(
        session_executor::SessionExecutor::new()
            .map_err(|_| CliFailure::new("application executor construction failed"))?,
    );
    let (result, actor) = executor
        .block_on(async {
            match NodeActor::open(
                config,
                actions,
                events,
                startup,
                std::rc::Rc::clone(&executor),
            )
            .await
            {
                Ok(mut actor) => (actor.run(one_shot).await, Some(actor)),
                Err(error) => (Err(error), None),
            }
        })
        .map_err(|_| CliFailure::new("application execution failed"))?;
    drop(actor);
    result
}

/// What one node's actor is for, as a value its single driver consults
/// rather than a second driver (design §3.2): which projections it runs and
/// what follows a turn that ends recovery-required. Only the root runs as an
/// actor so far.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct RolePolicy {
    /// The product projector plus the root's tree passes (spec 03 §4.4).
    pub(super) tree_passes: bool,
    pub(super) recovery: RecoveryPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RecoveryPolicy {
    /// The node reopens itself in place as soon as it is idle (design §6.0).
    EagerInPlace,
}

/// Where the root stands after a turn that ended recovery-required, or an
/// open that found one (design §6.2's process-local `recovering` overlay).
/// Nothing durable records it: the runtime's own state is the authority, and
/// a restarted host finds the same unfinished turn at open.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum RootRecovery {
    /// Nothing to recover.
    #[default]
    Clear,
    /// The actor recovers in place at its next idle entry.
    Due,
    /// The eager reopen failed; the next action that needs the session, or
    /// an explicit recovery, retries it.
    Retrying,
}

impl RootRecovery {
    /// Whether actions that need the session must recover it first.
    pub(super) const fn blocked(self) -> bool {
        !matches!(self, Self::Clear)
    }
}

impl RolePolicy {
    /// The attached root's policy.
    pub(super) const ROOT: Self = Self {
        tree_passes: true,
        recovery: RecoveryPolicy::EagerInPlace,
    };
}

/// The root node's actor: sole owner of the attachment, durable session
/// record, queue, interrupt and deferred state, and publication.
struct NodeActor<'a, P: plakous_host_protocol::ProtocolExtension> {
    role: RolePolicy,
    _storage_notices: Option<super::storage::StorageNotices>,
    config: HostConfig<P>,
    actions: &'a crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
    opened: crate::agent::AttachedCodingSession,
    quit_started_at: Option<Instant>,
    quit_abandoned_turn: bool,
    quit_job_stops: Option<crate::TaskHandle<Result<(), String>>>,
    /// The attachment's executor, shared with the thread that drives the
    /// actor; the actor spawns only onto its task set.
    executor: std::rc::Rc<session_executor::SessionExecutor>,
    log: Option<SessionLog>,
    stderr_buf: Vec<u8>,
    /// Shared, so a watched step can observe the mailbox while it borrows the
    /// actor.
    interrupt_state: std::rc::Rc<RefCell<InterruptWatchState<P>>>,
    pending_start: Option<SessionQueueItem>,
    /// The idle prompt's one capacity sample, shared by echo and admission.
    pending_storage_admission: Option<crate::session_store::StorageAdmission>,
    pending_resume: Option<transient::PendingResume>,
    transient_resumes_used: u8,
    last_tree_revision: TreeSnapshotPush,
    job_events: super::job_events::JobEvents,
    last_tree_observed_at: Instant,
    child_wake: wake::RootWake,
    storage_interrupted: bool,
    pending_track: pending::PendingTrack,
    /// The root's process-local recovery overlay; never durable.
    recovery: RootRecovery,
    /// This attachment's own open closed an unfinished turn and said so.
    recovered_at_open: bool,
    held_usage: HeldUsage,
    session_ledger: SessionLedger,
    pending_usage: std::sync::Arc<std::sync::Mutex<super::projection::presentation::ProcessUsage>>,
    subscription_usage_refresh: SubscriptionUsageRefresh<P>,
    readiness: super::shell::ReadinessRefresh,
    /// The session's projector task, spawned on the first tick.
    live: Option<super::projection::live::LiveProjector>,
    projection_finished: bool,
    shown_skill_notices: crate::skills::ShownSkillNotices,
    prep_hooks: prompt::preparation::Hooks,
    /// The actor entered idle without a settled turn's tree pass (a
    /// workflow); the next idle pass runs the root's settled tree pass.
    idle_entry_pass_due: bool,
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
mod restored;
mod run_until_idle;
mod session_executor;
pub(in crate::runtime) mod transient;
pub(in crate::runtime) mod wake;

impl<P: plakous_host_protocol::ProtocolExtension> NodeActor<'_, P> {
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
                let _ = events.send(ContentEvent::Row(row));
            }),
        }
    }

    /// The live projector tick (spec 02 §2.6): asks the session's projector
    /// task for a pass and returns at once (it also follows every commit the
    /// runtime acknowledges). A refused pass is projection debt; the next
    /// pass applies it.
    ///
    /// A session the product has not stored yet still gets its task: the
    /// first prompt stores it mid-turn. No pass is requested before that:
    /// an unstored view read cannot warm the attachment's persistent cache.
    async fn project(&mut self) {
        if self.config.store.storage_unavailable() {
            self.storage_interrupted |= self.config.store.storage_wait_cancelled();
            if self.storage_interrupted {
                return;
            }
        }
        let target = self.live_target();
        // A stopped task (it panicked or was aborted) is replaced: the new
        // one loads the views again and reconciles the settled rows once.
        if self
            .live
            .as_ref()
            .is_some_and(super::projection::live::LiveProjector::stopped)
        {
            self.live = None;
        }
        if self.live.is_none() {
            self.live = Some(super::projection::live::LiveProjector::spawn(
                self.executor.tasks(),
            ));
        }
        if self.config.session.store_revision != 0
            && let Some(live) = self.live.as_mut()
        {
            live.tick(target.clone());
            if let Some(error) = live.take_refusal() {
                self.log_projector_debt(&error);
            }
        }
        // The root's tree pass runs here only at idle entry (spec 03 §4.4):
        // after open, a recovery or a workflow, never on a tick or an intent
        // alone. A turn settles its own pass. The tick keeps the tree's
        // housekeeping.
        if self.role.tree_passes && self.config.session.store_revision != 0 {
            let due = std::mem::take(&mut self.idle_entry_pass_due);
            let passed = if due {
                target.tree_pass().await
            } else {
                target.tree_housekeeping().await
            };
            if let Err(error) = passed {
                // A refused idle-entry pass stays due: the next tick retries it.
                self.idle_entry_pass_due = due;
                self.log_projector_debt(&error);
            }
        }
        if self.config.session.queue_commands.has_pending()
            || self.config.session.queue_projection_debt
        {
            let joined = self
                .config
                .store
                .load_queue_commands(&self.config.session.id);
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
                    && !self.recovery.blocked()
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
    async fn project_now(&mut self) {
        if self.config.session.store_revision == 0 {
            return;
        }
        let target = self.live_target();
        let passed = async {
            target.pass().await?;
            target.tree_pass().await
        }
        .await;
        if passed.is_ok() {
            self.config.session.queue_projection_debt = false;
        }
        if let Err(error) = passed {
            self.log_projector_debt(&error);
        }
        self.publish_projected_usage();
    }

    /// Replace settled previews with the committed projector values before
    /// announcing idle. This also includes provider-hosted calls, which have
    /// no local ToolStarted/ToolCompleted observations.
    async fn publish_projected_transcript(&mut self, one_shot: bool) {
        self.commit_notice_intents();
        let target = self.live_target();
        let deadline = self.projection_deadline(one_shot);
        // The task keeps its views and the rows it has shown for the next
        // turn: no cold reload and no re-emission of settled rows.
        let live = self.live.as_mut();
        let pass = async {
            match live {
                Some(live) => live.settle(target.clone()).await,
                None => target.views(None).await.map(|projected| projected.views),
            }
        };
        let projected = if let Some(deadline) = deadline {
            tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), pass)
                .await
                .unwrap_or_else(
                    |_| Err("the terminal projector pass exceeded its bound".to_owned()),
                )
        } else {
            pass.await
        };
        let views = match projected {
            Ok(views) => views,
            Err(error) => {
                self.log_projector_debt(&error);
                return;
            }
        };
        self.publish_projected_usage();
        let Ok(intents) = self.config.store.load_intents(&self.config.session.id) else {
            return;
        };
        let Ok(stored) = self.config.store.load_queue_head(&self.config.session.id) else {
            return;
        };
        let unsaved = self.config.session.transcript.pending_notice_items();
        self.config.session.transcript =
            sessions::ProductTranscript::stored(views.transcript(&intents));
        for (notice, id) in unsaved {
            self.config
                .session
                .transcript
                .record_notice_item(notice, id);
        }
        self.config.session.root_turn_count = views.state.counters.root_turns;
        self.config.session.tool_call_count = views.state.counters.tool_calls;
        self.config.session.compaction_count = views.state.counters.compactions;
        self.config.session.plakous_locator = views.projected_locator(stored.recorded_locator);
        self.config.session.queue_commands =
            sessions::QueueCommands::from_join(&intents, Some(&views));
        self.config.session.pending_queue = stored.pending_queue;
        self.config
            .session
            .pending_queue
            .retain(|item| !self.config.session.queue_commands.is_consumed(item));
        self.config.session.queue_projection_debt = self.config.session.queue_projection_debt
            && self.config.session.queue_commands.has_pending();
        let snapshot = super::logging::project_session_snapshot(&self.config.session);
        let _ = self.events.publish_transcript(snapshot);
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
    async fn finish_projection(&mut self, one_shot: bool) -> CliResult<()> {
        self.projection_finished = true;
        if self.config.session.store_revision == 0 {
            return Ok(());
        }
        let target = self.live_target();
        let deadline = self.projection_deadline(one_shot);
        let live = self.live.take();
        let pass = async {
            let refused = match live {
                Some(live) => live.finish(target.clone(), None).await.err(),
                None => target.pass().await.err(),
            };
            (refused, target.tree_pass().await.err())
        };
        let (refused, tree_refused) = if let Some(deadline) = deadline {
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
        };
        if let Some(error) = tree_refused {
            self.log_projector_debt(&error);
        }
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
        // A new construction (this session, a fork, a spawn) queued what its
        // skills discovery left out; each distinct notice is shown once per
        // root session.
        let notices = self.config.state_roots.take_skill_notices();
        for notice in self
            .shown_skill_notices
            .fresh(&self.config.session.id, notices)
        {
            let _ = self.events.notice(sessions::TranscriptRow::info(
                sessions::NoticeScope::Session,
                notice,
            ));
        }
        if self.storage_interrupted && self.config.store.storage_unavailable() {
            return;
        }
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
            let _ = self.events.send(ContentEvent::Row(row));
        }
        self.actions
            .set_processes(self.config.state_roots.job_registry().snapshot());
    }

    async fn run(
        &mut self,
        mut one_shot: Option<&mut super::oneshot::OneShotState>,
    ) -> CliResult<()> {
        let driven = self.drive(one_shot.as_deref_mut()).await;
        let disowned = if one_shot
            .as_ref()
            .and_then(|state| state.run.as_ref())
            .is_some_and(|run| run.bounds.leave_jobs_running)
        {
            self.config
                .state_roots
                .job_registry()
                .disown_leftovers()
                .await
                .map_err(CliFailure::new)
        } else {
            Ok(())
        };
        let released = self.release().await.and(disowned);
        match (driven, released) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(error), Err(release)) => Err(error.with_release_failure(release)),
        }
    }

    async fn drive(
        &mut self,
        mut one_shot: Option<&mut super::oneshot::OneShotState>,
    ) -> CliResult<()> {
        // A one-shot's initial prompt belongs after the recovered turn, not
        // in its busy follow-up queue. Keep its delivery/provenance intact.
        let initial_prompt = if one_shot.is_some() && self.opened.has_recovered_turn() {
            crate::intent_dispatch::take_next(&self.interrupt_state, || self.actions.try_recv())
                .ok()
        } else {
            None
        };
        if matches!(self.drive_restored_turn().await?, HostStep::Quit) {
            return self.finish_projection(one_shot.is_some()).await;
        }
        if let Some(delivery) = initial_prompt {
            self.interrupt_state.borrow_mut().restore_front(delivery);
        }
        let mut passive_checkpoint_scheduled = false;
        if let Some(state) = one_shot.as_deref_mut()
            && let Err(error) = self.resume_one_shot_tree(state.resume_tree).await
        {
            state.result = Some(Err(error));
            state.attempted = true;
        }

        loop {
            if self.config.store.storage_unavailable() {
                self.storage_interrupted |= self.config.store.storage_wait_cancelled();
            } else {
                self.storage_interrupted = false;
            }
            self.commit_notice_intents();
            self.project().await;
            if !passive_checkpoint_scheduled && self.actions.is_promptable() {
                self.opened
                    .schedule_passive_checkpoint(self.executor.tasks());
                passive_checkpoint_scheduled = true;
            }
            if let Some(state) = one_shot.as_deref_mut() {
                if state.attempted && state.run.is_none() {
                    break;
                }
            }
            let next = if let Some(state) = one_shot.as_deref_mut()
                && state.attempted
            {
                self.next_run_prompt(state).await
            } else {
                self.next_prompt(one_shot.as_deref_mut()).await
            };
            let next = match next {
                Ok(next) => next,
                Err(error) if self.retain_interrupted_storage(&error) => continue,
                Err(error) => return Err(error),
            };
            let (raw_text, queued_delivery, user_echoed) = match next {
                HostStep::Continue => continue,
                HostStep::Quit => break,
                HostStep::Prompt {
                    text,
                    queued_delivery,
                    user_echoed,
                } => (text, queued_delivery, user_echoed),
            };
            let before = self.config.session.root_turn_count;
            let step = match self
                .run_prompt(
                    raw_text,
                    queued_delivery,
                    user_echoed,
                    one_shot.as_deref_mut(),
                )
                .await
            {
                Ok(step) => step,
                Err(error) if self.retain_interrupted_storage(&error) => continue,
                Err(error) => return Err(error),
            };
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
        self.finish_projection(one_shot.is_some()).await
    }

    /// A one-shot resumes a paused tree only when asked (`--resume-tree`);
    /// otherwise a paused tree refuses the run.
    async fn resume_one_shot_tree(&self, resume_tree: bool) -> CliResult<()> {
        let tree_id = durable_tree_id(&self.config.topology_rows(), &self.config.session);
        if resume_tree {
            topology_state::resume_tree(&tree_id)
                .await
                .map_err(CliFailure::new)?;
            crate::agent::rejoin_inflight_after_resume(&tree_id).await;
            self.spawn_tree_recovery();
            Ok(())
        } else if crate::agent::topology_tree_is_restored_paused(&tree_id) {
            Err(CliFailure::new(crate::agent::RESTORED_PAUSED_REFUSAL)
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeTree {}))
        } else {
            Ok(())
        }
    }

    fn retain_interrupted_storage(&mut self, error: &CliFailure) -> bool {
        if error.store_refusal() != Some(crate::error::StoreRefusal::StorageFull) {
            return false;
        }
        self.storage_interrupted = true;
        self.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
        self.pending_start = None;
        let _ = self.events.notice(sessions::TranscriptRow::info(
            sessions::NoticeScope::Settings,
            "storage write unavailable; retained work and queued inputs remain available",
        ));
        true
    }
}

#[cfg(test)]
impl<'a, P: plakous_host_protocol::ProtocolExtension> NodeActor<'a, P> {
    /// Opens the actor on an executor of its own, for synchronous tests.
    fn open_blocking(
        config: HostConfig<P>,
        actions: &'a crate::surface_protocol::UserActionReceiver<P>,
        events: Sender<P>,
        startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    ) -> CliResult<Self> {
        let executor = std::rc::Rc::new(
            session_executor::SessionExecutor::new()
                .map_err(|_| CliFailure::new("application executor construction failed"))?,
        );
        executor
            .block_on(Self::open(
                config,
                actions,
                events,
                startup,
                std::rc::Rc::clone(&executor),
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
    }

    fn test_executor(&self) -> std::rc::Rc<session_executor::SessionExecutor> {
        std::rc::Rc::clone(&self.executor)
    }

    /// Drives one of the actor's async steps to its end from a synchronous
    /// test, on the executor the actor's own tasks run on.
    fn block<T>(&mut self, step: impl AsyncFnOnce(&mut Self) -> T) -> T {
        let executor = self.test_executor();
        executor
            .block_on(step(self))
            .expect("the host executor drives the step")
    }

    /// Runs a synchronous step that spawns session tasks inside the actor's
    /// executor, as the actor itself would.
    fn in_runtime<T>(&mut self, step: impl FnOnce(&mut Self) -> T) -> T {
        let executor = self.test_executor();
        executor
            .block_on(async { step(self) })
            .expect("the host executor drives the step")
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

/// The fenced-turn abandonments this process's projector applied for
/// `session` since the last call (spec 02 §4.2).
fn take_projected_abandonments(
    pending: &std::sync::Mutex<super::projection::presentation::ProcessUsage>,
    session: plakous::model::SessionId,
) -> Vec<sessions::projector::ProjectedAbandonment> {
    pending
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take_abandonments()
        .into_iter()
        .filter(|abandonment| abandonment.session_id == session)
        .collect()
}

/// How long an interactive release waits for the final projector pass outside quit.
pub(in crate::runtime) const FINAL_PROJECTION_BOUND: Duration = Duration::from_millis(400);

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
#[cfg(test)]
mod tree_points_tests;
