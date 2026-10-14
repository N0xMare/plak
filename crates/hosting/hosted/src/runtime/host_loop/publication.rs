//! Commit the terminal product projection before advertising completion.

use super::{
    ContextDisposition, ExceptionalTerminalProjection, HostEvent, HostLoop, Instant,
    InterruptWatch, SessionEvent, SessionQueueDrainState, TurnHostOutcome,
    commit_definite_failure_projection, commit_exceptional_terminal_projection,
    commit_session_update, context_capacity_recovery_guidance, elapsed_ms_since,
    emit_unavailable_context, handle_compact, humanize_turn_error, log_ev, native_compact_facet,
    report_context, sessions, should_schedule_compaction,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow, observe_test_boundary};
use crate::sessions::{NoticeScope, TranscriptRow};

/// One-shot callers return terminal failures; interactive hosts publish them.
#[derive(Clone, Copy)]
pub(super) enum TerminalErrorPresentation {
    Events,
    Return,
}

impl<P: plakous_host_protocol::ProtocolExtension> HostLoop<'_, P> {
    pub(super) fn publish_terminal(
        &mut self,
        drive: Result<TurnHostOutcome, String>,
        turn_started: Instant,
        error_presentation: TerminalErrorPresentation,
    ) -> bool {
        let mut completed = matches!(&drive, Ok(TurnHostOutcome::Completed { .. }));
        let exchange = match &drive {
            Ok(TurnHostOutcome::Completed { .. } | TurnHostOutcome::ContextCapacity { .. }) => {
                Some(crate::agent::TerminalExchange::Closed)
            }
            Ok(
                TurnHostOutcome::Failed { exchange, .. }
                | TurnHostOutcome::Interrupted { exchange, .. },
            ) => Some(*exchange),
            _ => None,
        };
        if let Some(exchange) = exchange {
            self.settle_terminal_waits(exchange);
        }
        match drive {
            Ok(TurnHostOutcome::ConsumedDelivery) => completed = true,
            Ok(TurnHostOutcome::Completed {
                assistant,
                finish_reason,
                finish_detail,
                reasoning_tokens,
                output_tokens,
                tool_calls,
            }) => {
                let transcript_line = super::super::turn::completed_turn_row(&assistant);
                match commit_session_update(
                    &*self.config.store,
                    &self.config.home,
                    &mut self.config.session,
                    |candidate| {
                        candidate.append_transcript(transcript_line);
                        if finish_reason == plakous::model::FinishReason::Refusal {
                            candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                        }
                        true
                    },
                ) {
                    Ok(true) => {
                        #[cfg(any(test, feature = "test-support"))]
                        {
                            observe_test_boundary(
                                self.config.test_injection.as_deref(),
                                TestHostWorkflow::SendNow,
                                TestHostBoundary::ProductCommitted,
                            );
                        }
                        // An empty turn that stopped at its output bound with
                        // no tool call did not fail to answer: it spent the
                        // whole budget thinking. Two tasks stopped at exactly
                        // the bound
                        // with one token left for visible text, and both read
                        // as "empty stream — try again", which is the wrong
                        // advice. No retry either way; the operator is told
                        // which of the two happened.
                        let reasoning_only = spent_budget_on_reasoning(
                            assistant.is_empty(),
                            tool_calls,
                            finish_reason,
                        );
                        if reasoning_only {
                            let _ = self.events.notice(TranscriptRow::warning(
                                NoticeScope::Turn,
                                reasoning_only_note(reasoning_tokens, output_tokens),
                            ));
                        } else if assistant.is_empty()
                            && finish_reason != plakous::model::FinishReason::Refusal
                        {
                            let _ = self.events.notice(TranscriptRow::warning(NoticeScope::Turn,
                                "turn finished with no visible text (empty stream — try again, or choose another profile with /agents)",
                            ));
                        }
                        if assistant.is_empty() {
                            log_ev(
                                self.log.as_ref(),
                                &self.config,
                                if reasoning_only {
                                    SessionEvent::new("turn.assistant_reasoning_only")
                                        .u64("chars", 0)
                                        .u64("reasoning_tokens", reasoning_tokens)
                                        .u64("output_tokens", output_tokens)
                                } else {
                                    SessionEvent::new("turn.assistant_empty").u64("chars", 0)
                                },
                            );
                        } else {
                            log_ev(
                                self.log.as_ref(),
                                &self.config,
                                SessionEvent::new("turn.assistant")
                                    .str("text", &assistant)
                                    .u64("chars", assistant.len() as u64),
                            );
                            let _ = self.events.send(HostEvent::AssistantDone);
                        }
                        if finish_reason == plakous::model::FinishReason::Refusal {
                            // Discard streamed previews in favor of the published view.
                            let _ =
                                self.events
                                    .send(super::super::logging::project_session_snapshot(
                                        &self.config.session,
                                    ));
                            let _ = self.events.send(HostEvent::ModelRefusal {
                                detail: finish_detail,
                            });
                        }
                        self.events
                            .resnapshot_transcript_if_required(&self.config.session);
                        log_ev(
                            self.log.as_ref(),
                            &self.config,
                            SessionEvent::new("turn.completed")
                                .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                        );
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
                                NoticeScope::Turn,
                                error.message().to_owned(),
                            ));
                        }
                    }
                    Ok(false) => unreachable!("completion update always changes the candidate"),
                    Err(_) => {
                        completed = false;
                        self.recovery_blocked = true;
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
                        let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                            "turn completed but its product transcript could not be committed; the next prompt (or /recover) reopens the session and continues"
                                ,
                        ));
                    }
                }
            }
            Ok(TurnHostOutcome::ContextCapacity {
                source_checkpoint,
                recovery,
                error,
            }) => {
                let exact_source = self.opened.locator().checkpoint_id() == source_checkpoint;
                if !exact_source {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Compaction,
                        "context-capacity result did not match the exact attached checkpoint; compact recovery was refused"
                            ,
                    ));
                }
                let recovery = if exact_source {
                    recovery
                } else {
                    plakous_host_protocol::ContextCapacityRecovery::Protected
                };
                let native = self
                    .config
                    .target
                    .native_provider()
                    .is_some_and(native_compact_facet);
                let auto_compact = matches!(
                    recovery,
                    plakous_host_protocol::ContextCapacityRecovery::Compactable
                ) && exact_source
                    && self.config.compaction_enabled
                    && should_schedule_compaction(ContextDisposition::WouldOverflow);
                let saved = commit_session_update(
                    &*self.config.store,
                    &self.config.home,
                    &mut self.config.session,
                    |candidate| {
                        candidate.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                        candidate.append_transcript(TranscriptRow::error(
                            NoticeScope::Turn,
                            error.clone(),
                        ));
                        true
                    },
                );
                if saved.is_err() {
                    self.recovery_blocked = true;
                    self.config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "context-capacity stop could not be committed; queue drain remains fail-closed",
                    ));
                    return false;
                }
                if auto_compact {
                    let _ = self.events.notice(TranscriptRow::warning(NoticeScope::Turn,
                        "this turn stopped at the context limit; compacting once for subsequent prompts. The failed turn will not be retried automatically",
                    ));
                    match handle_compact(
                        &self.executor,
                        InterruptWatch::new(self.actions, &self.interrupt_state),
                        &mut self.config,
                        &self.opened,
                        &mut self.stderr_buf,
                        &self.events,
                        self.log.as_ref(),
                        sessions::ProductCompactReason::CapacityRecovery,
                        None,
                    ) {
                        Ok(_) => {
                            completed = true;
                            if let Err(error) = report_context(
                                &self.executor,
                                &self.opened,
                                &self.events,
                                native,
                                self.config.session.context_counters(),
                            ) {
                                emit_unavailable_context(&self.events);
                                let _ = self.events.notice(TranscriptRow::error(
                                    NoticeScope::Turn,
                                    error.message().to_owned(),
                                ));
                            }
                        }
                        Err(compact_error) => {
                            let guidance =
                                context_capacity_recovery_guidance(recovery, exact_source, native);
                            if matches!(error_presentation, TerminalErrorPresentation::Events) {
                                let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                                    NoticeScope::Turn,
                                    error,
                                )));
                            }
                            let _ = self.events.notice(TranscriptRow::error(
                                NoticeScope::Compaction,
                                format!("compact: {}", compact_error.message()),
                            ));
                            let _ = self
                                .events
                                .notice(TranscriptRow::info(NoticeScope::Turn, guidance));
                            let _ = self
                                .events
                                .send(HostEvent::ContextCapacityRecovery(Some(recovery)));
                        }
                    }
                } else {
                    let guidance =
                        context_capacity_recovery_guidance(recovery, exact_source, native);
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            error,
                        )));
                    }
                    let _ = self
                        .events
                        .notice(TranscriptRow::info(NoticeScope::Turn, guidance));
                    let _ = self
                        .events
                        .send(HostEvent::ContextCapacityRecovery(Some(recovery)));
                }
            }
            Ok(TurnHostOutcome::AdmissionRejected { error, .. }) => {
                if matches!(error_presentation, TerminalErrorPresentation::Events) {
                    let _ = self
                        .events
                        .notice(TranscriptRow::error(NoticeScope::Turn, error));
                }
            }
            Ok(TurnHostOutcome::Failed {
                error, transient, ..
            }) => {
                if let Ok(()) = commit_definite_failure_projection(
                    &mut self.config.session,
                    &error,
                    |candidate| self.config.store.save(candidate),
                ) {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("turn.failed")
                            .str("disposition", "definite")
                            .str("msg", &error)
                            .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            humanize_turn_error(&error),
                        )));
                    }
                    if !transient
                        .is_some_and(|failure| self.schedule_transient_resume(failure, &error))
                    {
                        let _ = self.events.notice(TranscriptRow::info(NoticeScope::Turn,
                            "turn failed definitively; the durable session remains available for a new prompt",
                        ));
                    }
                } else {
                    self.recovery_blocked = true;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "definite turn failure could not be saved; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Ok(TurnHostOutcome::RecoveryRequired { error }) => {
                self.recovery_blocked = true;
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::RecoveryRequired(&error),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("turn.recovery_required")
                            .str("disposition", "recovery_required")
                            .str("msg", &error)
                            .u64("elapsed_ms", elapsed_ms_since(turn_started)),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self.events.send(HostEvent::Row(TranscriptRow::error(
                            NoticeScope::Turn,
                            error,
                        )));
                    }
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Recovery,
                        RECOVERY_PENDING_NOTE,
                    ));
                } else {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "recovery-required terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Ok(TurnHostOutcome::Interrupted { assistant, .. }) => {
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::Interrupted(&assistant),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("control.interrupt"),
                    );
                    let _ = self.events.send(HostEvent::Row(TranscriptRow::Interrupted));
                } else {
                    self.recovery_blocked = true;
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "interrupted terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
            Err(msg) => {
                self.recovery_blocked = true;
                let saved = commit_exceptional_terminal_projection(
                    &mut self.config.session,
                    ExceptionalTerminalProjection::UnresolvedError(&msg),
                    |candidate| self.config.store.save(candidate),
                );
                if saved.is_ok() {
                    // drive_one_turn already applies operator_facing_error.
                    log_ev(
                        self.log.as_ref(),
                        &self.config,
                        SessionEvent::new("error")
                            .str("code", "agent")
                            .str("msg", &msg),
                    );
                    if matches!(error_presentation, TerminalErrorPresentation::Events) {
                        let _ = self
                            .events
                            .send(HostEvent::Row(TranscriptRow::error(NoticeScope::Turn, msg)));
                    }
                    let _ = self.events.notice(TranscriptRow::info(
                        NoticeScope::Recovery,
                        RECOVERY_PENDING_NOTE,
                    ));
                } else {
                    let _ = self.events.notice(TranscriptRow::error(NoticeScope::Host,
                        "failed terminal projection could not be committed; the next prompt (or /recover) reopens the session and continues"
                            ,
                    ));
                }
            }
        }

        completed
    }

    /// Settles the root's await receipts for the terminal being published,
    /// before anything below can compact (spec 03 section 4.4). A failure
    /// leaves them for the settlement at the next admission or open.
    fn settle_terminal_waits(&self, exchange: crate::agent::TerminalExchange) {
        let settled = self
            .executor
            .block_on(super::super::result_mail::settle_terminal_waits(
                &self.config.topology_rows(),
                &self.config.session,
                &self.opened,
                exchange,
            ))
            .unwrap_or_else(|_| Err("application execution failed".to_owned()));
        if let Err(error) = settled {
            let _ = self.events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("wait receipt settlement failed: {error}"),
            ));
        }
    }
}

/// What the operator is told when a turn's outcome could not be observed. The
/// session is not lost: the next action reopens it and records the uncertainty
/// once, and nothing the turn started is retried.
pub(crate) const RECOVERY_PENDING_NOTE: &str = "the turn's outcome is uncertain; the \
     next prompt (or /recover) records that and continues";

/// True when an empty turn stopped because it thought until the budget ran out.
///
/// All three conditions matter. Empty text alone is the ordinary empty-stream
/// case. A turn that made tool calls did work, whatever it printed. And a
/// finish other than `Length` did not hit the bound, so whatever silenced it
/// was not the budget.
pub(in crate::runtime) const fn spent_budget_on_reasoning(
    assistant_is_empty: bool,
    tool_calls: usize,
    finish_reason: plakous::model::FinishReason,
) -> bool {
    assistant_is_empty
        && tool_calls == 0
        && matches!(finish_reason, plakous::model::FinishReason::Length)
}

/// What the operator sees when a turn spends its whole output budget thinking.
///
/// `output_tokens` is the budget here, not merely a count: the completion
/// stopped at `Length`, so the provider emitted exactly the bound it was given
/// and the reasoning share is what is left of it. Saying "try again" would be
/// wrong advice — the same prompt will do the same thing — so the note names
/// the cause and leaves the decision to the reader.
pub(super) fn reasoning_only_note(reasoning_tokens: u64, output_tokens: u64) -> String {
    format!(
        "empty completion: the model spent its output budget on reasoning; \
         {reasoning_tokens} reasoning tokens of a {output_tokens}-token budget"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use super::{reasoning_only_note, spent_budget_on_reasoning};
    use plakous::model::FinishReason;

    #[test]
    fn capacity_commit_failure_requires_recovery_and_pauses_live_drain() {
        use super::SessionQueueDrainState;
        use crate::runtime::host_loop::HostLoop;
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
        use crate::runtime::{Sender, TurnHostOutcome};
        use crate::surface_protocol::HostEvent;
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        for presentation in [
            super::TerminalErrorPresentation::Events,
            super::TerminalErrorPresentation::Return,
        ] {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
            let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
            host.config.compaction_enabled = true;
            host.config.session.queue_drain_state = SessionQueueDrainState::LiveAutoDrain;
            crate::sessions::queue::enqueue(
                &mut host.config.session.pending_queue,
                "queued follow-up".into(),
            )
            .unwrap();
            host.config.store.save(&mut host.config.session).unwrap();
            let source = host.config.session.plakous_locator;
            let submission = host
                .executor
                .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
                .unwrap()
                .unwrap();
            host.executor
                .block_on(submission.result())
                .unwrap()
                .unwrap();
            let head = host.opened.locator();
            assert_ne!(source, Some(head));
            let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                host.config.store.clone(),
                SaveFailure {
                    fail: AtomicBool::new(true),
                },
            ));
            host.config.store = store.clone();
            assert!(!host.publish_terminal(
                Ok(TurnHostOutcome::ContextCapacity {
                    source_checkpoint: head.checkpoint_id(),
                    recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
                    error: "synthetic overflow".into(),
                }),
                super::Instant::now(),
                presentation,
            ));
            // Storage returning must not rearm the drain before result replay.
            store.faults.fail.store(false, Ordering::SeqCst);
            assert!(host.recovery_blocked);
            assert_eq!(
                host.config.session.queue_drain_state,
                SessionQueueDrainState::RestoredPaused
            );
            assert_eq!(host.config.session.pending_queue.len(), 1);

            assert_eq!(host.config.session.plakous_locator, source);
            assert_eq!(
                store
                    .inner
                    .load(&host.config.session.id)
                    .unwrap()
                    .plakous_locator,
                source
            );
            assert!(events.try_iter().any(|event| matches!(
                event,
                HostEvent::Row(row)
                    if row.text().is_some_and(|text| text.contains("queue drain remains fail-closed"))
            )));
            host.release().unwrap();
        }
    }

    #[test]
    fn capacity_auto_compact_sees_the_terminal_head_binding() {
        use crate::runtime::host_loop::HostLoop;
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;
        use crate::runtime::{Sender, TurnHostOutcome};
        use crate::surface_protocol::HostEvent;
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let prepared = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured = std::sync::Arc::clone(&prepared);
        let store = std::sync::Arc::clone(&config.store);
        let id = config.session.id.clone();
        std::sync::Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .observe = std::sync::Arc::new(move |workflow, boundary| {
            if workflow == super::TestHostWorkflow::Compact
                && boundary == super::TestHostBoundary::Prepared
            {
                *captured.lock().unwrap() = store.load_intents(&id).unwrap().workflow_requests;
            }
        });
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, events) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        host.config.compaction_enabled = true;
        host.config.store.save(&mut host.config.session).unwrap();
        let source = host.config.session.plakous_locator;
        let submission = host
            .executor
            .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
            .unwrap()
            .unwrap();
        host.executor
            .block_on(submission.result())
            .unwrap()
            .unwrap();
        let head = host.opened.locator();
        assert_ne!(source, Some(head));
        // As run_prompt reaches publish_terminal after a Compactable overflow
        // whose source is the exact attached head.
        host.publish_terminal(
            Ok(TurnHostOutcome::ContextCapacity {
                source_checkpoint: head.checkpoint_id(),
                recovery: plakous_host_protocol::ContextCapacityRecovery::Compactable,
                error: "synthetic overflow".into(),
            }),
            super::Instant::now(),
            super::TerminalErrorPresentation::Events,
        );
        let notes = events
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(row) => row.text().map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !notes
                .iter()
                .any(|note| note.contains("compact source does not match")),
            "capacity auto-compaction refused its own exact head: {notes:?}"
        );
        let requests = prepared.lock().unwrap();
        assert!(
            requests.iter().any(|workflow| matches!(
                workflow.kind,
                crate::sessions::SessionWorkflowKind::Compact(
                    crate::sessions::ProductCompactReason::CapacityRecovery
                )
            )),
            "auto-compaction never prepared its capacity-recovery workflow"
        );
        assert_eq!(requests[0].source_locator, head);

        let views = host
            .config
            .store
            .load_views(&host.config.session.id)
            .unwrap()
            .unwrap();
        assert_eq!(
            super::super::super::projection::head_locator(&views),
            Some(host.opened.locator())
        );
        host.release().unwrap();
    }

    #[test]
    fn completed_terminal_projector_publishes_once_and_tracks_head() {
        use crate::runtime::host_loop::HostLoop;
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;
        use crate::runtime::{Sender, TurnHostOutcome};
        use crate::sessions::projector::{Submission, SubmissionKind};
        use plakous::agent::{CommandId, TurnResultOutcome};
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let item =
            crate::sessions::queue::enqueue(&mut host.config.session.pending_queue, "root".into())
                .unwrap();
        store.save(&mut host.config.session).unwrap();
        let command = format!("plak.queue.{}.{}", item.id, item.revision);
        store
            .save_submission(
                &mut host.config.session,
                &Submission {
                    command_id: command.clone(),
                    kind: SubmissionKind::RootStart,
                    queue_item_id: item.id.clone(),
                    queue_item_revision: item.revision,
                    target_turn_id: None,
                    canonical_command: item.text.clone(),
                    submitted_at_ms: item.submitted_at_ms,
                },
            )
            .unwrap();
        let submission = host
            .executor
            .block_on(
                host.opened
                    .agent()
                    .unwrap()
                    .start(CommandId::new(&command).unwrap(), &item.text),
            )
            .unwrap()
            .unwrap();
        let handle = submission.into_result().unwrap();
        let result = host.executor.block_on(handle.result()).unwrap().unwrap();
        let TurnResultOutcome::Completed(response) = result.outcome() else {
            panic!("completed turn required")
        };
        let assistant = response.output_text();
        let source = host.config.session.plakous_locator;
        let head = host.opened.locator();
        host.publish_terminal(
            Ok(TurnHostOutcome::Completed {
                assistant: assistant.clone(),
                finish_reason: FinishReason::Stop,
                finish_detail: None,
                reasoning_tokens: 0,
                output_tokens: 0,
                tool_calls: 0,
            }),
            super::Instant::now(),
            super::TerminalErrorPresentation::Events,
        );
        host.publish_projected_transcript(true);
        let stored = store.load(&id).unwrap();
        assert_ne!(source, Some(head));
        assert_eq!(stored.plakous_locator, Some(head));
        host.release().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        HostLoop::open(config, &rx, Sender::over(tx), None)
            .unwrap()
            .release()
            .unwrap();
        let count = store
            .load(&id)
            .unwrap()
            .transcript
            .iter()
            .filter(|row| row.text() == Some(assistant.as_str()))
            .count();
        assert_eq!(count, 1, "the terminal result was published twice");
    }

    #[test]
    fn terminal_head_changes_only_after_the_projector_pass() {
        use crate::agent::TerminalExchange;
        use crate::runtime::host_loop::HostLoop;
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
        use crate::runtime::{Sender, TurnHostOutcome};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        for outcome in 0..3 {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
            let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
            host.config.store.save(&mut host.config.session).unwrap();
            let source = host.config.session.plakous_locator;
            let submission = host
                .executor
                .block_on(host.opened.agent().unwrap().prompt("synthetic terminal"))
                .unwrap()
                .unwrap();
            host.executor
                .block_on(submission.result())
                .unwrap()
                .unwrap();
            let head = host.opened.locator();
            assert_ne!(source, Some(head));
            let store = Arc::new(crate::runtime::fault_store::FaultStore::new(
                host.config.store.clone(),
                SaveFailure {
                    fail: AtomicBool::new(true),
                },
            ));
            host.config.store = store.clone();
            let drive = || {
                Ok(match outcome {
                    0 => TurnHostOutcome::Failed {
                        transient: None,
                        error: "synthetic failure".into(),
                        exchange: TerminalExchange::Closed,
                    },
                    1 => TurnHostOutcome::Interrupted {
                        assistant: "synthetic partial".into(),
                        exchange: TerminalExchange::Closed,
                    },
                    _ => TurnHostOutcome::RecoveryRequired {
                        error: "synthetic recovery".into(),
                    },
                })
            };
            host.publish_terminal(
                drive(),
                super::Instant::now(),
                super::TerminalErrorPresentation::Events,
            );
            assert_eq!(host.config.session.plakous_locator, source);
            assert_eq!(
                store
                    .inner
                    .load(&host.config.session.id)
                    .unwrap()
                    .plakous_locator,
                source
            );
            store.faults.fail.store(false, Ordering::SeqCst);
            host.publish_terminal(
                drive(),
                super::Instant::now(),
                super::TerminalErrorPresentation::Events,
            );
            assert_eq!(host.config.session.plakous_locator, source);
            host.publish_projected_transcript(true);
            assert_eq!(host.config.session.plakous_locator, Some(head));
            host.release().unwrap();
        }
    }

    /// `deepseek-v4-flash` lost two tasks,
    /// each stopping at exactly 4096 output tokens with 4095 of them reasoning
    /// and one token left for visible text. Both surfaced as "empty stream —
    /// try again", which is the wrong advice: the same prompt does the same
    /// thing. All three conditions are required, so an ordinary empty stream
    /// and a turn that did tool work keep the old note.
    #[test]
    fn only_an_empty_toolless_length_stop_is_a_reasoning_only_completion() {
        assert!(spent_budget_on_reasoning(true, 0, FinishReason::Length));
        assert!(
            !spent_budget_on_reasoning(false, 0, FinishReason::Length),
            "visible text is not an empty completion"
        );
        assert!(
            !spent_budget_on_reasoning(true, 1, FinishReason::Length),
            "a turn that called a tool did work, whatever it printed"
        );
        for finish in [
            FinishReason::Stop,
            FinishReason::ToolCalls,
            FinishReason::Refusal,
        ] {
            assert!(
                !spent_budget_on_reasoning(true, 0, finish),
                "{finish:?} did not hit the output bound"
            );
        }
    }

    /// The note names the cause and the numbers, and does not say "try again".
    #[test]
    fn the_reasoning_only_note_names_the_budget_it_spent() {
        let note = reasoning_only_note(4_095, 4_096);
        assert_eq!(
            note,
            "empty completion: the model spent its output budget on reasoning; \
             4095 reasoning tokens of a 4096-token budget"
        );
        assert!(
            !note.contains("try again"),
            "the same prompt will do the same thing"
        );
    }
}

/// Spec 03 section 4.4: a root terminal settles the root's await receipts by
/// what its checkpoint did with the turn's exchange.
#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod wait_settlement_tests {
    use super::{HostLoop, TerminalErrorPresentation, TurnHostOutcome};
    use crate::agent::TerminalExchange;
    use crate::agent_tree::{MailDurableState, ProductAgentTree};
    use crate::agent_tree_protocol::{SpawnAgentRequest, SubmitResultRequest, WriterMode};
    use crate::runtime::test_kit::TestExtension;
    use crate::runtime::test_support::fresh_injected_attached_host;
    use crate::topology_runtime::ProductTopologyRuntime;
    use std::sync::Arc;
    use std::time::Instant;

    /// One host over a tree whose root holds one unread Result envelope.
    fn with_unread_result(
        test: impl FnOnce(&mut HostLoop<'_, TestExtension>, &Arc<ProductTopologyRuntime>),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(dir.path());
        let mut tree = ProductAgentTree::bootstrap_root_for_test(
            &config.session.id,
            "root",
            "owner",
            config.session.workspace_root.clone(),
            "selected-tuple",
            WriterMode::SingleLease,
        )
        .unwrap();
        let child = tree
            .spawn_agent("root", &SpawnAgentRequest::test_review(), None)
            .unwrap();
        tree.submit_result(
            &child.agent_id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let runtime =
            crate::topology_runtime::insert_root(config.session.id.clone(), tree, None).unwrap();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _observed) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(
            config,
            &receiver,
            crate::runtime::Sender::over(events),
            None,
        )
        .unwrap();
        test(&mut host, &runtime);
        host.release().unwrap();
    }

    /// A bare wait under a fresh call id, which canonical history never
    /// holds: it acknowledges the unread envelope and keeps one receipt.
    fn wait_outside_history(
        host: &HostLoop<'_, TestExtension>,
        runtime: &Arc<ProductTopologyRuntime>,
    ) {
        let tree_id = runtime.read(|tree| tree.tree_id.clone()).unwrap();
        let receipt = host
            .executor
            .block_on(crate::agent::invoke_topology_tool_on_hub(
                &tree_id,
                None,
                "wait_agent",
                &serde_json::json!({ "timeout_ms": 1 }),
                &uuid::Uuid::new_v4().to_string(),
            ))
            .unwrap()
            .unwrap();
        assert!(receipt.succeeded, "{}", receipt.detail);
        assert_eq!(receipts_and_unread(runtime), (1, 0));
    }

    fn receipts_and_unread(runtime: &Arc<ProductTopologyRuntime>) -> (usize, usize) {
        runtime
            .read(|tree| {
                (
                    tree.retained_wait_receipts("root").len(),
                    tree.pending_mail("root").len(),
                )
            })
            .unwrap()
    }

    /// A failure the driver did not close (`Internal`) committed none of its
    /// final calls: publication settles from committed history and the mail
    /// is unread again. A terminal the host never observed leaves the
    /// receipt alone, and the next prompt settles it before it selects mail,
    /// so the restored envelope rides that prompt.
    #[test]
    fn a_failure_without_a_committed_exchange_returns_wait_mail_to_unread() {
        with_unread_result(|host, runtime| {
            wait_outside_history(host, runtime);
            host.publish_terminal(
                Ok(TurnHostOutcome::Failed {
                    transient: None,
                    error: "contained panic".into(),
                    exchange: TerminalExchange::Unclosed,
                }),
                Instant::now(),
                TerminalErrorPresentation::Events,
            );
            assert_eq!(receipts_and_unread(runtime), (0, 1));

            wait_outside_history(host, runtime);
            host.publish_terminal(
                Ok(TurnHostOutcome::Interrupted {
                    assistant: String::new(),
                    exchange: TerminalExchange::Unresolved,
                }),
                Instant::now(),
                TerminalErrorPresentation::Events,
            );
            assert_eq!(receipts_and_unread(runtime), (1, 0));
            let message_id = runtime
                .read(|tree| tree.messages().keys().next().cloned().unwrap())
                .unwrap();
            let item = crate::sessions::queue::enqueue(
                &mut host.config.session.pending_queue,
                "next".into(),
            )
            .unwrap();
            host.config.session.record_prompt(item.text.clone());
            host.config.store.save(&mut host.config.session).unwrap();
            host.run_prompt(item.text.clone(), Some(item), false, None)
                .unwrap();
            assert_eq!(receipts_and_unread(runtime).0, 0);
            assert!(
                host.config
                    .store
                    .load_intents(&host.config.session.id)
                    .unwrap()
                    .submissions
                    .iter()
                    .any(|submission| {
                        runtime
                            .read(|tree| {
                                tree.command(&submission.command_id)
                                    .is_some_and(|batch| batch.message_ids.contains(&message_id))
                            })
                            .unwrap()
                    }),
                "the restored envelope rides the next prompt"
            );
        });
    }

    /// A cancelled turn closed its exchange before its terminal was
    /// published: its receipts retire at publication, so neither the next
    /// prompt nor a reopen settles them and the mail is not delivered again.
    #[test]
    fn a_cancelled_turn_with_a_closed_exchange_retires_its_waits_for_good() {
        with_unread_result(|host, runtime| {
            use plakous::sessions::feed::{FeedCancelCause, FeedFact, FeedRecord, FeedTurnOutcome};
            wait_outside_history(host, runtime);
            // The live publication is presentation, not settlement authority.
            // Seed the committed cancellation this topology-only fixture's
            // synthetic TurnHostOutcome represents.
            let session = host.opened.locator().session_id();
            runtime.mutate_then_emit(false, |tree| {
                let seq = tree.feed_cursor(&session.to_string()) + 1;
                let header = serde_json::from_value(serde_json::json!({
                    "session_id": session, "seq": seq,
                    "branch_id": host.opened.locator().branch_id(), "committed_at_ms": seq,
                    "change": {"advanced": {"parent": host.opened.locator().checkpoint_id(),
                        "checkpoint": host.opened.locator().checkpoint_id(), "revision": seq, "head": "quiescent"}}
                })).unwrap();
                let record = FeedRecord::from_store(header, vec![FeedFact::TurnTerminal {
                    turn_id: "00000000-0000-4000-8000-000000000001".parse().unwrap(),
                    command_id: plakous::agent::CommandId::new("fixture:cancelled").unwrap(),
                    outcome: FeedTurnOutcome::Cancelled { cause: FeedCancelCause::Requested },
                    closed_exchange: true, steering_settled_unapplied: false,
                    usage: plakous::agent::TurnUsage::default(),
                }]);
                tree.project_feed("root", &session.to_string(), &[record], &crate::agent_tree::TreeFeedReads::default())
                    .map(|_| ()).map_err(|error| error.to_string())
            }).unwrap();
            host.publish_terminal(
                Ok(TurnHostOutcome::Interrupted {
                    assistant: String::new(),
                    exchange: TerminalExchange::Closed,
                }),
                Instant::now(),
                TerminalErrorPresentation::Events,
            );
            assert_eq!(receipts_and_unread(runtime), (0, 0));
            host.executor
                .block_on(host.opened.detach())
                .unwrap()
                .unwrap();
            let root = host.config.product_home.parent().unwrap();
            let (mut config, _) = fresh_injected_attached_host(root);
            config.state_roots = host.config.state_roots.clone();
            config.store = host.config.store.clone();
            config.session = config.store.load(&host.config.session.id).unwrap();
            config.test_injection = host.config.test_injection.clone();
            let mut reopened =
                HostLoop::open(config, host.actions, host.events.clone(), None).unwrap();
            assert_eq!(receipts_and_unread(runtime), (0, 0));
            assert!(
                runtime
                    .read(|tree| tree
                        .messages()
                        .values()
                        .all(|mail| mail.durable == MailDurableState::Acknowledged))
                    .unwrap()
            );
            reopened
                .executor
                .block_on(reopened.opened.detach())
                .unwrap()
                .unwrap();
            host.opened = reopened.opened;
            host.config.session = reopened.config.session;
        });
    }
}
