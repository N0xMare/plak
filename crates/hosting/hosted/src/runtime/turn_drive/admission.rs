//! Submit an immutable root intent and settle its queue through the projector.

use crate::runtime::ledger::RootLedger;
use crate::surface_protocol::ContentEvent;

use super::super::admission::{QueuePersistAction, log_queue_persist_failed};
use super::{
    Agent, CommandAckDisposition, CommandId, DriveTurnError, DrivenTurn,
    HOST_CANCEL_SETTLE_TIMEOUT, SessionQueueItem, TurnDrive, TurnHost, admitted_turn_lifecycle,
    emit_prompt_history, emit_queue_snapshot, emit_turn_lifecycle, operator_facing_agent_error,
    sessions, settle_cancel, terminal_turn_outcome, turn_lifecycle,
};
use super::{HostBoundary, HostWorkflow, observe_boundary};
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger, Settlement};
use crate::sessions::{NoticeScope, TranscriptRow};

#[allow(
    clippy::too_many_lines,
    reason = "Root admission owns prompt preparation, durable delivery identity, runtime dispatch, and refusal cleanup without replaying admitted work."
)]
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
        host_wakes,
        quit_started_at,
        quit_job_stops,
        actions,
        deferred,
        events,
        rows,
        roots,
        store,
        projection_store,
        mut live_projector,
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
        runtime_factory,
    } = host;
    job_agent.set_unread_mail_pending(false);
    job_agent.set_notice_delivery_available(host_wakes && !session.child_wake_suppressed);
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
                remedy: None,
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
        text: sessions::with_image_chips(
            &prompt,
            item.images.iter().map(sessions::draft_image_chip),
        ),
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
    let mut projected_root_turns = None;
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
        } else {
            let mut candidate = session.clone();
            RootLedger::new(rows, store, &mut candidate, item)
                .prepare(CommandIntent {
                    command_id: &command_text,
                    kind: CommandKind::RootStart,
                    base_text: &command,
                    batch: result_batch.map_or(BatchRule::None, BatchRule::RootResults),
                })
                .await
                .map_err(crate::session_store::RecordStoreError::from)
                .inspect_err(|error| {
                    log_queue_persist_failed(
                        home,
                        session,
                        QueuePersistAction::RootDelivery,
                        error.message(),
                    );
                })?;
            candidate
        };
        *session = prepared;
        observe_boundary(
            runtime_factory,
            HostWorkflow::RootDelivery,
            HostBoundary::Prepared,
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
                                input: item.user_input(&command),
                            }
                        } else if host_authored {
                            plakous::agent::TurnCommand::StartInstruction {
                                command_id,
                                text: command.clone(),
                            }
                        } else {
                            plakous::agent::TurnCommand::Start {
                                command_id,
                                input: item.user_input(&command),
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
                            RootLedger::new(rows, store, &mut candidate, item)
                                .settle(&command_text, Settlement::Refused { pause: None })
                                .await
                                .map_err(String::from)
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
                                remedy: admission_rejection_remedy(
                                    error.kind(),
                                    control_source(&error),
                                ),
                            },
                        ));
                    }
                    return Ok(DrivenTurn::continuing(
                        super::TurnHostOutcome::RecoveryRequired {
                            error: error.to_string(),
                            remedy: None,
                        },
                    ));
                }
            })
        } else {
            None
        };
        observe_boundary(
            runtime_factory,
            HostWorkflow::RootDelivery,
            HostBoundary::RuntimeReturned,
        );
        if let Some(followed) = session.plakous_locator {
            match super::super::projection::queue_after_ack(
                runtime,
                projection_store,
                &session.id,
                followed,
                live_projector
                    .as_deref_mut()
                    .map(|live| (live, &live_target)),
            )
            .await
            {
                Ok(after_ack) => {
                    session.queue_commands = after_ack.commands;
                    projected_root_turns = Some(after_ack.root_turns);
                }
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
                remedy: admission_rejection_remedy(error.kind(), control_source(error)),
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
                        remedy: rejection.remedy,
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
                    if let Err(error) = RootLedger::new(rows, store, &mut candidate, item)
                        .settle(
                            &command_text,
                            Settlement::Refused {
                                pause: rejection.session_wide.then(|| {
                                    sessions::projector::HostPauseCause::UnrecordedRefusal {
                                        reason: rejection.admission_message.clone(),
                                    }
                                }),
                            },
                        )
                        .await
                        .map_err(String::from)
                    {
                        log_queue_persist_failed(
                            home,
                            session,
                            QueuePersistAction::RootDelivery,
                            &error,
                        );
                        return Ok(DrivenTurn::continuing(super::TurnHostOutcome::RecoveryRequired {
                            error: "unrecorded refusal could not release its submission; recovery required".into(),
                         remedy: None, }));
                    }
                    *session = candidate;
                } else if rejection.session_wide {
                    session.queue_drain_state = sessions::QueueDrainState::RestoredPaused;
                }
                return Ok(DrivenTurn::continuing(
                    super::TurnHostOutcome::AdmissionRejected {
                        error: rejection.admission_message,
                        stop_drain: unrecorded || rejection.session_wide,
                        remedy: rejection.remedy,
                    },
                ));
            }
        } else if let Some(submission) = submission
            .as_ref()
            .filter(|submission| submission.acknowledgement().is_accepted())
        {
            if submission.acknowledgement().disposition() == CommandAckDisposition::Duplicate {
                RootLedger::new(rows, store, session, item)
                    .with_projection(projection_store)
                    .duplicate_consumed(
                        runtime,
                        runtime_session,
                        submission.acknowledgement().command_id().as_str(),
                    )
                    .await
                    .map_err(String::from)?
            } else {
                false
            }
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
            RootLedger::new(rows, store, &mut candidate, item)
                .settle(&command_text, Settlement::Consumed)
                .await
                .map_err(String::from)
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
            let _ = events.send(ContentEvent::Row(row));
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
        observe_boundary(
            runtime_factory,
            HostWorkflow::RootDelivery,
            HostBoundary::ProductPublished,
        );
        let through = submission
            .as_ref()
            .and_then(|submission| submission.acknowledgement().feed_seq())
            .map(plakous::sessions::feed::FeedSeq::get)
            .unwrap_or(0);
        if let Err(error) = super::super::projection::project_tree_as(
            runtime,
            rows,
            &super::super::rebind::durable_tree_id(rows, session),
            runtime_session,
            super::super::projection::TreePass::RunningTurn { through },
        )
        .await
        {
            let _ = events.notice(TranscriptRow::error(
                NoticeScope::Tree,
                format!("child mail projector deferred: {error}"),
            ));
        }
        observe_boundary(
            runtime_factory,
            HostWorkflow::RootDelivery,
            HostBoundary::ProductCommitted,
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
    // The after-ack pass's views already hold the count; a stored read is
    // only for a pass that left debt.
    if let Some(root_turns) = projected_root_turns {
        candidate.root_turn_count = root_turns;
    } else if let Some(views) = store.load_views(&session.id)? {
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
        let _ = events.send(ContentEvent::UserMessage { text, sent_at_ms });
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
    let turn_observer = runtime_factory.turn_observer(&prompt);
    drop(prompt);
    let driven = Box::pin(
        TurnDrive::new(
            turn.control(),
            TurnHost {
                job_agent,
                host_wakes,
                quit_started_at,
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
                runtime_factory,
            },
            turn_observer,
        )
        .run(turn),
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
        let _ = events.send(ContentEvent::Row(TranscriptRow::error(
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

fn admission_rejection_remedy(
    kind: plakous::agent::ControlErrorKind,
    source: Option<&plakous::agent::AgentError>,
) -> Option<plakous_host_protocol::notice_remedy::NoticeRemedy> {
    source
        .and_then(super::super::turn::agent_error_remedy)
        .or_else(|| {
            (kind == plakous::agent::ControlErrorKind::StoragePressure).then_some(
                plakous_host_protocol::notice_remedy::NoticeRemedy::ReclaimRetainedHistory {},
            )
        })
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
    crate::runtime::ledger::project_duplicate(
        runtime,
        store,
        product_id,
        locator,
        ack.command_id().as_str(),
    )
    .await
    .map_err(String::from)
}

struct StartRefusal {
    disposition: CommandAckDisposition,
    kind: plakous::agent::ControlErrorKind,
    message: String,
    admission_message: String,
    session_wide: bool,
    remedy: Option<plakous_host_protocol::notice_remedy::NoticeRemedy>,
}

/// Injected refusals, keyed by the delivery's operation id: whichever worker
/// thread polls the admission takes the one its test stubbed.
#[cfg(test)]
static TEST_START_REFUSALS: std::sync::Mutex<
    Option<std::collections::HashMap<String, StartRefusal>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
fn take_test_start_refusal(operation: &str) -> Option<StartRefusal> {
    TEST_START_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .and_then(|refusals| refusals.remove(operation))
}

#[cfg(test)]
pub(in crate::runtime) fn stub_start_refusal(
    operation: String,
    disposition: CommandAckDisposition,
    kind: plakous::agent::ControlErrorKind,
) {
    TEST_START_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(
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
                remedy: admission_rejection_remedy(kind, None),
            },
        );
}

#[cfg(test)]
static TEST_INGRESS_REFUSALS: std::sync::Mutex<
    Option<std::collections::HashMap<String, plakous::agent::ControlError>>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
fn take_test_ingress_refusal(operation: &str) -> Option<plakous::agent::ControlError> {
    TEST_INGRESS_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_mut()
        .and_then(|refusals| refusals.remove(operation))
}

#[cfg(test)]
pub(in crate::runtime) fn stub_ingress_backpressure(
    operation: String,
    error: plakous::agent::ControlError,
) {
    TEST_INGRESS_REFUSALS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get_or_insert_with(std::collections::HashMap::new)
        .insert(operation, error);
}
