//! Canonical rebind dispatch, successor projection, open and activation.

use super::{
    AgentSessionsErrorKind, AttachedCodingSession, CliFailure, CliResult, HostBoundary, HostConfig,
    HostLifecyclePhase, HostTargetAuthority, HostWorkflow, Instant, InterruptWatch, NoticeScope,
    RebindBefore, RebindRoute, RebindSessionOutcome, ResolvedRebindTarget, Sender,
    SessionOperationId, SessionRecord, SessionRuntimeBinding, SourceRelease, Succession,
    TranscriptRow, abandon_switch, durable_tree_id, emit_queue_snapshot,
    interrupted_before_dispatch, observe_boundary, observe_phase, rebind_note,
    release_source_attachment, report_rebind_context, restore_detached_source, send_binding,
    send_last_user_prompt_at, sessions, topology_state, watched,
};

use crate::binding::TargetSelection;

#[allow(
    clippy::too_many_arguments,
    reason = "durable source/target adoption requires complete host authority"
)]
pub(in crate::runtime) async fn complete_rebind<P: plakous_host_protocol::ProtocolExtension>(
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
    succession: Option<&Succession>,
) -> CliResult<()> {
    if succession.is_none() {
        crate::runtime::jobs_transition_guard(config, opened)?;
    }
    let source_was_detached = opened.agent().is_none();
    // Captured before the target overwrites them, so an effort-only fork is
    // not reported as a model switch.
    let previous_account = config.session.account.clone();
    let previous_model = config.model_id.clone();
    let previous_effort = config.effort;
    let mut transition = RebindTransition {
        watch,
        config,
        opened,
        stderr_buf,
        events,
        operation,
        source_locator,
        started,
        succession,
    };
    let mut target = target;
    let target_locator = transition
        .dispatch(source_was_detached, &target, runtime)
        .await?;
    let target_record = transition.project_target(target_locator).await?;
    if let Some(succession) = succession {
        transition
            .settle_succession(succession, &target_record, &mut target)
            .await?;
    }
    let (target_record, replacement, target_model) =
        transition.open_target(target_record, &target).await?;
    transition
        .activate_target(target_record, replacement, target_model, target)
        .await?;
    if let Some(succession) = succession {
        let _ = events.notice(TranscriptRow::info(
            NoticeScope::Settings,
            switched_note(opened, succession),
        ));
        return Ok(());
    }
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
    watch: InterruptWatch<'interrupt, P>,
    config: &'a mut HostConfig<P>,
    opened: &'a mut AttachedCodingSession,
    stderr_buf: &'a mut Vec<u8>,
    events: &'a Sender<P>,
    operation: &'a str,
    source_locator: plakous::sessions::SessionLocator,
    started: Instant,
    succession: Option<&'a Succession>,
}

impl<P: plakous_host_protocol::ProtocolExtension> RebindTransition<'_, '_, P> {
    #[allow(
        clippy::too_many_lines,
        reason = "Canonical rebind dispatch distinguishes published, unchanged, refused, and uncertain outcomes while preserving source restoration."
    )]
    async fn dispatch(
        &mut self,
        source_was_detached: bool,
        target: &ResolvedRebindTarget,
        runtime: SessionRuntimeBinding,
    ) -> CliResult<plakous::sessions::SessionLocator> {
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
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            }
            return Err(CliFailure::new("rebind operation identity is invalid"));
        };
        let succession = self.succession;
        let released = if succession.is_some() {
            opened.detach_for_transfer().await
        } else {
            opened.detach().await
        };
        let released_source = released.map_err(|error| {
            CliFailure::new(format!(
                "rebind source release failed ({:?}): {error}",
                error.kind()
            ))
        })?;
        if released_source != source_locator {
            restore_detached_source(config, opened, released_source, stderr_buf).await?;
            return Err(CliFailure::new(
                "rebind source advanced while entering its safe boundary; retry from the current head",
            ));
        }
        // an interrupt observed before the canonical transition rolls
        // the prepared workflow back and reattaches the exact source. No branch is
        // created and no receipt is claimed.
        if watch.observe_now() {
            let abandoned = abandon_switch(config, operation, succession).await;
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            abandoned?;
            return Err(interrupted_before_dispatch("rebind"));
        }
        let rebind_result = match watched(
            watch,
            Box::pin(opened.rebind_from(
                operation_id,
                source_locator,
                target.binding.clone(),
                runtime,
                succession.is_some(),
            )),
        )
        .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error)
                if matches!(
                    error.kind(),
                    AgentSessionsErrorKind::InvalidInput
                        | AgentSessionsErrorKind::IdentityConflict
                        | AgentSessionsErrorKind::Revalidation
                ) =>
            {
                let workflow_finish = abandon_switch(config, operation, succession).await;
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
                workflow_finish?;
                if let Some(refusal) = rebind_refusal(error.kind(), &error.to_string()) {
                    return Err(CliFailure::new(refusal).with_remedy(
                        plakous_host_protocol::notice_remedy::NoticeRemedy::StartFresh {},
                    ));
                }
                return Err(CliFailure::new(format!("canonical rebind failed: {error}")));
            }
            Err(error) => Err(CliFailure::new(format!("canonical rebind failed: {error}"))),
        };
        let outcome = match rebind_result {
            Ok(outcome) => outcome,
            Err(error) => {
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
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
                let recovery_save = abandon_switch(config, operation, succession).await;
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
                recovery_save?;
                return Err(CliFailure::new(
                    "the session has an interrupted turn, so the model change was not started; recover the session, then retry the change",
                ).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::RecoverSession {}));
            }
            _ => {
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
                return Err(CliFailure::new("canonical rebind receipt is inconsistent"));
            }
        };
        Ok(target_locator)
    }

    /// The successor's product record, projected to the published branch.
    async fn project_successor(
        &mut self,
        target_locator: plakous::sessions::SessionLocator,
    ) -> CliResult<SessionRecord> {
        let watch = self.watch;
        let config = &*self.config;
        let opened = &*self.opened;
        let operation = self.operation;
        let source_locator = self.source_locator;
        async {
            // Publish the successor and settle the canonical receipt before an
            // interrupt can stop catch-up on the saved, resumable target.
            watched(
                watch,
                crate::runtime::projection::project_to_tail(
                    opened.sessions(),
                    &config.store,
                    &config.session.id,
                    source_locator,
                ),
            )
            .await
            .map_err(CliFailure::new)?;
            let request = config
                .store
                .load_intents(&config.session.id)
                .map_err(CliFailure::new)?
                .workflow_request(operation)
                .cloned()
                .ok_or_else(|| CliFailure::new("rebind request is missing"))?;
            watched(
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
            .await
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
        }
        .await
    }

    async fn project_target(
        &mut self,
        target_locator: plakous::sessions::SessionLocator,
    ) -> CliResult<SessionRecord> {
        let mut target_record_result = self.project_successor(target_locator).await;
        // A switch cannot restore its source after the publication, so a
        // failed projection is tried once more in process (D-S8).
        if target_record_result.is_err() && self.succession.is_some() && !self.watch.interrupted() {
            target_record_result = self.project_successor(target_locator).await;
        }
        let watch = self.watch;
        let operation = self.operation;
        let source_locator = self.source_locator;
        let target_record = match target_record_result {
            Ok(record) => record,
            Err(error) if self.succession.is_some() => {
                // The successor is the line's head now: the source cannot
                // be restored, and the successor's next open finishes it.
                return Err(self
                    .hand_off_tree(format!(
                        "the profile switch was published but its session could not be projected ({}); open this session again to continue on the new profile",
                        error.message()
                    ))
                    .await);
            }
            Err(error) => {
                let config = &mut *self.config;
                let opened = &mut *self.opened;
                let stderr_buf = &mut *self.stderr_buf;
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
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
                        "fork stopped; saved as session {target_id}; this session stays attached; the saved session can be opened explicitly"
                    )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeSavedSession { session_id: target_id.parse().map_err(|e| CliFailure::new(format!("invalid saved session identity: {e}")))? }));
                }
                return Err(CliFailure::new(format!(
                    "the fork was saved as session {target_id} but could not project ({}); this session stays attached; the saved fork can be opened explicitly",
                    error.message(),
                )).with_remedy(plakous_host_protocol::notice_remedy::NoticeRemedy::ResumeSavedSession { session_id: target_id.parse().map_err(|e| CliFailure::new(format!("invalid saved session identity: {e}")))? }));
            }
        };
        observe_boundary(
            self.config.runtime_factory.as_ref(),
            HostWorkflow::Rebind,
            HostBoundary::ProductPublished,
        );
        Ok(target_record)
    }

    /// A published switch whose successor could not be projected or attached
    /// hands its tree to custody, as a completed switch does, so its live
    /// children keep running and the host's quit drain ends the tree; the
    /// successor's next open takes it back. Returns `message` with any
    /// hand-off failure.
    async fn hand_off_tree(&mut self, message: String) -> CliFailure {
        match self.opened.hand_off_topology().await {
            Ok(()) => CliFailure::new(message),
            Err(error) => CliFailure::new(message)
                .with_cleanup_failure(format!("the agent tree could not be handed off: {error}")),
        }
    }

    async fn open_target(
        &mut self,
        target_record: SessionRecord,
        target: &ResolvedRebindTarget,
    ) -> CliResult<(SessionRecord, AttachedCodingSession, String)> {
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let stderr_buf = &mut *self.stderr_buf;
        let events = self.events;
        let source_locator = self.source_locator;
        let started = self.started;

        let Some(resume_locator) = target_record.plakous_locator else {
            restore_detached_source(config, opened, source_locator, stderr_buf).await?;
            return Err(CliFailure::new("rebind target product binding is missing"));
        };
        let target_model = target_record
            .model
            .clone()
            .ok_or_else(|| CliFailure::new("rebind target product model is missing"));
        let target_model = match target_model {
            Ok(model) => model,
            Err(error) => {
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
                return Err(error);
            }
        };
        stderr_buf.clear();
        let spec = match &target.route {
            RebindRoute::Native { spec, .. } => spec.as_deref(),
            RebindRoute::Api(_) => None,
        };
        let mut replacement_result = match config
            .runtime_factory
            .open_rebind_target(config, &target_record, spec, resume_locator, stderr_buf)
            .await
        {
            Ok(replacement) => Ok(replacement),
            Err(error) => Err(*error.failure),
        };
        // A switch cannot restore its source after the publication, so a
        // failed attach is tried once more in process (D-S8).
        if replacement_result.is_err() && self.succession.is_some() {
            stderr_buf.clear();
            replacement_result = config
                .runtime_factory
                .open_rebind_target(config, &target_record, spec, resume_locator, stderr_buf)
                .await
                .map_err(|error| *error.failure);
        }
        let replacement = match replacement_result {
            Ok(replacement) => replacement,
            Err(error) if self.succession.is_some() => {
                // The successor is the line's head: the source cannot be
                // restored. Its next open retries the attach.
                return Err(self
                    .hand_off_tree(format!(
                        "the profile switch was published but its session could not attach ({}); open this session again to continue on the new profile",
                        error.message()
                    ))
                    .await);
            }
            Err(error) => {
                restore_detached_source(config, opened, source_locator, stderr_buf).await?;
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

    async fn activate_target(
        &mut self,
        target_record: SessionRecord,
        replacement: AttachedCodingSession,
        target_model: String,
        target: ResolvedRebindTarget,
    ) -> CliResult<()> {
        let config = &mut *self.config;
        let opened = &mut *self.opened;
        let events = self.events;
        let started = self.started;

        let previous = std::mem::replace(opened, replacement);
        crate::runtime::surface::leaving_session(
            config.surface.as_deref(),
            &config.session.id,
            &target_record.id,
        );
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
            SourceRelease::HandOff,
            config.runtime_factory.clone(),
        ));
        send_binding(events, config);
        crate::runtime::admission::emit_prompt_history(events, &config.session);
        emit_queue_snapshot(events, &config.session);
        let _ = events.replace_transcript(crate::runtime::logging::project_session_snapshot(
            &config.session,
        ));
        send_last_user_prompt_at(events, config, opened);
        report_rebind_context(opened, events, config).await;
        // A switch's tree took its epoch before the successor opened.
        if self.succession.is_none() {
            install_rebind_epoch(config).await?;
        }
        Ok(())
    }

    /// Step 5 of a switch, after the successor is published and projected:
    /// the tree takes the successor's epoch (or root-binding view) before
    /// the successor opens against it. A failure here leaves the successor
    /// published as the line's head; its next open installs the epoch.
    async fn settle_succession(
        &mut self,
        succession: &Succession,
        target_record: &SessionRecord,
        target: &mut ResolvedRebindTarget,
    ) -> CliResult<()> {
        let config = &mut *self.config;
        config.state_roots.set_pending_epoch(None);
        if let RebindRoute::Native {
            spec: Some(spec), ..
        } = &mut target.route
        {
            spec.state_roots.set_pending_epoch(None);
        }
        let settled = succession
            .settle(
                config,
                self.operation,
                target_record.account.as_deref().unwrap_or_default(),
                target_record.model.as_deref().unwrap_or_default(),
                target_record.effort.as_deref(),
            )
            .await;
        match settled {
            Ok(()) => Ok(()),
            Err(error) => Err(self
                .hand_off_tree(format!(
                    "the profile switch published session {} but its agent tree could not take the profile ({}); open it to finish",
                    target_record.id,
                    error.message()
                ))
                .await),
        }
    }
}

/// The notice a completed switch prints: what kept running across it.
fn switched_note(opened: &AttachedCodingSession, succession: &Succession) -> String {
    let agents = crate::topology_runtime::lookup(&succession.tree_id)
        .and_then(|runtime| {
            runtime
                .read(|tree| {
                    tree.nodes()
                        .values()
                        .filter(|node| {
                            node.agent_id != tree.root_agent_id && !node.status.is_finished()
                        })
                        .count()
                })
                .ok()
        })
        .unwrap_or(0);
    let processes = opened.agent().map_or(0, |agent| {
        agent
            .jobs()
            .iter()
            .filter(|job| job.status == plakous::tools::JobStatus::Running)
            .count()
    });
    let plural = |count: usize, one: &str, many: &str| {
        format!("{count} {}", if count == 1 { one } else { many })
    };
    format!(
        "switched to profile {}; {} and {} kept running",
        succession.profile,
        plural(agents, "agent", "agents"),
        plural(processes, "process", "processes")
    )
}

async fn install_rebind_epoch<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
) -> CliResult<()> {
    let tree_id = durable_tree_id(&config.topology_rows(), &config.session);
    // A profile fork's session froze its roster at its own genesis; only a
    // branch on the source's tree installs a successor epoch there, and that
    // epoch keeps the tree's roster: only the root bind moves.
    if let (Some(account), Some(model)) = (
        config.session.account.as_deref(),
        config.session.model.as_deref(),
    ) && tree_id != config.session.id
    {
        topology_state::install_root_bind_epoch(
            &config.topology_rows(),
            &tree_id,
            account,
            model,
            config.session.effort.as_deref(),
            &|observation| config.state_roots.emit_product(observation),
        )
        .await
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

/// The `/rebind` and `/agents` wording for a refusal the operator can act on.
/// A successor keeps its source's tools; when its build would change them
/// (Plakous: "rebind changed canonical tool authority") only a new session
/// can use the target's.
pub(super) fn rebind_refusal(kind: AgentSessionsErrorKind, message: &str) -> Option<&'static str> {
    (kind == AgentSessionsErrorKind::Revalidation
        && message.contains("rebind changed canonical tool authority"))
    .then_some(
        "cannot switch: this session's tools would change; start a new session on that model",
    )
}
