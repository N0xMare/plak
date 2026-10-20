//! Idle control dispatch and FIFO prompt selection.

use super::recover::RecoveryTrigger;
use super::{
    APPROVAL_REFUSED_NOTE, CliFailure, CliResult, ContextQuery, HostEvent, HostLoop, HostStep,
    IDLE_OBSERVE_POLL_INTERVAL, Instant, InterruptWatch, Rebound, RecvTimeoutError,
    SessionQueueDrainState, TREE_OBSERVE_INTERVAL, UserAction, admit_idle_immediate_prompt,
    clear_session_rebuild, commit_session_update, compact_may_resume_queue,
    compact_reason_for_disposition, dispatch_invoke_tool_sync, dispatch_topology_navigator_sync,
    drop_last_mutable, edit_mutable, emit_queue_snapshot, emit_tree_snapshot_if_revision_advanced,
    emit_unavailable_context, handle_compact, handle_fork, handle_rebind, handle_resume_picker,
    move_mutable, native_compact_facet, observe_compact, query_context, queue, queue_edit_applied,
    queue_edit_refused, queue_refusal, record_emitted_tree_revision, recovery_blocked_action,
    refresh_credential_readiness, refuse_recovery_blocked_action, reject_draft,
    reject_stale_queue_action, remove_mutable, report_context, resume_session_switch, sessions,
    surface,
};
use crate::runtime::admission::{
    QueuePersistAction, report_queue_persist_failed as queue_persist_failed,
};
use crate::sessions::{NoticeScope, TranscriptRow};

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn next_prompt(
        &mut self,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        crate::call_detail::note_attached(&self.config.session.id);
        self.last_tree_revision
            .child_live
            .retain_session(&self.config.session.id);
        self.update_background_process_count();
        self.publish_pending_continuation();
        // An action the operator already sent outranks the queue. Starting
        // a queued turn first would open a turn the very next iteration has to
        // cancel, so a deferred delivery is drained before `pending_start`.
        let deferred_first = self.interrupt_state.borrow().has_deferred();
        let next_prompt = if let Some(item) = (!deferred_first)
            .then(|| self.pending_start.take())
            .flatten()
        {
            Some((item.text.clone(), Some(item), false))
        } else {
            if !self.config.attached_one_shot {
                self.subscription_usage_refresh.poll_idle(
                    &self.held_usage,
                    &self.session_ledger,
                    self.config.target.native_subscriptions().cloned(),
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
            let observed_tree = self.last_tree_observed_at.elapsed() >= TREE_OBSERVE_INTERVAL;
            if observed_tree {
                self.last_tree_observed_at = Instant::now();
                let _ = self.executor.block_on(async {
                    emit_tree_snapshot_if_revision_advanced(
                        &crate::runtime::HostRows::new(
                            &self.config.state_roots,
                            self.config.store.as_ref(),
                        ),
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        &mut self.last_tree_revision,
                        &self.events,
                        false,
                    );
                });
            }
            let mut acknowledgement = None;
            let deferred = self.interrupt_state.borrow_mut().take_deferred();
            let received = deferred
                .map_or_else(|| self.actions.recv_timeout(IDLE_OBSERVE_POLL_INTERVAL), Ok)
                .map(|delivery| {
                    if let Some(started) = delivery.started() {
                        let _ = self.events.send(started);
                    }
                    let (action, guard) = delivery.into_parts();
                    acknowledgement = Some(guard);
                    action
                });
            let cancelled_resume = received.is_ok() && self.pending_resume.is_some();
            if received.is_ok() {
                self.cancel_transient_resume();
            }
            // A session a fenced turn stopped recovers on the next action that
            // needs it, which then proceeds as though nothing had happened.
            let received = match received {
                Ok(action) if self.recovery_blocked && recovery_blocked_action(&action) => {
                    match self.recover_session(RecoveryTrigger::Action) {
                        Ok(()) => {
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
                            refuse_recovery_blocked_action(&self.events, &action, &failure);
                            return Ok(HostStep::Continue);
                        }
                    }
                }
                other => other,
            };
            match received {
                Ok(UserAction::StopProcesses { id, interrupt: _ }) => {
                    if let Err(error) = self
                        .config
                        .state_roots
                        .job_registry()
                        .stop_user(id.as_deref())
                    {
                        let _ = self
                            .events
                            .notice(TranscriptRow::error(NoticeScope::Session, error));
                    }
                    self.update_background_process_count();
                    None
                }
                Ok(UserAction::Recover) => {
                    if self.recovery_blocked {
                        if let Err(failure) = self.recover_session(RecoveryTrigger::Explicit) {
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Session,
                                failure.message().to_owned(),
                            ));
                        }
                    } else {
                        let nothing = (!self.recovered_at_open).then(|| {
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
                    if self.config.session.store_revision != 0 {
                        let _ = commit_session_update(
                            &*self.config.store,
                            &self.config.home,
                            &mut self.config.session,
                            |candidate| {
                                candidate.queue_drain_state =
                                    SessionQueueDrainState::RestoredPaused;
                                true
                            },
                        );
                    }
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
                        reject_draft(&self.events, action);
                        None
                    } else {
                        let mut queued = None;
                        let mut refusal = None;
                        match commit_session_update(
                            &*self.config.store,
                            &self.config.home,
                            &mut self.config.session,
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
                                admit_idle_immediate_prompt(
                                    &self.events,
                                    &self.config.session,
                                    action,
                                    item,
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
                                    &self.events,
                                    "queue admission",
                                    refusal.unwrap_or(queue::Refusal::Stale),
                                );
                                reject_draft(&self.events, action);
                                None
                            }
                            Err(error) => {
                                queue_persist_failed(
                                    &self.events,
                                    &self.config.home,
                                    &self.config.session,
                                    QueuePersistAction::Admission,
                                    &error,
                                );
                                if let Some(state) = one_shot.as_deref_mut() {
                                    state.result = Some(Err(CliFailure::new(error)));
                                }
                                reject_draft(&self.events, action);
                                None
                            }
                        }
                    }
                }
                Ok(action @ UserAction::Steer(_)) => {
                    let _ = self.events.send(HostEvent::Status {
                        phase: "idle".into(),
                        queue_len: self.config.session.pending_queue.len(),
                        detail: Some("no active turn".into()),
                    });
                    reject_draft(&self.events, action);
                    None
                }
                Ok(UserAction::Interrupt) => {
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
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        drop_last_mutable,
                    ) {
                        Ok(true) => emit_queue_snapshot(&self.events, &self.config.session),
                        Ok(false) => {}
                        Err(error) => queue_persist_failed(
                            &self.events,
                            &self.config.home,
                            &self.config.session,
                            QueuePersistAction::Drop,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::DeleteQueued { id, revision }) => {
                    match commit_session_update(
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        |candidate| remove_mutable(candidate, &id, revision),
                    ) {
                        Ok(true) => emit_queue_snapshot(&self.events, &self.config.session),
                        Ok(false) => reject_stale_queue_action(&self.events, &self.config.session),
                        Err(error) => queue_persist_failed(
                            &self.events,
                            &self.config.home,
                            &self.config.session,
                            QueuePersistAction::Delete,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::EditQueued { id, revision, text }) => {
                    let mut refusal = None;
                    match commit_session_update(
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        |candidate| match edit_mutable(candidate, &id, revision, &text) {
                            Ok(()) => true,
                            Err(error) => {
                                refusal = Some(error);
                                false
                            }
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.events, &self.config.session);
                            queue_edit_applied(&self.events, id);
                        }
                        Ok(false) => {
                            emit_queue_snapshot(&self.events, &self.config.session);
                            queue_edit_refused(&self.events, id);
                            queue_refusal(
                                &self.events,
                                "queue edit",
                                refusal.unwrap_or(queue::Refusal::Stale),
                            );
                        }
                        Err(error) => {
                            queue_persist_failed(
                                &self.events,
                                &self.config.home,
                                &self.config.session,
                                QueuePersistAction::Edit,
                                &error,
                            );
                            queue_edit_refused(&self.events, id);
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
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        |candidate| move_mutable(candidate, &id, revision, new_index),
                    ) {
                        Ok(true) => emit_queue_snapshot(&self.events, &self.config.session),
                        Ok(false) => reject_stale_queue_action(&self.events, &self.config.session),
                        Err(error) => queue_persist_failed(
                            &self.events,
                            &self.config.home,
                            &self.config.session,
                            QueuePersistAction::Move,
                            &error,
                        ),
                    }
                    None
                }
                Ok(UserAction::SendQueuedNow { id, revision }) => {
                    if self.config.session.queue_projection_debt {
                        emit_queue_snapshot(&self.events, &self.config.session);
                        let _ = self.events.send(HostEvent::Status {
                            phase: "idle".into(),
                            queue_len: self.config.session.pending_queue.len(),
                            detail: Some(
                                "queue projection is catching up; prompt remains queued".into(),
                            ),
                        });
                        return Ok(HostStep::Continue);
                    }
                    let item = queue::get(&self.config.session.pending_queue, &id, revision);
                    if let Some(item) = item {
                        Some((item.text.clone(), Some(item), false))
                    } else {
                        reject_stale_queue_action(&self.events, &self.config.session);
                        None
                    }
                }
                Ok(UserAction::ResolveApproval { .. }) => {
                    // Decisions exist only inside an admitted turn; idle, there
                    // is nothing to resolve and the runtime is not contacted.
                    let _ = self.events.notice(TranscriptRow::error(
                        NoticeScope::Approval,
                        APPROVAL_REFUSED_NOTE,
                    ));
                    None
                }
                Ok(UserAction::ResumeQueueDrain) => {
                    match commit_session_update(
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        |candidate| {
                            candidate.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
                            true
                        },
                    ) {
                        Ok(true) => {
                            emit_queue_snapshot(&self.events, &self.config.session);
                            queue::next_drainable(&self.config.session)
                                .map(|item| (item.text.clone(), Some(item), false))
                        }
                        Ok(false) => None,
                        Err(error) => {
                            queue_persist_failed(
                                &self.events,
                                &self.config.home,
                                &self.config.session,
                                QueuePersistAction::ResumeDrain,
                                &error,
                            );
                            None
                        }
                    }
                }
                Ok(UserAction::ClearSession) => {
                    // A switch closes the agent view before its ClearView.
                    self.last_tree_revision.child_live.close_agent_view();
                    let _ = self.events.send(HostEvent::SetBusy(true));
                    let _ = self.events.send(HostEvent::Status {
                        phase: "clear".into(),
                        queue_len: self.config.session.pending_queue.len(),
                        detail: Some(
                            "creating a fresh conversation on the current exact binding".into(),
                        ),
                    });
                    match clear_session_rebuild(
                        &self.executor,
                        &mut self.config,
                        &mut self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        self.log.as_ref(),
                    ) {
                        Ok(()) => {
                            self.recovery_blocked = false;
                        }
                        Err(err) => {
                            let _ = self.events.notice(TranscriptRow::info(
                                NoticeScope::Session,
                                format!(
                                    "clear session failed; previous session remains active: {}",
                                    err.message()
                                ),
                            ));
                            let _ = self.events.send(HostEvent::SetBusy(false));
                            let _ = self.events.send(HostEvent::Status {
                                phase: "idle".into(),
                                queue_len: self.config.session.pending_queue.len(),
                                detail: Some(format!(
                                    "clear refused · {} remains active",
                                    self.config.model_id
                                )),
                            });
                        }
                    }
                    None
                }
                Ok(UserAction::Compact) => {
                    let current =
                        query_context(&self.executor, &self.opened, ContextQuery::Current);
                    // Every typed disposition asks for a compact now, so there
                    // is no disposition left to refuse up front. A
                    // protected-only tail reaches the placeholder rung, and
                    // `handle_compact` reports an exhausted ladder. Only an
                    // unreadable context authority still refuses here.
                    let reason = match current.as_ref().map(|projection| projection.disposition()) {
                        Ok(disposition) => compact_reason_for_disposition(disposition),
                        Err(error) => {
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Compaction,
                                format!(
                                    "compact refused: current context authority is unavailable: {}",
                                    error.message()
                                ),
                            ));
                            return Ok(HostStep::Continue);
                        }
                    };
                    let capacity_recovery =
                        reason == sessions::ProductCompactReason::CapacityRecovery;
                    observe_compact(&self.events, self.config.session.pending_queue.len(), true);
                    let compacted = handle_compact(
                        &self.executor,
                        InterruptWatch::new(self.actions, &self.interrupt_state),
                        &mut self.config,
                        &self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        self.log.as_ref(),
                        reason,
                        None,
                    );
                    observe_compact(&self.events, self.config.session.pending_queue.len(), false);
                    if let Err(error) = compacted {
                        let _ = self.events.notice(TranscriptRow::error(
                            NoticeScope::Compaction,
                            format!("compact: {}", error.message()),
                        ));
                        None
                    } else if compact_may_resume_queue(
                        capacity_recovery,
                        self.config.attached_one_shot,
                        self.config.session.queue_drain_state,
                    ) {
                        queue::next_drainable(&self.config.session)
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
                    let sessions = self.config.state_roots.runtime_sessions().ok();
                    let events = self.events.clone();
                    if let Some(request) = crate::call_detail::prepare(
                        &self.config.topology_rows(),
                        &self.config.session,
                        call_id.clone(),
                        agent_id.clone(),
                        &session_id,
                    ) {
                        // Only enters the runtime to spawn the read; the answer
                        // arrives as its own observation.
                        if let Err(error) = self
                            .executor
                            .block_on(async move { request.spawn(sessions, events) })
                        {
                            let _ = self.events.send(HostEvent::CallDetail(
                                crate::call_detail::unavailable(
                                    call_id,
                                    agent_id,
                                    self.config.session.id.clone(),
                                    format!("call detail could not run: {error}"),
                                ),
                            ));
                        }
                    }
                    None
                }
                Ok(UserAction::RefreshTemplates) => {
                    surface::publish_template_readiness(
                        self.config.surface.as_deref(),
                        &self.config.ready_catalog.accounts,
                        &self.events,
                    );
                    None
                }
                Ok(UserAction::ResumeSession { id: None }) => {
                    handle_resume_picker(&self.config, &self.events);
                    None
                }
                Ok(UserAction::ResumeSession { id: Some(id) }) => {
                    self.last_tree_revision.child_live.close_agent_view();
                    match resume_session_switch(
                        &self.executor,
                        &mut self.config,
                        &mut self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        &id,
                    ) {
                        Ok(()) => None,
                        Err(err) => {
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Session,
                                format!("resume: {}", err.message()),
                            ));
                            None
                        }
                    }
                }
                Ok(UserAction::ResumeSessionHandoff { .. }) => {
                    let _ = self.events.notice(TranscriptRow::error(
                        NoticeScope::Session,
                        "resume relaunch must be handled by the terminal owner",
                    ));
                    None
                }
                Ok(UserAction::ForkSession { account, compact }) => {
                    self.last_tree_revision.child_live.close_agent_view();
                    let _ = self.events.send(HostEvent::SetBusy(true));
                    let _ = self.events.send(HostEvent::Status {
                        phase: "fork".into(),
                        queue_len: self.config.session.pending_queue.len(),
                        detail: Some("forking session".into()),
                    });
                    let forked = handle_fork(
                        &self.executor,
                        InterruptWatch::new(self.actions, &self.interrupt_state),
                        &mut self.config,
                        &mut self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        account.as_deref(),
                    );
                    let _ = self.events.send(HostEvent::SetBusy(false));
                    let _ = self.events.send(HostEvent::Status {
                        phase: "idle".into(),
                        queue_len: self.config.session.pending_queue.len(),
                        detail: None,
                    });
                    match forked {
                        Ok(()) if compact => {
                            let current =
                                query_context(&self.executor, &self.opened, ContextQuery::Current);
                            let reason = match current
                                .as_ref()
                                .map(|projection| projection.disposition())
                            {
                                Ok(disposition) => compact_reason_for_disposition(disposition),
                                Err(error) => {
                                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Compaction, format!(
                                        "compact refused: current context authority is unavailable: {}",
                                        error.message()
                                    )));
                                    let _ = self.events.notice(TranscriptRow::info(NoticeScope::Compaction,
                                        "the forked session is active; /compact retries the reduction"
                                            ,
                                    ));
                                    return Ok(HostStep::Continue);
                                }
                            };
                            observe_compact(
                                &self.events,
                                self.config.session.pending_queue.len(),
                                true,
                            );
                            let compacted = handle_compact(
                                &self.executor,
                                InterruptWatch::new(self.actions, &self.interrupt_state),
                                &mut self.config,
                                &self.opened,
                                &mut self.stderr_buf,
                                &self.events,
                                self.log.as_ref(),
                                reason,
                                None,
                            );
                            observe_compact(
                                &self.events,
                                self.config.session.pending_queue.len(),
                                false,
                            );
                            if let Err(error) = compacted {
                                let _ = self.events.notice(TranscriptRow::error(
                                    NoticeScope::Compaction,
                                    format!("compact: {}", error.message()),
                                ));
                                let _ = self.events.notice(TranscriptRow::info(
                                    NoticeScope::Compaction,
                                    "the forked session is active; /compact retries the reduction",
                                ));
                            }
                        }
                        Ok(()) => {}
                        Err(err) => {
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Session,
                                format!("fork: {}", err.message()),
                            ));
                        }
                    }
                    None
                }
                // Publish host-owned meters before invoking a driver that requests them.
                Ok(UserAction::Extension(intent)) => {
                    if surface::needs_usage(self.config.surface.as_deref(), &intent) {
                        if let Err(error) = report_context(
                            &self.executor,
                            &self.opened,
                            &self.events,
                            self.config
                                .target
                                .native_provider()
                                .is_some_and(native_compact_facet),
                            self.config.session.context_counters(),
                        ) {
                            emit_unavailable_context(&self.events);
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Session,
                                error.message().to_owned(),
                            ));
                        }
                        if !self.config.attached_one_shot {
                            self.subscription_usage_refresh
                                .request(self.config.target.native_subscriptions().cloned());
                        }
                    }
                    let with_tools =
                        surface::needs_session_tools(self.config.surface.as_deref(), &intent);
                    let effect = surface::dispatch(
                        self.config.surface.as_deref(),
                        intent,
                        surface::SurfaceFacts {
                            session: Some(surface::session_facts(
                                &self.config.session,
                                Some(&self.config.topology_rows()),
                                with_tools,
                            )),
                            phase: surface::SurfacePhase::Idle,
                            attached_one_shot: self.config.attached_one_shot,
                            catalog: &self.config.ready_catalog.accounts,
                            queue_len: self.config.session.pending_queue.len(),
                        },
                        &self.events,
                    );
                    if let Some(saved) = effect.saved {
                        self.config.root_template = saved.root_template;
                    }
                    if let Some(fork) = effect.profile_fork {
                        self.last_tree_revision.child_live.close_agent_view();
                        let _ = self.events.send(HostEvent::SetBusy(true));
                        let _ = self.events.send(HostEvent::Status {
                            phase: "rebind".into(),
                            queue_len: self.config.session.pending_queue.len(),
                            detail: Some(format!("forking onto profile root `{}`", fork.model)),
                        });
                        let result = handle_rebind(
                            &self.executor,
                            InterruptWatch::new(self.actions, &self.interrupt_state),
                            &mut self.config,
                            &mut self.opened,
                            &mut self.stderr_buf,
                            &self.events,
                            &fork.account,
                            &fork.model,
                            fork.effort.as_deref(),
                            fork.extra.as_deref(),
                            fork.composition,
                            fork.protocol,
                            &fork.roster,
                        );
                        let _ = self.events.send(HostEvent::SetBusy(false));
                        let _ = self.events.send(HostEvent::Status {
                            phase: "idle".into(),
                            queue_len: self.config.session.pending_queue.len(),
                            detail: Some(format!("ready · {}", self.config.model_id)),
                        });
                        (fork.finish)(match result {
                            Ok(Rebound::Switched) => surface::ProfileForkOutcome::Forked,
                            Ok(Rebound::Unchanged) => surface::ProfileForkOutcome::AlreadyActive,
                            Err(error) => {
                                surface::ProfileForkOutcome::Refused(error.message().to_owned())
                            }
                        });
                    }
                    None
                }
                Ok(UserAction::RefreshReadiness) => {
                    refresh_credential_readiness(&mut self.config, &self.events);
                    None
                }
                Ok(UserAction::RetryAttach) => {
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Session,
                        "already attached — /attach is for the not-ready shell",
                    ));
                    None
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
                    dispatch_topology_navigator_sync(
                        &self.executor,
                        &self.config.topology_rows(),
                        &self.config.state_roots,
                        &self.config.session,
                        &self.events,
                        &action,
                        &mut self.last_tree_revision,
                    );
                    record_emitted_tree_revision(
                        &self.config.topology_rows(),
                        &self.config.session,
                        &mut self.last_tree_revision,
                    );
                    None
                }
                Ok(UserAction::InvokeTool {
                    correlation_id,
                    name,
                    arguments,
                    agent_id,
                }) => {
                    dispatch_invoke_tool_sync(
                        &self.executor,
                        &crate::runtime::HostRows::new(
                            &self.config.state_roots,
                            self.config.store.as_ref(),
                        ),
                        &*self.config.store,
                        &self.config.home,
                        &mut self.config.session,
                        &self.events,
                        &name,
                        &arguments,
                        agent_id.as_deref(),
                        correlation_id.as_deref(),
                        &mut self.last_tree_revision,
                    );
                    None
                }
                Err(RecvTimeoutError::Timeout) => {
                    if self.pending_resume.is_some() {
                        return self.transient_resume_prompt();
                    }
                    if one_shot.is_none() && observed_tree {
                        return self.child_wake_prompt(Instant::now());
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
mod tests {
    use super::*;
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::session_log;
    use std::sync::Arc;

    #[test]
    fn queue_save_failure_logs_safe_classification_and_preserves_queue_and_draft() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.session.enqueue_live("private older prompt").unwrap();
        config.store.save(&mut config.session).unwrap();
        let durable_store = Arc::clone(&config.store);
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &action_rx,
            crate::runtime::Sender::over(event_tx),
            None,
        )
        .unwrap();
        let before = host.config.session.clone();
        let stored_before = durable_store.load(&before.id).unwrap();
        let _ = event_rx.try_iter().collect::<Vec<_>>();

        for (reason, class) in [
            ("product session changed concurrently", "concurrent_change"),
            (
                "product session changed concurrently: private backend path /secret/token",
                "store_failure",
            ),
        ] {
            host.config.store = Arc::new(crate::runtime::fault_store::FaultStore::refusing(reason));
            let draft = "private unsaved draft";
            actions.send(UserAction::Prompt(draft.into())).unwrap();
            assert!(matches!(
                host.next_prompt(None).unwrap(),
                HostStep::Continue
            ));
            assert_eq!(host.config.session, before);
            assert_eq!(durable_store.load(&before.id).unwrap(), stored_before);
            let observed = event_rx.try_iter().collect::<Vec<_>>();
            assert!(observed.iter().any(|event| matches!(
                event,
                HostEvent::QueueSnapshot { items, .. }
                    if items.len() == 1 && items[0].text == "private older prompt"
            )));
            assert!(observed.iter().any(|event| matches!(
                event,
                HostEvent::DraftRejected(UserAction::Prompt(text)) if *text == draft
            )));
            assert!(!observed.iter().any(|event| matches!(
                event,
                HostEvent::DraftAccepted(_) | HostEvent::PromptAdmitted(_)
            )));

            let records = session_log::read_tail_for_kind(
                &host.config.home,
                &before.id,
                10,
                Some("queue.persist_failed"),
            )
            .unwrap();
            let raw = records.last().unwrap();
            let event: serde_json::Value = serde_json::from_str(raw).unwrap();
            assert_eq!(event["kind"], "queue.persist_failed");
            assert_eq!(event["action"], "admission");
            assert_eq!(event["failure_class"], class);
            assert_eq!(event["attempted_store_revision"], before.store_revision);
            assert_eq!(event.as_object().unwrap().len(), 6);
            assert!(!raw.contains("private"));
            assert!(!raw.contains("/secret/token"));
        }

        // A stale queue action is a refusal, not a failed store write.
        actions
            .send(UserAction::DeleteQueued {
                id: "missing".into(),
                revision: 1,
            })
            .unwrap();
        assert!(matches!(
            host.next_prompt(None).unwrap(),
            HostStep::Continue
        ));
        let failures = session_log::read_tail_for_kind(
            &host.config.home,
            &before.id,
            10,
            Some("queue.persist_failed"),
        )
        .unwrap();
        assert_eq!(failures.len(), 2, "one diagnostic per failed action");
        host.config.store = durable_store;
        actions.send(UserAction::DropQueued).unwrap();
        assert!(matches!(
            host.next_prompt(None).unwrap(),
            HostStep::Continue
        ));
        assert_eq!(
            host.config.session.pending_queue,
            [] as [queue::QueueItem; 0]
        );
        let after_success = session_log::read_tail_for_kind(
            &host.config.home,
            &before.id,
            10,
            Some("queue.persist_failed"),
        )
        .unwrap();
        assert_eq!(after_success, failures, "successful saves are not failures");
        host.release().unwrap();
    }
}
