//! Canonical rebind dispatch, successor projection, open and activation.

use super::{
    AgentSessionsErrorKind, ApplicationExecutor, AttachedCodingSession, CliFailure, CliResult,
    HostBoundary, HostConfig, HostLifecyclePhase, HostTargetAuthority, HostWorkflow, Instant,
    InterruptWatch, NoticeScope, RebindBefore, RebindRoute, RebindSessionOutcome,
    ResolvedRebindTarget, Sender, SessionOperationId, SessionRecord, SessionRuntimeBinding,
    TranscriptRow, abandon_prepared_rebind, durable_tree_id, emit_queue_snapshot,
    interrupted_before_dispatch, observe_boundary, observe_phase, rebind_note,
    release_source_attachment, report_rebind_context, restore_detached_source, run_watched,
    send_binding, send_last_user_prompt_at, sessions, topology_state,
};

use crate::binding::TargetSelection;

#[allow(
    clippy::too_many_arguments,
    reason = "durable source/target adoption requires complete host authority"
)]
pub(in crate::runtime) fn complete_rebind<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    operation: &str,
    source_locator: plakous::sessions::SessionLocator,
    target: ResolvedRebindTarget,
    runtime: SessionRuntimeBinding,
    started: Instant,
) -> CliResult<()> {
    crate::runtime::jobs_transition_guard(config, opened)?;
    let source_was_detached = opened.agent().is_none();
    // Captured before the target overwrites them, so an effort-only fork is
    // not reported as a model switch.
    let previous_account = config.session.account.clone();
    let previous_model = config.model_id.clone();
    let previous_effort = config.effort;
    let mut transition = RebindTransition {
        executor,
        watch,
        config,
        opened,
        stderr_buf,
        events,
        operation,
        source_locator,
        started,
    };
    let target_locator = transition.dispatch(source_was_detached, &target, runtime)?;
    let target_record = transition.project_target(target_locator)?;
    let (target_record, replacement, target_model) =
        transition.open_target(target_record, &target)?;
    transition.activate_target(target_record, replacement, target_model, target)?;
    // A profile fork onto the same root changes only the cards; the product
    // reports that itself, and "effort changed" would be false.
    let same_root = previous_account == config.session.account
        && previous_model == config.model_id
        && previous_effort == config.effort;
    if !same_root {
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Settings,
            rebind_note(
                RebindBefore {
                    account: previous_account.as_deref(),
                    model: &previous_model,
                    effort: previous_effort,
                },
                RebindBefore {
                    account: config.session.account.as_deref(),
                    model: &config.model_id,
                    effort: config.effort,
                },
            ),
        ));
    }
    Ok(())
}

struct RebindTransition<'a, 'interrupt, P: plakous_host_protocol::ProtocolExtension> {
    executor: &'a ApplicationExecutor,
    watch: InterruptWatch<'interrupt, P>,
    config: &'a mut HostConfig<P>,
    opened: &'a mut AttachedCodingSession,
    stderr_buf: &'a mut Vec<u8>,
    events: &'a Sender<P>,
    operation: &'a str,
    source_locator: plakous::sessions::SessionLocator,
    started: Instant,
}

impl<P: plakous_host_protocol::ProtocolExtension> RebindTransition<'_, '_, P> {
    fn dispatch(
        &mut self,
        source_was_detached: bool,
        target: &ResolvedRebindTarget,
        runtime: SessionRuntimeBinding,
    ) -> CliResult<plakous::sessions::SessionLocator> {
        let executor = self.executor;
        let watch = self.watch;
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let stderr_buf = &mut *self.stderr_buf;
        let events = self.events;
        let operation = self.operation;
        let source_locator = self.source_locator;
        let started = self.started;

        let Ok(operation_id) = SessionOperationId::new(operation) else {
            if source_was_detached {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            }
            return Err(CliFailure::new("rebind operation identity is invalid"));
        };
        let released_source = executor
            .block_on(opened.detach())
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(|error| {
                CliFailure::new(format!(
                    "rebind source release failed ({:?}): {error}",
                    error.kind()
                ))
            })?;
        if released_source != source_locator {
            restore_detached_source(executor, config, opened, released_source, stderr_buf)?;
            return Err(CliFailure::new(
                "rebind source advanced while entering its safe boundary; retry from the current head",
            ));
        }
        // an interrupt observed before the canonical transition rolls
        // the prepared workflow back and reattaches the exact source. No branch is
        // created and no receipt is claimed.
        if watch.observe_now() {
            let abandoned = abandon_prepared_rebind(executor, config, operation);
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            abandoned?;
            return Err(interrupted_before_dispatch("rebind"));
        }
        let rebind_result = match run_watched(
            executor,
            watch,
            opened.rebind_from(
                operation_id,
                source_locator,
                target.binding.clone(),
                runtime,
            ),
        ) {
            Ok(Ok(outcome)) => Ok(outcome),
            Ok(Err(error))
                if matches!(
                    error.kind(),
                    AgentSessionsErrorKind::InvalidInput
                        | AgentSessionsErrorKind::IdentityConflict
                        | AgentSessionsErrorKind::Revalidation
                ) =>
            {
                let workflow_finish = abandon_prepared_rebind(executor, config, operation);
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                workflow_finish?;
                return Err(CliFailure::new(format!("canonical rebind failed: {error}")));
            }
            Ok(Err(error)) => Err(CliFailure::new(format!("canonical rebind failed: {error}"))),
            Err(_) => Err(CliFailure::new("application execution failed")),
        };
        let outcome = match rebind_result {
            Ok(outcome) => outcome,
            Err(error) => {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                return Err(error);
            }
        };
        observe_phase(
            events,
            HostLifecyclePhase::RebindCanonicalTransition,
            started,
            config.session.pending_queue.len(),
        );
        observe_boundary(
            config.runtime_factory.as_ref(),
            HostWorkflow::Rebind,
            HostBoundary::RuntimeReturned,
        );
        let target_locator = match outcome {
            RebindSessionOutcome::Rebound {
                source,
                target: locator,
                binding,
                ..
            } if source == source_locator
                && binding.protocol() == target.binding.protocol()
                && binding.route() == target.binding.route()
                && binding.model() == target.binding.model()
                && binding.selected_provider_configuration()
                    == target.binding.selected_provider_configuration()
                && binding.effective_provider_configuration()
                    == target.binding.effective_provider_configuration() =>
            {
                locator
            }
            RebindSessionOutcome::RecoveryRequired { recovery, .. }
                if recovery.locator() == source_locator =>
            {
                let recovery_save = abandon_prepared_rebind(executor, config, operation);
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                recovery_save?;
                return Err(CliFailure::new(
                    "the session has an interrupted turn, so the model change was not started; the \
                     next prompt (or /recover) recovers it, then the change can be retried",
                ));
            }
            _ => {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                return Err(CliFailure::new("canonical rebind receipt is inconsistent"));
            }
        };
        Ok(target_locator)
    }

    fn project_target(
        &mut self,
        target_locator: plakous::sessions::SessionLocator,
    ) -> CliResult<SessionRecord> {
        let executor = self.executor;
        let watch = self.watch;
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let stderr_buf = &mut *self.stderr_buf;
        let operation = self.operation;
        let source_locator = self.source_locator;

        let target_record_result = (|| -> CliResult<SessionRecord> {
            // Publish the successor and settle the canonical receipt before an
            // interrupt can stop catch-up on the saved, resumable target.
            run_watched(
                executor,
                watch,
                crate::runtime::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    source_locator,
                ),
            )
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
            let request = config
                .store
                .load_intents(&config.session.id)
                .map_err(CliFailure::new)?
                .workflow_request(operation)
                .cloned()
                .ok_or_else(|| CliFailure::new("rebind request is missing"))?;
            run_watched(
                executor,
                watch,
                crate::runtime::projection::project_to_tail_interruptible(
                    opened.sessions(),
                    &config.store,
                    &request.successor_product_id(),
                    target_locator,
                    || watch.observe_now(),
                    |_, _| {},
                ),
            )
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
            let record = config
                .store
                .load(&request.successor_product_id())
                .map_err(CliFailure::new)?;
            if record.plakous_locator != Some(target_locator) {
                return Err(CliFailure::new(
                    "rebind successor differs from its projected branch fact",
                ));
            }
            Ok(record)
        })();
        let target_record = match target_record_result {
            Ok(record) => record,
            Err(error) => {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                let target_id = config
                    .store
                    .load_intents(&config.session.id)
                    .map_err(CliFailure::new)?
                    .workflow_request(operation)
                    .map(sessions::projector::WorkflowRequest::successor_product_id)
                    .ok_or_else(|| CliFailure::new("rebind request is missing"))?;
                if config.store.load(&target_id).is_err() {
                    return Err(error);
                }
                if watch.interrupted() {
                    return Err(CliFailure::new(format!(
                        "fork stopped; saved as session {target_id}; this session stays attached; /resume {target_id} opens it"
                    )));
                }
                return Err(CliFailure::new(format!(
                    "the fork was saved as session {target_id} but could not project ({}); this session stays attached, and /resume {target_id} opens the fork",
                    error.message(),
                )));
            }
        };
        observe_boundary(
            config.runtime_factory.as_ref(),
            HostWorkflow::Rebind,
            HostBoundary::ProductPublished,
        );
        Ok(target_record)
    }

    fn open_target(
        &mut self,
        target_record: SessionRecord,
        target: &ResolvedRebindTarget,
    ) -> CliResult<(SessionRecord, AttachedCodingSession, String)> {
        let executor = self.executor;
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let stderr_buf = &mut *self.stderr_buf;
        let events = self.events;
        let source_locator = self.source_locator;
        let started = self.started;

        let Some(resume_locator) = target_record.plakous_locator else {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            return Err(CliFailure::new("rebind target product binding is missing"));
        };
        let target_model = target_record
            .model
            .clone()
            .ok_or_else(|| CliFailure::new("rebind target product model is missing"));
        let target_model = match target_model {
            Ok(model) => model,
            Err(error) => {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                return Err(error);
            }
        };
        stderr_buf.clear();
        let spec = match &target.route {
            RebindRoute::Native { spec, .. } => spec.as_deref(),
            RebindRoute::Api(_) => None,
        };
        let replacement_result = match executor.block_on(config.runtime_factory.open_rebind_target(
            config,
            &target_record,
            spec,
            resume_locator,
            stderr_buf,
        )) {
            Ok(result) => result.map_err(|error| error.failure),
            Err(_) => Err(CliFailure::new("application execution failed")),
        };
        let replacement = match replacement_result {
            Ok(replacement) => replacement,
            Err(error) => {
                restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
                return Err(error);
            }
        };
        observe_phase(
            events,
            HostLifecyclePhase::RebindOpenTarget,
            started,
            config.session.pending_queue.len(),
        );
        Ok((target_record, replacement, target_model))
    }

    fn activate_target(
        &mut self,
        target_record: SessionRecord,
        replacement: AttachedCodingSession,
        target_model: String,
        target: ResolvedRebindTarget,
    ) -> CliResult<()> {
        let executor = self.executor;
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let events = self.events;
        let started = self.started;

        let previous = std::mem::replace(opened, replacement);
        let source = std::mem::replace(&mut config.session, target_record);
        config.target = match (target.route, &config.target) {
            (RebindRoute::Api(record), _) => {
                let subscriptions = config
                    .target
                    .native_subscriptions()
                    .cloned()
                    .ok_or_else(|| CliFailure::new("native subscription authority is missing"))?;
                match record.target_selection {
                    TargetSelection::Platform { provider } => HostTargetAuthority::Platform {
                        provider,
                        subscriptions,
                    },
                    TargetSelection::OpenRouter { .. } => HostTargetAuthority::OpenRouter {
                        subscriptions,
                        evidence: None,
                    },
                    TargetSelection::Native | TargetSelection::NativePriority => {
                        return Err(CliFailure::new("API rebind published a non-API target"));
                    }
                }
            }
            (
                RebindRoute::Native {
                    provider, resolved, ..
                },
                HostTargetAuthority::Native { subscriptions, .. }
                | HostTargetAuthority::OpenRouter { subscriptions, .. }
                | HostTargetAuthority::Platform { subscriptions, .. },
            ) => HostTargetAuthority::Native {
                provider,
                resolved: resolved
                    .ok_or_else(|| CliFailure::new("resolved rebind target is missing"))?,
                subscriptions: subscriptions.clone(),
                catalog_candidates: match &config.target {
                    HostTargetAuthority::Native {
                        catalog_candidates, ..
                    } => std::sync::Arc::clone(catalog_candidates),
                    HostTargetAuthority::OpenRouter { .. }
                    | HostTargetAuthority::Platform { .. } => std::sync::Arc::from([]),

                    HostTargetAuthority::Registered { .. } => std::sync::Arc::from([]),
                },
            },

            (
                RebindRoute::Native { provider, .. },
                HostTargetAuthority::Registered { token, .. },
            ) => HostTargetAuthority::Registered {
                provider,
                token: token.clone(),
            },
        };
        config.model_id = target_model;
        config.effort = target.effort;
        config.effort_explicit = target.effort_explicit;
        observe_phase(
            events,
            HostLifecyclePhase::RebindActivate,
            started,
            config.session.pending_queue.len(),
        );
        config.source_releases.track(release_source_attachment(
            previous,
            source.id,
            events.pinned_to(&config.session.id),
            "rebind",
            config.runtime_factory.clone(),
        ));
        emit_queue_snapshot(events, &config.session);
        let _ = events.replace_transcript(crate::runtime::logging::project_session_snapshot(
            &config.session,
        ));
        send_binding(events, config);
        send_last_user_prompt_at(events, opened);
        report_rebind_context(executor, opened, events, config);
        install_rebind_epoch(config)?;
        Ok(())
    }
}

fn install_rebind_epoch<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<()> {
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    // A profile fork's session froze its roster at its own genesis; only a
    // branch on the source's tree installs a successor epoch there.
    if let (Some(account), Some(model)) = (
        config.session.account.as_deref(),
        config.session.model.as_deref(),
    ) && tree_id != config.session.id
    {
        let snapshot = crate::orchestration::OrchestrationSnapshot::freeze(
            config.state_roots.roster_source().roster(
                account,
                model,
                config.session.effort.as_deref(),
            )?,
        );
        topology_state::install_successor_epoch(
            &config.topology_rows(),
            snapshot,
            &tree_id,
            &|observation| config.state_roots.emit_product(observation),
        )
        .map_err(CliFailure::new)?;
        if let (Some(session_id), Some(tree_id_text)) = (
            crate::product_observation::session_id_from_text(&tree_id),
            crate::product_observation::BoundedText64::new(&tree_id),
        ) {
            config.state_roots.emit_product(
                crate::product_observation::ProductObservation::TreeRebound {
                    at: crate::product_observation::wall_timestamp(),
                    session_id,
                    tree_id: tree_id_text,
                },
            );
        }
    }
    Ok(())
}
