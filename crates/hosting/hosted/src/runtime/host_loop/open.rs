//! Open and reconcile one canonical attachment before accepting work.

use super::{
    ApplicationExecutor, CliFailure, CliResult, ContextDisposition, ContextQuery, Duration,
    HeldUsage, HostConfig, HostEvent, HostLoop, Instant, InterruptWatchState, Ordering, RefCell,
    Sender, SessionEvent, SessionLedger, SessionLog, SessionQueueDrainState, SessionQueueItem,
    SubscriptionUsageRefresh, TreeSnapshotPush, build_session_from_config, elapsed_ms_since,
    emit_agent_plan_notes, emit_prompt_history, emit_queue_snapshot, emit_restored_paused_note,
    emit_unavailable_context, log_ev, log_host_attached, log_host_ready, log_session_ready,
    native_compact_facet, persist, publish_recovered_turn_result, query_context, report_context,
    restored_paused_status_suffix, send_binding, sessions, surface,
};
use crate::sessions::projector::{ProductTreeJoin as _, SubmissionKind, pending_command_ids};
use crate::sessions::{NoticeScope, TranscriptRow};

impl<'a, P: plakous_host_protocol::ProtocolExtension> HostLoop<'a, P> {
    pub(super) fn open(
        mut config: HostConfig<P>,
        actions: &'a crate::surface_protocol::UserActionReceiver<P>,
        events: Sender<P>,
        mut startup: Option<std::sync::mpsc::SyncSender<Result<(), String>>>,
    ) -> CliResult<Self> {
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
        #[cfg(any(test, feature = "test-support"))]
        let injected_child_runtime = super::super::startup::install_injected_child_runtime(&config);
        let log = SessionLog::open(&config.home, &config.session.id).ok();
        let mut stderr_buf = Vec::new();
        let executor = ApplicationExecutor::new()
            .map_err(|_| CliFailure::new("application executor construction failed"))?;

        // compact, rebind and fork observe Interrupt/Quit between
        // their provider round-trips. Ordinary actions drained while reaching the
        // urgent lane are deferred back to this loop in exact arrival order, so
        // looking for an interrupt never consumes product work.
        let interrupt_state = RefCell::new(InterruptWatchState::default());

        let session_open_started = Instant::now();
        // The open below blocks this thread for seconds on a cold session. Tell
        // the TUI it started so the startup row can tick its own elapsed seconds
        // instead of showing an unqualified "starting…" (spec 02 §5.4).
        let _ = events.send(HostEvent::SessionOpening);
        let mut opened = executor
            .block_on(build_session_from_config(&mut config, &mut stderr_buf))
            .map_err(|_| CliFailure::new("application execution failed"))??;
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
        // Read before `opened` is consumed below; reported near `session.resumed`.
        let output_bound_rederived = opened.output_bound_rederived;
        #[cfg(any(test, feature = "test-support"))]
        if config.test_injection.is_some() {
            opened.claim_injected_topology_owner(&super::durable_tree_id(
                &config.topology_rows(),
                &config.session,
            ));
        }
        let session_open_ms = elapsed_ms_since(session_open_started);

        if let Err(error) = executor
            .block_on(super::super::result_mail::settle_waits(
                &config.topology_rows(),
                &config.session,
                &opened,
            ))
            .unwrap_or_else(|_| Err("application execution failed".to_owned()))
        {
            let failure = CliFailure::new(error);
            let release = executor
                .block_on(opened.release())
                .map_err(|_| CliFailure::new("application execution failed"))?;
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
                    return Err(release_after_open_failure(
                        &executor,
                        opened,
                        CliFailure::new(error),
                    ));
                }
            };
            if intents
                .submissions
                .iter()
                .any(|submission| submission.kind == SubmissionKind::HostWake)
            {
                let projected = match executor.block_on(super::super::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    opened.locator(),
                )) {
                    Ok(projected) => projected,
                    Err(_) => {
                        return Err(release_after_open_failure(
                            &executor,
                            opened,
                            CliFailure::new("application execution failed"),
                        ));
                    }
                };
                match projected {
                    Ok(views) => {
                        for submission in &intents.submissions {
                            if submission.kind != SubmissionKind::HostWake
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
                                    &executor,
                                    opened,
                                    CliFailure::new(error),
                                ));
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
                                    &executor,
                                    opened,
                                    CliFailure::new(error),
                                ));
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
            let release = executor
                .block_on(opened.release())
                .map_err(|_| CliFailure::new("application execution failed"))?;
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
            let projected = executor.block_on(async {
                let views = super::super::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    followed,
                )
                .await?;
                let withdrawn = config
                    .store
                    .withdraw_unadmitted_workflows(&mut config.session)?;
                for row in withdrawn {
                    let _ = events.inner.send(HostEvent::Row(row));
                }
                let rows = config.topology_rows();
                super::super::projection::project_tree(
                    opened.sessions(),
                    &rows,
                    &super::durable_tree_id(&rows, &config.session),
                    followed.session_id(),
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
            });
            if let Ok(Err(error)) = projected {
                log_ev(
                    log.as_ref(),
                    &config,
                    SessionEvent::new("projector.deferred").str("error", &error),
                );
            }
        }
        // Refresh the joined queue after catch-up, before announcing restored work.
        if config.session.store_revision != 0
            && let Ok(stored) = config.store.load(&config.session.id)
        {
            let unsaved = config.session.transcript.pending_notices().to_vec();
            config.session.transcript = stored.transcript;
            for notice in unsaved {
                config.session.record_notice(notice);
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
        // be told, once, the one thing that is true of it.
        if let Some(report) = &abandoned_turn {
            super::recover::log_abandoned_report(log.as_ref(), &config, report);
            if !root_published {
                let _ = events.send(HostEvent::Row(TranscriptRow::info(
                    NoticeScope::Recovery,
                    super::super::events::abandoned_turn_operator_line(report),
                )));
            }
            let _ = events.send(HostEvent::Recovered {
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
        let _ = events.send(HostEvent::SessionOpened {
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
        emit_agent_plan_notes(&events, &config, &stderr_buf);
        send_binding(&events, &config);
        let _ = events.send(super::super::logging::project_session_snapshot(
            &config.session,
        ));
        emit_prompt_history(&events, &config.session);
        super::super::info::send_last_user_prompt_at(&events, &opened);
        emit_queue_snapshot(&events, &config.session);
        let mut restored_recovery_blocked = false;
        if opened.has_recovered_turn() {
            let _ = events.send(HostEvent::SetBusy(true));
            let _ = events.send(HostEvent::Status {
                phase: "running".into(),
                queue_len: config.session.pending_queue.len(),
                detail: Some("restoring the original admitted turn".into()),
            });
            if let Some(startup) = startup.take() {
                let _ = startup.send(Ok(()));
            }
        }
        if let Some(turn) = opened.take_recovered_turn() {
            let result = executor
                .block_on(turn.result().result())
                .map_err(|_| CliFailure::new("application execution failed"))?;
            // The rejoined turn kept its receipts for replay; its terminal
            // settles them as any root terminal does.
            let exchange = result
                .as_ref()
                .map_or(crate::agent::TerminalExchange::Unresolved, |result| {
                    crate::agent::TerminalExchange::of(result)
                });
            if let Err(error) = executor
                .block_on(super::super::result_mail::settle_terminal_waits(
                    &config.topology_rows(),
                    &config.session,
                    &opened,
                    exchange,
                ))
                .unwrap_or_else(|_| Err("application execution failed".to_owned()))
            {
                let _ = events.notice(TranscriptRow::error(
                    NoticeScope::Tree,
                    format!("wait receipt settlement failed: {error}"),
                ));
            }
            restored_recovery_blocked = publish_recovered_turn_result(
                result,
                &*config.store,
                &config.home,
                &mut config.session,
                &events,
            );
            {
                let rows = config.topology_rows();
                executor
                    .block_on(super::super::projection::project_tree(
                        opened.sessions(),
                        &rows,
                        &super::durable_tree_id(&rows, &config.session),
                        opened.locator().session_id(),
                    ))
                    .map_err(|_| CliFailure::new("application execution failed"))?
                    .map_err(CliFailure::new)?;
            }
            if restored_recovery_blocked {
                config.session.queue_drain_state = SessionQueueDrainState::RestoredPaused;
            }
            events.resnapshot_transcript_if_required(&config.session);
        }
        if let Err(error) = report_context(
            &executor,
            &opened,
            &events,
            config
                .target
                .native_provider()
                .is_some_and(native_compact_facet),
            config.session.context_counters(),
        ) {
            emit_unavailable_context(&events);
            let _ = events.send(HostEvent::Row(TranscriptRow::error(
                NoticeScope::Recovery,
                error.message().to_owned(),
            )));
        }
        emit_restored_paused_note(
            &events,
            &config.topology_rows(),
            &config.session,
            config.interactive,
        );
        let paused_suffix = restored_paused_status_suffix(&config.topology_rows(), &config.session);
        let with_pause = |detail: String| match &paused_suffix {
            Some(suffix) => format!("{detail} · {suffix}"),
            None => detail,
        };
        if restored_working {
            let _ = events.send(HostEvent::Status {
                phase: if restored_recovery_blocked {
                    "error"
                } else {
                    "idle"
                }
                .into(),
                queue_len: config.session.pending_queue.len(),
                detail: Some(with_pause(
                    if restored_recovery_blocked {
                        "restored turn's outcome is uncertain; the next prompt recovers it"
                    } else {
                        "restored turn complete"
                    }
                    .to_owned(),
                )),
            });
        } else {
            let _ = events.send(HostEvent::Status {
                phase: "idle".into(),
                queue_len: config.session.pending_queue.len(),
                detail: Some(with_pause(format!("ready · {}", config.model_id))),
            });
        }
        let _ = events.send(HostEvent::SetBusy(false));
        let _ = events.send(HostEvent::AgentAttached(true));
        let _ = events.send(HostEvent::Activity {
            label: if restored_recovery_blocked {
                "agent recovery blocked"
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
        let last_tree_revision = TreeSnapshotPush::default();
        let last_tree_observed_at = Instant::now();
        let recovery_blocked = restored_recovery_blocked
            || query_context(&executor, &opened, ContextQuery::Current).is_ok_and(|projection| {
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
        let mut host = Self {
            config,
            actions,
            events,
            opened,
            quit_started_at: None,
            quit_abandoned_turn: false,
            quit_job_stops: None,
            executor,
            log,
            stderr_buf,
            interrupt_state,
            pending_start,
            pending_resume: None,
            transient_resumes_used: 0,
            last_tree_revision,
            job_events: super::super::job_events::JobEvents::default(),
            last_tree_observed_at,
            child_wake: super::wake::ChildWake::default(),
            pending_track: super::pending::PendingTrack::default(),
            recovery_blocked,
            recovered_at_open: abandoned_turn.is_some(),
            held_usage,
            session_ledger,
            pending_usage,
            subscription_usage_refresh,
            live: None,
            projection_finished: false,
            #[cfg(any(test, feature = "test-support"))]
            _injected_child_runtime: injected_child_runtime,
        };
        host.publish_pending_continuation();
        host.publish_projected_usage();
        host.spawn_tree_recovery();
        Ok(host)
    }
}

pub(super) fn release_after_open_failure(
    executor: &ApplicationExecutor,
    opened: crate::agent::AttachedCodingSession,
    failure: CliFailure,
) -> CliFailure {
    match executor.block_on(opened.release()) {
        Ok(Ok(_)) => failure,
        Ok(Err(error)) => {
            failure.with_cleanup_failure(format!("canonical session release also failed: {error}"))
        }
        Err(error) => failure.with_cleanup_failure(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use plakous::agent::CommandId;

    fn unpublished_fixture(
        root: &std::path::Path,
    ) -> (
        std::sync::Arc<dyn crate::session_store::SessionRecordStore>,
        String,
        plakous::sessions::SessionLocator,
    ) {
        unpublished_control_fixture(root, None)
    }

    #[derive(Clone, Copy)]
    enum RootControlCut {
        Steer,
        Cancel,
        BusyQuit,
        ApprovalAllowed,
        ApprovalDenied,
        Shutdown,
    }

    fn control_cut_host(
        root: &std::path::Path,
        cut: RootControlCut,
    ) -> HostConfig<crate::runtime::test_kit::TestExtension> {
        use crate::runtime::test_support::fresh_injected_attached_host;
        use plakous_offline_crash_harness::{BoundaryController, protocol::CrashBoundary};
        let (mut config, injection) = fresh_injected_attached_host(root);
        drop(injection);
        let approval = matches!(
            cut,
            RootControlCut::ApprovalAllowed | RootControlCut::ApprovalDenied
        );
        let controller = BoundaryController::quiet_gated_from_evidence_path(
            if approval {
                CrashBoundary::ProviderDispatchTwo
            } else {
                CrashBoundary::ProviderDispatchOne
            },
            root.join("control-cut.jsonl"),
            root.join("control-release"),
        )
        .unwrap();
        let workspace = config.cwd.clone();
        std::sync::Arc::get_mut(config.test_injection.as_mut().unwrap())
            .unwrap()
            .runtime = std::sync::Arc::new(move |_| {
            if approval {
                let script = serde_json::from_value(serde_json::json!({
                        "version": 1, "tool_policy": "decision_required",
                        "steps": [
                            {"output": {"tool": {"name": "loopback_effect", "arguments": {}}}, "finish": "tool_calls"},
                            {"output": {"text": "approved terminal"}, "finish": "stop"}
                        ]
                    })).unwrap();
                plakous_offline_crash_harness::scripted_runtime_binding(
                    &workspace,
                    controller.clone(),
                    std::sync::Arc::new(script),
                )
                .map_err(|error| error.to_string())
            } else {
                plakous_offline_crash_harness::runtime_binding(&workspace, controller.clone())
                    .map_err(|error| error.to_string())
            }
        });
        config
    }

    fn unpublished_control_fixture(
        root: &std::path::Path,
        cut: Option<RootControlCut>,
    ) -> (
        std::sync::Arc<dyn crate::session_store::SessionRecordStore>,
        String,
        plakous::sessions::SessionLocator,
    ) {
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;
        let config = cut.map_or_else(
            || fresh_injected_attached_host(root).0,
            |cut| control_cut_host(root, cut),
        );
        let evidence = root.join("control-cut.jsonl");
        let gate = root.join("control-release");
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let item =
            sessions::queue::enqueue(&mut host.config.session.pending_queue, "unpublished".into())
                .unwrap();
        store.save(&mut host.config.session).unwrap();
        let command = format!("plak.queue.{}.{}", item.id, item.revision);
        let intent = sessions::projector::Submission {
            command_id: command.clone(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        };
        store
            .save_submission(&mut host.config.session, &intent)
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
        let handle = submission.into_turn().unwrap();
        if let Some(cut) = cut {
            host.executor
                .block_on(async {
                    use plakous_offline_crash_harness::protocol::CrashBoundary;
                    let approval = matches!(
                        cut,
                        RootControlCut::ApprovalAllowed | RootControlCut::ApprovalDenied
                    );
                    if approval {
                        let decision = tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                let view = host
                                    .opened
                                    .agent()
                                    .unwrap()
                                    .attach(plakous::agent::ViewRequest::metadata())
                                    .await
                                    .unwrap();
                                if let Some(decision) =
                                    view.snapshot().pending_effect_decisions().first()
                                {
                                    break decision.decision().clone();
                                }
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        if matches!(cut, RootControlCut::ApprovalAllowed) {
                            handle
                                .control()
                                .allow_effect(
                                    decision,
                                    plakous::tools::EffectDecisionAuditId::new("cut-approval")
                                        .unwrap(),
                                )
                                .await
                                .unwrap();
                        } else {
                            handle
                                .control()
                                .deny_tool(
                                    decision,
                                    plakous::tools::ToolPolicyDenial::new("cut-denial"),
                                )
                                .await
                                .unwrap();
                        }
                    }
                    let boundary = if approval {
                        CrashBoundary::ProviderDispatchTwo
                    } else {
                        CrashBoundary::ProviderDispatchOne
                    };
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while !plakous_offline_crash_harness::evidence::read_evidence(&evidence)
                            .unwrap()
                            .iter()
                            .any(|row| row.boundary == boundary)
                        {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    match cut {
                        RootControlCut::Steer
                        | RootControlCut::ApprovalAllowed
                        | RootControlCut::ApprovalDenied => {
                            if matches!(cut, RootControlCut::Steer) {
                                handle
                                    .control()
                                    .steer("direct steering without a delivery intent")
                                    .await
                                    .unwrap();
                            }
                            std::fs::write(&gate, b"release").unwrap();
                        }
                        RootControlCut::Cancel | RootControlCut::BusyQuit => {
                            handle.control().cancel().await.unwrap();
                        }
                        RootControlCut::Shutdown => {
                            host.opened
                                .agent()
                                .unwrap()
                                .shutdown_with_id(CommandId::new("cut-shutdown").unwrap())
                                .await
                                .unwrap();
                        }
                    }
                })
                .unwrap();
        }
        host.executor.block_on(handle.result()).unwrap().unwrap();
        let head = host
            .executor
            .block_on(host.opened.release())
            .unwrap()
            .unwrap();
        (store, id, head)
    }

    #[test]
    fn direct_steer_cut_projects_the_terminal_once() {
        control_cut_publication(RootControlCut::Steer);
    }

    #[test]
    fn cancel_cut_projects_the_terminal_once() {
        control_cut_publication(RootControlCut::Cancel);
    }

    #[test]
    fn shutdown_cut_projects_the_terminal_once() {
        control_cut_publication(RootControlCut::Shutdown);
    }

    #[test]
    fn busy_quit_cancelled_terminal_cut_publishes_once() {
        control_cut_publication(RootControlCut::BusyQuit);
    }

    #[test]
    fn approval_cuts_project_the_terminal_once() {
        control_cut_publication(RootControlCut::ApprovalAllowed);
        control_cut_publication(RootControlCut::ApprovalDenied);
    }

    fn control_cut_publication(cut: RootControlCut) {
        use crate::runtime::test_kit::TestExtension;
        let root = tempfile::tempdir().unwrap();
        let (store, id, head) = unpublished_control_fixture(root.path(), Some(cut));
        let mut config = control_cut_host(root.path(), cut);
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        let published = store.load(&id).unwrap();
        assert_eq!(published.plakous_locator, Some(head));
        assert_eq!(
            published.pending_queue,
            [] as [sessions::queue::QueueItem; 0]
        );
        let rows = published.transcript.len();
        reopened.release().unwrap();
        let mut config = control_cut_host(root.path(), cut);
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &rx, Sender::over(tx), None).unwrap();
        assert_eq!(
            store.load(&id).unwrap().transcript.len(),
            rows,
            "no second terminal"
        );
        reopened.release().unwrap();
    }

    #[test]
    fn terminal_projection_survives_attachment_save_failure_and_repeated_open() {
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::{SaveFailure, fresh_injected_attached_host};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let root = tempfile::tempdir().unwrap();
        let (store, id, head) = unpublished_fixture(root.path());
        let fail = Arc::new(crate::runtime::fault_store::FaultStore::new(
            store.clone(),
            SaveFailure {
                fail: AtomicBool::new(true),
            },
        ));
        let (_, rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        for failing in [true, false, false] {
            let (mut config, _) = fresh_injected_attached_host(root.path());
            config.session = store.load(&id).unwrap();
            config.store = fail.clone();
            fail.faults.fail.store(failing, Ordering::SeqCst);
            let (tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let opened = HostLoop::open(config, &rx, Sender::over(tx), None);
            if failing {
                assert!(opened.is_err());
            } else {
                let opened = opened.unwrap();
                assert_eq!(opened.config.session.plakous_locator, Some(head));
                opened.release().unwrap();
            }
            let projected = store.load(&id).unwrap();
            assert_eq!(projected.root_turn_count, 1);
            assert_eq!(
                projected
                    .transcript
                    .iter()
                    .filter(|row| row.text() == Some("loopback turn committed"))
                    .count(),
                1,
            );
        }
        assert_eq!(
            store
                .load(&id)
                .unwrap()
                .transcript
                .iter()
                .filter(|row| row.text() == Some("loopback turn committed"))
                .count(),
            1
        );
    }

    #[test]
    fn unrecorded_wake_submission_reopens_without_a_legacy_delivery_receipt() {
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;

        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let item = sessions::queue::enqueue(
            &mut host.config.session.pending_queue,
            super::super::wake::WAKE_PROMPT.into(),
        )
        .unwrap();
        let submission = sessions::projector::Submission {
            command_id: "wake-without-legacy-receipt".into(),
            kind: SubmissionKind::HostWake,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        };
        store
            .save_submission(&mut host.config.session, &submission)
            .unwrap();
        assert_eq!(
            store
                .load_intents(&host.config.session.id)
                .unwrap()
                .submissions,
            vec![submission.clone()]
        );
        host.release().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (events, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let reopened = HostLoop::open(config, &actions, Sender::over(events), None).unwrap();
        assert_eq!(
            reopened.config.session.pending_queue,
            [] as [sessions::queue::QueueItem; 0]
        );
        assert_eq!(
            store.load_intents(&id).unwrap().submissions,
            [] as [sessions::projector::Submission; 0]
        );
        assert_eq!(reopened.config.session.root_turn_count, 0);
        reopened.release().unwrap();
    }

    #[test]
    fn pending_commands_reopen_paused_and_resubmit_their_stored_kind_bytes_and_target() {
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;
        use crate::sessions::projector::{Delivery, Submission};
        for steer in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (config, _) = fresh_injected_attached_host(root.path());
            let (actions, action_rx) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut host =
                HostLoop::open(config, &action_rx, Sender::over(event_tx), None).unwrap();
            let store = host.config.store.clone();
            let id = host.config.session.id.clone();
            let target = host
                .executor
                .block_on(async {
                    let turn = host
                        .opened
                        .agent()
                        .unwrap()
                        .prompt("advance head")
                        .await
                        .unwrap();
                    let id = turn.id();
                    turn.result().await.unwrap();
                    id
                })
                .unwrap();
            let item = sessions::queue::enqueue(
                &mut host.config.session.pending_queue,
                "operator text".into(),
            )
            .unwrap();
            let intent = Submission {
                command_id: "custom-stored-command".into(),
                kind: if steer {
                    SubmissionKind::SendNowSteer
                } else {
                    SubmissionKind::RootStart
                },
                queue_item_id: item.id.clone(),
                queue_item_revision: item.revision,
                target_turn_id: steer.then(|| target.to_string()),
                canonical_command: "exact stored bytes\nincluding immutable context".into(),
                submitted_at_ms: item.submitted_at_ms,
            };
            store
                .save_submission(&mut host.config.session, &intent)
                .unwrap();
            host.release().unwrap();
            let (mut config, _) = fresh_injected_attached_host(root.path());
            config.store = store.clone();
            config.session = store.load(&id).unwrap();
            let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
            let mut reopened =
                HostLoop::open(config, &action_rx, Sender::over(event_tx), None).unwrap();
            assert_eq!(reopened.config.session.pending_queue, vec![item.clone()]);
            assert_eq!(
                reopened.config.session.queue_drain_state,
                SessionQueueDrainState::RestoredPaused
            );
            assert!(reopened.config.session.queue_commands.is_locked(&item.id));
            assert!(reopened.pending_start.is_none());
            assert_eq!(
                store.load_intents(&id).unwrap().submissions,
                vec![intent.clone()]
            );
            let count = reopened.config.session.root_turn_count;
            actions
                .send(crate::surface_protocol::UserAction::SendQueuedNow {
                    id: item.id.clone(),
                    revision: item.revision,
                })
                .unwrap();
            let super::super::HostStep::Prompt {
                text,
                queued_delivery,
                user_echoed,
            } = reopened.next_prompt(None).unwrap()
            else {
                panic!("explicit resubmission")
            };
            reopened
                .run_prompt(text, queued_delivery, user_echoed, None)
                .unwrap();
            let views = store.load_views(&id).unwrap().unwrap();
            if steer {
                assert!(matches!(
                    views.state.deliveries.get(&intent.command_id),
                    Some(Delivery::Rejected { .. })
                ));
                assert_eq!(reopened.config.session.root_turn_count, count);
                assert_eq!(reopened.config.session.pending_queue, vec![item]);
            } else {
                assert_eq!(
                    views.state.deliveries.get(&intent.command_id),
                    Some(&Delivery::Accepted)
                );
                assert_eq!(
                    reopened.config.session.pending_queue,
                    [] as [sessions::queue::QueueItem; 0]
                );
                let duplicate = reopened
                    .executor
                    .block_on(reopened.opened.agent().unwrap().command(
                        plakous::agent::TurnCommand::Start {
                            command_id: CommandId::new(&intent.command_id).unwrap(),
                            input: intent.canonical_command.clone().into(),
                        },
                    ))
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    duplicate.acknowledgement().disposition(),
                    plakous::agent::CommandAckDisposition::Duplicate
                );
                assert!(
                    duplicate.acknowledgement().is_accepted(),
                    "the runtime accepted the exact stored bytes, not the editable queue text"
                );
                assert_eq!(reopened.config.session.root_turn_count, count + 1);
            }
            assert_eq!(store.load_intents(&id).unwrap().submissions, vec![intent]);
            reopened.release().unwrap();
        }
    }

    #[test]
    fn submission_anchored_below_retention_is_consumed_silently_without_counting_a_turn() {
        use crate::runtime::test_kit::TestExtension;
        use crate::runtime::test_support::fresh_injected_attached_host;
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let (actions, action_rx) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let (event_tx, _) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut host = HostLoop::open(config, &action_rx, Sender::over(event_tx), None).unwrap();
        let store = host.config.store.clone();
        let id = host.config.session.id.clone();
        let item =
            sessions::queue::enqueue(&mut host.config.session.pending_queue, "already ran".into())
                .unwrap();
        let intent = sessions::projector::Submission {
            command_id: "below-retention-root".into(),
            kind: SubmissionKind::RootStart,
            queue_item_id: item.id.clone(),
            queue_item_revision: item.revision,
            target_turn_id: None,
            canonical_command: item.text.clone(),
            submitted_at_ms: item.submitted_at_ms,
        };
        store
            .save_submission(&mut host.config.session, &intent)
            .unwrap();
        drop(host.live.take());
        host.executor
            .block_on(async {
                host.opened
                    .agent()
                    .unwrap()
                    .start(
                        CommandId::new(&intent.command_id).unwrap(),
                        &intent.canonical_command,
                    )
                    .await
                    .unwrap()
                    .result()
                    .unwrap()
                    .clone()
                    .result()
                    .await
                    .unwrap();
            })
            .unwrap();
        // Release only the runtime lease: simulates losing the host before its projector pass.
        host.executor
            .block_on(host.opened.release_opened())
            .unwrap()
            .unwrap();
        let connection =
            rusqlite::Connection::open(root.path().join("home/runtime.sqlite3")).unwrap();
        connection.execute_batch("DELETE FROM session_feed WHERE seq < (SELECT feed_tail FROM sessions); UPDATE sessions SET feed_retained_from=feed_tail, feed_bytes=(SELECT COALESCE(sum(length(record)),0) FROM session_feed);").unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.store = store.clone();
        config.session = store.load(&id).unwrap();
        let (event_tx, event_rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let mut reopened =
            HostLoop::open(config, &action_rx, Sender::over(event_tx), None).unwrap();
        assert_eq!(reopened.config.session.root_turn_count, 0);
        actions
            .send(crate::surface_protocol::UserAction::ResumeQueueDrain)
            .unwrap();
        assert!(matches!(
            reopened.next_prompt(None).unwrap(),
            super::super::HostStep::Continue
        ));
        assert_eq!(reopened.config.session.root_turn_count, 0);
        assert_eq!(
            reopened.config.session.pending_queue,
            [] as [sessions::queue::QueueItem; 0]
        );
        let intents = store.load_intents(&id).unwrap();
        assert_eq!(intents.submissions, vec![intent.clone()]);
        let views = store.load_views(&id).unwrap().unwrap();
        assert!(intents.submission_consumed(&intent.command_id, views.state.retained_from));
        assert_eq!(
            views.state.deliveries.get(&intent.command_id),
            Some(&sessions::projector::Delivery::Consumed)
        );
        assert_eq!(event_rx.try_iter().filter(|event| matches!(event, HostEvent::Row(row) if row.text() == Some("queued prompt already ran; its result is no longer kept"))).count(), 0);
        assert_eq!(
            store
                .load(&id)
                .unwrap()
                .transcript
                .iter()
                .filter(|row| row.text()
                    == Some("queued prompt already ran; its result is no longer kept"))
                .count(),
            0
        );
        reopened.release().unwrap();
    }
}
