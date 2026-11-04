//! Idle control dispatch and FIFO prompt selection.
use super::RootRecovery;

use super::recover::RecoveryTrigger;
use super::{
    APPROVAL_REFUSED_NOTE, CliFailure, CliResult, ContextQuery, HostStep,
    IDLE_OBSERVE_POLL_INTERVAL, Instant, InterruptWatch, Rebound, RecvTimeoutError, RootActor,
    SessionQueueDrainState, TREE_OBSERVE_INTERVAL, UserAction, admit_idle_immediate_prompt,
    clear_session_rebuild, commit_session_update, compact_may_resume_queue,
    compact_reason_for_disposition, dispatch_invoke_tool, dispatch_topology_navigator,
    drop_last_mutable, edit_mutable, emit_queue_snapshot, emit_tree_snapshot_if_revision_advanced,
    emit_unavailable_context, handle_compact, handle_fork, handle_rebind, handle_resume_picker,
    move_mutable, native_compact_facet, observe_compact, query_context, queue, queue_edit_applied,
    queue_edit_refused, queue_refusal, record_emitted_tree_revision, recovery_blocked_action,
    refuse_recovery_blocked_action, reject_draft, reject_stale_queue_action, remove_mutable,
    report_context, resume_session_switch, sessions, surface,
};
use crate::runtime::admission::{
    QueuePersistAction, report_queue_persist_failed as queue_persist_failed,
};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Reopens a fenced session in place as soon as the actor is idle, with
    /// no action needed. A failed reopen is reported and left to the next
    /// action that needs the session, or to an explicit recovery. The reopen
    /// is watched: what the operator sends meanwhile, an interrupt or a quit
    /// included, is left for the idle dispatcher to answer after it.
    async fn recover_eagerly(&mut self) -> Result<HostStep, ()> {
        let state = std::rc::Rc::clone(&self.role.interrupt_state);
        let watch = InterruptWatch::deferring(self.role.actions, &state);
        let recovered = crate::interrupt::watched(
            watch,
            Box::pin(self.recover_session(RecoveryTrigger::Eager)),
        )
        .await;
        match recovered {
            Ok(step) => {
                // The recovery reported on this session; store that before an
                // action can switch to another.
                self.commit_notice_intents();
                Ok(step)
            }
            Err(failure) => {
                self.role.recovery = RootRecovery::Retrying;
                let _ = self.role.events.notice(
                    TranscriptRow::error(NoticeScope::Session, failure.message().to_owned())
                        .with_optional_remedy(failure.remedy()),
                );
                Err(())
            }
        }
    }

    /// The next intent, or the end of one idle observation `window` without
    /// one: a biased select that takes the mailbox, whose urgent lane
    /// outranks everything queued, ahead of the window's deadline. An intent
    /// wakes it on arrival.
    pub(super) async fn receive(
        &self,
        window: std::time::Duration,
    ) -> Result<crate::surface_protocol::UserActionDelivery<P>, RecvTimeoutError> {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            match self.role.actions.try_recv() {
                Ok(delivery) => return Ok(delivery),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err(RecvTimeoutError::Disconnected);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
            tokio::select! {
                biased;
                () = self.role.actions.ready() => {}
                () = tokio::time::sleep_until(deadline) => return Err(RecvTimeoutError::Timeout),
            }
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "The idle owner arbitrates operator intents, queued work, child wakes, transient retries, and shutdown without a second dispatcher."
    )]
    pub(super) async fn next_prompt(
        &mut self,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        // A quit the operator already sent is answered first; the next open
        // recovers the fenced turn.
        if self.policy.recovery == crate::node::RecoveryPolicy::EagerInPlace
            && self.role.recovery == RootRecovery::Due
            && !self.role.interrupt_state.borrow().has_deferred_quit()
            && !self.role.actions.abandon_and_release_requested()
        {
            match self.recover_eagerly().await {
                Ok(HostStep::Quit) => return Ok(HostStep::Quit),
                Ok(_) | Err(()) => {}
            }
        }
        crate::call_detail::note_attached(&self.role.config.session.id);
        self.role
            .last_tree_revision
            .child_live
            .retain_session(&self.role.config.session.id);
        self.update_background_process_count();
        self.publish_pending_continuation();
        self.role
            .readiness
            .apply_settled(&mut self.role.config, &self.role.events);
        // An action the operator already sent outranks the queue. Starting
        // a queued turn first would open a turn the very next iteration has to
        // cancel, so a deferred delivery is drained before `pending_start`.
        let deferred_first = self.role.interrupt_state.borrow().has_deferred();
        let next_prompt = if let Some(item) = (!deferred_first)
            .then(|| self.role.pending_start.take())
            .flatten()
        {
            Some((item.text.clone(), Some(item), false))
        } else {
            if !self.role.config.attached_one_shot {
                self.role.subscription_usage_refresh.poll_idle(
                    &self.role.held_usage,
                    &self.role.session_ledger,
                    self.role.config.target.native_subscriptions().cloned(),
                );
            }
            // Child status transitions, grandchild spawns, and mail changes have
            // no root receipt behind them, so the idle loop observes the tree at
            // its existing wake, no faster than the observe interval. Resolving
            // the durable tree id can touch the store, but `TreeSnapshotPush`
            // resolves at most once per bound session, so the steady-state cost
            // here is two hub-map probes. Gating this on "a tree has already
            // been snapshotted" would silence the canvas for the whole of a
            // session that switched under the attach, because a switch
            // clears the recorded revision along with the tree id. The emitter
            // drops every non-advancing revision, so at most one push per
            // revision leaves.
            let observed_tree = self.role.last_tree_observed_at.elapsed() >= TREE_OBSERVE_INTERVAL;
            if observed_tree
                && !(self.role.storage_interrupted && self.role.config.store.storage_unavailable())
            {
                self.role.last_tree_observed_at = Instant::now();
                emit_tree_snapshot_if_revision_advanced(
                    &crate::runtime::HostRows::new(
                        &self.role.config.state_roots,
                        self.role.config.store.as_ref(),
                    ),
                    &*self.role.config.store,
                    &self.role.config.home,
                    &mut self.role.config.session,
                    &mut self.role.last_tree_revision,
                    &self.role.events,
                    false,
                );
            }
            let mut acknowledgement = None;
            let mut command_reply = false;
            let deferred = self.role.interrupt_state.borrow_mut().take_deferred();
            let received = match deferred {
                Some(delivery) => Ok(delivery),
                None => self.receive(IDLE_OBSERVE_POLL_INTERVAL).await,
            }
            .map(|delivery| {
                if let Some(started) = delivery.started() {
                    command_reply = delivery.correlated();
                    let _ = self.role.events.send(started);
                }
                let (action, guard) = delivery.into_parts();
                acknowledgement = Some(guard);
                action
            });
            let cancels_resume =
                received.is_ok() && !matches!(&received, Ok(UserAction::ResyncTranscript { .. }));
            let cancelled_resume = cancels_resume && self.role.pending_resume.is_some();
            if cancels_resume {
                self.cancel_transient_resume();
            }
            // A session whose eager reopen failed recovers on the next action
            // that needs it, which then proceeds as though nothing had happened.
            let received = match received {
                Ok(action) if self.role.recovery.blocked() && recovery_blocked_action(&action) => {
                    match self.recover_session(RecoveryTrigger::Action).await {
                        Ok(HostStep::Quit) => return Ok(HostStep::Quit),
                        Ok(_) => {
                            // The recovery reported on this session; store
                            // that before the action can switch to another.
                            self.commit_notice_intents();
                            Ok(action)
                        }
                        Err(failure) => {
                            if let Some(state) = one_shot.as_deref_mut() {
                                state.attempted = true;
                                state.result = Some(Err(CliFailure::recovery_required(
                                    failure.message().to_owned(),
                                )));
                            }
                            refuse_recovery_blocked_action(&self.role.events, &action, &failure);
                            return Ok(HostStep::Continue);
                        }
                    }
                }
                other => other,
            };
            match received {
                Ok(UserAction::StopProcesses { id, interrupt: _ }) => {
                    if let Err(error) = self
                        .role
                        .config
                        .state_roots
                        .job_registry()
                        .stop_user(id.as_deref())
                    {
                        let _ = self
                            .role
                            .events
                            .notice(TranscriptRow::error(NoticeScope::Session, error));
                    }
                    self.update_background_process_count();
                    None
                }
                Ok(UserAction::Recover) => {
                    if self.role.recovery.blocked() {
                        match self.recover_session(RecoveryTrigger::Explicit).await {
                            Ok(HostStep::Quit) => return Ok(HostStep::Quit),
                            Ok(_) => {}
                            Err(failure) => {
                                let _ = self.role.events.notice(
                                    TranscriptRow::error(
                                        NoticeScope::Session,
                                        failure.message().to_owned(),
                                    )
                                    .with_optional_remedy(failure.remedy()),
                                );
                            }
                        }
                    } else {
                        let nothing = (!self.role.recovered_at_open).then(|| {
                            "nothing to recover: no turn in this session was interrupted".to_owned()
                        });
                        self.recover_tree_children(nothing);
                    }
                    None
                }
                Ok(UserAction::Quit) => {
                    self.quit_started_at.get_or_insert_with(Instant::now);
                    if let Some(state) = one_shot.as_deref_mut() {
                        state.result =
                            Some(Ok(super::super::oneshot::OneShotOutcome::Interrupted {
                                assistant: String::new(),
                            }));
                    }
                    if self.role.config.session.store_revision != 0 {
                        let _ = commit_session_update(
                            &*self.role.config.store,
                            &self.role.config.home,
                            &mut self.role.config.session,
                            |candidate| {
                                candidate.queue_drain_state =
                                    SessionQueueDrainState::RestoredPaused;
                                true
                            },
                        );
                    }
                    super::log_ev(
                        self.role.log.as_ref(),
                        &self.role.config,
                        super::SessionEvent::new("session.quit"),
                    );
                    return Ok(HostStep::Quit);
                }
                Ok(action @ (UserAction::Prompt(_) | UserAction::FollowUp(_))) => {
                    if let Some(state) = one_shot.as_deref_mut() {
                        state.attempted = true;
                    }
                    let (UserAction::Prompt(draft) | UserAction::FollowUp(draft)) = &action else {
                        unreachable!()
                    };
                    if draft.text.trim().is_empty() && draft.images.is_empty() {
                        reject_draft(&self.role.events, action);
                        None
                    } else {
                        let mut queued = None;
                        let mut refusal = None;
                        match commit_session_update(
                            &*self.role.config.store,
                            &self.role.config.home,
                            &mut self.role.config.session,
                            |candidate| match candidate.enqueue_live(draft.clone()) {
                                Ok(item) => {
                                    candidate.record_prompt(item.text.clone());
                                    queued = Some(item);
                                    true
                                }
                                Err(error) => {
                                    refusal = Some(error);
                                    false
                                }
                            },
                        ) {
                            Ok(true) => queued.map(|item| {
                                let capacity = self.role.config.store.storage_admission();
                                let echo = !matches!(
                                    capacity,
                                    crate::session_store::StorageAdmission::Refused(_)
                                );
                                self.role.pending_storage_admission = Some(capacity);
                                admit_idle_immediate_prompt(
                                    &self.role.events,
                                    &self.role.config.session,
                                    action,
                                    item,
                                    echo,
                                )
                            }),
                            Ok(false) => {
                                if let Some(state) = one_shot.as_deref_mut() {
                                    state.result = Some(Err(CliFailure::classified(
                                        crate::error::CliFailureKind::Usage,
                                        "prompt queue admission was refused",
                                    )));
                                }
                                queue_refusal(
                                    &self.role.events,
                                    "queue admission",
                                    refusal.unwrap_or(queue::Refusal::Stale),
                                );
                                reject_draft(&self.role.events, action);
                                None
                            }
                            Err(error) => {
                                queue_persist_failed(
                                    &self.role.events,
                                    &self.role.config.home,
                                    &self.role.config.session,
                                    QueuePersistAction::Admission,
                                    &error,
                                );
                                if let Some(state) = one_shot.as_deref_mut() {
                                    state.result = Some(Err(CliFailure::new(error)));
                                }
                                reject_draft(&self.role.events, action);
                                None
                            }
                        }
                    }
                }
                Ok(action @ UserAction::Steer(_)) => {
                    let _ = self.role.events.send(ContentEvent::Status {
                        phase: HostPhase::Idle,
                        queue_len: self.role.config.session.pending_queue.len(),
                        detail: Some(HostStatusDetail::NoActiveTurn),
                    });
                    reject_draft(&self.role.events, action);
                    None
                }
                Ok(UserAction::Interrupt) => {
                    if self.role.config.store.storage_unavailable() {
                        self.role.storage_interrupted = true;
                        self.role.config.session.queue_drain_state =
                            SessionQueueDrainState::RestoredPaused;
                        self.role.pending_start = None;
                        return Ok(HostStep::Continue);
                    }
                    if !cancelled_resume
                        && !self.has_automatic_continue_work()
                        && one_shot.is_none()
                    {
                        return Ok(HostStep::Continue);
                    }
                    self.suppress_child_wake(None)?;
                    self.publish_pending_continuation();
                    if let Some(state) = one_shot {
                        state.attempted = true;
                        state.result =
                            Some(Ok(super::super::oneshot::OneShotOutcome::Interrupted {
                                assistant: String::new(),
                            }));
                        return Ok(if state.run.is_some() {
                            HostStep::Continue
                        } else {
                            HostStep::Quit
                        });
                    }
                    None
                }
                Ok(UserAction::Background) => None,
                Ok(UserAction::DropQueued) => {
                    match commit_session_update(
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        drop_last_mutable,
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                        }
                        Ok(false) => {}
                        Err(error) => queue_persist_failed(
                            &self.role.events,
                            &self.role.config.home,
                            &self.role.config.session,
                            QueuePersistAction::Drop,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::DeleteQueued { id, revision }) => {
                    match commit_session_update(
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        |candidate| remove_mutable(candidate, &id, revision),
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                        }
                        Ok(false) => {
                            reject_stale_queue_action(&self.role.events, &self.role.config.session);
                        }
                        Err(error) => queue_persist_failed(
                            &self.role.events,
                            &self.role.config.home,
                            &self.role.config.session,
                            QueuePersistAction::Delete,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::EditQueued { id, revision, text }) => {
                    let mut refusal = None;
                    match commit_session_update(
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        |candidate| match edit_mutable(candidate, &id, revision, &text) {
                            Ok(()) => true,
                            Err(error) => {
                                refusal = Some(error);
                                false
                            }
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                            queue_edit_applied(&self.role.events, id);
                        }
                        Ok(false) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                            queue_edit_refused(&self.role.events, id);
                            queue_refusal(
                                &self.role.events,
                                "queue edit",
                                refusal.unwrap_or(queue::Refusal::Stale),
                            );
                        }
                        Err(error) => {
                            queue_persist_failed(
                                &self.role.events,
                                &self.role.config.home,
                                &self.role.config.session,
                                QueuePersistAction::Edit,
                                &error,
                            );
                            queue_edit_refused(&self.role.events, id);
                        }
                    }
                    None
                }
                Ok(UserAction::MoveQueued {
                    id,
                    revision,
                    new_index,
                }) => {
                    match commit_session_update(
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        |candidate| move_mutable(candidate, &id, revision, new_index),
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                        }
                        Ok(false) => {
                            reject_stale_queue_action(&self.role.events, &self.role.config.session);
                        }
                        Err(error) => queue_persist_failed(
                            &self.role.events,
                            &self.role.config.home,
                            &self.role.config.session,
                            QueuePersistAction::Move,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::SendQueuedNow { id, revision }) => {
                    if self.role.config.session.queue_projection_debt {
                        emit_queue_snapshot(&self.role.events, &self.role.config.session);
                        let _ = self.role.events.send(ContentEvent::Status {
                            phase: HostPhase::Idle,
                            queue_len: self.role.config.session.pending_queue.len(),
                            detail: Some(HostStatusDetail::QueueProjectionCatchingUp),
                        });
                        return Ok(HostStep::Continue);
                    }
                    let item = queue::get(&self.role.config.session.pending_queue, &id, revision);
                    if let Some(item) = item {
                        Some((item.text.clone(), Some(item), false))
                    } else {
                        reject_stale_queue_action(&self.role.events, &self.role.config.session);
                        None
                    }
                }
                Ok(UserAction::ResolveApproval {
                    approval_id,
                    choice,
                }) => {
                    // The root's decisions exist only inside its admitted
                    // turn; idle, only a live child can hold one.
                    let tree_id = crate::runtime::rebind::durable_tree_id(
                        &self.role.config.topology_rows(),
                        &self.role.config.session,
                    );
                    match crate::runtime::events::decide_on_child(&tree_id, &approval_id, choice)
                        .await
                    {
                        Some(Ok(())) if command_reply => {
                            let choice = match choice {
                                plakous_host_protocol::ApprovalChoice::Allow => "allow",
                                plakous_host_protocol::ApprovalChoice::Deny => "deny",
                            };
                            let _ = self.role.events.notice(TranscriptRow::info(
                                NoticeScope::Approval,
                                format!("approval accepted: {choice}"),
                            ));
                        }
                        Some(Ok(())) => {}
                        refused => {
                            let refusal = refused.and_then(Result::err);
                            let _ = self.role.events.notice(TranscriptRow::error(
                                NoticeScope::Approval,
                                refusal.unwrap_or_else(|| APPROVAL_REFUSED_NOTE.to_owned()),
                            ));
                        }
                    }
                    None
                }
                Ok(UserAction::ResumeQueueDrain) => {
                    match commit_session_update(
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        |candidate| {
                            candidate.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
                            true
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.role.events, &self.role.config.session);
                            queue::next_drainable(&self.role.config.session)
                                .map(|item| (item.text.clone(), Some(item), false))
                        }
                        Ok(false) => None,
                        Err(error) => {
                            queue_persist_failed(
                                &self.role.events,
                                &self.role.config.home,
                                &self.role.config.session,
                                QueuePersistAction::ResumeDrain,
                                &error,
                            );
                            None
                        }
                    }
                }
                Ok(UserAction::NewSession) => {
                    self.role.idle_entry_pass_due = true;
                    // A switch closes the agent view before its ClearView.
                    self.role.last_tree_revision.child_live.close_agent_view();
                    let _ = self.role.events.send(ContentEvent::SetBusy(true));
                    let _ = self.role.events.send(ContentEvent::Status {
                        phase: HostPhase::Clear,
                        queue_len: self.role.config.session.pending_queue.len(),
                        detail: Some(HostStatusDetail::CreatingFreshConversation),
                    });
                    match clear_session_rebuild(
                        &mut self.role.config,
                        &mut self.opened,
                        &mut self.role.stderr_buf,
                        &self.role.events,
                        self.role.log.as_ref(),
                    )
                    .await
                    {
                        Ok(()) => {
                            self.role.recovery = RootRecovery::Clear;
                        }
                        Err(err) => {
                            let _ = self.role.events.notice(
                                TranscriptRow::info(
                                    NoticeScope::Session,
                                    format!(
                                        "new session not started; this session remains active: {}",
                                        err.message()
                                    ),
                                )
                                .with_optional_remedy(err.remedy()),
                            );
                            let _ = self.role.events.send(ContentEvent::SetBusy(false));
                            let _ = self.role.events.send(ContentEvent::Status {
                                phase: HostPhase::Idle,
                                queue_len: self.role.config.session.pending_queue.len(),
                                detail: Some(HostStatusDetail::ClearRefused {
                                    model_id: self.role.config.model_id.clone(),
                                }),
                            });
                        }
                    }
                    None
                }
                Ok(UserAction::Compact) => {
                    self.role.idle_entry_pass_due = true;
                    let current = query_context(&self.opened, ContextQuery::Current).await;
                    // Every typed disposition asks for a compact now, so there
                    // is no disposition left to refuse up front. A
                    // protected-only tail reaches the placeholder rung, and
                    // `handle_compact` reports an exhausted ladder. Only an
                    // unreadable context authority still refuses here.
                    let reason = match current.as_ref().map(|projection| projection.disposition()) {
                        Ok(disposition) => compact_reason_for_disposition(disposition),
                        Err(error) => {
                            let _ = self.role.events.notice(TranscriptRow::error(
                                NoticeScope::Compaction,
                                format!(
                                    "compact refused: current context authority is unavailable: {}",
                                    error.message()
                                ),
                            ).with_optional_remedy(error.remedy()));
                            return Ok(HostStep::Continue);
                        }
                    };
                    let capacity_recovery =
                        reason == sessions::ProductCompactReason::CapacityRecovery;
                    observe_compact(
                        &self.role.events,
                        self.role.config.session.pending_queue.len(),
                        true,
                    );
                    let compacted = handle_compact(
                        InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
                        &mut self.role.config,
                        &self.opened,
                        &mut self.role.stderr_buf,
                        &self.role.events,
                        self.role.log.as_ref(),
                        reason,
                        None,
                    )
                    .await;
                    observe_compact(
                        &self.role.events,
                        self.role.config.session.pending_queue.len(),
                        false,
                    );
                    if let Err(error) = compacted {
                        let _ = self.role.events.notice(
                            TranscriptRow::error(
                                NoticeScope::Compaction,
                                format!("compact: {}", error.message()),
                            )
                            .with_optional_remedy(error.remedy()),
                        );
                        None
                    } else if compact_may_resume_queue(
                        capacity_recovery,
                        self.role.config.attached_one_shot,
                        self.role.config.session.queue_drain_state,
                    ) {
                        queue::next_drainable(&self.role.config.session)
                            .map(|item| (item.text.clone(), Some(item), false))
                    } else {
                        None
                    }
                }
                Ok(UserAction::RequestCallDetail {
                    call_id,
                    agent_id,
                    session_id,
                }) => {
                    let sessions = self.role.config.state_roots.runtime_sessions().ok();
                    let events = self.role.events.clone();
                    if let Some(request) = crate::call_detail::prepare(
                        &self.role.config.topology_rows(),
                        &self.role.config.session,
                        call_id.clone(),
                        agent_id.clone(),
                        &session_id,
                    ) {
                        // Only enters the runtime to spawn the read; the answer
                        // arrives as its own observation.
                        request.spawn(self.role.executor.tasks(), sessions, events);
                    }
                    None
                }
                Ok(UserAction::RefreshTemplates) => {
                    surface::publish_template_readiness(
                        self.role.config.surface.as_deref(),
                        &self.role.config.ready_catalog.accounts,
                        &self.role.events,
                    );
                    None
                }
                Ok(UserAction::ResumeSession { id: None }) => {
                    handle_resume_picker(&self.role.config, &self.role.events);
                    None
                }
                Ok(UserAction::ResumeSession { id: Some(id) }) => {
                    self.role.idle_entry_pass_due = true;
                    self.role.last_tree_revision.child_live.close_agent_view();
                    match resume_session_switch(
                        &mut self.role.config,
                        &mut self.opened,
                        &mut self.role.stderr_buf,
                        &self.role.events,
                        &id,
                    )
                    .await
                    {
                        Ok(()) => {
                            if matches!(self.drive_restored_turn().await?, HostStep::Quit) {
                                return Ok(HostStep::Quit);
                            }
                            None
                        }
                        Err(err) => {
                            let _ = self.role.events.notice(
                                TranscriptRow::error(
                                    NoticeScope::Session,
                                    format!("resume: {}", err.message()),
                                )
                                .with_optional_remedy(err.remedy()),
                            );
                            None
                        }
                    }
                }
                Ok(UserAction::ResumeSessionHandoff { .. }) => {
                    let _ = self.role.events.notice(TranscriptRow::error(
                        NoticeScope::Session,
                        "resume relaunch must be handled by the terminal owner",
                    ));
                    None
                }
                Ok(UserAction::ForkSession { account, compact }) => {
                    self.role.idle_entry_pass_due = true;
                    self.role.last_tree_revision.child_live.close_agent_view();
                    let _ = self.role.events.send(ContentEvent::SetBusy(true));
                    let _ = self.role.events.send(ContentEvent::Status {
                        phase: HostPhase::Fork,
                        queue_len: self.role.config.session.pending_queue.len(),
                        detail: Some(HostStatusDetail::ForkingSession),
                    });
                    let forked = Box::pin(handle_fork(
                        InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
                        &mut self.role.config,
                        &mut self.opened,
                        &mut self.role.stderr_buf,
                        &self.role.events,
                        account.as_deref(),
                    ))
                    .await;
                    let _ = self.role.events.send(ContentEvent::SetBusy(false));
                    let _ = self.role.events.send(ContentEvent::Status {
                        phase: HostPhase::Idle,
                        queue_len: self.role.config.session.pending_queue.len(),
                        detail: None,
                    });
                    match forked {
                        Ok(()) if compact => {
                            let current = query_context(&self.opened, ContextQuery::Current).await;
                            let reason = match current
                                .as_ref()
                                .map(|projection| projection.disposition())
                            {
                                Ok(disposition) => compact_reason_for_disposition(disposition),
                                Err(error) => {
                                    let _ = self.role.events.notice(TranscriptRow::error(NoticeScope::Compaction, format!(
                                        "compact refused: current context authority is unavailable: {}",
                                        error.message()
                                    )).with_optional_remedy(error.remedy()));
                                    let _ = self.role.events.notice(TranscriptRow::info(NoticeScope::Compaction,
                                        "the forked session is active; another explicit compaction retries the reduction"
                                            ,
                                    ).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::RetryCompaction {}));
                                    return Ok(HostStep::Continue);
                                }
                            };
                            observe_compact(
                                &self.role.events,
                                self.role.config.session.pending_queue.len(),
                                true,
                            );
                            let compacted = handle_compact(
                                InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
                                &mut self.role.config,
                                &self.opened,
                                &mut self.role.stderr_buf,
                                &self.role.events,
                                self.role.log.as_ref(),
                                reason,
                                None,
                            )
                            .await;
                            observe_compact(
                                &self.role.events,
                                self.role.config.session.pending_queue.len(),
                                false,
                            );
                            if let Err(error) = compacted {
                                let _ = self.role.events.notice(
                                    TranscriptRow::error(
                                        NoticeScope::Compaction,
                                        format!("compact: {}", error.message()),
                                    )
                                    .with_optional_remedy(error.remedy()),
                                );
                                let _ = self.role.events.notice(TranscriptRow::info(
                                    NoticeScope::Compaction,
                                    "the forked session is active; another explicit compaction retries the reduction",
                                ).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::RetryCompaction {}));
                            }
                        }
                        Ok(()) => {}
                        Err(err) => {
                            let _ = self.role.events.notice(
                                TranscriptRow::error(
                                    NoticeScope::Session,
                                    format!("fork: {}", err.message()),
                                )
                                .with_optional_remedy(err.remedy()),
                            );
                        }
                    }
                    None
                }
                // Publish host-owned meters before invoking a driver that requests them.
                Ok(UserAction::Extension(intent)) => {
                    if surface::needs_usage(self.role.config.surface.as_deref(), &intent) {
                        if let Err(error) = report_context(
                            &self.opened,
                            &self.role.events,
                            "on idle refresh",
                            self.role
                                .config
                                .target
                                .native_provider()
                                .is_some_and(native_compact_facet),
                            self.role.config.session.context_counters(),
                        )
                        .await
                        {
                            emit_unavailable_context(&self.role.events);
                            let _ = self.role.events.notice(
                                TranscriptRow::error(
                                    NoticeScope::Session,
                                    error.message().to_owned(),
                                )
                                .with_optional_remedy(error.remedy()),
                            );
                        }
                        if !self.role.config.attached_one_shot {
                            if surface::fresh_usage(self.role.config.surface.as_deref(), &intent) {
                                self.role.subscription_usage_refresh.request_fresh(
                                    self.role.config.target.native_subscriptions().cloned(),
                                );
                            } else {
                                self.role.subscription_usage_refresh.request(
                                    self.role.config.target.native_subscriptions().cloned(),
                                );
                            }
                        }
                    }
                    let with_tools =
                        surface::needs_session_tools(self.role.config.surface.as_deref(), &intent);
                    let effect = surface::dispatch(
                        self.role.config.surface.as_deref(),
                        intent,
                        surface::SurfaceFacts {
                            session: Some(surface::session_facts(
                                &self.role.config.session,
                                Some(&self.role.config.topology_rows()),
                                with_tools,
                            )),
                            phase: surface::SurfacePhase::Idle,
                            attached_one_shot: self.role.config.attached_one_shot,
                            catalog: &self.role.config.ready_catalog.accounts,
                            queue_len: self.role.config.session.pending_queue.len(),
                            usage_reads: self.role.subscription_usage_refresh.reads(),
                        },
                        &self.role.events,
                    );
                    if let Some(saved) = effect.saved {
                        self.role.config.root_template = saved.root_template;
                    }
                    if let Some(fork) = effect.profile_fork {
                        self.role.idle_entry_pass_due = true;
                        self.role.last_tree_revision.child_live.close_agent_view();
                        let _ = self.role.events.send(ContentEvent::SetBusy(true));
                        let _ = self.role.events.send(ContentEvent::Status {
                            phase: HostPhase::Rebind,
                            queue_len: self.role.config.session.pending_queue.len(),
                            detail: Some(HostStatusDetail::ForkingProfileRoot {
                                model: fork.model.clone(),
                            }),
                        });
                        let result = Box::pin(handle_rebind(
                            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
                            &mut self.role.config,
                            &mut self.opened,
                            &mut self.role.stderr_buf,
                            &self.role.events,
                            &fork.account,
                            &fork.model,
                            fork.effort.as_deref(),
                            fork.extra.as_deref(),
                            fork.composition,
                            fork.protocol,
                            &fork.roster,
                            fork.switch.as_deref(),
                        ))
                        .await;
                        let _ = self.role.events.send(ContentEvent::SetBusy(false));
                        let _ = self.role.events.send(ContentEvent::Status {
                            phase: HostPhase::Idle,
                            queue_len: self.role.config.session.pending_queue.len(),
                            detail: Some(HostStatusDetail::Ready {
                                model_id: self.role.config.model_id.clone(),
                                pause: plakous_host_protocol::status::HostPause::default(),
                            }),
                        });
                        (fork.finish)(match result {
                            Ok(Rebound::Switched) => surface::ProfileForkOutcome::Forked,
                            Ok(Rebound::Succeeded) => surface::ProfileForkOutcome::Switched,
                            Ok(Rebound::Unchanged) => surface::ProfileForkOutcome::AlreadyActive,
                            Err(error) => {
                                surface::ProfileForkOutcome::Refused(error.host_failure())
                            }
                        });
                    }
                    None
                }
                Ok(UserAction::RefreshReadiness) => {
                    self.role.readiness.start(&self.role.config);
                    None
                }
                Ok(UserAction::ResyncTranscript { scope }) => {
                    self.role.events.resync_transcript(&scope);
                    None
                }
                Ok(UserAction::RetryAttach) => {
                    let _ = self.role.events.notice(TranscriptRow::info(
                        NoticeScope::Session,
                        "already attached; attachment retry applies only to a detached session",
                    ).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::AttachOnlyWhenDetached {}));
                    None
                }
                Ok(
                    action @ (UserAction::OpenTopologyNavigator
                    | UserAction::RefreshTopologyTree
                    | UserAction::OpenAgentTranscript { .. }
                    | UserAction::CloseAgentTranscript
                    | UserAction::InterruptTopologyNode { .. }
                    | UserAction::CloseTopologyNode { .. }
                    | UserAction::StopAllTopology
                    | UserAction::RecoverTopologyNode { .. }),
                ) => {
                    dispatch_topology_navigator(
                        &self.role.config.topology_rows(),
                        &self.role.config.state_roots,
                        &self.role.config.session,
                        &self.role.events,
                        &action,
                        &mut self.role.last_tree_revision,
                    )
                    .await;
                    record_emitted_tree_revision(
                        &self.role.config.topology_rows(),
                        &self.role.config.session,
                        &mut self.role.last_tree_revision,
                    );
                    None
                }
                Ok(UserAction::InvokeTool {
                    correlation_id,
                    name,
                    arguments,
                    agent_id,
                }) => {
                    dispatch_invoke_tool(
                        &crate::runtime::HostRows::new(
                            &self.role.config.state_roots,
                            self.role.config.store.as_ref(),
                        ),
                        &*self.role.config.store,
                        &self.role.config.home,
                        &mut self.role.config.session,
                        &self.role.events,
                        &name,
                        &arguments,
                        agent_id.as_deref(),
                        correlation_id.as_deref(),
                        &mut self.role.last_tree_revision,
                    )
                    .await;
                    None
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.role.pending_resume.is_some() {
                        return self.transient_resume_prompt().await;
                    }
                    if one_shot.is_none() && self.role.config.continuation.is_some() {
                        return self.continuation_prompt().await;
                    }
                    if one_shot.is_none() && observed_tree {
                        return self.child_wake_prompt(Instant::now()).await;
                    }
                    return Ok(HostStep::Continue);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.quit_started_at.get_or_insert_with(Instant::now);
                    return Ok(HostStep::Quit);
                }
            }
        };

        Ok(match next_prompt {
            Some((text, queued_delivery, user_echoed)) => HostStep::Prompt {
                text,
                queued_delivery,
                user_echoed,
            },
            None => HostStep::Continue,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
