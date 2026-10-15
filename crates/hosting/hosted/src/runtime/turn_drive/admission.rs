//! Submit an immutable root intent and settle its queue through the projector.

use super::super::admission::{QueuePersistAction, log_queue_persist_failed};
use super::{
    Agent, CommandAckDisposition, CommandId, DriveTurnError, DrivenTurn,
    HOST_CANCEL_SETTLE_TIMEOUT, HostEvent, SessionQueueItem, TurnDrive, TurnHost,
    admitted_turn_lifecycle, emit_prompt_history, emit_queue_snapshot, emit_turn_lifecycle,
    operator_facing_agent_error, sessions, settle_cancel, terminal_turn_outcome, turn_lifecycle,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow, observe_test_boundary};
use crate::sessions::{NoticeScope, TranscriptRow};

pub(in crate::runtime) async fn drive_one_turn<P: plakous_host_protocol::ProtocolExtension>(
    agent: &Agent,
    runtime: &plakous::sessions::AgentSessions,
    prompt: String,
    host: TurnHost<'_, P>,
    item: &SessionQueueItem,
    user_echoed: bool,
    selected_command: (String, Option<sessions::ResultMailBatch>),
) -> Result<DrivenTurn, DriveTurnError> {
    let TurnHost {
        job_agent,
        quit_started_at,
        quit_abandoned_turn,
        quit_job_stops,
        actions,
        deferred,
        events,
        rows,
        roots,
        store,
        projection_store,
        live_projector,
        live_target,
        home,
        session,
        context_projection,
        enrolled_source_bytes,
        root_template,
        attached_one_shot,
        ready_catalog,
        surface,
        log,
        subscription_service,
        subscription_usage_refresh,
        held_usage,
        ledger,
        last_tree_revision,
        job_events,
        #[cfg(any(test, feature = "test-support"))]
        test_injection,
    } = host;
    let runtime_session = session
        .plakous_locator
        .ok_or_else(|| "root mail projector has no runtime session".to_owned())?
        .session_id();
    let existing_submission = session.queue_commands.pending_for(item).cloned();
    if session.queue_commands.has_pending() && existing_submission.is_none() {
        return Ok(DrivenTurn::continuing(
            super::TurnHostOutcome::AdmissionRejected {
                error:
                    "an earlier command submission is still pending; resume that exact queued item"
                        .into(),
                stop_drain: true,
            },
        ));
    }
    let host_authored = existing_submission
        .as_ref()
        .is_some_and(|submission| submission.kind == sessions::projector::SubmissionKind::HostWake);
    // A host-authored wake prompt is model input, not an operator line: it
    // leaves no transcript row. The row's send time is the queue item's
    // submit time.
    let user_line = (!host_authored).then(|| TranscriptRow::User {
        text: prompt.clone(),
        sent_at_ms: Some(item.submitted_at_ms),
    });
    let (command, result_batch) = if let Some(intent) = &existing_submission {
        (intent.canonical_command.clone(), None)
    } else {
        selected_command
    };
    let resubmitted_steer = existing_submission.as_ref().is_some_and(|submission| {
        submission.kind == sessions::projector::SubmissionKind::SendNowSteer
    });
    let (turn, duplicate_result) = {
        #[cfg(test)]
        let operation_id = format!("plak:deliver:{}:{}", item.id, item.revision);
        let command_text = existing_submission.as_ref().map_or_else(
            || format!("plak.queue.{}.{}", item.id, item.revision),
            |intent| intent.command_id.clone(),
        );
        let command_id = CommandId::new(&command_text)
            .map_err(|_| "queued command identity is invalid".to_owned())?;
        let prepared = if existing_submission.is_some() {
            session.clone()
        } else if let Some(batch) = result_batch {
            super::super::result_mail::prepare(rows, store, session, item, &command, batch)
                .inspect_err(|error| {
                    log_queue_persist_failed(
                        home,
                        session,
                        QueuePersistAction::RootDelivery,
                        error,
                    );
                })?
        } else {
            let mut candidate = session.clone();
            store
                .save_submission(
                    &mut candidate,
                    &sessions::projector::Submission {
                        command_id: command_text.clone(),
                        kind: sessions::projector::SubmissionKind::RootStart,
                        queue_item_id: item.id.clone(),
                        queue_item_revision: item.revision,
                        target_turn_id: None,
                        canonical_command: command.clone(),
                        submitted_at_ms: item.submitted_at_ms,
                    },
                )
                .map_err(String::from)
                .inspect_err(|error| {
                    log_queue_persist_failed(
                        home,
                        session,
                        QueuePersistAction::RootDelivery,
                        error,
                    );
                })?;
            candidate
        };
        *session = prepared;
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            test_injection,
            TestHostWorkflow::RootDelivery,
            TestHostBoundary::Prepared,
        );
        #[cfg(test)]
        let stub = take_test_start_refusal(&operation_id);
        #[cfg(test)]
        let dispatch = stub.is_none();
        #[cfg(not(test))]
        let dispatch = true;
        let submission = if dispatch {
            let dispatch_result = {
                #[cfg(test)]
                let ingress_refusal = take_test_ingress_refusal(&operation_id);
                #[cfg(not(test))]
                let ingress_refusal: Option<plakous::agent::ControlError> = None;
                if let Some(error) = ingress_refusal {
                    Err(error)
                } else {
                    agent
                        .command(if resubmitted_steer {
                            plakous::agent::TurnCommand::Steer {
                                command_id,
                                turn_id: existing_submission
                                    .as_ref()
                                    .and_then(|submission| submission.target_turn_id.as_deref())
                                    .ok_or_else(|| "stored steer has no target turn".to_owned())?
                                    .parse()
                                    .map_err(|_| "stored steer target is invalid".to_owned())?,
                                text: command.clone(),
                            }
                        } else if host_authored {
                            plakous::agent::TurnCommand::StartInstruction {
                                command_id,
                                text: command.clone(),
                            }
                        } else {
                            plakous::agent::TurnCommand::Start {
                                command_id,
                                text: command.clone(),
                            }
                        })
                        .await
                }
            };
            Some(match dispatch_result {
                Ok(submission) => submission,
                Err(error) => {
                    if error.kind() == plakous::agent::ControlErrorKind::Backpressure {
                        if existing_submission.is_none() {
                            let mut candidate = session.clone();
                            forget_unadmitted_root_submission(&mut candidate, item, host_authored);
                            store
                                .discard_submission(&mut candidate, &command_text, None)
                                .inspect_err(|save_error| {
                                    log_queue_persist_failed(
                                        home,
                                        session,
                                        QueuePersistAction::RootDelivery,
                                        save_error,
                                    );
                                })?;
                            *session = candidate;
                        }
                        return Ok(DrivenTurn::continuing(
                            super::TurnHostOutcome::AdmissionRejected {
                                error: admission_rejection_message(
                                    error.kind(),
                                    error.message(),
                                    control_source(&error),
                                ),
                                stop_drain: true,
                            },
                        ));
                    }
                    return Ok(DrivenTurn::continuing(
                        super::TurnHostOutcome::RecoveryRequired {
                            error: error.to_string(),
                        },
                    ));
                }
            })
        } else {
            None
        };
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            test_injection,
            TestHostWorkflow::RootDelivery,
            TestHostBoundary::RuntimeReturned,
        );
        if let Some(followed) = session.plakous_locator {
            match super::super::projection::queue_after_ack(
                runtime,
                projection_store,
                &session.id,
                followed,
            )
            .await
            {
                Ok(commands) => session.queue_commands = commands,
                Err(error) => {
                    session.queue_projection_debt = true;
                    super::super::logging::log_event(
                        log,
                        &session.id,
                        crate::session_log::SessionEvent::new("projector.deferred")
                            .str("error", &error),
                    );
                }
            }
        }
        let refusal = submission.as_ref().and_then(|submission| {
            let ack = submission.acknowledgement();
            ack.rejection().map(|error| StartRefusal {
                disposition: ack.disposition(),
                kind: error.kind(),
                message: error.message().to_owned(),
                admission_message: admission_rejection_message(
                    error.kind(),
                    error.message(),
                    control_source(error),
                ),
                session_wide: session_wide_refusal(error.kind()),
            })
        });
        #[cfg(test)]
        let refusal = stub.or(refusal);
        let consumed = if let Some(rejection) = refusal {
            if rejection.kind == plakous::agent::ControlErrorKind::Reclaimed
                && matches!(
                    rejection.disposition,
                    CommandAckDisposition::Rejected | CommandAckDisposition::Duplicate
                )
            {
                true
            } else if matches!(
                rejection.kind,
                plakous::agent::ControlErrorKind::OutcomeUnknown
                    | plakous::agent::ControlErrorKind::Internal
                    | plakous::agent::ControlErrorKind::Shutdown
                    | plakous::agent::ControlErrorKind::LedgerFull
            ) {
                return Ok(DrivenTurn::continuing(
                    super::TurnHostOutcome::RecoveryRequired {
                        error: rejection.message,
                    },
                ));
            } else {
                let unrecorded = submission
                    .as_ref()
                    .is_none_or(|submission| submission.acknowledgement().feed_seq().is_none())
                    && store
                        .load_views(&session.id)?
                        .is_none_or(|views| !views.state.deliveries.contains_key(&command_text));
                if unrecorded {
                    let mut candidate = session.clone();
                    forget_unadmitted_root_submission(&mut candidate, item, host_authored);
                    if rejection.session_wide {
                        candidate.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
                    }
                    if let Err(error) = store.discard_submission(
                        &mut candidate,
                        &command_text,
                        rejection.session_wide.then(|| {
                            sessions::projector::HostPauseCause::UnrecordedRefusal {
                                reason: rejection.admission_message.clone(),
                            }
                        }),
                    ) {
                        log_queue_persist_failed(
                            home,
                            session,
                            QueuePersistAction::RootDelivery,
                            &error,
                        );
                        return Ok(DrivenTurn::continuing(super::TurnHostOutcome::RecoveryRequired {
                            error: "unrecorded refusal could not release its submission; recovery required".into(),
                        }));
                    }
                    *session = candidate;
                } else if rejection.session_wide {
                    session.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
                }
                return Ok(DrivenTurn::continuing(
                    super::TurnHostOutcome::AdmissionRejected {
                        error: rejection.admission_message,
                        stop_drain: unrecorded || rejection.session_wide,
                    },
                ));
            }
        } else if let Some(submission) = submission
            .as_ref()
            .filter(|submission| submission.acknowledgement().is_accepted())
        {
            duplicate_consumed(
                runtime,
                projection_store,
                &session.id,
                submission,
                session
                    .plakous_locator
                    .ok_or_else(|| "submission has no runtime locator".to_owned())?,
            )
            .await?
        } else {
            return Err("queued command acknowledgement is unsupported".into());
        };
        if consumed {
            let row = TranscriptRow::info(
                NoticeScope::Queue,
                "queued prompt already ran; its result is no longer kept",
            );
            let mut candidate = session.clone();
            candidate
                .pending_queue
                .retain(|queued| queued.id != item.id || queued.revision != item.revision);
            candidate.record_notice(row.clone());
            store
                .discard_submission(&mut candidate, &command_text, None)
                .inspect_err(|error| {
                    log_queue_persist_failed(
                        home,
                        session,
                        QueuePersistAction::RootDelivery,
                        error,
                    );
                })?;
            *session = candidate;
            emit_queue_snapshot(events, session);
            let _ = events.send(HostEvent::Row(row));
            return Ok(DrivenTurn::continuing(
                super::TurnHostOutcome::ConsumedDelivery,
            ));
        }
        if resubmitted_steer {
            emit_queue_snapshot(events, session);
            return Ok(DrivenTurn::continuing(
                super::TurnHostOutcome::ConsumedDelivery,
            ));
        }
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            test_injection,
            TestHostWorkflow::RootDelivery,
            TestHostBoundary::ProductPublished,
        );
        if let Err(error) = super::super::projection::project_tree(
            runtime,
            rows,
            &super::super::rebind::durable_tree_id(rows, session),
            runtime_session,
        )
        .await
        {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("child mail projector deferred: {error}"),
            ));
        }
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            test_injection,
            TestHostWorkflow::RootDelivery,
            TestHostBoundary::ProductCommitted,
        );
        let submission =
            submission.ok_or_else(|| "accepted command omitted its submission".to_owned())?;
        let duplicate_result = submission.result().cloned();
        let turn = submission.into_turn();
        (turn, duplicate_result)
    };
    let mut candidate = session.clone();

    // Product transcript and queue acknowledgement commit only after Plakous
    // has admitted the prompt. Candidate persistence precedes in-memory/UI
    // publication, so a failed commit leaves the exact queue order unchanged.
    if let Some(user_line) = user_line {
        candidate.append_transcript(user_line);
    }
    if let Some(views) = store.load_views(&session.id)? {
        candidate.root_turn_count = views.state.counters.root_turns;
    }
    if !host_authored {
        candidate.child_wake_suppressed = false;
    }
    if !host_authored && candidate.title.as_ref().is_none_or(String::is_empty) {
        let title: String = candidate
            .transcript
            .last()
            .and_then(|row| match row {
                TranscriptRow::User { text, .. } => Some(text.chars().take(48).collect()),
                _ => None,
            })
            .unwrap_or_default();
        if !title.is_empty() {
            candidate.title = Some(title);
        }
    }
    candidate.touch();
    if let Err(error) = store.save(&mut candidate) {
        log_queue_persist_failed(home, &candidate, QueuePersistAction::RootDelivery, &error);
        let settled = match turn.as_ref() {
            Some(turn) => settle_cancel(turn.control().cancel(), HOST_CANCEL_SETTLE_TIMEOUT)
                .await
                .settled(),
            None => true,
        };
        return Err(if settled {
            "admitted prompt could not be persisted; turn cancelled".into()
        } else {
            "admitted prompt could not be persisted and the cancel did not settle; the turn may still be running"
                .into()
        });
    }
    *session = candidate;
    session
        .pending_queue
        .retain(|item| !session.queue_commands.is_consumed(item));
    emit_prompt_history(events, session);
    emit_queue_snapshot(events, session);
    if !user_echoed
        && !host_authored
        && let Some((text, sent_at_ms)) = session.transcript.last().and_then(|row| match row {
            TranscriptRow::User { text, sent_at_ms } => Some((text.clone(), *sent_at_ms)),
            _ => None,
        })
    {
        let _ = events.send(HostEvent::UserMessage { text, sent_at_ms });
    }
    if turn.is_none() {
        let result = duplicate_result
            .ok_or_else(|| "duplicate command omitted its exact result authority".to_owned())?
            .result()
            .await
            .map_err(|error| operator_facing_agent_error("agent turn failed", &error))?;
        emit_turn_lifecycle(events, log, session, turn_lifecycle(&result));
        // A duplicate command replays a completed result; no operation of
        // this process reported a prepared input for it.
        let outcome = terminal_turn_outcome(&result);
        super::super::projection::project_tree(
            runtime,
            rows,
            &super::super::rebind::durable_tree_id(rows, session),
            runtime_session,
        )
        .await
        .map_err(String::from)?;
        return Ok(DrivenTurn::continuing(outcome));
    }
    let turn = turn.ok_or_else(|| "new command omitted its turn authority".to_owned())?;
    // The product save above is the admission boundary: the turn's owner now
    // exists, so the correlated lifecycle marker opens here.
    emit_turn_lifecycle(
        events,
        log,
        session,
        admitted_turn_lifecycle(turn.control().turn_id()),
    );
    let driven = Box::pin(
        TurnDrive::new(
            turn.control(),
            TurnHost {
                job_agent,
                quit_started_at,
                quit_abandoned_turn,
                quit_job_stops,
                actions,
                deferred,
                events,
                rows,
                roots,
                store,
                projection_store,
                live_projector,
                live_target,
                home,
                session: &mut *session,
                context_projection,
                enrolled_source_bytes,
                root_template,
                attached_one_shot,
                ready_catalog,
                surface,
                log,
                subscription_service,
                subscription_usage_refresh,
                held_usage,
                ledger,
                last_tree_revision,
                job_events,
                #[cfg(any(test, feature = "test-support"))]
                test_injection,
            },
        )
        .run(
            turn,
            #[cfg(any(test, feature = "test-support"))]
            prompt,
        ),
    )
    .await;
    if let Err(error) = super::super::projection::project_tree(
        runtime,
        rows,
        &super::super::rebind::durable_tree_id(rows, session),
        runtime_session,
    )
    .await
    {
        let _ = events.send(HostEvent::Row(TranscriptRow::error(
            NoticeScope::Tree,
            format!("child mail projector deferred: {error}"),
        )));
    }
    driven
}

fn control_source(error: &plakous::agent::ControlError) -> Option<&plakous::agent::AgentError> {
    std::error::Error::source(error).and_then(|source| source.downcast_ref())
}

fn admission_rejection_message(
    kind: plakous::agent::ControlErrorKind,
    message: &str,
    source: Option<&plakous::agent::AgentError>,
) -> String {
    if kind == plakous::agent::ControlErrorKind::StoragePressure {
        return crate::storage_pressure::storage_pressure_message(
            source.and_then(plakous::sessions::SessionStorePressure::from_agent_error),
            &format!("{}: {message}", crate::runtime::PROMPT_NOT_ADMITTED),
        );
    }
    if source.is_some_and(plakous::agent::AgentError::is_storage_full) {
        return crate::storage_pressure::storage_full_message(message);
    }
    if kind == plakous::agent::ControlErrorKind::AdmissionRejected {
        message.to_owned()
    } else {
        format!("{}: {message}", crate::runtime::PROMPT_NOT_ADMITTED)
    }
}

fn session_wide_refusal(kind: plakous::agent::ControlErrorKind) -> bool {
    // Store integrity, physical exhaustion and precommit validation refusals
    // share AdmissionRejected; none justify draining more queued work.
    matches!(
        kind,
        plakous::agent::ControlErrorKind::StoragePressure
            | plakous::agent::ControlErrorKind::AdmissionRejected
    )
}

fn forget_unadmitted_root_submission(
    session: &mut sessions::SessionRecord,
    item: &SessionQueueItem,
    host_authored: bool,
) {
    if host_authored {
        session
            .pending_queue
            .retain(|queued| queued.id != item.id || queued.revision != item.revision);
        session.child_wake_suppressed = true;
    }
}

pub(in crate::runtime) async fn duplicate_consumed(
    runtime: &plakous::sessions::AgentSessions,
    store: &std::sync::Arc<dyn crate::session_store::SessionRecordStore>,
    product_id: &str,
    submission: &plakous::agent::TurnSubmission,
    locator: plakous::sessions::SessionLocator,
) -> Result<bool, String> {
    let ack = submission.acknowledgement();
    if ack.disposition() != CommandAckDisposition::Duplicate || !ack.is_accepted() {
        return Ok(false);
    }
    let views = tokio::time::timeout(
        super::super::host_loop::FINAL_PROJECTION_BOUND,
        super::super::projection::project_to_tail(runtime, store, product_id, locator),
    )
    .await
    .map_err(|_| "duplicate settlement projector pass exceeded its bound".to_owned())??;
    accepted_duplicate_consumed(&views, ack.command_id().as_str())
}

fn accepted_duplicate_consumed(
    views: &sessions::projector::SessionViews,
    command_id: &str,
) -> Result<bool, String> {
    if views.state.deliveries.contains_key(command_id) {
        return Ok(false);
    }
    if views
        .state
        .retained_from
        .is_some_and(|retained_from| retained_from > 0)
    {
        return Ok(true);
    }
    Err("accepted duplicate has no acceptance fact in the unpruned feed".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_duplicate_settles_from_the_fact_or_the_retained_feed_floor() {
        let mut views = sessions::projector::SessionViews::empty();
        for retained_from in [None, Some(0)] {
            views.state.retained_from = retained_from;
            assert!(accepted_duplicate_consumed(&views, "command").is_err());
        }
        views.state.retained_from = Some(7);
        assert_eq!(accepted_duplicate_consumed(&views, "command"), Ok(true));
        views.state.deliveries.insert(
            "command".to_owned(),
            sessions::projector::Delivery::Accepted,
        );
        assert_eq!(accepted_duplicate_consumed(&views, "command"), Ok(false));
        views.state.retained_from = None;
        assert_eq!(accepted_duplicate_consumed(&views, "command"), Ok(false));
    }
}

struct StartRefusal {
    disposition: CommandAckDisposition,
    kind: plakous::agent::ControlErrorKind,
    message: String,
    admission_message: String,
    session_wide: bool,
}

#[cfg(test)]
thread_local! {
    static TEST_START_REFUSALS: std::cell::RefCell<std::collections::HashMap<String, StartRefusal>> = std::cell::RefCell::default();
}

#[cfg(test)]
fn take_test_start_refusal(operation: &str) -> Option<StartRefusal> {
    TEST_START_REFUSALS.with(|refusals| refusals.borrow_mut().remove(operation))
}

#[cfg(test)]
pub(in crate::runtime) fn stub_start_refusal(
    operation: String,
    disposition: CommandAckDisposition,
    kind: plakous::agent::ControlErrorKind,
) {
    TEST_START_REFUSALS.with(|refusals| {
        refusals.borrow_mut().insert(
            operation,
            StartRefusal {
                disposition,
                kind,
                message: "injected runtime refusal".into(),
                admission_message: admission_rejection_message(
                    kind,
                    "injected runtime refusal",
                    None,
                ),
                session_wide: session_wide_refusal(kind),
            },
        );
    });
}

#[cfg(test)]
thread_local! {
    static TEST_INGRESS_REFUSALS: std::cell::RefCell<std::collections::HashMap<String, plakous::agent::ControlError>> = std::cell::RefCell::default();
}

#[cfg(test)]
fn take_test_ingress_refusal(operation: &str) -> Option<plakous::agent::ControlError> {
    TEST_INGRESS_REFUSALS.with(|refusals| refusals.borrow_mut().remove(operation))
}

#[cfg(test)]
pub(in crate::runtime) fn stub_ingress_backpressure(
    operation: String,
    error: plakous::agent::ControlError,
) {
    TEST_INGRESS_REFUSALS.with(|refusals| {
        refusals.borrow_mut().insert(operation, error);
    });
}
