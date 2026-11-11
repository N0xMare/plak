//! Root prompt preflight, dispatch, terminal publication and queue disposition.
use super::RootRecovery;

use super::{
    CliFailure, CliResult, ContextQuery, HostStep, Instant, InterruptWatch, RootActor,
    ScheduledPrompt, SessionEvent, SessionLog, SessionQueueDrainState, SessionQueueItem,
    TerminalPhase, TurnHostOutcome, commit_session_update, drive_one_turn, durable_tree_id,
    emit_context_projection, emit_queue_snapshot, emit_terminal_status, emit_unavailable_context,
    host_outcome_blocks_continuation, last_enrolled_source_bytes, log_ev, observe_compact, persist,
    query_context, queue, queue_persist_failed, reject_draft, run_scheduled_compaction_until_fits,
    scheduled_compaction_disabled_note, send_session_info, sessions, topology_state,
};
use super::{HostBoundary, HostWorkflow, observe_boundary};
use crate::runtime::admission::{QueuePersistAction, log_queue_persist_failed};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::HostPhase;

pub(super) mod preparation;

enum PromptOutcome {
    Stopped(preparation::Stopped),
    Refused,
    Dispatched(HostStep),
}

struct PromptInput {
    user_text: String,
    queued_delivery: Option<SessionQueueItem>,
    user_echoed: bool,
    host_authored: bool,
    transient_resume: bool,
    images: std::sync::Arc<[plakous_host_protocol::DraftImage]>,
}

struct PromptMail {
    canonical_command: String,
    selected_mail: Option<sessions::ResultMailBatch>,
    replayed_mail: bool,
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    pub(super) async fn run_prompt(
        &mut self,
        raw_text: String,
        queued_delivery: Option<SessionQueueItem>,
        user_echoed: bool,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        let wake = queued_delivery
            .as_ref()
            .and_then(|item| self.role.config.session.queue_commands.pending_for(item))
            .filter(|submission| submission.kind == sessions::projector::SubmissionKind::HostWake)
            .cloned();
        if self.refuse_archived_prompt(&raw_text)? {
            return Ok(HostStep::Continue);
        }
        // A child wake already passed the capacity check before its immutable
        // receipt was recorded. Do not turn that receipt into suppression here.
        if wake.is_none() {
            let admission = self
                .role
                .pending_storage_admission
                .take()
                .unwrap_or_else(|| self.role.config.store.storage_admission());
            super::super::info::send_session_info_with_storage(
                &self.role.events,
                &self.role.config,
                &admission,
            );
            if let crate::session_store::StorageAdmission::Refused(_) = admission {
                let reason = "new turns refused: operator storage is below the admission floor; input remains queued; inspect storage health";
                if let Some(state) = one_shot {
                    state.result = Some(Err(CliFailure::from(
                        crate::session_store::RecordStoreError::refused(
                            crate::error::StoreRefusal::LowSpace,
                            reason,
                        ),
                    )));
                }
                self.role.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                emit_queue_snapshot(&self.role.events, &self.role.config.session);
                if queued_delivery.is_none() {
                    reject_draft(
                        &self.role.events,
                        super::UserAction::Prompt(raw_text.into()),
                    );
                }
                // Operator-only feedback never enters the transcript/export.
                let _ = self.role.events.send(ContentEvent::StorageAdmissionRefused(
                    plakous_host_protocol::status::DiagnosticText::new(reason),
                ));
                return Ok(HostStep::Continue);
            }
        }
        let echoed = self.take_echoed_prompt(queued_delivery.as_ref());
        let before_recovery = self.role.recovery;
        let before_drain = self.role.config.session.queue_drain_state;
        let _ = self.role.events.send(ContentEvent::SetBusy(true));
        let outcome = self
            .run_prompt_inner(raw_text, queued_delivery, user_echoed, one_shot)
            .await;
        let stopped = match &outcome {
            Ok(PromptOutcome::Stopped(stopped)) => Some(*stopped),
            _ => None,
        };
        if stopped.is_some() && echoed {
            self.end_stopped_echo();
        }
        if !matches!(outcome, Ok(PromptOutcome::Dispatched(_)))
            && !self.role.events.root_boundary().1
        {
            emit_terminal_status(
                &self.role.events,
                if self.role.recovery.blocked() {
                    TerminalPhase::RecoveryPending
                } else {
                    TerminalPhase::AdmissionRejected
                },
                self.role.config.session.pending_queue.len(),
            );
        }
        if self.role.events.root_boundary().0 {
            let _ = self.role.events.send(ContentEvent::SetBusy(false));
        }
        if let Some(intent) = &wake
            && let Err(error) = self
                .discard_undispatched_wake(intent, before_recovery, before_drain, stopped)
                .await
        {
            return self.suppress_wake_error(&error);
        }
        if wake.is_some()
            && let Err(error) = outcome
        {
            return self.suppress_wake_error(&error);
        }
        outcome.map(|outcome| match outcome {
            PromptOutcome::Dispatched(step) => step,
            PromptOutcome::Stopped(_) | PromptOutcome::Refused => HostStep::Continue,
        })
    }

    fn suppress_wake_error(&mut self, error: &CliFailure) -> CliResult<HostStep> {
        self.suppress_child_wake(None)?;
        let _ = self.role.events.notice(
            TranscriptRow::error(NoticeScope::Queue, error.message().to_owned())
                .with_optional_remedy(error.remedy()),
        );
        Ok(HostStep::Continue)
    }

    fn refuse_archived_prompt(&self, raw_text: &str) -> CliResult<bool> {
        let archived = if self.role.config.session.store_revision == 0 {
            self.role.config.session.archived
        } else {
            self.role
                .config
                .store
                .load(&self.role.config.session.id)
                .map_err(CliFailure::new)?
                .archived
        };
        if archived {
            reject_draft(
                &self.role.events,
                super::UserAction::Prompt(raw_text.into()),
            );
            let _ = self.role.events.notice(TranscriptRow::info(
                NoticeScope::Session,
                "session_archived: unarchive this session before admitting work",
            ));
        }
        Ok(archived)
    }

    /// Whether `delivery` is the idle prompt whose row the host echoed.
    fn take_echoed_prompt(&mut self, delivery: Option<&SessionQueueItem>) -> bool {
        self.role
            .echoed_prompt
            .take()
            .is_some_and(|id| delivery.is_some_and(|item| item.id == id))
    }

    /// The operator already sees the stopped prompt's row: a stop before its
    /// admission ends it with the marker a stopped turn leaves, never as a
    /// bare prompt. The prompt itself stays queued.
    fn end_stopped_echo(&self) {
        let _ = self
            .role
            .events
            .send(ContentEvent::Row(TranscriptRow::Interrupted));
    }

    async fn discard_undispatched_wake(
        &mut self,
        intent: &sessions::projector::Submission,
        before_recovery: RootRecovery,
        before_drain: SessionQueueDrainState,
        stopped: Option<preparation::Stopped>,
    ) -> CliResult<()> {
        let restored_drain = if stopped.is_some() {
            SessionQueueDrainState::RestoredPaused
        } else {
            before_drain
        };
        if !self
            .role
            .config
            .store
            .load_intents(&self.role.config.session.id)
            .map_err(CliFailure::new)?
            .submissions
            .iter()
            .any(|submission| submission.command_id == intent.command_id)
        {
            self.role.config.session.queue_drain_state = restored_drain;
            self.role.recovery = if self.opened.agent().is_none() && !before_recovery.blocked() {
                RootRecovery::Due
            } else {
                before_recovery
            };
            emit_queue_snapshot(&self.role.events, &self.role.config.session);
            return Ok(());
        }
        let views = super::super::projection::project_to_tail(
            self.opened.sessions(),
            &self.role.config.store,
            &self.role.config.session.id,
            self.opened.locator(),
        )
        .await
        .map_err(CliFailure::new)?;
        if !matches!(
            views.state.deliveries.get(&intent.command_id),
            Some(sessions::projector::Delivery::Accepted)
        ) {
            self.role.config.session.pending_queue.retain(|item| {
                item.id != intent.queue_item_id || item.revision != intent.queue_item_revision
            });
            self.role.config.session.queue_drain_state = restored_drain;
            if stopped.is_none() {
                self.suppress_child_wake(None)?;
            }
            self.role.recovery = if self.opened.agent().is_none() && !before_recovery.blocked() {
                RootRecovery::Due
            } else {
                before_recovery
            };
            if let Err(error) = self.role.config.store.discard_submission(
                &mut self.role.config.session,
                &intent.command_id,
                None,
            ) {
                let _ = self.role.events.notice(TranscriptRow::error(
                    NoticeScope::Tree,
                    format!("undispatched child wake cleanup could not be persisted: {error}"),
                ));
            }
            emit_queue_snapshot(&self.role.events, &self.role.config.session);
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One prompt admission owns preparation, interrupt handling, dispatch, and terminal publication as an ordered host transition."
    )]
    async fn run_prompt_inner(
        &mut self,
        raw_text: String,
        queued_delivery: Option<SessionQueueItem>,
        user_echoed: bool,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<PromptOutcome> {
        if self.role.config.session.queue_projection_debt {
            emit_queue_snapshot(&self.role.events, &self.role.config.session);
            return Ok(PromptOutcome::Refused);
        }
        let host_authored = queued_delivery
            .as_ref()
            .is_some_and(|item| super::wake::is_wake_delivery(&self.role.config.session, item));
        // A host continuation of the root's own work (a transient resume, or
        // the continuation after a lost host) is host-authored but no child
        // wake: it prepares its context and authority as a prompt does.
        let transient_resume = queued_delivery.as_ref().is_some_and(|item| {
            self.role
                .config
                .session
                .queue_commands
                .pending_for(item)
                .is_some_and(|submission| {
                    submission
                        .command_id
                        .starts_with(crate::driver::transient::ROOT_COMMAND_PREFIX)
                        || crate::runtime::root_loss::is_root_loss_command(&submission.command_id)
                })
        });
        if !host_authored {
            self.cancel_transient_resume();
            self.role.transient_resumes_used = 0;
            self.role.child_wake = super::wake::RootWake::default();
        }
        if !user_echoed && !host_authored {
            let _ = self.role.events.send(ContentEvent::PromptStarted(
                queued_delivery.as_ref().map(|item| item.protocol(false)),
            ));
        }
        if self.role.log.is_none() && self.role.config.session.store_revision != 0 {
            self.role.log =
                SessionLog::open(&self.role.config.home, &self.role.config.session.id).ok();
        }

        let user_text = raw_text;
        // The images the prompt carries are part of every pending-input
        // measurement, so the estimate counts them by the dialect's bound.
        let images = queued_delivery
            .as_ref()
            .map(|item| std::sync::Arc::clone(&item.images))
            .unwrap_or_default();
        let watch = InterruptWatch::new(self.role.actions, &self.role.interrupt_state);
        watch.begin();
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Preparing,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: None,
        });
        match self
            .prepare_prompt_authority(host_authored, transient_resume, &mut one_shot)
            .await?
        {
            std::ops::ControlFlow::Break(preparation::End::Stopped(stopped)) => {
                return Ok(PromptOutcome::Stopped(stopped));
            }
            std::ops::ControlFlow::Break(preparation::End::Refused) => {
                return Ok(PromptOutcome::Refused);
            }
            std::ops::ControlFlow::Continue(()) => {}
        }
        let rows = self.role.config.topology_rows();
        preparation::run_step(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.prep_hooks,
            preparation::Stage::Tree,
            Box::pin(super::super::projection::root_tree_pass(
                self.opened.sessions(),
                &rows,
                &durable_tree_id(&rows, &self.role.config.session),
                self.opened.locator().session_id(),
                super::super::projection::RootTreePass::Admission,
            )),
        )
        .await
        .map_err(CliFailure::from)?;
        if let Some(stopped) = self.stop_preparation(&mut one_shot)? {
            return Ok(PromptOutcome::Stopped(stopped));
        }
        // A replayed delivery's command and batch are immutable.
        let mut replayed_mail = false;
        let (canonical_command, selected_mail) = if let Some(intent) = queued_delivery
            .as_ref()
            .and_then(|item| self.role.config.session.queue_commands.pending_for(item))
        {
            // An immutable command may include a stored batch. Never trim or
            // rebuild it after a scheduled pass.
            replayed_mail = intent.canonical_command != user_text;
            (intent.canonical_command.clone(), None)
        } else {
            let selected = super::super::result_mail::select(
                &self.role.config.topology_rows(),
                &self.role.config.session,
                &user_text,
                super::super::result_mail::LossNotice::Always,
            );
            #[cfg(test)]
            let selected = self.role.prep_hooks.mail_error.take().map_or(selected, Err);
            match selected {
                Ok(selected) => selected,
                Err(error) => {
                    if let Some(state) = one_shot.as_deref_mut() {
                        state.result = Some(Err(CliFailure::new(error.clone())));
                    }
                    let _ = self.role.events.notice(TranscriptRow::error(
                        NoticeScope::Tree,
                        format!("child mail preparation failed: {error}"),
                    ));
                    return Ok(PromptOutcome::Refused);
                }
            }
        };

        self.preflight_prompt(
            PromptInput {
                user_text,
                queued_delivery,
                user_echoed,
                host_authored,
                transient_resume,
                images,
            },
            PromptMail {
                canonical_command,
                selected_mail,
                replayed_mail,
            },
            one_shot,
        )
        .await
    }

    async fn prepare_prompt_authority(
        &mut self,
        host_authored: bool,
        transient_resume: bool,
        one_shot: &mut Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<std::ops::ControlFlow<preparation::End>> {
        if (!host_authored || transient_resume)
            && let Err(error) = self.refresh_native_authority_before_prompt().await
        {
            if self.opened.agent().is_none() {
                self.role.recovery = RootRecovery::Due;
            }
            if let Some(state) = one_shot.as_deref_mut() {
                state.result = Some(Err(error.clone()));
            }
            self.pause_unrecorded(error.message().to_owned())?;
            let _ = self.role.events.notice(TranscriptRow::error(
                NoticeScope::Queue,
                format!(
                    "fresh model authority: {}; the prompt remains queued and drain stays paused",
                    error.message()
                ),
            ).with_optional_remedy(error.remedy()));
            return Ok(std::ops::ControlFlow::Break(preparation::End::Refused));
        }
        if let Some(stopped) = self.stop_preparation(one_shot)? {
            return Ok(std::ops::ControlFlow::Break(preparation::End::Stopped(
                stopped,
            )));
        }

        // Receipts a terminal could not settle (its settlement failed, or it
        // left no authoritative result) settle from committed history before
        // anything is dispatched, and before mail selection so restored mail
        // rides this command (spec 03 section 4.4).
        if let Err(error) = preparation::run_step(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.prep_hooks,
            preparation::Stage::Settle,
            Box::pin(super::super::result_mail::settle_waits(
                &self.role.config.topology_rows(),
                &self.role.config.session,
                &self.opened,
            )),
        )
        .await
        {
            if let Some(state) = one_shot.as_deref_mut() {
                state.result = Some(Err(CliFailure::from(error.clone())));
            }
            let _ = self.role.events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("wait receipt settlement failed: {error}"),
            ));
            return Ok(std::ops::ControlFlow::Break(preparation::End::Refused));
        }
        if let Some(stopped) = self.stop_preparation(one_shot)? {
            return Ok(std::ops::ControlFlow::Break(preparation::End::Stopped(
                stopped,
            )));
        }

        Ok(std::ops::ControlFlow::Continue(()))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Prompt preflight reconciles queue identity, product pressure, recovery, and scheduled compaction before admitting inference."
    )]
    async fn preflight_prompt(
        &mut self,
        input: PromptInput,
        mail: PromptMail,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<PromptOutcome> {
        let PromptInput {
            user_text,
            queued_delivery,
            user_echoed,
            host_authored,
            transient_resume,
            images,
        } = input;
        let PromptMail {
            mut canonical_command,
            mut selected_mail,
            replayed_mail,
        } = mail;
        let pending_input = |command: &str| queue::user_input(command, &images);
        // One failed context query must not end the host: the TUI would only
        // see a dead channel. Park the queue and stay attached instead.
        let queried = if host_authored && !transient_resume {
            self.role
                .child_wake
                .prepared_context
                .take()
                .ok_or_else(|| CliFailure::new("child wake preflight is unavailable"))
        } else {
            #[cfg(test)]
            self.role.prep_hooks.called.push(preparation::Stage::Query);
            query_context(
                &self.opened,
                ContextQuery::WithPendingUserInput(pending_input(&canonical_command)),
            )
            .await
        };
        let mut pending_context = match queried {
            Ok(projection) => projection,
            Err(error) => {
                if host_authored {
                    let _ = self.role.events.notice(
                        TranscriptRow::error(
                            NoticeScope::Tree,
                            format!(
                                "child wake preflight: {}; mail remains unread",
                                error.message()
                            ),
                        )
                        .with_optional_remedy(error.remedy()),
                    );
                    return Ok(PromptOutcome::Refused);
                }
                if let Some(state) = one_shot.as_deref_mut() {
                    state.result = Some(Err(error.clone()));
                }
                self.pause_unrecorded(error.message().to_owned())?;
                emit_unavailable_context(&self.role.events);
                if one_shot.is_none() {
                    let _ = self.role.events.notice(
                        TranscriptRow::error(
                            NoticeScope::Queue,
                            format!(
                                "context: {}; the prompt remains queued and drain stays paused",
                                error.message()
                            ),
                        )
                        .with_optional_remedy(error.remedy()),
                    );
                } else {
                    let _ = self.role.events.notice(TranscriptRow::info(
                        NoticeScope::Queue,
                        "context query failed; the prompt remains queued and drain stays paused",
                    ));
                }
                return Ok(PromptOutcome::Refused);
            }
        };
        let enrolled_source_bytes = last_enrolled_source_bytes(&self.opened);
        emit_context_projection(
            &self.role.events,
            &pending_context,
            self.role.config.session.context_counters(),
            enrolled_source_bytes,
        );
        preparation::run_step(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.prep_hooks,
            preparation::Stage::Context,
            std::future::ready(()),
        )
        .await;
        if let Some(stopped) = self.stop_preparation(&mut one_shot)? {
            return Ok(PromptOutcome::Stopped(stopped));
        }
        if !host_authored && pending_context.compaction_verdict().is_due() {
            if self.role.config.compaction_enabled {
                // A scheduled compact is one paid round-trip the host blocks
                // on; without the busy row the TUI paints idle while every
                // prompt is silently deferred (the 2026-09-03 "freeze").
                observe_compact(
                    &self.role.events,
                    self.role.config.session.pending_queue.len(),
                    true,
                );
                #[cfg(test)]
                self.role
                    .prep_hooks
                    .called
                    .push(preparation::Stage::Compact);
                let compacted = run_scheduled_compaction_until_fits(
                    InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
                    &mut self.role.config,
                    &self.opened,
                    &mut self.role.stderr_buf,
                    &self.role.events,
                    self.role.log.as_ref(),
                    &pending_input(&canonical_command),
                    selected_mail
                        .is_some()
                        .then(|| pending_input(&user_text))
                        .as_ref(),
                )
                .await;
                if compacted.is_ok()
                    && let Some(stopped) = self.stop_preparation(&mut one_shot)?
                {
                    return Ok(PromptOutcome::Stopped(stopped));
                }
                match compacted {
                    Ok(ScheduledPrompt::Send {
                        over_window,
                        drop_mail,
                    }) => match mail_after_scheduled_send(over_window, drop_mail, replayed_mail) {
                        ScheduledMail::Keep => {}
                        ScheduledMail::Drop => {
                            selected_mail = None;
                            canonical_command.clone_from(&user_text);
                        }
                        ScheduledMail::Park => {
                            observe_compact(
                                &self.role.events,
                                self.role.config.session.pending_queue.len(),
                                false,
                            );
                            return self
                                .park_prompt(
                                    one_shot.as_deref_mut(),
                                    REPLAYED_MAIL_OVER_WINDOW_NOTE,
                                )
                                .map(|_| PromptOutcome::Refused);
                        }
                    },
                    Ok(ScheduledPrompt::Parked) => {
                        observe_compact(
                            &self.role.events,
                            self.role.config.session.pending_queue.len(),
                            false,
                        );
                        return Ok(PromptOutcome::Refused);
                    }
                    Err(error) => {
                        if let Some(state) = one_shot.as_deref_mut() {
                            state.result = Some(Err(error.clone()));
                        }
                        self.pause_unrecorded(error.message().to_owned())?;
                        observe_compact(
                            &self.role.events,
                            self.role.config.session.pending_queue.len(),
                            false,
                        );
                        if one_shot.is_none() {
                            let _ = self.role.events.notice(
                                TranscriptRow::error(
                                    NoticeScope::Compaction,
                                    format!("compact: {}", error.message()),
                                )
                                .with_optional_remedy(error.remedy()),
                            );
                        } else {
                            let _ = self.role.events.notice(TranscriptRow::info(NoticeScope::Compaction,
                                "automatic compaction failed; the prompt remains queued and drain stays paused",
                            ));
                        }
                        // A recovery-required compact fences this source for
                        // good; name it now instead of echoing and queueing
                        // every later prompt before refusing it.

                        return Ok(PromptOutcome::Refused);
                    }
                }
            } else if pending_context.compaction_verdict().overflow()
                && !self
                    .send_bare_without_mail(
                        &mut canonical_command,
                        &mut selected_mail,
                        &user_text,
                        &images,
                        &mut pending_context,
                    )
                    .await
            {
                if let Some(state) = one_shot.as_deref_mut() {
                    state.result = Some(Err(CliFailure::classified(
                        crate::error::CliFailureKind::Agent,
                        scheduled_compaction_disabled_note(pending_context.disposition()),
                    )));
                }
                self.pause_unrecorded(
                    scheduled_compaction_disabled_note(pending_context.disposition()).to_owned(),
                )?;
                if one_shot.is_none() {
                    let _ = self.role.events.notice(TranscriptRow::error(
                        NoticeScope::Compaction,
                        scheduled_compaction_disabled_note(pending_context.disposition()),
                    ));
                }
                return Ok(PromptOutcome::Refused);
            }
        }

        preparation::run_step(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.prep_hooks,
            preparation::Stage::Dispatch,
            std::future::ready(()),
        )
        .await;
        if let Some(stopped) = self.stop_preparation(&mut one_shot)? {
            return Ok(PromptOutcome::Stopped(stopped));
        }
        self.dispatch_prompt(
            PromptInput {
                user_text,
                queued_delivery,
                user_echoed,
                host_authored,
                transient_resume,
                images,
            },
            PromptMail {
                canonical_command,
                selected_mail,
                replayed_mail,
            },
            pending_context,
            enrolled_source_bytes,
            one_shot,
        )
        .await
        .map(PromptOutcome::Dispatched)
    }

    async fn dispatch_prompt(
        &mut self,
        input: PromptInput,
        mail: PromptMail,
        pending_context: crate::context::ContextProjection,
        enrolled_source_bytes: u64,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        let turn = RootTurn {
            input,
            mail,
            pending_context,
            enrolled_source_bytes,
        };
        crate::node::drive_turns(self, &mut one_shot, turn).await
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Prompt completion publishes the canonical terminal before deciding queue continuation, transient retry, and child-wake suppression."
    )]
    async fn finish_prompt(
        &mut self,
        input: PromptInput,
        drive: Result<TurnHostOutcome, String>,
        turn_started: Instant,
        quit_after_terminal: bool,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        let PromptInput {
            user_text,
            queued_delivery,
            host_authored,
            images,
            ..
        } = input;
        if matches!(&drive, Ok(TurnHostOutcome::ContextCapacity { .. })) {
            if host_authored {
                // A wake refused at the window pauses as a failed wake
                // preflight does (spec 03 section 8): its returned batch waits
                // for the next prompt. The terminal's commit persists this, and
                // the wake prompt is the host's, not the operator's draft.
                self.role.config.session.child_wake_suppressed = true;
            } else {
                let _ = self.role.events.send(ContentEvent::DraftRestored(
                    plakous_host_protocol::Draft::new(user_text.clone(), images.to_vec()),
                ));
            }
        } else if matches!(&drive, Ok(TurnHostOutcome::AdmissionRejected { .. })) {
            if queued_delivery.is_some() {
                let _ = self.role.events.notice(TranscriptRow::info(
                    NoticeScope::Queue,
                    "prompt remains queued; resume to retry, or edit a rejected item",
                ));
            } else {
                reject_draft(
                    &self.role.events,
                    super::UserAction::Prompt(plakous_host_protocol::Draft::new(
                        user_text,
                        images.to_vec(),
                    )),
                );
            }
        }
        if host_outcome_blocks_continuation(&drive) {
            self.role.recovery = RootRecovery::Due;
        }
        observe_boundary(
            self.role.config.runtime_factory.as_ref(),
            HostWorkflow::RootDelivery,
            HostBoundary::TerminalReady,
        );
        let one_shot_terminal = one_shot
            .as_ref()
            .map(|_| super::super::oneshot::terminal(&drive));
        let error_presentation = if one_shot.is_some() {
            super::publication::TerminalErrorPresentation::Return
        } else {
            super::publication::TerminalErrorPresentation::Events
        };
        let admission_rejected = matches!(&drive, Ok(TurnHostOutcome::AdmissionRejected { .. }));
        let stop_drain = matches!(
            &drive,
            Ok(TurnHostOutcome::AdmissionRejected {
                stop_drain: true,
                ..
            })
        );
        let interrupted = matches!(&drive, Ok(TurnHostOutcome::Interrupted { .. }));
        if interrupted {
            self.suppress_child_wake(None)?;
        }
        // Plain one-shot commands own exactly one turn, unlike bounded `run`.
        let resume_allowed = one_shot.as_ref().is_none_or(|state| {
            state.run.as_ref().is_some_and(|run| {
                run.turns.saturating_add(1) < run.bounds.max_turns
                    && !run.timed_out.load(std::sync::atomic::Ordering::Relaxed)
                    && run
                        .deadline
                        .is_none_or(|deadline| Instant::now() < deadline)
            })
        });
        let mut drive = drive;
        if (!resume_allowed || quit_after_terminal)
            && let Ok(TurnHostOutcome::Failed { transient, .. }) = &mut drive
        {
            *transient = None;
        }
        let completed = self
            .publish_terminal(drive, turn_started, error_presentation)
            .await;
        self.publish_projected_transcript(one_shot.is_some()).await;
        self.update_background_process_count();
        self.publish_pending_continuation();
        let _ = self.role.events.send(ContentEvent::SetBusy(false));
        emit_terminal_status(
            &self.role.events,
            if self.role.recovery.blocked() {
                TerminalPhase::RecoveryPending
            } else if interrupted {
                TerminalPhase::Interrupted
            } else if admission_rejected {
                TerminalPhase::AdmissionRejected
            } else {
                TerminalPhase::Idle
            },
            self.role.config.session.pending_queue.len(),
        );

        if let (Some(state), Some(terminal)) = (one_shot, one_shot_terminal) {
            state.result = Some(
                if (self.role.recovery.blocked() && terminal.is_ok())
                    || (!completed
                        && matches!(
                            &terminal,
                            Ok(super::super::oneshot::OneShotOutcome::Completed { .. }
                                | super::super::oneshot::OneShotOutcome::Truncated { .. }
                                | super::super::oneshot::OneShotOutcome::Refused { .. })
                        ))
                {
                    Err(CliFailure::classified(
                        crate::error::CliFailureKind::Agent,
                        "turn terminalized but its product projection could not be committed; the session recovers the next time it is used",
                    ))
                } else {
                    terminal
                },
            );
            if state.run.is_some() {
                return Ok(HostStep::Continue);
            }
            // A one-shot attaches to exactly one root turn. Retained work is
            // durable and paused; only an explicit later host may drain it.
            self.role.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            if let Err(error) = self.role.config.store.save(&mut self.role.config.session) {
                log_queue_persist_failed(
                    &self.role.config.home,
                    &self.role.config.session,
                    QueuePersistAction::PauseDrain,
                    &error,
                );
                let message = match state.result.take() {
                    Some(Err(terminal)) => format!(
                        "{}; queue pause could not be committed: {error}",
                        terminal.message()
                    ),
                    _ => format!("queue pause could not be committed: {error}"),
                };
                state.result = Some(Err(CliFailure::new(message)));
            }
            return Ok(HostStep::Quit);
        }

        if quit_after_terminal {
            log_ev(
                self.role.log.as_ref(),
                &self.role.config,
                SessionEvent::new("session.quit"),
            );
            emit_queue_snapshot(&self.role.events, &self.role.config.session);
            persist(&*self.role.config.store, &mut self.role.config.session);
            return Ok(HostStep::Quit);
        }

        self.continue_prompt_queue(completed, admission_rejected, stop_drain)
    }

    fn continue_prompt_queue(
        &mut self,
        completed: bool,
        admission_rejected: bool,
        stop_drain: bool,
    ) -> CliResult<HostStep> {
        if !completed && !admission_rejected && !self.role.config.session.pending_queue.is_empty() {
            match commit_session_update(
                &*self.role.config.store,
                &self.role.config.home,
                &mut self.role.config.session,
                |candidate| {
                    candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                    true
                },
            ) {
                Ok(_) => {}
                Err(error) => {
                    log_queue_persist_failed(
                        &self.role.config.home,
                        &self.role.config.session,
                        QueuePersistAction::PauseDrain,
                        &error,
                    );
                    queue_persist_failed(&self.role.events, &self.role.config.session);
                }
            }
        }

        // Only settled refusals can let the next item pass; an unresolved
        // delivery must keep its exact identity until an explicit retry.
        self.role.pending_start = if self.role.config.session.queue_drain_state
            == SessionQueueDrainState::LiveAutoDrain
            && (completed
                || (admission_rejected
                    && !stop_drain
                    && !self.role.config.session.queue_commands.has_pending()))
        {
            queue::next_drainable(&self.role.config.session)
        } else {
            None
        };
        emit_queue_snapshot(&self.role.events, &self.role.config.session);
        persist(&*self.role.config.store, &mut self.role.config.session);
        Ok(HostStep::Continue)
    }

    /// With compaction disabled, a prompt whose fresh child-mail batch alone
    /// overflows is sent bare when the bare prompt does not park; the batch
    /// stays unread. Returns whether the prompt is sent.
    async fn send_bare_without_mail(
        &self,
        canonical_command: &mut String,
        selected_mail: &mut Option<sessions::ResultMailBatch>,
        user_text: &str,
        images: &[plakous_host_protocol::DraftImage],
        pending_context: &mut crate::context::ContextProjection,
    ) -> bool {
        if selected_mail.is_none() {
            return false;
        }
        let Ok(bare) = query_context(
            &self.opened,
            ContextQuery::WithPendingUserInput(queue::user_input(user_text, images)),
        )
        .await
        else {
            return false;
        };
        if bare.compaction_verdict().overflow() {
            return false;
        }
        *selected_mail = None;
        user_text.clone_into(canonical_command);
        *pending_context = bare;
        true
    }

    /// Keeps a prompt queued with drain paused, naming why.
    fn park_prompt(
        &mut self,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
        note: &'static str,
    ) -> CliResult<HostStep> {
        if let Some(state) = one_shot {
            state.result = Some(Err(CliFailure::classified(
                crate::error::CliFailureKind::Agent,
                note,
            )
            .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {})));
        }
        self.pause_unrecorded(note.to_owned())?;
        let _ = self.role.events.notice(
            TranscriptRow::error(NoticeScope::Compaction, note)
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {}),
        );
        Ok(HostStep::Continue)
    }

    /// Rebuild stale native dispatch authority before context/compaction or
    /// admission. Exact reopen owns the semantic comparison and historical
    /// obligations; ordinary rebind would publish a new branch/tree epoch.
    pub(super) async fn refresh_native_authority_before_prompt(&mut self) -> CliResult<()> {
        let changed = preparation::run_step(
            InterruptWatch::new(self.role.actions, &self.role.interrupt_state),
            &mut self.role.prep_hooks,
            preparation::Stage::Refresh,
            super::super::startup::refresh_native_target(&mut self.role.config),
        )
        .await?;
        self.reopen_after_native_authority_refresh(changed).await
    }

    async fn reopen_after_native_authority_refresh(&mut self, changed: bool) -> CliResult<()> {
        if changed {
            if let Err(error) = self.reopen_in_place().await {
                self.role.recovery = RootRecovery::Due;
                return Err(error);
            }
        }
        Ok(())
    }
}

/// Operator note when a replayed delivery's immutable batch does not fit.
const REPLAYED_MAIL_OVER_WINDOW_NOTE: &str = "the queued prompt and the sub-agent mail bound to its delivery do not fit the context window; the prompt remains queued and drain is paused; start a fresh conversation";

/// What a scheduled pass's send leaves for the command's child mail.
#[derive(Debug, Eq, PartialEq)]
enum ScheduledMail {
    /// The command goes as measured.
    Keep,
    /// The freshly selected batch is dropped and stays unread.
    Drop,
    /// The command stays queued.
    Park,
}

/// An over-window command is withheld by its turn after admission has
/// acknowledged its batch, so the model would never see that mail. A fresh
/// batch is dropped (`drop_mail`); a replayed delivery's batch is bound to
/// its command and cannot be, so that command parks instead.
const fn mail_after_scheduled_send(
    over_window: bool,
    drop_mail: bool,
    replayed_mail: bool,
) -> ScheduledMail {
    if over_window && replayed_mail {
        ScheduledMail::Park
    } else if drop_mail {
        ScheduledMail::Drop
    } else {
        ScheduledMail::Keep
    }
}

/// A root prompt the actor admitted, as its turn loop drives it.
pub(super) struct RootTurn {
    input: PromptInput,
    mail: PromptMail,
    pending_context: crate::context::ContextProjection,
    enrolled_source_bytes: u64,
}

/// A root turn at its terminal, or the drive's own failure before one.
pub(super) struct RootTerminal {
    input: PromptInput,
    driven: Result<super::super::DrivenTurn, super::super::DriveTurnError>,
    turn_started: Instant,
}

/// The root's turns: one per admitted prompt. What follows a root terminal
/// (a queued item, a transient resume, a child wake) is the idle step's.
impl<'o, P: plakous_host_protocol::ProtocolExtension>
    crate::node::TurnRole<Option<&'o mut super::super::oneshot::OneShotState>>
    for RootActor<'_, P>
{
    type Turn = RootTurn;
    type Terminal = CliResult<RootTerminal>;
    type Settled = CliResult<HostStep>;

    #[allow(
        clippy::too_many_lines,
        reason = "Prompt dispatch records delivery and reconciles definite refusal versus admitted canonical work before returning to the host loop."
    )]
    async fn drive(
        &mut self,
        _one_shot: &mut Option<&'o mut super::super::oneshot::OneShotState>,
        turn: RootTurn,
    ) -> CliResult<RootTerminal> {
        let RootTurn {
            input,
            mail,
            pending_context,
            enrolled_source_bytes,
        } = turn;
        let PromptInput {
            user_text,
            queued_delivery,
            user_echoed,
            host_authored,
            transient_resume,
            images,
        } = input;
        let PromptMail {
            canonical_command,
            selected_mail,
            ..
        } = mail;
        let _ = self.role.events.send(ContentEvent::SetBusy(true));
        let _ = self.role.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.role.config.session.pending_queue.len(),
            detail: None,
        });

        let tree_id = durable_tree_id(&self.role.config.topology_rows(), &self.role.config.session);
        if let Err(error) = topology_state::mark_root_live_for_prompt(&tree_id).await {
            let _ = self.role.events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("tree admission restore failed: {error}"),
            ));
        }

        let agent = self
            .opened
            .agent()
            .ok_or_else(|| CliFailure::new("session source is detached"))?;
        let subscription_service = self.role.config.target.native_subscriptions().cloned();
        let turn_started = Instant::now();
        let live_target = self.live_target();
        let driven = Box::pin(drive_one_turn(
            agent,
            self.opened.sessions(),
            user_text.clone(),
            super::super::TurnHost {
                job_agent: agent,
                host_wakes: self.role.config.prompt_host_facts().host_wakes
                    && !self.role.config.session.child_wake_suppressed,
                quit_started_at: &mut self.quit_started_at,
                quit_job_stops: &mut self.quit_job_stops,
                actions: self.role.actions,
                deferred: &self.role.interrupt_state,
                events: &self.role.events,
                rows: &super::super::HostRows::new(
                    &self.role.config.state_roots,
                    self.role.config.store.as_ref(),
                ),
                roots: &self.role.config.state_roots,
                store: &*self.role.config.store,
                projection_store: &self.role.config.store,
                live_projector: self.role.live.as_mut(),
                live_target,
                home: &self.role.config.home,
                session: &mut self.role.config.session,
                context_projection: pending_context,
                enrolled_source_bytes,
                root_template: &mut self.role.config.root_template,
                attached_one_shot: self.role.config.attached_one_shot,
                ready_catalog: &self.role.config.ready_catalog.accounts,
                surface: &self.role.config.surface,
                log: self.role.log.as_ref(),
                subscription_service,
                subscription_usage_refresh: &self.role.subscription_usage_refresh,
                held_usage: &self.role.held_usage,
                ledger: &self.role.session_ledger,
                last_tree_revision: &mut self.role.last_tree_revision,
                job_events: &mut self.role.job_events,
                runtime_factory: self.role.config.runtime_factory.as_ref(),
            },
            queued_delivery
                .as_ref()
                .ok_or_else(|| CliFailure::new("root start requires a queued delivery"))?,
            user_echoed,
            (canonical_command, selected_mail),
        ))
        .await;
        Ok(RootTerminal {
            input: PromptInput {
                user_text,
                queued_delivery,
                user_echoed,
                host_authored,
                transient_resume,
                images,
            },
            driven,
            turn_started,
        })
    }

    async fn after_terminal(
        &mut self,
        one_shot: &mut Option<&'o mut super::super::oneshot::OneShotState>,
        terminal: CliResult<RootTerminal>,
    ) -> crate::node::AfterTerminal<RootTurn, CliResult<HostStep>> {
        crate::node::AfterTerminal::Settled(
            self.settle_terminal(one_shot.as_deref_mut(), terminal)
                .await,
        )
    }
}

impl<P: plakous_host_protocol::ProtocolExtension> RootActor<'_, P> {
    /// Publishes a driven root terminal and decides the host's next step.
    async fn settle_terminal(
        &mut self,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
        terminal: CliResult<RootTerminal>,
    ) -> CliResult<HostStep> {
        let RootTerminal {
            input,
            driven,
            turn_started,
        } = terminal?;
        self.role
            .session_ledger
            .record_dispatch(turn_started.elapsed());
        if self.role.actions.abandon_and_release_requested() && driven.is_err() {
            return Ok(HostStep::Quit);
        }
        if let Err(error) = &driven
            && let Some(failure) = error.refused_failure()
        {
            return Err(failure);
        }
        // The row used to move only when a binding was republished.
        send_session_info(&self.role.events, &self.role.config);
        let quit_after_terminal = match &driven {
            Ok(driven) => driven.quit_after_terminal,
            Err(error) => error.quit_after_terminal,
        };
        self.role.handoff_root_due = quit_after_terminal;
        let drive = driven
            .map(|driven| driven.outcome)
            .map_err(|error| error.message);
        self.finish_prompt(input, drive, turn_started, quit_after_terminal, one_shot)
            .await
    }
}

#[cfg(test)]
mod tests;
