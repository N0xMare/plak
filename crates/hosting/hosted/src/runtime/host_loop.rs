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
use crate::node::NodeActor;
use crate::session_log::{SessionEvent, SessionLog};
use crate::sessions::{
    self, QueueDrainState as SessionQueueDrainState, QueueItem as SessionQueueItem,
};
use crate::{CliFailure, CliResult};

use super::admission::{
    admit_idle_immediate_prompt, commit_session_update, emit_prompt_history, emit_queue_pause_note,
    emit_queue_snapshot, emit_unavailable_context, queue_edit_applied, queue_edit_refused,
    queue_pause, queue_persist_failed, queue_refusal, recovery_blocked_action,
    refuse_recovery_blocked_action, reject_draft, reject_stale_queue_action,
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
            match RootActor::open(
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

/// The attached root's actor.
type RootActor<'a, P> = NodeActor<RootRole<'a, P>>;

/// The root's port: the attachment's surface, its durable session record,
/// queue, interrupt and deferred state, and publication.
struct RootRole<'a, P: plakous_host_protocol::ProtocolExtension> {
    _storage_notices: Option<super::storage::StorageNotices>,
    config: HostConfig<P>,
    actions: &'a crate::surface_protocol::UserActionReceiver<P>,
    events: Sender<P>,
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
    /// The queue item of the idle prompt whose row the host echoed before
    /// running it.
    echoed_prompt: Option<String>,
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
    /// The root's continuation after a lost host is due at the next idle
    /// boundary: consent was given (a Resume, `resume_after_loss =
    /// "continue"`, or a decision a crash left pending).
    root_loss_resume_due: bool,
    /// This attachment quit while driving a root turn, not from idle.
    handoff_root_due: bool,
    /// Since when nothing has kept the session resident, while the host
    /// releases idle sessions ([`super::HostConfig::idle_release`]).
    idle_since: Option<Instant>,
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

mod handoff;
#[cfg(test)]
pub(in crate::runtime) use handoff::hooks as handoff_hooks;
mod idle;
mod loss_resume;
mod open;
mod pending;
mod prompt;
pub(super) mod publication;
mod recover;
mod release;
mod residency;
mod restored;
mod run_until_idle;
mod session_executor;
mod stop;
pub(in crate::runtime) mod transient;
pub(in crate::runtime) mod wake;

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// What the live projector follows now: the attached session's feed.
    fn live_target(&self) -> super::projection::live::LiveTarget {
        let rows = self.role.config.topology_rows();
        let events = self.role.events.clone();
        self.role
            .pending_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target = Some(self.opened.locator().session_id());
        super::projection::live::LiveTarget {
            sessions: self.opened.shared_sessions(),
            store: self.role.config.store.clone(),
            roots: self.role.config.state_roots.clone(),
            product_id: self.role.config.session.id.clone(),
            followed: self.opened.locator(),
            tree_id: durable_tree_id(&rows, &self.role.config.session),
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
        if self.role.config.store.storage_unavailable() {
            self.role.storage_interrupted |= self.role.config.store.storage_wait_cancelled();
            if self.role.storage_interrupted {
                return;
            }
        }
        let target = self.live_target();
        // A stopped task (it panicked or was aborted) is replaced: the new
        // one loads the views again and reconciles the settled rows once.
        if self
            .role
            .live
            .as_ref()
            .is_some_and(super::projection::live::LiveProjector::stopped)
        {
            self.role.live = None;
        }
        if self.role.live.is_none() {
            self.role.live = Some(super::projection::live::LiveProjector::spawn(
                self.role.executor.tasks(),
            ));
        }
        if self.role.config.session.store_revision != 0
            && let Some(live) = self.role.live.as_mut()
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
        if self.policy.tree_passes && self.role.config.session.store_revision != 0 {
            let due = std::mem::take(&mut self.role.idle_entry_pass_due);
            let passed = if due {
                target.tree_pass().await
            } else {
                target.tree_housekeeping().await
            };
            if let Err(error) = passed {
                // A refused idle-entry pass stays due: the next tick retries it.
                self.role.idle_entry_pass_due = due;
                self.log_projector_debt(&error);
            }
        }
        if self.role.config.session.queue_commands.has_pending()
            || self.role.config.session.queue_projection_debt
        {
            let joined = self
                .role
                .config
                .store
                .load_queue_commands(&self.role.config.session.id);
            if let Ok(commands) = joined {
                let changed = self.role.config.session.queue_commands != commands;
                self.role.config.session.queue_commands = commands;
                self.role.config.session.queue_projection_debt =
                    self.role.config.session.queue_projection_debt
                        && self.role.config.session.queue_commands.has_pending();
                self.role
                    .config
                    .session
                    .pending_queue
                    .retain(|item| !self.role.config.session.queue_commands.is_consumed(item));
                if changed {
                    emit_queue_snapshot(&self.role.events, &self.role.config.session);
                }
                if self.role.pending_start.is_none()
                    && !self.role.recovery.blocked()
                    && self.role.config.session.queue_drain_state
                        == SessionQueueDrainState::LiveAutoDrain
                {
                    self.role.pending_start = queue::next_drainable(&self.role.config.session);
                }
            }
        }
        self.publish_projected_usage();
    }

    /// A projector pass to the tail that the caller waits for: a reopen's
    /// step 2, whose rows the next step reads.
    async fn project_now(&mut self) {
        if self.role.config.session.store_revision == 0 {
            return;
        }
        let target = self.live_target();
        let passed = async {
            target.pass().await?;
            target.tree_pass().await
        }
        .await;
        if passed.is_ok() {
            self.role.config.session.queue_projection_debt = false;
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
        let live = self.role.live.as_mut();
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
        let Ok(intents) = self
            .role
            .config
            .store
            .load_intents(&self.role.config.session.id)
        else {
            return;
        };
        let Ok(stored) = self
            .role
            .config
            .store
            .load_queue_head(&self.role.config.session.id)
        else {
            return;
        };
        let unsaved = self.role.config.session.transcript.pending_notice_items();
        let mut transcript = sessions::ProductTranscript::stored(views.transcript(&intents));
        transcript.restore_pending_notices(unsaved, &intents);
        self.role.config.session.transcript = transcript;
        self.role.config.session.root_turn_count = views.state.counters.root_turns;
        self.role.config.session.tool_call_count = views.state.counters.tool_calls;
        self.role.config.session.compaction_count = views.state.counters.compactions;
        self.role.config.session.plakous_locator = views.projected_locator(stored.recorded_locator);
        self.role.config.session.queue_commands =
            sessions::QueueCommands::from_join(&intents, Some(&views));
        self.role.config.session.pending_queue = stored.pending_queue;
        self.role
            .config
            .session
            .pending_queue
            .retain(|item| !self.role.config.session.queue_commands.is_consumed(item));
        self.role.config.session.queue_projection_debt =
            self.role.config.session.queue_projection_debt
                && self.role.config.session.queue_commands.has_pending();
        let snapshot = super::logging::project_session_snapshot(&self.role.config.session);
        let _ = self.role.events.publish_transcript(snapshot);
    }

    fn publish_projected_usage(&self) {
        for terminal in
            take_pending_usage(&self.role.pending_usage, self.opened.locator().session_id())
        {
            super::usage::emit_projected_turn_usage(
                &self.role.events,
                &self.role.config.session.id,
                terminal.turn_id,
                terminal.usage,
                self.role.log.as_ref(),
                &self.role.session_ledger,
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
        self.role.projection_finished = true;
        if self.role.config.session.store_revision == 0 {
            return Ok(());
        }
        let target = self.live_target();
        let deadline = self.projection_deadline(one_shot);
        let live = self.role.live.take();
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
            self.role.log.as_ref(),
            &self.role.config,
            SessionEvent::new("projector.deferred").str("error", error),
        );
    }

    fn commit_notice_intents(&mut self) {
        // A new construction (this session, a fork, a spawn) queued what its
        // skills discovery left out; each distinct notice is shown once per
        // root session.
        let notices = self.role.config.state_roots.take_skill_notices();
        for notice in self
            .role
            .shown_skill_notices
            .fresh(&self.role.config.session.id, notices)
        {
            let _ = self.role.events.notice(sessions::TranscriptRow::info(
                sessions::NoticeScope::Session,
                notice,
            ));
        }
        if self.role.storage_interrupted && self.role.config.store.storage_unavailable() {
            return;
        }
        super::events::commit_notice_intents(
            &self.role.events,
            &*self.role.config.store,
            &self.role.config.home,
            self.role.log.as_ref(),
            &mut self.role.config.session,
        );
    }

    fn update_background_process_count(&mut self) {
        for failure in self
            .role
            .config
            .state_roots
            .job_registry()
            .take_user_stop_failures()
        {
            let _ = self.role.events.notice(sessions::TranscriptRow::error(
                sessions::NoticeScope::Session,
                failure,
            ));
        }
        // Counting prunes the registry's services that are gone, as each pass
        // always has; the published count is the rows' own.
        let _ = self.role.config.state_roots.job_registry().running_count();
        for row in self.role.job_events.emit(
            self.role.config.state_roots.job_registry(),
            &self.role.events,
        ) {
            // Live only: the projector derives the stored process row.
            self.role.config.session.append_transcript(row.clone());
            let _ = self.role.events.send(ContentEvent::Row(row));
        }
        self.role
            .events
            .publish_processes(self.role.config.state_roots.job_registry().snapshot());
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
            self.role
                .config
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
            crate::intent_dispatch::take_next(&self.role.interrupt_state, || {
                self.role.actions.try_recv()
            })
            .ok()
        } else {
            None
        };
        if matches!(self.drive_restored_turn().await?, HostStep::Quit) {
            return self.finish_projection(one_shot.is_some()).await;
        }
        if let Some(delivery) = initial_prompt {
            self.role
                .interrupt_state
                .borrow_mut()
                .restore_front(delivery);
        }
        self.main_loop(&mut RootLoop {
            one_shot: one_shot.as_deref_mut(),
            passive_checkpoint_scheduled: false,
        })
        .await?;
        self.commit_notice_intents();
        self.finish_projection(one_shot.is_some()).await
    }

    fn retain_interrupted_storage(&mut self, error: &CliFailure) -> bool {
        if error.store_refusal() != Some(crate::error::StoreRefusal::StorageFull) {
            return false;
        }
        self.role.storage_interrupted = true;
        self.role.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
        self.role.pending_start = None;
        let _ = self.role.events.notice(sessions::TranscriptRow::info(
            sessions::NoticeScope::Settings,
            "storage write unavailable; retained work and queued inputs remain available",
        ));
        true
    }
}

/// What the root's main loop borrows from its caller: the one-shot run it
/// drives, if any, and whether its passive checkpoint is scheduled.
struct RootLoop<'o> {
    one_shot: Option<&'o mut super::oneshot::OneShotState>,
    passive_checkpoint_scheduled: bool,
}

/// An operator prompt the root's idle step admitted.
struct RootPrompt {
    text: String,
    queued_delivery: Option<SessionQueueItem>,
    user_echoed: bool,
}

impl<'a, 'o, P: plakous_host_protocol::ProtocolExtension> crate::node::Role<RootLoop<'o>>
    for RootRole<'a, P>
{
    type Input = RootPrompt;
    /// `Ok` once the loop leaves for the root's release.
    type Output = CliResult<()>;

    async fn next(
        actor: &mut RootActor<'a, P>,
        cx: &mut RootLoop<'o>,
    ) -> crate::node::Next<RootPrompt, CliResult<()>> {
        if actor.role.config.store.storage_unavailable() {
            actor.role.storage_interrupted |= actor.role.config.store.storage_wait_cancelled();
        } else {
            actor.role.storage_interrupted = false;
        }
        actor.commit_notice_intents();
        actor.project().await;
        if !cx.passive_checkpoint_scheduled && actor.role.actions.is_promptable() {
            actor
                .opened
                .schedule_passive_checkpoint(actor.role.executor.tasks());
            cx.passive_checkpoint_scheduled = true;
        }
        if let Some(state) = cx.one_shot.as_deref_mut() {
            if state.attempted && state.run.is_none() {
                return crate::node::Next::End(Ok(()));
            }
        }
        let next = if let Some(state) = cx.one_shot.as_deref_mut()
            && state.attempted
        {
            actor.next_run_prompt(state).await
        } else {
            actor.next_prompt(cx.one_shot.as_deref_mut()).await
        };
        let next = match next {
            Ok(next) => next,
            Err(error) if actor.retain_interrupted_storage(&error) => {
                return crate::node::Next::Again;
            }
            Err(error) => return crate::node::Next::End(Err(error)),
        };
        match next {
            HostStep::Continue => crate::node::Next::Again,
            HostStep::Quit => crate::node::Next::End(Ok(())),
            HostStep::Prompt {
                text,
                queued_delivery,
                user_echoed,
            } => crate::node::Next::Input(RootPrompt {
                text,
                queued_delivery,
                user_echoed,
            }),
        }
    }

    async fn serve(
        actor: &mut RootActor<'a, P>,
        cx: &mut RootLoop<'o>,
        prompt: RootPrompt,
    ) -> crate::node::Served<CliResult<()>> {
        let RootPrompt {
            text: raw_text,
            queued_delivery,
            user_echoed,
        } = prompt;
        let before = actor.role.config.session.root_turn_count;
        let step = match actor
            .run_prompt(
                raw_text,
                queued_delivery,
                user_echoed,
                cx.one_shot.as_deref_mut(),
            )
            .await
        {
            Ok(step) => step,
            Err(error) if actor.retain_interrupted_storage(&error) => {
                return crate::node::Served::Continue;
            }
            Err(error) => return crate::node::Served::End(Err(error)),
        };
        if let Some(run) = cx
            .one_shot
            .as_deref_mut()
            .and_then(|state| state.run.as_mut())
        {
            run.turns = run
                .turns
                .saturating_add(u8::from(actor.role.config.session.root_turn_count > before));
        }
        if matches!(step, HostStep::Quit) {
            return crate::node::Served::End(Ok(()));
        }
        crate::node::Served::Continue
    }
}

#[cfg(test)]
impl<'a, P: plakous_host_protocol::ProtocolExtension> RootActor<'a, P> {
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
        std::rc::Rc::clone(&self.role.executor)
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
pub(in crate::runtime) mod loss_policy_tests;
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
#[cfg(test)]
pub(in crate::runtime) mod transcript_keys_tests;
#[cfg(test)]
mod tree_points_tests;
