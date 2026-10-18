//! One active turn: control authority, observed projection, and authoritative result.

use super::usage::{LiveOccupancy, emit_live_context_projection};
use super::{
    Agent, AgentError, AgentErrorKind, AgentEventKind, CancelRefusal, CancelSettlement,
    CancellationStatus, CommandAckDisposition, CommandId, DriveTurnError, DrivenTurn, Duration,
    HOST_CANCEL_SETTLE_TIMEOUT, HeldUsage, HostEvent, Instant, InterruptWatchState,
    NativeSubscriptions, RefCell, Sender, SessionHome, SessionLedger, SessionLog,
    SessionQueueDrainState, SessionQueueItem, SessionRecord, SessionRecordStore,
    SubscriptionUsageRefresh, Surface, ToolSubjectStash, TopologyRows, TreeSnapshotPush,
    TryRecvError, TurnControl, TurnEventError, TurnHostOutcome, TurnProjection, TurnResult,
    TurnResultOutcome, UserAction, accept_draft, admitted_turn_lifecycle, apply_cancel_settlement,
    apply_event, commit_session_update, drop_last_mutable, edit_mutable, emit_prompt_history,
    emit_queue_snapshot, emit_tree_snapshot_if_revision_advanced, emit_turn_lifecycle,
    flush_assistant_delta, flush_deferred_interrupted_status, move_mutable, note_idle_only,
    observe_cancelled_tool_activity, observe_pending_cancel_settlement,
    observe_terminal_tool_activity, operator_facing_agent_error, queue, queue_edit_applied,
    queue_edit_refused, queue_refusal, reconcile_authoritative_assistant,
    record_emitted_tree_revision, reject_draft, reject_stale_queue_action, remove_mutable,
    request_subscription_usage_service, resolve_approval_intent, sessions, settle_cancel,
    settle_late_cancel, settle_live_thinking, surface, terminal_turn_outcome,
    turn_finalization_ready, turn_lifecycle, unresolved_turn_lifecycle,
};
#[cfg(any(test, feature = "test-support"))]
use super::{
    COMPLETION_WINS_QUIT_PROMPT, TestHostBoundary, TestHostInjection, TestHostWorkflow,
    observe_test_boundary,
};

pub(super) struct TurnHost<'a, P: plakous_host_protocol::ProtocolExtension> {
    pub(super) job_agent: &'a Agent,
    pub(super) quit_started_at: &'a mut Option<Instant>,
    pub(super) quit_abandoned_turn: &'a mut bool,
    pub(super) quit_job_stops: &'a mut Option<tokio::task::JoinHandle<Result<(), String>>>,
    pub(super) actions: &'a crate::surface_protocol::UserActionReceiver<P>,
    pub(super) deferred: &'a RefCell<InterruptWatchState<P>>,
    pub(super) events: &'a Sender<P>,
    pub(super) rows: &'a (dyn TopologyRows + Sync),
    pub(super) roots: &'a crate::agent::ProductStateRoots,
    pub(super) store: &'a dyn SessionRecordStore,
    pub(super) projection_store: &'a std::sync::Arc<dyn SessionRecordStore>,
    pub(super) live_projector: Option<&'a mut super::projection::live::LiveProjector>,
    pub(super) live_target: super::projection::live::LiveTarget,
    pub(super) home: &'a SessionHome,
    pub(super) session: &'a mut SessionRecord,
    pub(super) context_projection: crate::context::ContextProjection,
    pub(super) enrolled_source_bytes: u64,
    pub(super) root_template: &'a mut Option<crate::binding::RootTemplateFact>,
    pub(super) attached_one_shot: bool,
    pub(super) ready_catalog: &'a [plakous_host_protocol::views::ProviderSection],
    pub(super) surface: &'a Surface<P>,
    pub(super) log: Option<&'a SessionLog>,
    pub(super) subscription_service: Option<NativeSubscriptions>,
    pub(super) subscription_usage_refresh: &'a SubscriptionUsageRefresh<P>,
    pub(super) held_usage: &'a HeldUsage,
    pub(super) ledger: &'a SessionLedger,
    pub(super) last_tree_revision: &'a mut TreeSnapshotPush,
    pub(super) job_events: &'a mut super::job_events::JobEvents,
    #[cfg(any(test, feature = "test-support"))]
    pub(super) test_injection: Option<&'a TestHostInjection>,
}

impl<P: plakous_host_protocol::ProtocolExtension> TurnHost<'_, P> {
    fn begin_quit_job_stops(&mut self) {
        if self.quit_job_stops.is_some() {
            return;
        }
        self.quit_started_at.get_or_insert_with(Instant::now);
        self.job_agent.begin_job_stop();
        let tree_id = super::topology_state::durable_tree_id(self.rows, self.session);
        let root_owner = self.job_agent.id().to_string();
        let registry = self.roots.job_registry().clone();
        *self.quit_job_stops = Some(tokio::spawn(async move {
            let agents = crate::topology_runtime::abandon_live_agents(&tree_id).await;
            for agent in &agents {
                agent.begin_job_stop();
            }
            let mut owners = agents
                .iter()
                .map(|agent| agent.id().to_string())
                .collect::<Vec<_>>();
            owners.push(root_owner);
            registry
                .stop_all_except(Duration::from_millis(200), &owners)
                .await
        }));
    }
}

struct TurnDrive<'a, P: plakous_host_protocol::ProtocolExtension> {
    host: TurnHost<'a, P>,
    control: TurnControl,
    assistant: String,
    delta_buf: String,
    last_flush: Instant,
    tool_subjects: ToolSubjectStash,
    projection: TurnProjection<P>,
    quit_after_terminal: bool,
    /// After Quit, wait this long for a cancelled terminal, then abandon.
    quit_join_deadline: Option<Instant>,
    cancel_unsettled: Option<String>,
    cancel_pending: bool,
    cancel_settle_deadline: Option<Instant>,
    interrupted_status_pending: bool,
    abandoned_by_quit: bool,
    events_done: bool,
    turn_result: Option<Result<TurnResult, AgentError>>,
    event_failure: Option<String>,
    compacted_mid_turn: bool,
    /// Largest single-operation prepared input this turn observed.
    peak_prepared_input_tokens: u64,
    /// Presentation-only context measure advanced since the last canonical
    /// context query.
    live_occupancy: LiveOccupancy,
    last_context_refresh_bytes: u64,
    request_tool_names: Vec<String>,
    queue_refresh_at: Instant,
    queue_refresh: Option<queue_projection::QueueRefresh>,
    queue_wait_noted: bool,
}

mod admission;
pub(super) use admission::drive_one_turn;
#[cfg(test)]
pub(super) use admission::duplicate_consumed;
#[cfg(test)]
pub(in crate::runtime) use admission::{stub_ingress_backpressure, stub_start_refusal};
mod control;
mod delivery;
mod finish;
mod queue_projection;

impl<'a, P: plakous_host_protocol::ProtocolExtension> TurnDrive<'a, P> {
    fn new(control: TurnControl, host: TurnHost<'a, P>) -> Self {
        let live_occupancy = LiveOccupancy::from_projection(&host.context_projection);
        Self {
            host,
            control,
            assistant: String::new(),
            delta_buf: String::new(),
            last_flush: Instant::now(),
            tool_subjects: ToolSubjectStash::new(),
            projection: TurnProjection::default(),
            quit_after_terminal: false,
            quit_join_deadline: None,
            cancel_unsettled: None,
            cancel_pending: false,
            cancel_settle_deadline: None,
            interrupted_status_pending: false,
            abandoned_by_quit: false,
            events_done: false,
            turn_result: None,
            event_failure: None,
            compacted_mid_turn: false,
            peak_prepared_input_tokens: 0,
            live_occupancy,
            last_context_refresh_bytes: 0,
            request_tool_names: Vec::new(),
            queue_refresh_at: Instant::now(),
            queue_refresh: None,
            queue_wait_noted: false,
        }
    }

    async fn run(
        mut self,
        mut turn: plakous::agent::Turn,
        #[cfg(any(test, feature = "test-support"))] prompt: String,
    ) -> Result<DrivenTurn, DriveTurnError> {
        let mut event_stream = turn
            .take_events()
            .map_err(|e| format!("events unavailable: {e}"))?;
        let result_fut = turn.result();
        tokio::pin!(result_fut);

        loop {
            self.poll_queue_projection().await;
            super::events::commit_notice_intents(
                self.host.events,
                self.host.store,
                self.host.home,
                self.host.log,
                self.host.session,
            );
            for row in self
                .host
                .job_events
                .emit(self.host.roots.job_registry(), self.host.events)
            {
                // Live only: the projector derives the stored process row.
                self.host.session.append_transcript(row.clone());
                let _ = self.host.events.send(HostEvent::Row(row));
            }
            self.cancel_pending = observe_pending_cancel_settlement(
                &self.control,
                self.cancel_pending,
                &mut self.cancel_unsettled,
                &mut self.cancel_settle_deadline,
                self.host.rows,
                self.host.store,
                self.host.home,
                self.host.session,
                self.host.events,
                &mut self.projection,
                &mut self.interrupted_status_pending,
            )
            .await;
            if self
                .poll_actions(
                    #[cfg(any(test, feature = "test-support"))]
                    &prompt,
                )
                .await
            {
                continue;
            }

            if self.abandoned_by_quit
                || self
                    .quit_join_deadline
                    .is_some_and(|deadline| Instant::now() >= deadline)
            {
                self.abandoned_by_quit = true;
                break;
            }
            if turn_finalization_ready(self.turn_result.is_some(), self.events_done) {
                break;
            }

            tokio::select! {
                biased;
                res = &mut result_fut, if self.turn_result.is_none() => {
                    self.turn_result = Some(res);
                    #[cfg(any(test, feature = "test-support"))]
                    if prompt == COMPLETION_WINS_QUIT_PROMPT {
                        observe_test_boundary(
                            self.host.test_injection,
                            TestHostWorkflow::SendNow,
                            TestHostBoundary::RuntimeReturned,
                        );
                    }
                }
                () = tokio::time::sleep(
                    self.quit_join_deadline
                        .map(|deadline| deadline.saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::MAX),
                ), if self.quit_join_deadline.is_some() => {
                    self.abandoned_by_quit = true;
                    break;
                }
                ev = event_stream.recv(), if !self.events_done => {
                    match ev {
                        Ok(event) => {
                            let refresh_at_boundary = match event.kind() {
                                AgentEventKind::RequestPrepared { input_parts, occupied_tokens, prepared_input_upper_bound, tool_names } => {
                                    self.host.context_projection = self.host.context_projection.with_prepared_request(*input_parts, *occupied_tokens, *prepared_input_upper_bound);
                                    self.live_occupancy = self.live_occupancy.next_request(&self.host.context_projection);
                                    self.request_tool_names = tool_names.iter().map(|name| name.as_str().to_owned()).collect();
                                    self.compacted_mid_turn = false;
                                    true
                                }
                                AgentEventKind::ModelStarted => {
                                    self.live_occupancy.round_started();
                                    self.last_context_refresh_bytes = 0;
                                    true
                                }
                                AgentEventKind::ModelDispatchStarted => {
                                    let _ = self.host.events.send(HostEvent::ModelRequestStarted);
                                    false
                                }
                                AgentEventKind::Model { event: Some(model_event), .. } => {
                                    match model_event.as_ref() {
                                        plakous::model::ModelEvent::TextDelta(delta) => {
                                            self.live_occupancy.appended(u64::try_from(delta.bytes().len()).unwrap_or(u64::MAX));
                                            let pending = self.live_occupancy.pending_round_bytes();
                                            if pending.saturating_sub(self.last_context_refresh_bytes) >= 2048 {
                                                self.last_context_refresh_bytes = pending;
                                                true
                                            } else {
                                                false
                                            }
                                        }
                                        plakous::model::ModelEvent::Usage(update) => {
                                            self.live_occupancy.provider_input(update.usage().input_tokens());
                                            true
                                        }
                                        // A discarded response's text never reaches the
                                        // context; only this call's text is pending here.
                                        plakous::model::ModelEvent::Retry(retry) if retry.output_discarded() => {
                                            self.live_occupancy.discard_round_output();
                                            self.last_context_refresh_bytes = 0;
                                            true
                                        }
                                        _ => false,
                                    }
                                }
                                AgentEventKind::ToolCompleted { observation: Some(observation), .. } => {
                                    self.live_occupancy.appended(u64::try_from(observation.preview().len()).unwrap_or(u64::MAX));
                                    false
                                }
                                AgentEventKind::CompactionFinished { outcome: plakous::agent::CompactionEventOutcome::Committed, summary: Some(summary), .. } => {
                                    self.last_context_refresh_bytes = 0;
                                    self.live_occupancy.compacted(summary.occupancy_tokens_after, summary.input_tokens_after);
                                    true
                                }
                                _ => false,
                            };
                            let terminal = matches!(
                                event.kind(),
                                AgentEventKind::TurnCompleted { .. }
                                    | AgentEventKind::TurnFailed { .. }
                            );
                            let observed_session_id = self.host.session.id.clone();
                            apply_event(
                                &event,
                                self.host.roots,
                                self.host.rows,
                                self.host.store,
                                self.host.home,
                                self.host.session,
                                self.host.events,
                                &mut self.assistant,
                                &mut self.delta_buf,
                                &mut self.last_flush,
                                &mut self.tool_subjects,
                                &mut self.projection,
                                self.host.log,
                                &observed_session_id,
                                self.host.last_tree_revision,
                                &mut self.compacted_mid_turn,
                                &mut self.peak_prepared_input_tokens,
                            );
                            if refresh_at_boundary || matches!(
                                event.kind(),
                                AgentEventKind::Model {
                                    event: Some(model_event),
                                    ..
                                } if matches!(model_event.as_ref(), plakous::model::ModelEvent::Usage(_))
                            ) {
                                emit_live_context_projection(
                                    self.host.events,
                                    &self.host.context_projection,
                                    self.host.session.context_counters(),
                                    self.host.enrolled_source_bytes,
                                    self.compacted_mid_turn,
                                    self.live_occupancy.tokens(),
                                    self.live_occupancy.anchored(),
                                    &self.request_tool_names,
                                );
                            }
                            // No push here: the canvas is fed from the coalesce
                            // tick below and from tool lifecycle terminals inside
                            // `apply_event`. Pushing per streamed event made every
                            // `TextDelta` pay a tree lookup for a revision that a
                            // text delta cannot advance.
                            // A tool row that just terminalised may be the last
                            // one a settled cancel is waiting behind.
                            flush_deferred_interrupted_status(
                                &mut self.interrupted_status_pending,
                                self.host.session,
                                self.host.events,
                                &mut self.projection,
                            );
                            self.events_done = terminal;
                        }
                        Err(TurnEventError::TerminalFailure { error }) => {
                            flush_assistant_delta(&mut self.delta_buf, &mut self.last_flush, self.host.events, true);
                            self.event_failure = Some(operator_facing_agent_error(
                                "turn failed",
                                &error,
                            ));
                            self.events_done = true;
                        }
                        Err(TurnEventError::Closed) => {
                            self.events_done = true;
                        }
                        Err(err) => {
                            flush_assistant_delta(&mut self.delta_buf, &mut self.last_flush, self.host.events, true);
                            // A lagged or failed observation stream is exactly the
                            // event gap requires Compose counts to be
                            // labelled by: they are no longer totals.
                            self.projection.gap_observed = true;
                            self.event_failure = Some(format!("event stream: {err}"));
                            self.events_done = true;
                        }
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(20)) => {
                    // Coalesce window: flush buffered text deltas if due, and take
                    // the one tree observation of this window. The emitter drops
                    // every non-advancing revision, so an idle tree costs a lookup
                    // and nothing leaves.
                    flush_assistant_delta(&mut self.delta_buf, &mut self.last_flush, self.host.events, false);
                    emit_tree_snapshot_if_revision_advanced(
                        self.host.rows,
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        self.host.last_tree_revision,
                        self.host.events,
                        false,
                    );
                }
            }
        }

        if let Some(refresh) = self.queue_refresh.take() {
            refresh.task.abort();
        }
        self.finish().await
    }
}
