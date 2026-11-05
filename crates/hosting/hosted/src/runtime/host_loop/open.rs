//! Open and reconcile one canonical attachment before accepting work.

use super::{
    CliFailure, CliResult, ContextDisposition, ContextQuery, Duration, HeldUsage, HostConfig,
    Instant, InterruptWatchState, Ordering, RefCell, RootActor, Sender, SessionEvent,
    SessionLedger, SessionLog, SessionQueueDrainState, SessionQueueItem, SubscriptionUsageRefresh,
    TreeSnapshotPush, build_session_from_config, elapsed_ms_since, emit_agent_plan_notes,
    emit_prompt_history, emit_queue_pause_note, emit_queue_snapshot, emit_unavailable_context,
    log_ev, log_host_attached, log_host_ready, log_session_ready, native_compact_facet, persist,
    query_context, queue_pause, report_context, send_binding, sessions, surface,
};
use crate::sessions::projector::{ProductTreeJoin as _, SubmissionKind, pending_command_ids};
use crate::sessions::{NoticeScope, TranscriptRow};
use crate::surface_protocol::ContentEvent;
use plakous_host_protocol::status::{HostPhase, HostStatusDetail};

impl<'a, P: plakous_host_protocol::ProtocolExtension> RootActor<'a, P> {
    #[allow(
        clippy::too_many_lines,
        reason = "Host open reconstructs projection, workflow settlements, queued submissions, and live attachments before exposing a ready session."
    )]
    pub(super) async fn open(
        mut config: HostConfig<P>,
        actions: &'a crate::surface_protocol::UserActionReceiver<P>,
        events: Sender<P>,
        startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
        executor: std::rc::Rc<super::session_executor::SessionExecutor>,
    ) -> CliResult<Self> {
        let storage_interrupt = actions.storage_interrupt_probe();
        let shutdown = crate::agent_tree_tools::HostShutdown::new();
        actions.on_shutdown({
            let shutdown = shutdown.clone();
            move || shutdown.signal()
        });
        config.state_roots = config.state_roots.clone().with_storage_shutdown(shutdown);
        config.store = config
            .store
            .clone()
            .with_storage_interrupt(storage_interrupt.clone());
        let pending_usage = std::sync::Arc::new(std::sync::Mutex::new(
            super::super::projection::presentation::ProcessUsage::default(),
        ));
        pending_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target = config
            .session
            .plakous_locator
            .map(plakous::sessions::SessionLocator::session_id);
        config.store =
            std::sync::Arc::new(super::super::projection::presentation::PresentingStore {
                inner: config.store.clone(),
                usage: pending_usage.clone(),
            });
        config.session.queue_projection_debt = false;
        let attach_started = Instant::now();
        config.runtime_factory.clone().configure(&mut config);
        let log = SessionLog::open(&config.home, &config.session.id).ok();
        let mut stderr_buf = Vec::new();
        config.state_roots = config
            .state_roots
            .clone()
            .with_session_tasks(executor.tasks().clone());

        // compact, rebind and fork observe Interrupt/Quit between
        // their provider round-trips. Ordinary actions drained while reaching the
        // urgent lane are deferred back to this loop in exact arrival order, so
        // looking for an interrupt never consumes product work.
        let interrupt_state = std::rc::Rc::new(RefCell::new(InterruptWatchState::default()));

        let session_open_started = Instant::now();
        // The open below blocks this thread for seconds on a cold session. Tell
        // the TUI it started so the startup row can tick its own elapsed seconds
        // instead of showing an unqualified "starting…" (spec 02 §5.4).
        let _ = events.send(ContentEvent::SessionOpening);
        // Creation freezes the root's tools; a wildcard MCP tool left out then
        // is a pending notice the operator must see (spec 07 §3.5).
        let freezing_tools = config.session.frozen_tools.is_none();
        // Where the scan for a root turn a lost host left starts, read before
        // anything is projected (spec 03 §14.1).
        let loss_scan = (config.session.store_revision != 0)
            .then(|| {
                crate::runtime::root_loss::loss_scan(config.store.as_ref(), &config.session)
                    .ok()
                    .flatten()
            })
            .flatten();
        // A switch successor published before a crash takes its epoch from
        // the recorded bytes before its tools compile.
        super::super::rebind::settle_before_open(&config).await?;
        let mut opened = build_session_from_config(&mut config, &mut stderr_buf).await?;
        // Observe owner faults before attachment hooks can retire children;
        // the face's first snapshot may be much later.
        let last_tree_revision =
            TreeSnapshotPush::registered(&config.topology_rows(), &config.session);
        if let Some(agent) = opened.agent() {
            agent.set_storage_interrupt(storage_interrupt);
        }
        pending_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target = Some(opened.locator().session_id());
        let root_published = false;
        let restored_working = opened.has_recovered_turn();
        // A reopen that had to close an unfinished turn. Read before `opened`
        // is consumed; reported near `session.resumed`.
        let abandoned_turn = opened
            .agent()
            .and_then(plakous::agent::Agent::abandoned_turn)
            .cloned();
        let runtime_session = opened.locator().session_id();
        // Read before `opened` is consumed below; reported near `session.resumed`.
        let output_bound_rederived = opened.output_bound_rederived;
        config.runtime_factory.attached(&config, &mut opened);
        let session_open_ms = elapsed_ms_since(session_open_started);

        if let Err(error) = super::super::result_mail::settle_waits(
            &config.topology_rows(),
            &config.session,
            &opened,
        )
        .await
        {
            let failure = CliFailure::from(error);
            let release = opened.release().await;
            return match release {
                Ok(_) => Err(failure),
                Err(error) => Err(CliFailure::new(format!(
                    "{}; canonical session release also failed: {error}",
                    failure.message()
                ))),
            };
        }
        if config.session.store_revision != 0 {
            let intents = match config.store.load_intents(&config.session.id) {
                Ok(intents) => intents,
                Err(error) => {
                    return Err(release_after_open_failure(opened, CliFailure::new(error)).await);
                }
            };
            if intents
                .submissions
                .iter()
                .any(|submission| submission.kind == SubmissionKind::HostWake)
            {
                let projected = super::super::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    opened.locator(),
                )
                .await;
                match projected {
                    Ok(views) => {
                        // A pending root continuation after a loss is consent
                        // the operator already gave: it is resubmitted, not
                        // discarded. One whose continuation ended goes.
                        let pending_loss = match crate::runtime::root_loss::root_loss_state(
                            config.store.as_ref(),
                            &config.session,
                        ) {
                            Ok(Some(crate::runtime::root_loss::RootLossState::Pending(
                                command,
                            ))) => Some(command),
                            _ => None,
                        };
                        for submission in &intents.submissions {
                            if submission.kind != SubmissionKind::HostWake
                                || pending_loss.as_deref() == Some(submission.command_id.as_str())
                                || intents.submission_consumed(
                                    &submission.command_id,
                                    views.state.retained_from,
                                )
                                || matches!(
                                    views.state.deliveries.get(&submission.command_id),
                                    Some(sessions::projector::Delivery::Accepted)
                                )
                            {
                                continue;
                            }
                            if let Err(error) =
                                config
                                    .store
                                    .write_host_controls(&config.session.id, true, None)
                            {
                                return Err(release_after_open_failure(
                                    opened,
                                    CliFailure::new(error),
                                )
                                .await);
                            }
                            config.session.child_wake_suppressed = true;
                            config.session.pending_queue.retain(|item| {
                                item.id != submission.queue_item_id
                                    || item.revision != submission.queue_item_revision
                            });
                            if let Err(error) = config.store.discard_submission(
                                &mut config.session,
                                &submission.command_id,
                                None,
                            ) {
                                return Err(release_after_open_failure(
                                    opened,
                                    CliFailure::new(error),
                                )
                                .await);
                            }
                        }
                    }
                    Err(error) => log_ev(
                        log.as_ref(),
                        &config,
                        SessionEvent::new("projector.deferred").str("error", error.to_string()),
                    ),
                }
            }
        }
        // A fresh creation has no stored construction binding yet.
        if config.session.plakous_locator.is_none() {
            config.session.plakous_locator = Some(opened.locator());
        }
        let publishing_resume = config.resume_publication.is_some();
        if publishing_resume {
            config.session.touch();
        }
        if config.session.store_revision != 0 && config.store.save(&mut config.session).is_err() {
            let release = opened.release().await;
            if let Err(error) = release {
                return Err(CliFailure::new(format!(
                    "product binding persistence and canonical session release failed: {error}"
                )));
            }
            return Err(CliFailure::new(
                "canonical session was created but its product binding could not be persisted",
            ));
        }

        // Spec 02 §4.2 step 2: apply what the open committed. A refused pass
        // is projection debt, never a fence: the live tick applies it.
        if config.session.store_revision != 0 {
            let followed = opened.locator();
            let projected = async {
                let views = super::super::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    followed,
                )
                .await?;
                let withdrawn_forks = super::super::fork::withdrawable_fork_targets(
                    &*config.store,
                    &config.session.id,
                )?;
                let withdrawn = config
                    .store
                    .withdraw_unadmitted_workflows(&mut config.session)?;
                for row in withdrawn {
                    let _ = events.send(ContentEvent::Row(row));
                }
                // This head holds its lease, so an epoch still pending is a
                // switch from it that never published.
                super::super::rebind::withdraw_after_open(&config).await?;
                let rows = config.topology_rows();
                for target in &withdrawn_forks {
                    super::super::fork::discard_withdrawn_fork_tree(&rows, &*config.store, target);
                }
                super::super::projection::root_tree_pass(
                    opened.sessions(),
                    &rows,
                    &super::durable_tree_id(&rows, &config.session),
                    followed.session_id(),
                    super::super::projection::RootTreePass::Settled,
                )
                .await?;
                let store = config.store.clone();
                let id = config.session.id.clone();
                let intents = tokio::task::spawn_blocking(move || store.load_intents(&id))
                    .await
                    .map_err(|_| "root batch join could not read submissions".to_owned())??;
                let pending = pending_command_ids(&views, &intents)
                    .into_iter()
                    .filter(|id| {
                        intents
                            .submission(id)
                            .is_some_and(|submission| submission.kind != SubmissionKind::HostWake)
                    })
                    .collect::<Vec<_>>();
                super::super::result_mail::RootBatchJoin::new(&rows)
                    .join_pending_root_commands(&config.session.id, &pending)
                    .await
            }
            .await;
            if let Err(error) = projected {
                log_ev(
                    log.as_ref(),
                    &config,
                    SessionEvent::new("projector.deferred").str("error", &error),
                );
            }
        }
        // A root turn a lost host left, which this open or an earlier one
        // closed, is the root's owner-lost interrupt; its held continuation
        // is derived from it. Recording it once its terminal is applied keeps
        // the lost turn's own submission older than the loss.
        if let Some(scan) = loss_scan.filter(|scan| scan.session() == runtime_session) {
            let recorded =
                match crate::runtime::root_loss::lost_root_turn(opened.sessions(), scan).await {
                    Ok(Some(lost)) => crate::runtime::root_loss::record_root_owner_lost(
                        config.store.as_ref(),
                        &config.session,
                        &lost,
                    )
                    .map(drop),
                    Ok(None) => Ok(()),
                    Err(error) => Err(error),
                };
            if let Err(error) = recorded {
                log_ev(
                    log.as_ref(),
                    &config,
                    SessionEvent::new("session.root_loss_unrecorded").str("error", &error),
                );
            }
        }
        // Refresh the joined queue after catch-up, before announcing restored work.
        if config.session.store_revision != 0
            && let Ok(stored) = config.store.load(&config.session.id)
        {
            let unsaved = config.session.transcript.pending_notice_items();
            // Unsaved notices are deduplicated against the committed ones; an
            // unreadable intent set keeps the shown projection until the next
            // refresh rather than showing a named notice twice.
            let intents = if unsaved.is_empty() {
                Ok(sessions::projector::ProductIntents::default())
            } else {
                config.store.load_intents(&config.session.id)
            };
            match intents {
                Ok(intents) => {
                    config.session.transcript = stored.transcript;
                    config
                        .session
                        .transcript
                        .restore_pending_notices(unsaved, &intents);
                }
                Err(error) => log_ev(
                    log.as_ref(),
                    &config,
                    SessionEvent::new("transcript.reload_deferred").str("error", &error),
                ),
            }
            config.session.root_turn_count = stored.root_turn_count;
            config.session.tool_call_count = stored.tool_call_count;
            config.session.compaction_count = stored.compaction_count;
            if let Some(views) = config.store.load_views(&config.session.id).ok().flatten() {
                config.session.plakous_locator = Some(super::super::projection::projected_head(
                    opened.locator(),
                    &views,
                ));
                if let Ok(intents) = config.store.load_intents(&config.session.id) {
                    config.session.child_wake_suppressed = views.wake_suppressed(&intents);
                    if views.pause_cause(&intents).is_some() {
                        config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
                    }
                }
            }
            config.session.pending_queue = stored.pending_queue;
            config.session.queue_commands = stored.queue_commands;
        }

        // A reopened session keeps the output budget it was created with,
        // because that figure is identity-bearing: it reaches the configuration
        // identity and from there the persisted harness plan, tool projection
        // target, request prefix and admission evidence, none of which can be
        // re-signed. So a session on a route that has since grown is starved,
        // and saying so is the only way the operator learns there is a larger
        // budget and that a new session is how to get it. Once per open, and
        // only when the route has actually moved.
        if let Some(rederived) = output_bound_rederived {
            // A session created with no per-request bound at all has no figure,
            // which is not the figure zero. The note already renders it as
            // "none"; the record omits the key rather than inventing a number a
            // reader would take for a budget.
            let mut record = SessionEvent::new("session.output_bound_rederived");
            if let Some(previous) = rederived.previous {
                record = record.u64("previous", u64::from(previous.get()));
            }
            if let Some(current) = rederived.current {
                record = record.u64("current", u64::from(current.get()));
            }
            log_ev(log.as_ref(), &config, record);
            let _ = events.notice(TranscriptRow::warning(
                NoticeScope::Settings,
                super::super::events::output_bound_rederived_note(
                    rederived.previous,
                    rederived.current,
                ),
            ));
        }

        // A session whose turn was left unfinished is only back because this
        // reopen abandoned what that turn left outstanding. The operator has to
        // be told, once, the one thing that is true of it. A fenced turn a
        // previous process closed but did not project is reported by the
        // projector that applied the closure here (spec 02 §4.2).
        let projected = super::take_projected_abandonments(&pending_usage, runtime_session);
        let abandoned_turn = abandoned_turn.or_else(|| {
            projected
                .into_iter()
                .next()
                .map(|abandonment| abandonment.report)
        });
        if let Some(report) = &abandoned_turn {
            super::recover::log_abandoned_report(log.as_ref(), &config, report);
            if !root_published {
                let _ = events.send(ContentEvent::Row(TranscriptRow::info(
                    NoticeScope::Recovery,
                    super::super::events::abandoned_turn_operator_line(report),
                )));
            }
            let _ = events.send(ContentEvent::Recovered {
                claim: Some(report.claim().as_str().to_owned()),
                cause: Some(report.cause().as_str().to_owned()),
                operations: report.operations().to_vec(),
            });
        }

        if let Some(publication) = config.resume_publication.take() {
            log_ev(
                log.as_ref(),
                &config,
                SessionEvent::new("session.resumed")
                    .str("id", &config.session.id)
                    .opt_str("account", config.session.account.as_deref())
                    .opt_str("model", config.session.model.as_deref())
                    .str("workspace", config.cwd.display().to_string()),
            );
            publication.store(true, Ordering::Release);
        }

        log_host_attached(log.as_ref(), &config, attach_started);
        log_session_ready(log.as_ref(), &config, attach_started);
        log_host_ready(
            log.as_ref(),
            &config,
            session_open_ms,
            elapsed_ms_since(attach_started),
        );
        let _ = events.send(ContentEvent::SessionOpened {
            session_open_ms: Some(session_open_ms),
        });
        // Spec 8.7: template readiness is host-computed at session open.
        surface::publish_template_readiness(
            config.surface.as_deref(),
            &config.ready_catalog.accounts,
            &events,
        );
        if let Some(session_id) =
            crate::product_observation::session_id_from_text(&config.session.id)
        {
            let elapsed = Duration::from_millis(session_open_ms);
            let observation = if publishing_resume {
                crate::product_observation::ProductObservation::SessionResumed {
                    at: crate::product_observation::wall_timestamp(),
                    session_id,
                    elapsed: attach_started.elapsed(),
                }
            } else {
                crate::product_observation::ProductObservation::SessionOpened {
                    at: crate::product_observation::wall_timestamp(),
                    session_id,
                    elapsed,
                }
            };
            config.state_roots.emit_product(observation);
        }
        if freezing_tools {
            announce_omitted_tools(&events, log.as_ref(), &config);
        }
        emit_agent_plan_notes(&events, &config, &stderr_buf);
        send_binding(&events, &config);
        let _ = events.replace_transcript(super::super::logging::project_session_snapshot(
            &config.session,
        ));
        emit_prompt_history(&events, &config.session);
        super::super::info::send_last_user_prompt_at(&events, &config, &opened);
        emit_queue_snapshot(&events, &config.session);
        if let Err(error) = report_context(
            &opened,
            &events,
            "on open",
            config
                .target
                .native_provider()
                .is_some_and(native_compact_facet),
            config.session.context_counters(),
        )
        .await
        {
            emit_unavailable_context(&events);
            let _ = events.send(ContentEvent::Row(
                TranscriptRow::error(NoticeScope::Recovery, error.message().to_owned())
                    .with_optional_remedy(error.remedy()),
            ));
        }
        emit_queue_pause_note(&events, &config.session);
        let pause = queue_pause(&config.session);
        if restored_working {
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::Running,
                queue_len: config.session.pending_queue.len(),
                detail: Some(HostStatusDetail::RestoringAdmittedTurn { pause }),
            });
        } else {
            let _ = events.send(ContentEvent::Status {
                phase: HostPhase::Idle,
                queue_len: config.session.pending_queue.len(),
                detail: Some(HostStatusDetail::Ready {
                    model_id: config.model_id.clone(),
                    pause,
                }),
            });
        }
        let _ = events.send(ContentEvent::SetBusy(restored_working));
        let _ = events.send(ContentEvent::AgentAttached(true));
        let _ = events.send(ContentEvent::Activity {
            label: if restored_working {
                "agent restoring"
            } else {
                "agent ready"
            }
            .into(),
            active: false,
        });
        if let Some(startup) = startup {
            let _ = startup.send(Ok(()));
        }

        // Disk-loaded queues are deliberately paused until the operator resumes them.
        let pending_start: Option<SessionQueueItem> = None;
        let last_tree_observed_at = Instant::now();
        let recovery_required = query_context(&opened, ContextQuery::Current)
            .await
            .is_ok_and(|projection| {
                projection.disposition() == ContextDisposition::RecoveryRequired
            });
        let held_usage = HeldUsage::default();
        let session_ledger = SessionLedger::default();
        let subscription_usage_refresh = SubscriptionUsageRefresh::new(
            &events,
            config.state_roots.openrouter_credentials(),
            held_usage.clone(),
            session_ledger.clone(),
        );
        if config.session.store_revision != 0 {
            persist(&*config.store, &mut config.session);
        }

        pending_usage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .target = Some(opened.locator().session_id());
        let storage_notices = opened.agent().cloned().map(|agent| {
            super::super::storage::StorageNotices::start(
                executor.tasks(),
                agent,
                config.store.clone(),
                events.clone(),
                config.state_roots.clone(),
            )
        });
        let mut host = Self::new(
            crate::node::RolePolicy::ROOT,
            opened,
            super::RootRole {
                _storage_notices: storage_notices,
                config,
                actions,
                events,
                executor,
                log,
                stderr_buf,
                interrupt_state,
                pending_start,
                pending_resume: None,
                transient_resumes_used: 0,
                pending_storage_admission: None,
                echoed_prompt: None,
                last_tree_revision,
                job_events: super::super::job_events::JobEvents::default(),
                last_tree_observed_at,
                child_wake: super::wake::RootWake::default(),
                storage_interrupted: false,
                pending_track: super::pending::PendingTrack::default(),
                recovery: if recovery_required {
                    super::RootRecovery::Due
                } else {
                    super::RootRecovery::Clear
                },
                recovered_at_open: abandoned_turn.is_some(),
                held_usage,
                session_ledger,
                pending_usage,
                subscription_usage_refresh,
                readiness: super::super::shell::ReadinessRefresh::new(),
                live: None,
                projection_finished: false,
                shown_skill_notices: crate::skills::ShownSkillNotices::default(),
                prep_hooks: super::prompt::preparation::Hooks::new(),
                idle_entry_pass_due: false,
                root_loss_resume_due: false,
            },
        );
        host.publish_pending_continuation();
        host.publish_projected_usage();
        host.arm_root_loss_resume();
        Ok(host)
    }
}

pub(super) async fn release_after_open_failure(
    opened: crate::agent::AttachedCodingSession,
    failure: CliFailure,
) -> CliFailure {
    match opened.release().await {
        Ok(_) => failure,
        Err(error) => {
            failure.with_cleanup_failure(format!("canonical session release also failed: {error}"))
        }
    }
}

/// Sends each MCP tool the creation bind left out as a live row (the snapshot
/// that follows already carries it for a transcript view; `plak run` and JSONL
/// read only rows) and records it in the session log.
fn announce_omitted_tools<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    log: Option<&SessionLog>,
    config: &HostConfig<P>,
) {
    for row in config.session.transcript.pending_notices() {
        let TranscriptRow::Notice(notice) = row else {
            continue;
        };
        // `plakous-mcp` listing writes `mcp tool <server>/<tool>: ...; omitted`.
        if !(notice.text.starts_with("mcp tool ") && notice.text.ends_with("; omitted")) {
            continue;
        }
        log_ev(
            log,
            config,
            SessionEvent::new("tool.omitted").str("text", &notice.text),
        );
        let _ = events.send(ContentEvent::Row(row.clone()));
    }
}

#[cfg(test)]
mod tests;
