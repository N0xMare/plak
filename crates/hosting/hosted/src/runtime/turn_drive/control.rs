//! Busy-turn control intents and their acknowledgements.

use super::HostBoundary;
use super::{
    CancelRefusal, CancelSettlement, CancellationStatus, HOST_CANCEL_SETTLE_TIMEOUT, Instant,
    RootTurn, SessionQueueDrainState, TryRecvError, UserAction, accept_draft,
    apply_cancel_settlement, commit_session_update, drop_last_mutable, edit_mutable,
    emit_prompt_history, emit_queue_snapshot, flush_assistant_delta, move_mutable, note_idle_only,
    queue, queue_edit_applied, queue_edit_refused, queue_refusal, record_emitted_tree_revision,
    reject_draft, reject_stale_queue_action, remove_mutable, request_subscription_usage_service,
    resolve_approval_intent, settle_cancel, surface,
};
use crate::intent_dispatch::{DispatchPhase, Disposition, IntentClass, take_next};
use crate::runtime::admission::{
    QueuePersistAction, report_queue_persist_failed as queue_persist_failed,
};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{
    CancelUnsettled, ControlFailure, DiagnosticText, HostPhase, HostStatusDetail, SteerSource,
};
use std::time::Duration;

const QUIT_CANCEL_GRACE: Duration = Duration::from_millis(300);
const QUIT_RESULT_JOIN: Duration = Duration::from_millis(100);

impl<P: plakous_host_protocol::ProtocolExtension> RootTurn<'_, P> {
    async fn request_cancel(&self) -> Result<CancellationStatus, plakous::agent::ControlError> {
        #[cfg(test)]
        if let Some(error) = super::quit_trace::take_cancel_refusal(&self.host.session.id) {
            return Err(error);
        }
        self.control.request_cancel().await
    }

    /// Whether [`Self::poll_actions`] takes an action: after a quit it no
    /// longer reads the mailbox, so readiness must not wake the loop.
    pub(super) const fn takes_actions(&self) -> bool {
        !self.quit_after_terminal
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The running-turn owner serializes queue edits, steering, cancellation, navigation, usage, and shutdown intents against one admission."
    )]
    pub(super) async fn poll_actions(&mut self) -> bool {
        crate::call_detail::note_attached(&self.host.session.id);
        self.host
            .last_tree_revision
            .child_live
            .retain_session(&self.host.session.id);
        for failure in self.host.roots.job_registry().take_user_stop_failures() {
            let _ = self
                .host
                .events
                .notice(TranscriptRow::error(NoticeScope::Session, failure));
        }
        self.host
            .actions
            .set_processes(self.host.roots.job_registry().snapshot());
        // Poll control-plane actions (non-blocking). Actions a long host
        // operation drained but did not handle come first: they arrived before
        // anything still in the mailbox, and the FIFO `interrupt.rs` documents
        // has to hold across the turn boundary too. Without this a deferred
        // `Quit` (or an earlier `FollowUp`) is invisible for the whole turn.
        if self.takes_actions() {
            let mut acknowledgement = None;
            let mut command_reply = false;
            let received =
                take_next(self.host.deferred, || self.host.actions.try_recv()).map(|delivery| {
                    if let Some(started) = delivery.started() {
                        command_reply = delivery.correlated();
                        let _ = self.host.events.send(started);
                    }
                    let (action, guard) = delivery.into_parts();
                    acknowledgement = Some(guard);
                    action
                });
            let received = match received {
                Ok(UserAction::StopProcesses { id, interrupt }) => {
                    if let Err(error) = self.host.roots.job_registry().stop_user(id.as_deref()) {
                        let _ = self
                            .host
                            .events
                            .notice(TranscriptRow::error(NoticeScope::Session, error));
                    }
                    self.host
                        .actions
                        .set_processes(self.host.roots.job_registry().snapshot());
                    if interrupt {
                        Ok(UserAction::Interrupt)
                    } else {
                        Err(TryRecvError::Empty)
                    }
                }
                Ok(action) => match IntentClass::of(&action).disposition(DispatchPhase::Busy) {
                    Disposition::RefuseWhileBusy { noun } => {
                        note_idle_only(self.host.events, noun);
                        Err(TryRecvError::Empty)
                    }
                    _ => Ok(action),
                },
                other => other,
            };
            match received {
                Ok(UserAction::StopProcesses { .. }) => {
                    unreachable!("process intents handled above")
                }
                // The table refused these above; were one reclassified, a
                // running turn still has no handler for it and refuses it.
                Ok(
                    action @ (UserAction::NewSession
                    | UserAction::Compact
                    | UserAction::ResumeSession { .. }
                    | UserAction::ResumeSessionHandoff { .. }
                    | UserAction::ForkSession { .. }
                    | UserAction::InvokeTool { .. }
                    | UserAction::RetryAttach
                    | UserAction::Recover),
                ) => note_idle_only(
                    self.host.events,
                    IntentClass::of(&action)
                        .idle_only_noun()
                        .unwrap_or("this action"),
                ),
                Ok(UserAction::Quit) | Err(TryRecvError::Disconnected) => {
                    self.host.quit_started_at.get_or_insert_with(Instant::now);
                    flush_assistant_delta(
                        &mut self.delta_buf,
                        &mut self.last_flush,
                        self.host.events,
                        true,
                    );
                    // Spec 02 §5.3 ladder step 3: quit escalates a stuck cancel.
                    // The quit intent is honoured even when the runtime never
                    // joins; the turn is abandoned without a synthesised
                    // terminal rather than the host hanging on one. A cancel
                    // the runtime already accepted (a busy quit sends
                    // Interrupt first) is the request: quit only bounds its
                    // settlement and never issues a second cancel command,
                    // whose durable acceptance would race the abandonment. A
                    // refused interrupt accepted nothing, so quit requests.
                    let settlement = if self.turn_result.is_some() {
                        #[cfg(test)]
                        super::quit_trace::step(&self.host.session.id, "terminal");
                        CancelSettlement::Settled
                    } else if self.cancel_accepted {
                        #[cfg(test)]
                        super::quit_trace::step(&self.host.session.id, "bounded");
                        let settled = async {
                            self.control.wait_until_settled().await;
                            Ok::<(), plakous::agent::ControlError>(())
                        };
                        settle_cancel(settled, QUIT_CANCEL_GRACE).await
                    } else {
                        #[cfg(test)]
                        super::quit_trace::step(&self.host.session.id, "requested");
                        settle_cancel(self.control.cancel(), QUIT_CANCEL_GRACE).await
                    };
                    self.cancel_unsettled = None;
                    if let CancelSettlement::Unsettled(detail) = &settlement {
                        self.cancel_unsettled = Some(detail.clone());
                        let _ = self.host.events.notice(TranscriptRow::error(
                            NoticeScope::Host,
                            format!("quit escalated a cancel that did not settle: {detail}"),
                        ));
                        // Persist the escalation notice here, before the
                        // queue write, rather than at whichever loop pass the
                        // 100 ms join leaves: the product commit sequence of a
                        // quit must not depend on whether an event arrives.
                        #[cfg(test)]
                        let revision = self.host.session.store_revision;
                        crate::runtime::events::commit_notice_intents(
                            self.host.events,
                            self.host.store,
                            self.host.home,
                            self.host.log,
                            self.host.session,
                        );
                        #[cfg(test)]
                        if self.host.session.store_revision != revision {
                            super::quit_trace::step(&self.host.session.id, "notice committed");
                        }
                    }
                    if self.host.session.store_revision != 0 {
                        let _ = commit_session_update(
                            self.host.store,
                            self.host.home,
                            self.host.session,
                            |candidate| {
                                candidate.queue_drain_state =
                                    SessionQueueDrainState::RestoredPaused;
                                true
                            },
                        );
                        #[cfg(test)]
                        super::quit_trace::step(&self.host.session.id, "queue paused");
                    }
                    self.quit_after_terminal = true;
                    self.quit_join_deadline = Some(Instant::now() + QUIT_RESULT_JOIN);
                    if let Some(observer) = &self.turn_observer {
                        observer.observe(HostBoundary::ProductPublished);
                    }
                }
                Ok(action @ (UserAction::FollowUp(_) | UserAction::Prompt(_))) => {
                    let draft = match &action {
                        UserAction::FollowUp(draft) | UserAction::Prompt(draft) => draft.clone(),
                        _ => unreachable!(),
                    };
                    let mut refusal = None;
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| match candidate.enqueue_live(draft) {
                            Ok(item) => {
                                candidate.record_prompt(item.text);
                                true
                            }
                            Err(error) => {
                                refusal = Some(error);
                                false
                            }
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(self.host.events, self.host.session);
                            emit_prompt_history(self.host.events, self.host.session);
                            accept_draft(self.host.events, action);
                            if let Some(item) = self.host.session.pending_queue.last() {
                                #[cfg(test)]
                                super::wake_trace::took(&item.text, self.last_wake);
                                let _ = self
                                    .host
                                    .events
                                    .send(ContentEvent::PromptAdmitted(item.protocol(false)));
                            }
                            let _ = self.host.events.send(ContentEvent::Status {
                                phase: HostPhase::Running,
                                queue_len: self.host.session.pending_queue.len(),
                                detail: Some(HostStatusDetail::QueuedFollowUp),
                            });
                        }
                        Ok(false) => {
                            queue_refusal(
                                self.host.events,
                                "queue admission",
                                refusal.unwrap_or(queue::Refusal::Stale),
                            );
                            reject_draft(self.host.events, action);
                        }
                        Err(error) => {
                            queue_persist_failed(
                                self.host.events,
                                self.host.home,
                                self.host.session,
                                QueuePersistAction::Admission,
                                &error,
                            );
                            reject_draft(self.host.events, action);
                        }
                    }
                }
                Ok(action @ UserAction::Steer(_)) => {
                    let (text, images) = match &action {
                        UserAction::Steer(draft) => (draft.text.trim().to_owned(), &draft.images),
                        _ => unreachable!(),
                    };
                    let input = queue::user_input(&text, images);
                    if text.is_empty() && images.is_empty() {
                        reject_draft(self.host.events, action);
                    } else {
                        match self.control.steer(input).await {
                            Ok(_) => {
                                accept_draft(self.host.events, action);
                                let _ = self.host.events.send(ContentEvent::Status {
                                    phase: HostPhase::Running,
                                    queue_len: self.host.session.pending_queue.len(),
                                    detail: Some(HostStatusDetail::Steering {
                                        source: SteerSource::Direct,
                                    }),
                                });
                            }
                            Err(err) => {
                                reject_draft(self.host.events, action);
                                let _ = self.host.events.send(ContentEvent::Status {
                                    phase: HostPhase::Running,
                                    queue_len: self.host.session.pending_queue.len(),
                                    detail: Some(HostStatusDetail::SteerRejected {
                                        cause: ControlFailure {
                                            diagnostic: DiagnosticText::new(err.to_string()),
                                        },
                                    }),
                                });
                            }
                        }
                    }
                }
                Ok(UserAction::Background) => {
                    if let Err(error) = self.control.background().await {
                        let _ = self
                            .host
                            .events
                            .notice(TranscriptRow::error(NoticeScope::Host, error.to_string()));
                    }
                }
                Ok(UserAction::SendQueuedNow { id, revision }) => {
                    if self.send_queued_now(id, revision).await {
                        return true;
                    }
                }
                Ok(UserAction::Interrupt) if self.turn_result.is_some() => {}
                // a second Interrupt while the first cancel has not
                // settled is not a new cancel. It never re-issues the request,
                // never restarts the settle window, and never repeats the
                // queue write; it only repaints what the host still observes.
                Ok(UserAction::Interrupt)
                    if self.cancel_pending || self.cancel_unsettled.is_some() =>
                {
                    let detail = self
                        .cancel_unsettled
                        .clone()
                        .map_or(HostStatusDetail::InterruptRequested, |cause| {
                            HostStatusDetail::CancelUnsettled { cause }
                        });
                    let _ = self.host.events.send(ContentEvent::Status {
                        phase: HostPhase::Interrupting,
                        queue_len: self.host.session.pending_queue.len(),
                        detail: Some(detail),
                    });
                }
                Ok(UserAction::Interrupt) => {
                    match self.request_cancel().await {
                        Ok(CancellationStatus::AlreadyTerminal) => self.cancel_accepted = true,
                        Ok(
                            status @ (CancellationStatus::AlreadyRequested
                            | CancellationStatus::Requested),
                        ) => {
                            self.cancel_accepted = true;
                            self.cancel_pending = true;
                            self.projection.cancel_requested = true;
                            if matches!(status, CancellationStatus::Requested)
                                || self.cancel_settle_deadline.is_none()
                            {
                                self.cancel_settle_deadline =
                                    Some(Instant::now() + HOST_CANCEL_SETTLE_TIMEOUT);
                            }
                            let _ = self.host.events.send(ContentEvent::Status {
                                phase: HostPhase::Interrupting,
                                queue_len: self.host.session.pending_queue.len(),
                                detail: Some(HostStatusDetail::InterruptRequested),
                            });
                        }
                        Ok(_) => {}
                        // A refusal that names the turn as gone is an observation
                        // that it ended, not evidence it is still running: the
                        // runtime drops a turn from its admitted set only after
                        // publishing that turn's terminal. The host waits for that
                        // terminal instead of latching an unsettled cancel.
                        Err(error) if error.turn_is_no_longer_admitted() => {
                            self.cancel_accepted = true;
                        }
                        Err(error) => {
                            let detail = CancelUnsettled::RuntimeRefused {
                                cause: ControlFailure {
                                    diagnostic: DiagnosticText::new(error.to_string()),
                                },
                            };
                            self.cancel_unsettled = Some(detail.clone());
                            self.cancel_pending = true;
                            self.projection.cancel_requested = true;
                            apply_cancel_settlement(
                                self.host.rows,
                                self.host.store,
                                self.host.home,
                                self.host.session,
                                self.host.events,
                                &CancelSettlement::Unsettled(detail),
                                None,
                            )
                            .await;
                        }
                    }
                }
                Ok(UserAction::DropQueued) => {
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        drop_last_mutable,
                    ) {
                        Ok(true) => emit_queue_snapshot(self.host.events, self.host.session),
                        Ok(false) => {}
                        Err(error) => queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::Drop,
                            &error,
                        ),
                    }
                }
                Ok(UserAction::DeleteQueued { id, revision }) => {
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| remove_mutable(candidate, &id, revision),
                    ) {
                        Ok(true) => emit_queue_snapshot(self.host.events, self.host.session),
                        Ok(false) => reject_stale_queue_action(self.host.events, self.host.session),
                        Err(error) => queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::Delete,
                            &error,
                        ),
                    }
                }
                Ok(UserAction::ResolveApproval {
                    approval_id,
                    choice,
                }) => {
                    // A decision this turn did not offer is routed to the
                    // live child that holds it.
                    let routed = if self.projection.approvals.contains_key(&approval_id) {
                        None
                    } else {
                        let tree_id = crate::runtime::rebind::durable_tree_id(
                            self.host.rows,
                            self.host.session,
                        );
                        crate::runtime::events::decide_on_child(&tree_id, &approval_id, choice)
                            .await
                    };
                    let applied = match routed {
                        Some(Ok(())) => true,
                        Some(Err(refusal)) => {
                            let _ = self
                                .host
                                .events
                                .notice(TranscriptRow::error(NoticeScope::Approval, refusal));
                            false
                        }
                        None => {
                            resolve_approval_intent(
                                &self.control,
                                self.host.events,
                                &mut self.projection,
                                &approval_id,
                                choice,
                            )
                            .await
                        }
                    };
                    if applied && command_reply {
                        let choice = match choice {
                            plakous_host_protocol::ApprovalChoice::Allow => "allow",
                            plakous_host_protocol::ApprovalChoice::Deny => "deny",
                        };
                        let _ = self.host.events.notice(TranscriptRow::info(
                            NoticeScope::Approval,
                            format!("approval accepted: {choice}"),
                        ));
                    }
                }
                Ok(UserAction::EditQueued { id, revision, text }) => {
                    let mut refusal = None;
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| match edit_mutable(candidate, &id, revision, &text) {
                            Ok(()) => true,
                            Err(error) => {
                                refusal = Some(error);
                                false
                            }
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(self.host.events, self.host.session);
                            queue_edit_applied(self.host.events, id);
                        }
                        Ok(false) => {
                            emit_queue_snapshot(self.host.events, self.host.session);
                            queue_edit_refused(self.host.events, id);
                            queue_refusal(
                                self.host.events,
                                "queue edit",
                                refusal.unwrap_or(queue::Refusal::Stale),
                            );
                        }
                        Err(error) => {
                            queue_persist_failed(
                                self.host.events,
                                self.host.home,
                                self.host.session,
                                QueuePersistAction::Edit,
                                &error,
                            );
                            queue_edit_refused(self.host.events, id);
                        }
                    }
                }
                Ok(UserAction::MoveQueued {
                    id,
                    revision,
                    new_index,
                }) => {
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| move_mutable(candidate, &id, revision, new_index),
                    ) {
                        Ok(true) => emit_queue_snapshot(self.host.events, self.host.session),
                        Ok(false) => reject_stale_queue_action(self.host.events, self.host.session),
                        Err(error) => queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::Move,
                            &error,
                        ),
                    }
                }
                // During a turn the root holds nothing after a loss (the turn
                // ended that), so the one Resume resumes the held children
                // and the restored queue.
                Ok(action @ UserAction::ResumeLossContinuation { agent_id: None }) => {
                    if crate::topology_runtime::lookup(&crate::runtime::durable_topology_tree_id(
                        self.host.rows,
                        self.host.session,
                    ))
                    .is_some_and(|runtime| crate::agent::children::holds_after_loss(&runtime))
                    {
                        crate::topology_navigator::dispatch_topology_navigator_action(
                            self.host.rows,
                            self.host.session,
                            self.host.events,
                            self.host.roots.job_registry(),
                            &action,
                        )
                        .await;
                    }
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| {
                            candidate.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
                            true
                        },
                    ) {
                        Ok(true) => emit_queue_snapshot(self.host.events, self.host.session),
                        Ok(false) => {}
                        Err(error) => queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::ResumeDrain,
                            &error,
                        ),
                    }
                }
                Ok(UserAction::ResumeQueueDrain) => {
                    match commit_session_update(
                        self.host.store,
                        self.host.home,
                        self.host.session,
                        |candidate| {
                            candidate.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
                            true
                        },
                    ) {
                        Ok(true) => emit_queue_snapshot(self.host.events, self.host.session),
                        Ok(false) => {}
                        Err(error) => queue_persist_failed(
                            self.host.events,
                            self.host.home,
                            self.host.session,
                            QueuePersistAction::ResumeDrain,
                            &error,
                        ),
                    }
                }
                Ok(UserAction::RefreshTemplates) => {
                    surface::publish_template_readiness(
                        self.host.surface.as_deref(),
                        self.host.ready_catalog,
                        self.host.events,
                    );
                }
                Ok(UserAction::RequestCallDetail {
                    call_id,
                    agent_id,
                    session_id,
                }) => {
                    if let Some(request) = crate::call_detail::prepare(
                        self.host.rows,
                        self.host.session,
                        call_id,
                        agent_id,
                        &session_id,
                    ) {
                        request.spawn(
                            self.host.roots.session_tasks(),
                            self.host.roots.runtime_sessions().ok(),
                            self.host.events.clone(),
                        );
                    }
                }
                // The one product-intent arm; see the idle loop's twin.
                Ok(UserAction::Extension(intent)) => {
                    if surface::needs_usage(self.host.surface.as_deref(), &intent) {
                        // Provider limits are independent of the running driver.
                        // Show held meters while the coalesced refresh runs.
                        if let Some(stale) = self.host.held_usage.stale() {
                            let _ = self.host.events.send(ContentEvent::UsageLimits(stale));
                        }
                        if surface::fresh_usage(self.host.surface.as_deref(), &intent) {
                            self.host
                                .subscription_usage_refresh
                                .request_fresh(self.host.subscription_service.clone());
                        } else {
                            request_subscription_usage_service(
                                self.host.subscription_service.clone(),
                                self.host.subscription_usage_refresh,
                            );
                        }
                    }
                    let with_tools =
                        surface::needs_session_tools(self.host.surface.as_deref(), &intent);
                    let effect = surface::dispatch(
                        self.host.surface.as_deref(),
                        intent,
                        surface::SurfaceFacts {
                            session: Some(surface::session_facts(
                                self.host.session,
                                Some(self.host.rows),
                                with_tools,
                            )),
                            phase: surface::SurfacePhase::Busy,
                            attached_one_shot: self.host.attached_one_shot,
                            catalog: self.host.ready_catalog,
                            queue_len: self.host.session.pending_queue.len(),
                            usage_reads: self.host.subscription_usage_refresh.reads(),
                        },
                        self.host.events,
                    );
                    if let Some(saved) = effect.saved {
                        *self.host.root_template = saved.root_template;
                    }
                    if let Some(fork) = effect.profile_fork {
                        fork.refuse("a turn is running; fork onto a profile when the root is idle");
                    }
                }
                Ok(UserAction::CloseAgentTranscript) => {
                    self.host
                        .last_tree_revision
                        .child_live
                        .close_agent_transcript(self.host.events);
                }
                Ok(
                    action @ (UserAction::OpenTopologyNavigator
                    | UserAction::RefreshTopologyTree
                    | UserAction::OpenAgentTranscript { .. }
                    | UserAction::InterruptTopologyNode { .. }
                    | UserAction::CloseTopologyNode { .. }
                    | UserAction::MailTopologyNode { .. }
                    | UserAction::BackgroundTopologyNode { .. }
                    | UserAction::StopTopologyNodeProcesses { .. }
                    | UserAction::ReadTopologyNodeUsage { .. }
                    | UserAction::StopAllTopology
                    | UserAction::RecoverTopologyNode { .. }
                    | UserAction::ResumeLossContinuation { .. }),
                ) => {
                    crate::topology_navigator::dispatch_topology_navigator_action(
                        self.host.rows,
                        self.host.session,
                        self.host.events,
                        self.host.roots.job_registry(),
                        &action,
                    )
                    .await;
                    if let Some(runtime) = self
                        .host
                        .last_tree_revision
                        .runtime(self.host.rows, self.host.session)
                    {
                        self.host
                            .last_tree_revision
                            .child_live
                            .action(
                                self.host.roots.session_tasks(),
                                &runtime,
                                self.host.roots.runtime_sessions().ok(),
                                &self.host.session.id,
                                &action,
                                self.host.events,
                            )
                            .await;
                        crate::runtime::tree::emit_child_progress_on_refresh(
                            &runtime,
                            self.host.session,
                            self.host.last_tree_revision,
                            self.host.events,
                            &action,
                        );
                    }
                    record_emitted_tree_revision(
                        self.host.rows,
                        self.host.session,
                        self.host.last_tree_revision,
                    );
                }
                // Readiness is a read, so it stays truthful mid-turn; every
                // credential mutation waits for the idle boundary.
                Ok(UserAction::RefreshReadiness) => {
                    surface::publish_template_readiness(
                        self.host.surface.as_deref(),
                        self.host.ready_catalog,
                        self.host.events,
                    );
                }
                Ok(UserAction::ResyncTranscript { scope }) => {
                    self.host.events.resync_transcript(&scope);
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        false
    }
}
