//! Root prompt preflight, dispatch, terminal publication and queue disposition.

use super::{
    CliFailure, CliResult, ContextQuery, HostLoop, HostStep, Instant, InterruptWatch,
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

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn run_prompt(
        &mut self,
        raw_text: String,
        queued_delivery: Option<SessionQueueItem>,
        user_echoed: bool,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
        let wake = queued_delivery
            .as_ref()
            .and_then(|item| self.config.session.queue_commands.pending_for(item))
            .filter(|submission| submission.kind == sessions::projector::SubmissionKind::HostWake)
            .cloned();
        // A child wake already passed the capacity check before its immutable
        // receipt was recorded. Do not turn that receipt into suppression here.
        if wake.is_none() {
            let admission = self
                .pending_storage_admission
                .take()
                .unwrap_or_else(|| self.config.store.storage_admission());
            super::super::info::send_session_info_with_storage(
                &self.events,
                &self.config,
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
                self.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                emit_queue_snapshot(&self.events, &self.config.session);
                if queued_delivery.is_none() {
                    reject_draft(&self.events, super::UserAction::Prompt(raw_text.into()));
                }
                // Operator-only feedback never enters the transcript/export.
                let _ = self
                    .events
                    .notice(TranscriptRow::warning(NoticeScope::Settings, reason));
                return Ok(HostStep::Continue);
            }
        }
        let before_recovery = self.recovery_blocked;
        let before_drain = self.config.session.queue_drain_state;
        let _ = self.events.send(ContentEvent::SetBusy(true));
        let outcome = self.run_prompt_inner(raw_text, queued_delivery, user_echoed, one_shot);
        let stopped = match &outcome {
            Ok(PromptOutcome::Stopped(stopped)) => Some(*stopped),
            _ => None,
        };
        if !matches!(outcome, Ok(PromptOutcome::Dispatched(_))) && !self.events.root_boundary().1 {
            emit_terminal_status(
                &self.events,
                if self.recovery_blocked {
                    TerminalPhase::RecoveryPending
                } else {
                    TerminalPhase::AdmissionRejected
                },
                self.config.session.pending_queue.len(),
            );
        }
        if self.events.root_boundary().0 {
            let _ = self.events.send(ContentEvent::SetBusy(false));
        }
        if let Some(intent) = &wake
            && let Err(error) =
                self.discard_undispatched_wake(intent, before_recovery, before_drain, stopped)
        {
            self.suppress_child_wake(None)?;
            let _ = self.events.notice(
                TranscriptRow::error(NoticeScope::Queue, error.message().to_owned())
                    .with_optional_remedy(error.remedy()),
            );
            return Ok(HostStep::Continue);
        }
        if wake.is_some()
            && let Err(error) = outcome
        {
            self.suppress_child_wake(None)?;
            let _ = self.events.notice(
                TranscriptRow::error(NoticeScope::Queue, error.message().to_owned())
                    .with_optional_remedy(error.remedy()),
            );
            return Ok(HostStep::Continue);
        }
        outcome.map(|outcome| match outcome {
            PromptOutcome::Dispatched(step) => step,
            PromptOutcome::Stopped(_) | PromptOutcome::Refused => HostStep::Continue,
        })
    }

    fn discard_undispatched_wake(
        &mut self,
        intent: &sessions::projector::Submission,
        before_recovery: bool,
        before_drain: SessionQueueDrainState,
        stopped: Option<preparation::Stopped>,
    ) -> CliResult<()> {
        let restored_drain = if stopped.is_some() {
            SessionQueueDrainState::RestoredPaused
        } else {
            before_drain
        };
        if !self
            .config
            .store
            .load_intents(&self.config.session.id)
            .map_err(CliFailure::new)?
            .submissions
            .iter()
            .any(|submission| submission.command_id == intent.command_id)
        {
            self.config.session.queue_drain_state = restored_drain;
            self.recovery_blocked = before_recovery || self.opened.agent().is_none();
            emit_queue_snapshot(&self.events, &self.config.session);
            return Ok(());
        }
        let views = self
            .executor
            .block_on(super::super::projection::project_to_tail(
                self.opened.sessions(),
                &self.config.store,
                &self.config.session.id,
                self.opened.locator(),
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        if !matches!(
            views.state.deliveries.get(&intent.command_id),
            Some(sessions::projector::Delivery::Accepted)
        ) {
            self.config.session.pending_queue.retain(|item| {
                item.id != intent.queue_item_id || item.revision != intent.queue_item_revision
            });
            self.config.session.queue_drain_state = restored_drain;
            if stopped.is_none() {
                self.suppress_child_wake(None)?;
            }
            self.recovery_blocked = before_recovery || self.opened.agent().is_none();
            if let Err(error) = self.config.store.discard_submission(
                &mut self.config.session,
                &intent.command_id,
                None,
            ) {
                let _ = self.events.notice(TranscriptRow::error(
                    NoticeScope::Tree,
                    format!("undispatched child wake cleanup could not be persisted: {error}"),
                ));
            }
            emit_queue_snapshot(&self.events, &self.config.session);
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "One prompt admission owns preparation, interrupt handling, dispatch, and terminal publication as an ordered host transition."
    )]
    fn run_prompt_inner(
        &mut self,
        raw_text: String,
        queued_delivery: Option<SessionQueueItem>,
        user_echoed: bool,
        mut one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<PromptOutcome> {
        if self.config.session.queue_projection_debt {
            emit_queue_snapshot(&self.events, &self.config.session);
            return Ok(PromptOutcome::Refused);
        }
        let host_authored = queued_delivery
            .as_ref()
            .is_some_and(|item| super::wake::is_wake_delivery(&self.config.session, item));
        let transient_resume = queued_delivery.as_ref().is_some_and(|item| {
            self.config
                .session
                .queue_commands
                .pending_for(item)
                .is_some_and(|submission| {
                    submission
                        .command_id
                        .starts_with(crate::driver::transient::ROOT_COMMAND_PREFIX)
                })
        });
        if !host_authored {
            self.cancel_transient_resume();
            self.transient_resumes_used = 0;
            self.child_wake = super::wake::RootWake::default();
        }
        if !user_echoed && !host_authored {
            let _ = self.events.send(ContentEvent::PromptStarted(
                queued_delivery.as_ref().map(|item| item.protocol(false)),
            ));
        }
        if self.log.is_none() && self.config.session.store_revision != 0 {
            self.log = SessionLog::open(&self.config.home, &self.config.session.id).ok();
        }

        let user_text = raw_text;
        // The images the prompt carries are part of every pending-input
        // measurement, so the estimate counts them by the dialect's bound.
        let images = queued_delivery
            .as_ref()
            .map(|item| std::sync::Arc::clone(&item.images))
            .unwrap_or_default();
        let watch = InterruptWatch::new(self.actions, &self.interrupt_state);
        watch.begin();
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Preparing,
            queue_len: self.config.session.pending_queue.len(),
            detail: None,
        });
        match self.prepare_prompt_authority(host_authored, transient_resume, &mut one_shot)? {
            std::ops::ControlFlow::Break(preparation::End::Stopped(stopped)) => {
                return Ok(PromptOutcome::Stopped(stopped));
            }
            std::ops::ControlFlow::Break(preparation::End::Refused) => {
                return Ok(PromptOutcome::Refused);
            }
            std::ops::ControlFlow::Continue(()) => {}
        }
        let rows = self.config.topology_rows();
        preparation::run_step(
            &self.executor,
            InterruptWatch::new(self.actions, &self.interrupt_state),
            preparation::Stage::Tree,
            super::super::projection::project_tree(
                self.opened.sessions(),
                &rows,
                &durable_tree_id(&rows, &self.config.session),
                self.opened.locator().session_id(),
            ),
        )
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?;
        if let Some(stopped) = self.stop_preparation(&mut one_shot)? {
            return Ok(PromptOutcome::Stopped(stopped));
        }
        // A replayed delivery's command and batch are immutable.
        let mut replayed_mail = false;
        let (canonical_command, selected_mail) = if let Some(intent) = queued_delivery
            .as_ref()
            .and_then(|item| self.config.session.queue_commands.pending_for(item))
        {
            // An immutable command may include a stored batch. Never trim or
            // rebuild it after a scheduled pass.
            replayed_mail = intent.canonical_command != user_text;
            (intent.canonical_command.clone(), None)
        } else {
            let selected = super::super::result_mail::select(
                &self.config.topology_rows(),
                &self.config.session,
                &user_text,
            );
            #[cfg(test)]
            let selected = preparation::hooks::MAIL_ERROR
                .with(|slot| slot.borrow_mut().take())
                .map_or(selected, Err);
            match selected {
                Ok(selected) => selected,
                Err(error) => {
                    if let Some(state) = one_shot.as_deref_mut() {
                        state.result = Some(Err(CliFailure::new(error.clone())));
                    }
                    let _ = self.events.notice(TranscriptRow::error(
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
    }

    fn prepare_prompt_authority(
        &mut self,
        host_authored: bool,
        transient_resume: bool,
        one_shot: &mut Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<std::ops::ControlFlow<preparation::End>> {
        if (!host_authored || transient_resume)
            && let Err(error) = self.refresh_native_authority_before_prompt()
        {
            self.recovery_blocked |= self.opened.agent().is_none();
            if let Some(state) = one_shot.as_deref_mut() {
                state.result = Some(Err(error.clone()));
            }
            self.pause_unrecorded(error.message().to_owned())?;
            let _ = self.events.notice(TranscriptRow::error(
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
            &self.executor,
            InterruptWatch::new(self.actions, &self.interrupt_state),
            preparation::Stage::Settle,
            super::super::result_mail::settle_waits(
                &self.config.topology_rows(),
                &self.config.session,
                &self.opened,
            ),
        )
        .unwrap_or_else(|_| Err("application execution failed".to_owned()))
        {
            if let Some(state) = one_shot.as_deref_mut() {
                state.result = Some(Err(CliFailure::new(error.clone())));
            }
            let _ = self.events.notice(TranscriptRow::error(
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
    fn preflight_prompt(
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
            self.child_wake
                .prepared_context
                .take()
                .ok_or_else(|| CliFailure::new("child wake preflight is unavailable"))
        } else {
            #[cfg(test)]
            preparation::hooks::CALLED
                .with(|calls| calls.borrow_mut().push(preparation::Stage::Query));
            query_context(
                &self.executor,
                &self.opened,
                ContextQuery::WithPendingUserInput(pending_input(&canonical_command)),
            )
        };
        let mut pending_context = match queried {
            Ok(projection) => projection,
            Err(error) => {
                if host_authored {
                    let _ = self.events.notice(
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
                emit_unavailable_context(&self.events);
                if one_shot.is_none() {
                    let _ = self.events.notice(
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
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Queue,
                        "context query failed; the prompt remains queued and drain stays paused",
                    ));
                }
                return Ok(PromptOutcome::Refused);
            }
        };
        let enrolled_source_bytes = last_enrolled_source_bytes(&self.opened);
        emit_context_projection(
            &self.events,
            &pending_context,
            self.config.session.context_counters(),
            enrolled_source_bytes,
        );
        preparation::run_step(
            &self.executor,
            InterruptWatch::new(self.actions, &self.interrupt_state),
            preparation::Stage::Context,
            std::future::ready(()),
        )
        .map_err(|_| CliFailure::new("application execution failed"))?;
        if let Some(stopped) = self.stop_preparation(&mut one_shot)? {
            return Ok(PromptOutcome::Stopped(stopped));
        }
        if !host_authored && pending_context.compaction_verdict().is_due() {
            if self.config.compaction_enabled {
                // A scheduled compact is one paid round-trip the host blocks
                // on; without the busy row the TUI paints idle while every
                // prompt is silently deferred (the 2026-09-03 "freeze").
                observe_compact(&self.events, self.config.session.pending_queue.len(), true);
                #[cfg(test)]
                preparation::hooks::CALLED
                    .with(|calls| calls.borrow_mut().push(preparation::Stage::Compact));
                let compacted = run_scheduled_compaction_until_fits(
                    &self.executor,
                    InterruptWatch::new(self.actions, &self.interrupt_state),
                    &mut self.config,
                    &self.opened,
                    &mut self.stderr_buf,
                    &self.events,
                    self.log.as_ref(),
                    &pending_input(&canonical_command),
                    selected_mail
                        .is_some()
                        .then(|| pending_input(&user_text))
                        .as_ref(),
                );
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
                                &self.events,
                                self.config.session.pending_queue.len(),
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
                            &self.events,
                            self.config.session.pending_queue.len(),
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
                            &self.events,
                            self.config.session.pending_queue.len(),
                            false,
                        );
                        if one_shot.is_none() {
                            let _ = self.events.notice(
                                TranscriptRow::error(
                                    NoticeScope::Compaction,
                                    format!("compact: {}", error.message()),
                                )
                                .with_optional_remedy(error.remedy()),
                            );
                        } else {
                            let _ = self.events.notice(TranscriptRow::info(NoticeScope::Compaction,
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
                && !self.send_bare_without_mail(
                    &mut canonical_command,
                    &mut selected_mail,
                    &user_text,
                    &images,
                    &mut pending_context,
                )
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
                    let _ = self.events.notice(TranscriptRow::error(
                        NoticeScope::Compaction,
                        scheduled_compaction_disabled_note(pending_context.disposition()),
                    ));
                }
                return Ok(PromptOutcome::Refused);
            }
        }

        preparation::run_step(
            &self.executor,
            InterruptWatch::new(self.actions, &self.interrupt_state),
            preparation::Stage::Dispatch,
            std::future::ready(()),
        )
        .map_err(|_| CliFailure::new("application execution failed"))?;
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
        .map(PromptOutcome::Dispatched)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Prompt dispatch records delivery and reconciles definite refusal versus admitted canonical work before returning to the host loop."
    )]
    fn dispatch_prompt(
        &mut self,
        input: PromptInput,
        mail: PromptMail,
        pending_context: crate::context::ContextProjection,
        enrolled_source_bytes: u64,
        one_shot: Option<&mut super::super::oneshot::OneShotState>,
    ) -> CliResult<HostStep> {
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
        let _ = self.events.send(ContentEvent::SetBusy(true));
        let _ = self.events.send(ContentEvent::Status {
            phase: HostPhase::Running,
            queue_len: self.config.session.pending_queue.len(),
            detail: None,
        });

        let tree_id = durable_tree_id(&self.config.topology_rows(), &self.config.session);
        if let Err(error) = topology_state::mark_root_live_for_prompt(&tree_id) {
            let _ = self.events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("tree admission restore failed: {error}"),
            ));
        }

        let agent = self
            .opened
            .agent()
            .ok_or_else(|| CliFailure::new("session source is detached"))?;
        let subscription_service = self.config.target.native_subscriptions().cloned();
        let turn_started = Instant::now();
        let live_target = self.live_target();
        let driven = self
            .executor
            .block_on(drive_one_turn(
                agent,
                self.opened.sessions(),
                user_text.clone(),
                super::super::TurnHost {
                    job_agent: agent,
                    host_wakes: self.config.prompt_host_facts().host_wakes
                        && !self.config.session.child_wake_suppressed,
                    quit_started_at: &mut self.quit_started_at,
                    quit_job_stops: &mut self.quit_job_stops,
                    actions: self.actions,
                    deferred: &self.interrupt_state,
                    events: &self.events,
                    rows: &super::super::HostRows::new(
                        &self.config.state_roots,
                        self.config.store.as_ref(),
                    ),
                    roots: &self.config.state_roots,
                    store: &*self.config.store,
                    projection_store: &self.config.store,
                    live_projector: self.live.as_mut(),
                    live_target,
                    home: &self.config.home,
                    session: &mut self.config.session,
                    context_projection: pending_context,
                    enrolled_source_bytes,
                    root_template: &mut self.config.root_template,
                    attached_one_shot: self.config.attached_one_shot,
                    ready_catalog: &self.config.ready_catalog.accounts,
                    surface: &self.config.surface,
                    log: self.log.as_ref(),
                    subscription_service,
                    subscription_usage_refresh: &self.subscription_usage_refresh,
                    held_usage: &self.held_usage,
                    ledger: &self.session_ledger,
                    last_tree_revision: &mut self.last_tree_revision,
                    job_events: &mut self.job_events,
                    runtime_factory: self.config.runtime_factory.as_ref(),
                },
                queued_delivery
                    .as_ref()
                    .ok_or_else(|| CliFailure::new("root start requires a queued delivery"))?,
                user_echoed,
                (canonical_command, selected_mail),
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?;
        self.session_ledger.record_dispatch(turn_started.elapsed());
        if self.actions.abandon_and_release_requested() && driven.is_err() {
            return Ok(HostStep::Quit);
        }
        if let Err(error) = &driven
            && let Some(failure) = error.storage_failure()
        {
            return Err(failure);
        }
        // The row used to move only when a binding was republished.
        send_session_info(&self.events, &self.config);
        let quit_after_terminal = match &driven {
            Ok(driven) => driven.quit_after_terminal,
            Err(error) => error.quit_after_terminal,
        };
        let drive = driven
            .map(|driven| driven.outcome)
            .map_err(|error| error.message);
        self.finish_prompt(
            PromptInput {
                user_text,
                queued_delivery,
                user_echoed,
                host_authored,
                transient_resume,
                images,
            },
            drive,
            turn_started,
            quit_after_terminal,
            one_shot,
        )
    }

    #[allow(
        clippy::too_many_lines,
        reason = "Prompt completion publishes the canonical terminal before deciding queue continuation, transient retry, and child-wake suppression."
    )]
    fn finish_prompt(
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
                self.config.session.child_wake_suppressed = true;
            } else {
                let _ = self.events.send(ContentEvent::DraftRestored(
                    plakous_host_protocol::Draft::new(user_text.clone(), images.to_vec()),
                ));
            }
        } else if matches!(&drive, Ok(TurnHostOutcome::AdmissionRejected { .. })) {
            if queued_delivery.is_some() {
                let _ = self.events.notice(TranscriptRow::info(
                    NoticeScope::Queue,
                    "prompt remains queued; resume to retry, or edit a rejected item",
                ));
            } else {
                reject_draft(
                    &self.events,
                    super::UserAction::Prompt(plakous_host_protocol::Draft::new(
                        user_text,
                        images.to_vec(),
                    )),
                );
            }
        }
        self.recovery_blocked |= host_outcome_blocks_continuation(&drive);
        observe_boundary(
            self.config.runtime_factory.as_ref(),
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
        let completed = self.publish_terminal(drive, turn_started, error_presentation);
        self.publish_projected_transcript(one_shot.is_some());
        self.update_background_process_count();
        self.publish_pending_continuation();
        let _ = self.events.send(ContentEvent::SetBusy(false));
        emit_terminal_status(
            &self.events,
            if self.recovery_blocked {
                TerminalPhase::RecoveryPending
            } else if interrupted {
                TerminalPhase::Interrupted
            } else if admission_rejected {
                TerminalPhase::AdmissionRejected
            } else {
                TerminalPhase::Idle
            },
            self.config.session.pending_queue.len(),
        );

        if let (Some(state), Some(terminal)) = (one_shot, one_shot_terminal) {
            state.result = Some(
                if (self.recovery_blocked && terminal.is_ok())
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
            self.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            if let Err(error) = self.config.store.save(&mut self.config.session) {
                log_queue_persist_failed(
                    &self.config.home,
                    &self.config.session,
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
                self.log.as_ref(),
                &self.config,
                SessionEvent::new("session.quit"),
            );
            emit_queue_snapshot(&self.events, &self.config.session);
            persist(&*self.config.store, &mut self.config.session);
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
        if !completed && !admission_rejected && !self.config.session.pending_queue.is_empty() {
            match commit_session_update(
                &*self.config.store,
                &self.config.home,
                &mut self.config.session,
                |candidate| {
                    candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                    true
                },
            ) {
                Ok(_) => {}
                Err(error) => {
                    log_queue_persist_failed(
                        &self.config.home,
                        &self.config.session,
                        QueuePersistAction::PauseDrain,
                        &error,
                    );
                    queue_persist_failed(&self.events, &self.config.session);
                }
            }
        }

        // Only settled refusals can let the next item pass; an unresolved
        // delivery must keep its exact identity until an explicit retry.
        self.pending_start = if self.config.session.queue_drain_state
            == SessionQueueDrainState::LiveAutoDrain
            && (completed
                || (admission_rejected
                    && !stop_drain
                    && !self.config.session.queue_commands.has_pending()))
        {
            queue::next_drainable(&self.config.session)
        } else {
            None
        };
        emit_queue_snapshot(&self.events, &self.config.session);
        persist(&*self.config.store, &mut self.config.session);
        Ok(HostStep::Continue)
    }

    /// With compaction disabled, a prompt whose fresh child-mail batch alone
    /// overflows is sent bare when the bare prompt does not park; the batch
    /// stays unread. Returns whether the prompt is sent.
    fn send_bare_without_mail(
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
            &self.executor,
            &self.opened,
            ContextQuery::WithPendingUserInput(queue::user_input(user_text, images)),
        ) else {
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
        let _ = self.events.notice(
            TranscriptRow::error(NoticeScope::Compaction, note)
                .with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {}),
        );
        Ok(HostStep::Continue)
    }

    /// Rebuild stale native dispatch authority before context/compaction or
    /// admission. Exact reopen owns the semantic comparison and historical
    /// obligations; ordinary rebind would publish a new branch/tree epoch.
    pub(super) fn refresh_native_authority_before_prompt(&mut self) -> CliResult<()> {
        let changed = preparation::run_step(
            &self.executor,
            InterruptWatch::new(self.actions, &self.interrupt_state),
            preparation::Stage::Refresh,
            super::super::startup::refresh_native_target(&mut self.config),
        )
        .map_err(|_| CliFailure::new("application execution failed"))??;
        self.reopen_after_native_authority_refresh(changed)
    }

    fn reopen_after_native_authority_refresh(&mut self, changed: bool) -> CliResult<()> {
        if changed {
            if let Err(error) = self.reopen_in_place() {
                self.recovery_blocked = true;
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
#[cfg(test)]
mod tests;
