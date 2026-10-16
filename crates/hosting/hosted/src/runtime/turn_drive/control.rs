//! Busy-turn control intents and their acknowledgements.

use std::time::Duration;

#[cfg(any(test, feature = "test-support"))]
use super::{
    COMPLETION_WINS_QUIT_PROMPT, TestHostBoundary, TestHostWorkflow, observe_test_boundary,
};
use super::{
    CancelRefusal, CancelSettlement, CancellationStatus, HOST_CANCEL_SETTLE_TIMEOUT, HostEvent,
    Instant, SessionQueueDrainState, TryRecvError, TurnDrive, UserAction, accept_draft,
    apply_cancel_settlement, commit_session_update, drop_last_mutable, edit_mutable,
    emit_prompt_history, emit_queue_snapshot, flush_assistant_delta, move_mutable, note_idle_only,
    queue, queue_edit_applied, queue_edit_refused, queue_refusal, record_emitted_tree_revision,
    reject_draft, reject_stale_queue_action, remove_mutable, request_subscription_usage_service,
    resolve_approval_intent, settle_cancel, surface,
};
use crate::runtime::admission::{
    QueuePersistAction, report_queue_persist_failed as queue_persist_failed,
};
use crate::sessions::{NoticeScope, TranscriptRow};

const QUIT_CANCEL_GRACE: Duration = Duration::from_millis(300);
const QUIT_RESULT_JOIN: Duration = Duration::from_millis(100);

impl<P: plakous_host_protocol::ProtocolExtension> TurnDrive<'_, P> {
    pub(super) async fn poll_actions(
        &mut self,
        #[cfg(any(test, feature = "test-support"))] prompt: &str,
    ) -> bool {
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
        if !self.quit_after_terminal {
            let mut acknowledgement = None;
            let received = self
                .host
                .deferred
                .borrow_mut()
                .take_deferred()
                .map_or_else(|| self.host.actions.try_recv(), Ok)
                .map(|delivery| {
                    if let Some(started) = delivery.started() {
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
                other => other,
            };
            match received {
                Ok(UserAction::StopProcesses { .. }) => {
                    unreachable!("process intents handled above")
                }
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
                    // terminal rather than the host hanging on one.
                    let settlement = settle_cancel(self.control.cancel(), QUIT_CANCEL_GRACE).await;
                    self.cancel_unsettled = None;
                    if let CancelSettlement::Unsettled(detail) = &settlement {
                        self.cancel_unsettled = Some(detail.clone());
                        let _ = self.host.events.notice(TranscriptRow::error(
                            NoticeScope::Host,
                            format!("quit escalated a cancel that did not settle: {detail}"),
                        ));
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
                    }
                    self.quit_after_terminal = true;
                    self.quit_join_deadline = Some(Instant::now() + QUIT_RESULT_JOIN);
                    #[cfg(any(test, feature = "test-support"))]
                    if prompt == COMPLETION_WINS_QUIT_PROMPT {
                        observe_test_boundary(
                            self.host.test_injection,
                            TestHostWorkflow::SendNow,
                            TestHostBoundary::ProductPublished,
                        );
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
                                let _ = self
                                    .host
                                    .events
                                    .send(HostEvent::PromptAdmitted(item.protocol(false)));
                            }
                            let _ = self.host.events.send(HostEvent::Status {
                                phase: "running".into(),
                                queue_len: self.host.session.pending_queue.len(),
                                detail: Some("queued follow-up".into()),
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
                    let row_text = crate::sessions::with_image_chips(
                        &text,
                        images.iter().map(crate::sessions::draft_image_chip),
                    );
                    if text.is_empty() && images.is_empty() {
                        reject_draft(self.host.events, action);
                    } else {
                        match self.control.steer(input).await {
                            Ok(_) => {
                                accept_draft(self.host.events, action);
                                // Live only: the projector derives the
                                // stored steer row from its acceptance fact.
                                let row = TranscriptRow::Steer {
                                    text: row_text,
                                    sent_at_ms: Some(plakous::model::now_unix_millis()),
                                };
                                self.host.session.append_transcript(row.clone());
                                let _ = self.host.events.notice(row);
                                let _ = self.host.events.send(HostEvent::Status {
                                    phase: "running".into(),
                                    queue_len: self.host.session.pending_queue.len(),
                                    detail: Some("steering…".into()),
                                });
                            }
                            Err(err) => {
                                reject_draft(self.host.events, action);
                                let _ = self.host.events.send(HostEvent::Status {
                                    phase: "running".into(),
                                    queue_len: self.host.session.pending_queue.len(),
                                    detail: Some(format!("steer rejected: {err}")),
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
                        .unwrap_or_else(|| "interrupt requested".into());
                    let _ = self.host.events.send(HostEvent::Status {
                        phase: "interrupting".into(),
                        queue_len: self.host.session.pending_queue.len(),
                        detail: Some(detail),
                    });
                }
                Ok(UserAction::Interrupt) => match self.control.request_cancel().await {
                    Ok(CancellationStatus::AlreadyTerminal) => {}
                    Ok(
                        status @ (CancellationStatus::AlreadyRequested
                        | CancellationStatus::Requested),
                    ) => {
                        self.cancel_pending = true;
                        self.projection.cancel_requested = true;
                        if matches!(status, CancellationStatus::Requested)
                            || self.cancel_settle_deadline.is_none()
                        {
                            self.cancel_settle_deadline =
                                Some(Instant::now() + HOST_CANCEL_SETTLE_TIMEOUT);
                        }
                        let _ = self.host.events.send(HostEvent::Status {
                            phase: "interrupting".into(),
                            queue_len: self.host.session.pending_queue.len(),
                            detail: Some("interrupt requested".into()),
                        });
                    }
                    Ok(_) => {}
                    // A refusal that names the turn as gone is an observation
                    // that it ended, not evidence it is still running: the
                    // runtime drops a turn from its admitted set only after
                    // publishing that turn's terminal. The host waits for that
                    // terminal instead of latching an unsettled cancel.
                    Err(error) if error.turn_is_no_longer_admitted() => {}
                    Err(error) => {
                        let detail = format!(
                            "cancel was refused by the runtime ({error}); the turn is still running"
                        );
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
                },
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
                    resolve_approval_intent(
                        &self.control,
                        self.host.events,
                        &mut self.projection,
                        &approval_id,
                        choice,
                    )
                    .await;
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
                Ok(UserAction::ClearSession) => note_idle_only(self.host.events, "clear"),
                Ok(UserAction::Compact) => note_idle_only(self.host.events, "compact"),
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
                            self.host.roots.runtime_sessions().ok(),
                            self.host.events.clone(),
                        );
                    }
                }
                Ok(UserAction::ResumeSession { .. } | UserAction::ResumeSessionHandoff { .. }) => {
                    note_idle_only(self.host.events, "resume");
                }
                Ok(UserAction::ForkSession { .. }) => note_idle_only(self.host.events, "fork"),
                // The one product-intent arm; see the idle loop's twin.
                Ok(UserAction::Extension(intent)) => {
                    if surface::needs_usage(self.host.surface.as_deref(), &intent) {
                        // Provider limits are independent of the running driver.
                        // Show held meters while the coalesced refresh runs.
                        if let Some(stale) = self.host.held_usage.stale() {
                            let _ = self.host.events.send(HostEvent::UsageLimits(stale));
                        }
                        request_subscription_usage_service(
                            self.host.subscription_service.clone(),
                            self.host.subscription_usage_refresh,
                        );
                    }
                    let effect = surface::dispatch(
                        self.host.surface.as_deref(),
                        intent,
                        surface::SurfaceFacts {
                            session: Some(surface::session_facts(
                                self.host.session,
                                Some(self.host.rows),
                            )),
                            phase: surface::SurfacePhase::Busy,
                            attached_one_shot: self.host.attached_one_shot,
                            catalog: self.host.ready_catalog,
                            queue_len: self.host.session.pending_queue.len(),
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
                Ok(
                    action @ (UserAction::OpenTopologyNavigator
                    | UserAction::RefreshTopologyTree
                    | UserAction::OpenAgentTranscript { .. }
                    | UserAction::CloseAgentTranscript
                    | UserAction::ResumeTopologyTree
                    | UserAction::InterruptTopologyNode { .. }
                    | UserAction::CloseTopologyNode { .. }
                    | UserAction::StopAllTopology),
                ) => {
                    crate::topology_navigator::dispatch_topology_navigator_action(
                        self.host.rows,
                        self.host.session,
                        |event| {
                            let _ = self.host.events.send(event);
                        },
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
                    if matches!(action, UserAction::ResumeTopologyTree) {
                        crate::runtime::tree::start_child_recovery(
                            self.host.events,
                            &self.host.session.id,
                            crate::runtime::rebind::durable_tree_id(
                                self.host.rows,
                                self.host.session,
                            ),
                            false,
                            None,
                        );
                    }
                    record_emitted_tree_revision(
                        self.host.rows,
                        self.host.session,
                        self.host.last_tree_revision,
                    );
                }
                Ok(UserAction::InvokeTool { .. }) => {
                    note_idle_only(self.host.events, "invoke_tool");
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
                Ok(UserAction::RetryAttach) => {
                    note_idle_only(self.host.events, "binding");
                }
                Ok(UserAction::Recover) => {
                    note_idle_only(self.host.events, "recover");
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        false
    }
}
