//! Model rebinding: immutable requests, canonical dispatch and feed projection.

use std::time::Instant;

use plakous::application::ApplicationExecutor;
use plakous::model::{ModelId, ProcessingTierDisposition, ProviderServiceTierId};
use plakous::sessions::{
    AgentSessionsErrorKind, RebindSessionOutcome, SessionOperationId, SessionRuntimeBinding,
};
use plakous::subscriptions::NativeProvider;

use crate::agent::{
    AttachedCodingSession, ProductAgentSpec, coding_session_target_binding, open_coding_session,
    selected_coding_provider_configuration_with_processing_tier,
};
use crate::binding::TargetSelection;
use crate::interrupt::{InterruptWatch, run_watched};
use crate::sessions::{self, SessionRecord};
use crate::{CliFailure, CliResult};

use super::admission::{emit_queue_snapshot, emit_unavailable_context};
use super::info::{effort_label, send_binding, send_last_user_prompt_at};
use super::models::{effort_from_config, native_compact_facet, oauth_provider_for_account};
use super::shell::{api_root_binding, profile_target_selection};
use super::source_release::release_source_attachment;
use super::startup::{
    api_runtime_for_record, bounded_transition_stage, host_runtime_binding, observe_phase,
    open_session_for_host,
};
#[cfg(any(test, feature = "test-support"))]
use super::startup::{injected_rebind_target, observe_test_boundary};
use super::usage::report_context;
use super::{
    HOST_TRANSITION_RESOLVE_TIMEOUT, HostConfig, HostLifecyclePhase, HostTargetAuthority,
    RebindBefore, RebindRoute, ResolvedRebindTarget, Sender, TestRuntimeRole, TopologyRows,
    TreeRows, topology_state,
};
#[cfg(any(test, feature = "test-support"))]
use super::{TestHostBoundary, TestHostWorkflow};
use crate::sessions::{NoticeScope, TranscriptRow};

fn report_rebind_context<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    opened: &AttachedCodingSession,
    events: &Sender<P>,
    config: &HostConfig<P>,
) {
    if let Err(error) = report_context(
        executor,
        opened,
        events,
        config
            .target
            .native_provider()
            .is_some_and(native_compact_facet),
        config.session.context_counters(),
    ) {
        emit_unavailable_context(events);
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            error.message().to_owned(),
        ));
    }
}

pub(super) fn restore_detached_source<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &HostConfig<P>,
    opened: &mut AttachedCodingSession,
    locator: plakous::sessions::SessionLocator,
    stderr_buf: &mut Vec<u8>,
) -> CliResult<()> {
    stderr_buf.clear();
    let replacement = executor
        .block_on(open_session_for_host(
            config,
            &config.session,
            locator,
            stderr_buf,
            TestRuntimeRole::Primary,
        ))
        .map_err(|_| CliFailure::new("application execution failed while restoring source"))?
        .map_err(|error| error.failure)?;
    let previous = std::mem::replace(opened, replacement);
    executor
        .block_on(previous.release())
        .map_err(|_| {
            CliFailure::new("application execution failed while releasing detached source")
        })?
        .map_err(|error| CliFailure::new(format!("detached source release failed: {error}")))?;
    Ok(())
}

/// The tree a rebind compiles against, from the one own-tree decision the
/// caller already made. A profile fork's seeded target uses that id; every
/// other rebind keeps the source's tree. This does not read the store again.
pub(super) fn decided_topology_tree_id<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    own_tree: bool,
    target_product_id: &str,
) -> String {
    if own_tree {
        target_product_id.to_owned()
    } else {
        durable_tree_id(&config.topology_rows(), &config.session)
    }
}

/// A native rebind target's runtime, and the tree id when this build took an
/// `AttachRoot` lease.
///
/// The lease id is returned even when the binding fails after the attach, so
/// the caller can drop exactly that lease. `None` means this path took none:
/// a lookup, an injected runtime, or a failure before attach. The caller
/// releases only that id with [`release_profile_build_lease`].
pub(super) fn native_rebind_runtime<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    route: &RebindRoute,
    own_tree: bool,
    stderr_buf: &mut Vec<u8>,
) -> (CliResult<SessionRuntimeBinding>, Option<String>) {
    let attach = if own_tree {
        crate::topology_runtime::TopologyHubAttach::AttachRoot
    } else {
        crate::topology_runtime::TopologyHubAttach::LookupOnly
    };
    match route {
        RebindRoute::Native {
            spec: Some(spec), ..
        } => {
            let mut attached = None;
            let binding = crate::agent::coding_session_runtime_binding_with_attach(
                (**spec).clone(),
                stderr_buf,
                attach,
                &mut attached,
            );
            (binding, attached)
        }
        RebindRoute::Native { spec: None, .. } => {
            #[cfg(any(test, feature = "test-support"))]
            if let Some(injection) = config.test_injection.as_deref() {
                injection
                    .rebind_builds
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(attach);
            }
            (
                host_runtime_binding(config, stderr_buf, TestRuntimeRole::Rebind),
                None,
            )
        }
        RebindRoute::Api(_) => (
            host_runtime_binding(config, stderr_buf, TestRuntimeRole::Rebind),
            None,
        ),
    }
}

/// Drops the lease a profile fork's runtime build took on the tree it
/// attached. Callers pass only an id this path acquired. The opened branch
/// holds its own lease; a build that took none is not released here.
pub(super) fn release_profile_build_lease<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    events: &Sender<P>,
    tree_id: &str,
) -> CliResult<()> {
    let released = executor
        .block_on(crate::topology_runtime::release_root_lease(tree_id))
        .map_err(|_| CliFailure::new("application execution failed"))?;
    if let Err(error) = released {
        let _ = events.notice(TranscriptRow::error(
            NoticeScope::Settings,
            format!("the profile tree's build lease could not be released: {error}"),
        ));
    }
    Ok(())
}

pub(super) fn resolve_rebind_target<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &HostConfig<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    plan_state_authority: &str,
    composition: crate::binding::CompositionSelection,
    target_selection: &TargetSelection,
    topology_tree_id: &str,
) -> CliResult<ResolvedRebindTarget> {
    let topology_enabled = config.live_topology_enabled()?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(binding) = injected_rebind_target(config) {
        let (parsed_effort, explicit) = effort_from_config(effort);
        if effort.is_some() && !explicit {
            return Err(CliFailure::new("injected rebind effort is invalid"));
        }
        return Ok(ResolvedRebindTarget {
            route: RebindRoute::Native {
                provider: NativeProvider::Codex,
                resolved: None,
                spec: None,
            },
            effort: explicit.then_some(parsed_effort),
            effort_explicit: explicit,
            binding,
        });
    }
    let provider = oauth_provider_for_account(account)
        .ok_or_else(|| CliFailure::new("rebind target account is not a native OAuth provider"))?;
    let (parsed_effort, explicit) = effort_from_config(effort);
    if effort.is_some() && !explicit {
        return Err(CliFailure::new("rebind target effort is invalid"));
    }
    let model_id = ModelId::new(model.to_owned())
        .map_err(|_| CliFailure::new("rebind target model identity is invalid"))?;
    let subscriptions = config
        .target
        .native_subscriptions()
        .ok_or_else(|| CliFailure::new("subscription authority is missing"))?;
    let candidate = match &config.target {
        HostTargetAuthority::Native {
            catalog_candidates, ..
        } => catalog_candidates
            .iter()
            .find(|candidate| candidate.account == account && candidate.exact_id == model),
        HostTargetAuthority::OpenRouter { .. } | HostTargetAuthority::Platform { .. } => None,
        #[cfg(any(test, feature = "test-support"))]
        HostTargetAuthority::Injected { .. } => None,
    };
    let (reasoning_efforts, service_tiers, native_tool_mode) = if let Some(candidate) = candidate {
        (
            candidate.reasoning_efforts.clone(),
            candidate
                .service_tiers
                .iter()
                .map(|tier| tier.0.clone())
                .collect::<Vec<_>>(),
            candidate.native_tool_mode,
        )
    } else {
        let catalog = run_watched(
            executor,
            watch,
            bounded_transition_stage(
                HOST_TRANSITION_RESOLVE_TIMEOUT,
                subscriptions.model_catalog(provider),
            ),
        )
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(|()| CliFailure::new("rebind catalog refresh timed out; source unchanged"))?
        .map_err(|_| CliFailure::new("rebind catalog refresh failed; source unchanged"))?;
        let catalog_entry = catalog
            .models()
            .iter()
            .find(|entry| entry.exact_id() == model)
            .ok_or_else(|| {
                CliFailure::new("rebind model is absent from the authenticated catalog")
            })?;
        (
            catalog_entry.reasoning_efforts().to_vec(),
            catalog_entry
                .service_tiers()
                .iter()
                .map(|tier| tier.id().to_owned())
                .collect(),
            catalog_entry.native_tool_mode(),
        )
    };
    if reasoning_efforts.is_empty() {
        if explicit {
            return Err(CliFailure::new(
                "rebind target does not admit a reasoning effort",
            ));
        }
    } else if !explicit {
        return Err(CliFailure::new(
            "reasoning rebind target requires an explicit effort",
        ));
    } else if !reasoning_efforts.contains(&parsed_effort) {
        return Err(CliFailure::new(
            "rebind target effort is absent from the authenticated catalog",
        ));
    }
    let target_effort = explicit.then_some(parsed_effort);
    let processing_tier = match target_selection {
        TargetSelection::Native => ProcessingTierDisposition::Standard,
        TargetSelection::NativePriority
            if provider == NativeProvider::Codex
                && service_tiers.iter().any(|tier| tier == "priority") =>
        {
            ProcessingTierDisposition::ProviderNamed(
                ProviderServiceTierId::new("priority")
                    .map_err(|_| CliFailure::new("Codex priority tier identity is invalid"))?,
            )
        }
        TargetSelection::NativePriority => {
            return Err(CliFailure::new(
                "Fast is absent from this exact authenticated Codex model row",
            ));
        }
        TargetSelection::OpenRouter { .. } | TargetSelection::Platform { .. } => {
            return Err(CliFailure::new(
                "OpenRouter target selection cannot be used for a native OAuth rebind",
            ));
        }
    };
    let selected = selected_coding_provider_configuration_with_processing_tier(
        provider,
        model_id,
        target_effort,
        native_tool_mode,
        composition,
        processing_tier,
    )?;
    let limits = crate::agent::product_subscription_request_limits()
        .map_err(|_| CliFailure::new("rebind subscription gate is invalid"))?;
    let resolved = run_watched(
        executor,
        watch,
        bounded_transition_stage(
            HOST_TRANSITION_RESOLVE_TIMEOUT,
            subscriptions.resolve_turn_scoped_provider(
                selected,
                limits,
                plakous::model::SessionLineage::Root,
            ),
        ),
    )
    .map_err(|_| CliFailure::new("application execution failed"))?
    .map_err(|()| CliFailure::new("rebind target resolution timed out; source unchanged"))?
    .map_err(|error| {
        CliFailure::new(format!("rebind target resolution failed: {}", error.kind()))
    })?;
    let resolved = std::sync::Arc::new(resolved);
    let binding = coding_session_target_binding(resolved.as_ref());
    let spec = ProductAgentSpec {
        prompt_host_facts: config.prompt_host_facts(),
        product_home: Some(config.product_home.clone()),
        tool_selection: super::startup::root_tool_selection(&config.session),
        provider,
        resolved: std::sync::Arc::clone(&resolved),
        effort: target_effort,
        effort_explicit: explicit,
        workspace_root: config.session.workspace_root.clone(),
        cwd: config.cwd.clone(),
        plan_state_authority: plan_state_authority.to_owned(),
        composition,
        topology_tree_id: Some(topology_tree_id.to_owned()),
        topology_caller_id: None,
        state_roots: config.state_roots.clone(),
        scheduled_compaction: config.compaction_enabled,
        max_model_rounds: config.max_model_rounds,
        topology_enabled,
    };
    Ok(ResolvedRebindTarget {
        route: RebindRoute::Native {
            provider,
            resolved: Some(resolved),
            spec: Some(Box::new(spec)),
        },
        effort: target_effort,
        effort_explicit: explicit,
        binding,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "durable source/target adoption requires complete host authority"
)]
pub(super) fn complete_rebind<P: plakous_host_protocol::ProtocolExtension>(
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
    super::jobs_transition_guard(config, opened)?;
    let source_was_detached = opened.agent().is_none();
    // Captured before the target overwrites them, so an effort-only fork is
    // not reported as a model switch.
    let previous_account = config.session.account.clone();
    let previous_model = config.model_id.clone();
    let previous_effort = config.effort;
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
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Rebind,
        TestHostBoundary::RuntimeReturned,
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
    let target_record_result = (|| -> CliResult<SessionRecord> {
        executor
            .block_on(super::projection::project_to_tail(
                opened.sessions(),
                &config.store,
                &config.session.id,
                source_locator,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        let request = config
            .store
            .load_intents(&config.session.id)
            .map_err(CliFailure::new)?
            .workflow_request(operation)
            .cloned()
            .ok_or_else(|| CliFailure::new("rebind request is missing"))?;
        executor
            .block_on(super::projection::project_to_tail(
                opened.sessions(),
                &config.store,
                &request.successor_product_id(),
                target_locator,
            ))
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
            return Err(error);
        }
    };
    #[cfg(any(test, feature = "test-support"))]
    observe_test_boundary(
        config.test_injection.as_deref(),
        TestHostWorkflow::Rebind,
        TestHostBoundary::ProductPublished,
    );
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
    let replacement_result = match &target.route {
        RebindRoute::Native {
            spec: Some(spec), ..
        } => {
            let target_open = open_coding_session((**spec).clone(), resume_locator, stderr_buf);
            match executor.block_on(target_open) {
                Ok(result) => result.map_err(|error| error.failure),
                Err(_) => Err(CliFailure::new("application execution failed")),
            }
        }
        route => {
            // An API target opens from its own published record; an injected
            // native target from the host's.
            let record = match route {
                RebindRoute::Api(_) => &target_record,
                RebindRoute::Native { .. } => &config.session,
            };
            match executor.block_on(open_session_for_host(
                config,
                record,
                resume_locator,
                stderr_buf,
                TestRuntimeRole::Rebind,
            )) {
                Ok(result) => result.map_err(|error| error.failure),
                Err(_) => Err(CliFailure::new("application execution failed")),
            }
        }
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
                HostTargetAuthority::OpenRouter { .. } | HostTargetAuthority::Platform { .. } => {
                    std::sync::Arc::from([])
                }
                #[cfg(any(test, feature = "test-support"))]
                HostTargetAuthority::Injected { .. } => std::sync::Arc::from([]),
            },
        },
        #[cfg(any(test, feature = "test-support"))]
        (RebindRoute::Native { provider, .. }, HostTargetAuthority::Injected { .. }) => {
            HostTargetAuthority::Injected { provider }
        }
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
        config.test_injection.clone(),
    ));
    emit_queue_snapshot(events, &config.session);
    let _ = events.send(super::logging::project_session_snapshot(&config.session));
    send_binding(events, config);
    send_last_user_prompt_at(events, opened);
    report_rebind_context(executor, opened, events, config);
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

/// The refusal of a rebind whose target route cannot serve one of the
/// session's frozen tools. A rebind keeps those tools, so canonical tool
/// authority never changes across it; this refuses before any durable work.
fn frozen_tools_rebind_refusal<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    account: &str,
    target_selection: &TargetSelection,
) -> Option<String> {
    let tools = super::startup::root_tool_selection(&config.session);
    if tools.selection.is_empty() {
        return None;
    }
    let route = match target_selection.api_target() {
        Some(target) => Some(crate::tool_plan::ToolRoute::api(target)),
        None => oauth_provider_for_account(account).and_then(crate::tool_plan::ToolRoute::native),
    };
    let Some(route) = route else {
        return Some(format!(
            "this session keeps its tools across a model change, and {account} serves none of \
             them; remedy: /clear, or start a new session on that model"
        ));
    };
    crate::tool_plan::bind_tool_plan(&tools.selection, route, account)
        .err()
        .map(|refusal| refusal.rebind_line())
}

/// The transcript line a completed rebind prints.
///
/// A profile whose root differs from the source only in effort still forks.
/// Only a genuine model or provider change is a model switch; an effort-only
/// change says so and drops the
/// "native reasoning from the previous model" caveat, which does not apply
/// when the model has not moved.
pub(super) fn rebind_note(before: RebindBefore<'_>, after: RebindBefore<'_>) -> String {
    let account = after.account.unwrap_or("-");
    if before.account == after.account && before.model == after.model {
        let effort = after
            .effort
            .map_or_else(|| "default".to_owned(), effort_label);
        return format!(
            "Effort changed \u{b7} {account} \u{b7} {} \u{b7} {effort}\nThis chat continues on the same model.",
            after.model
        );
    }
    format!(
        "Model switched \u{b7} {account} \u{b7} {}\nThis chat continues; native reasoning from the previous model is not sent.\nPrevious session remains available to resume.",
        after.model
    )
}

pub(super) fn durable_tree_id(rows: &dyn TopologyRows, session: &SessionRecord) -> String {
    topology_state::durable_tree_id(rows, session)
}

#[cfg(test)]
pub(super) fn tree_epoch_blocks_rebind(rows: &dyn TopologyRows, session: &SessionRecord) -> bool {
    tree_epoch_rebind_refusal(rows, session).is_some()
}

pub(super) fn tree_epoch_rebind_refusal(
    rows: &dyn TopologyRows,
    session: &SessionRecord,
) -> Option<&'static str> {
    topology_state::epoch_rebind_refusal(rows, &durable_tree_id(rows, session))
}

pub(super) fn session_rebind_boundary_ready(session: &SessionRecord) -> bool {
    session.pending_queue.is_empty() && !session.queue_commands.has_pending()
}

pub(super) fn interrupted_before_dispatch(operation: &str) -> CliFailure {
    CliFailure::new(format!(
        "{operation} was interrupted before its canonical dispatch; the exact source is unchanged"
    ))
}

pub(super) fn abandon_prepared_rebind<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &mut HostConfig<P>,
    operation: &str,
) -> CliResult<()> {
    let sessions = config.state_roots.runtime_sessions()?;
    let source = config
        .session
        .plakous_locator
        .ok_or_else(|| CliFailure::new("rebind source locator is missing"))?;
    executor
        .block_on(super::projection::project_to_tail(
            &sessions,
            &config.store,
            &config.session.id,
            source,
        ))
        .map_err(|_| CliFailure::new("application execution failed"))?
        .map_err(CliFailure::new)?;
    config
        .store
        .discard_unadmitted_workflow(&mut config.session, operation)
        .map_err(CliFailure::new)
}

/// How a rebind that did not fail ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Rebound {
    /// The attached session moved to a new branch or a resumed ancestor.
    Switched,
    /// The requested binding was already active; nothing changed.
    Unchanged,
}

/// Whether a profile's cards are the ones this session's tree already froze.
fn profile_cards_active<P: plakous_host_protocol::ProtocolExtension>(
    config: &HostConfig<P>,
    roster: &crate::orchestration::RosterInput,
) -> CliResult<bool> {
    if !config.live_topology_enabled()? {
        // No spawn tool is compiled, so the cards cannot differ.
        return Ok(true);
    }
    let rows = config.topology_rows();
    let tree_id = durable_tree_id(&rows, &config.session);
    let frozen = match crate::topology_runtime::lookup(&tree_id) {
        Some(runtime) => runtime
            .read(|tree| tree.snapshot.templates_digest.clone())
            .ok(),
        None => match rows.tree(&tree_id) {
            Ok(Some(tree)) => Some(tree.snapshot.templates_digest),
            // A topology-admitted fork must seed its own tree.
            Ok(None) => return Ok(false),
            Err(_) => None,
        },
    };
    Ok(frozen.is_some_and(|digest| digest == roster.templates_digest))
}

/// What a profile fork resolved before any runtime is built.
enum ProfileTarget {
    Native(Box<ResolvedRebindTarget>),
    Api(Box<SessionRecord>),
}

/// Forks the attached session onto a profile's root: the branch opens a
/// tree of its own frozen on `roster`, so the source keeps its tree and cards.
pub(super) fn handle_rebind<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    watch: InterruptWatch<'_, P>,
    config: &mut HostConfig<P>,
    opened: &mut AttachedCodingSession,
    stderr_buf: &mut Vec<u8>,
    events: &Sender<P>,
    account: &str,
    model: &str,
    effort: Option<&str>,
    extra: Option<&str>,
    composition: Option<crate::binding::CompositionSelection>,
    protocol: Option<crate::binding::ProtocolSelection>,
    roster: &crate::orchestration::RosterInput,
) -> CliResult<Rebound> {
    super::jobs_transition_guard(config, opened)?;
    watch.begin();
    let started = Instant::now();
    if let Some(reason) = tree_epoch_rebind_refusal(&config.topology_rows(), &config.session) {
        return Err(CliFailure::new(reason));
    }
    if !session_rebind_boundary_ready(&config.session) {
        return Err(CliFailure::new(
            "rebind requires an idle session with no queued or unresolved product work",
        ));
    }
    let account = account.to_owned();
    // A Code Mode (native-ptc) prefix is replayable onto every presentation:
    // each kernel re-projects `exec`/`wait` cells truthfully at dispatch
    // (Responses/Chat carry the original string input, Messages wraps it as
    // `{"input": ...}`), so the rebind branches at the canonical checkpoint
    // like any other and nothing is refused up front.
    let target_selection =
        profile_target_selection(&account, extra, protocol).map_err(CliFailure::new)?;
    let api_root = target_selection.api_target().is_some();
    let (effort_label, composition) = if api_root {
        // An API root carries no catalog row to follow; it binds as a new
        // session from the same template would.
        api_root_binding(
            &target_selection,
            model,
            effort,
            composition.unwrap_or(crate::binding::CompositionSelection::FollowCatalog),
        )
        .map_err(CliFailure::new)?
    } else {
        (
            validate_rebind_effort(&config.ready_catalog.accounts, &account, model, effort)?,
            composition.unwrap_or(config.session.composition),
        )
    };
    // refuse before the first resolution round-trip,
    // while no durable rebind work exists to roll back.
    if watch.observe_now() {
        return Err(interrupted_before_dispatch("rebind"));
    }
    let same_binding = config.session.account.as_deref() == Some(account.as_str())
        && config.session.model.as_deref() == Some(model)
        && config.session.effort.as_deref() == effort_label.as_deref()
        && config.session.composition == composition
        && config.session.target_selection == target_selection;
    if same_binding && profile_cards_active(config, roster)? {
        return Ok(Rebound::Unchanged);
    }
    if let Some(refusal) = frozen_tools_rebind_refusal(config, &account, &target_selection) {
        return Err(CliFailure::new(refusal));
    }
    let operation = format!("plak:rebind:{}", uuid::Uuid::new_v4());
    let target_product_id = sessions::projector::WorkflowRequest {
        operation_id: operation.clone(),
        source_locator: opened.locator(),
        kind: sessions::SessionWorkflowKind::Fork,
    }
    .successor_product_id();
    let own_tree = config.live_topology_enabled()?;
    if own_tree {
        crate::agent::seed_genesis_tree(
            &config.state_roots,
            &target_product_id,
            &config.session.workspace_root,
            model,
            &account,
            roster.clone(),
        )?;
    }
    let topology_tree_id = decided_topology_tree_id(config, own_tree, &target_product_id);
    let target = if api_root {
        let mut record = SessionRecord::new(config.session.workspace_root.clone());
        record.id.clone_from(&target_product_id);
        record.account = Some(account.clone());
        record.model = Some(model.to_owned());
        record.effort.clone_from(&effort_label);
        record.composition = composition;
        record.target_selection = target_selection.clone();
        record.source_product_session_id = Some(config.session.id.clone());
        record.source_workflow_operation_id = Some(operation.clone());
        record.frozen_tools.clone_from(&config.session.frozen_tools);
        record.validate_for_store().map_err(CliFailure::new)?;
        ProfileTarget::Api(Box::new(record))
    } else {
        ProfileTarget::Native(Box::new(resolve_rebind_target(
            executor,
            watch,
            config,
            &account,
            model,
            effort_label.as_deref(),
            &target_product_id,
            composition,
            &target_selection,
            &topology_tree_id,
        )?))
    };
    observe_phase(
        events,
        HostLifecyclePhase::RebindResolveTarget,
        started,
        config.session.pending_queue.len(),
    );
    let instructions_before = instruction_sources(opened);
    let mut attached_tree = None;
    let branched = (|| -> CliResult<Rebound> {
        stderr_buf.clear();
        // A profile's own tree has no hub yet: attach one so the new
        // session's tools compile against it. Only a lease this build
        // took is dropped below.
        let (runtime, mut target) = match target {
            ProfileTarget::Native(target) => {
                let (runtime, tree) =
                    native_rebind_runtime(config, &target.route, own_tree, stderr_buf);
                attached_tree = tree;
                (runtime?, *target)
            }
            ProfileTarget::Api(record) => {
                let attach = if own_tree {
                    crate::topology_runtime::TopologyHubAttach::AttachRoot
                } else {
                    crate::topology_runtime::TopologyHubAttach::LookupOnly
                };
                let mut tree = None;
                let runtime = run_watched(
                    executor,
                    watch,
                    api_runtime_for_record(
                        config,
                        &record,
                        false,
                        attach,
                        Some(&topology_tree_id),
                        &mut tree,
                    ),
                )
                .map_err(|_| CliFailure::new("application execution failed"))?;
                attached_tree = tree;
                let runtime = runtime?;
                let (effort, effort_explicit) = effort_from_config(effort_label.as_deref());
                let binding = runtime.target_binding();
                let target = ResolvedRebindTarget {
                    route: RebindRoute::Api(record),
                    effort: effort_explicit.then_some(effort),
                    effort_explicit,
                    binding,
                };
                (runtime, target)
            }
        };
        target.binding = runtime.target_binding();
        observe_phase(
            events,
            HostLifecyclePhase::RebindPrepareRuntime,
            started,
            config.session.pending_queue.len(),
        );
        // the target is resolved but nothing durable exists yet, so
        // an intent observed here refuses without creating a branch.
        if watch.observe_now() {
            return Err(interrupted_before_dispatch("rebind"));
        }
        let provider = match &target.route {
            RebindRoute::Native { provider, .. } => format!("{provider:?}").to_ascii_lowercase(),
            RebindRoute::Api(_) => account.clone(),
        };
        let workflow_kind =
            sessions::SessionWorkflowKind::Rebind(Box::new(sessions::RebindSelection {
                provider,
                account: account.clone(),
                model: model.to_owned(),
                effort: effort_label.clone(),
                composition,
                target_selection,
                protocol: target.binding.protocol().as_str().to_owned(),
                route: target.binding.route().as_str().to_owned(),
                requested_model: target.binding.model().as_str().to_owned(),
                selected_configuration: serde_json::to_string(
                    &target.binding.selected_provider_configuration(),
                )
                .map_err(|_| CliFailure::new("selected configuration could not be persisted"))?,
                effective_configuration: serde_json::to_string(
                    &target.binding.effective_provider_configuration(),
                )
                .map_err(|_| CliFailure::new("effective configuration could not be persisted"))?,
                account_binding: serde_json::to_string(&target.binding.account_binding())
                    .map_err(|_| CliFailure::new("account binding could not be persisted"))?,
            }));
        let attached_locator = opened.locator();
        if config.session.plakous_locator != Some(attached_locator) {
            return Err(CliFailure::new(
                "rebind source does not match the exact durable product binding",
            ));
        }
        if config.session.store_revision == 0 {
            config
                .store
                .save(&mut config.session)
                .map_err(CliFailure::new)?;
        }
        executor
            .block_on(super::projection::project_to_tail(
                opened.sessions(),
                &config.store,
                &config.session.id,
                attached_locator,
            ))
            .map_err(|_| CliFailure::new("application execution failed"))?
            .map_err(CliFailure::new)?;
        let intents = config
            .store
            .load_intents(&config.session.id)
            .map_err(CliFailure::new)?;
        let views = config
            .store
            .load_views(&config.session.id)
            .map_err(CliFailure::new)?
            .ok_or_else(|| CliFailure::new("rebind requires projected views"))?;
        if intents.workflow_requests.iter().any(|request| {
            views
                .state
                .workflows
                .get(&request.operation_id)
                .is_none_or(|workflow| workflow.settled.is_none())
        }) {
            return Err(CliFailure::new(
                "rebind requires a safe boundary with no unresolved workflow request",
            ));
        }
        config
            .store
            .submit_workflow_request(
                &mut config.session,
                &sessions::projector::WorkflowRequest {
                    operation_id: operation.clone(),
                    source_locator: attached_locator,
                    kind: workflow_kind,
                },
            )
            .map_err(CliFailure::new)?;
        observe_phase(
            events,
            HostLifecyclePhase::RebindPublishIntent,
            started,
            config.session.pending_queue.len(),
        );
        #[cfg(any(test, feature = "test-support"))]
        observe_test_boundary(
            config.test_injection.as_deref(),
            TestHostWorkflow::Rebind,
            TestHostBoundary::Prepared,
        );
        let source_locator = match run_watched(executor, watch, opened.detach()) {
            Ok(Ok(locator)) => locator,
            Ok(Err(error)) => {
                abandon_prepared_rebind(executor, config, &operation)?;
                return Err(CliFailure::new(format!(
                    "rebind source release failed after durable preparation ({:?}): {error}",
                    error.kind()
                )));
            }
            Err(_) => {
                abandon_prepared_rebind(executor, config, &operation)?;
                return Err(CliFailure::new("application execution failed"));
            }
        };
        if source_locator != attached_locator {
            restore_detached_source(executor, config, opened, source_locator, stderr_buf)?;
            abandon_prepared_rebind(executor, config, &operation)?;
            return Err(CliFailure::new(
                "rebind source advanced after durable preparation; exact reconciliation is required",
            ));
        }
        complete_rebind(
            executor,
            watch,
            config,
            opened,
            stderr_buf,
            events,
            &operation,
            source_locator,
            target,
            runtime,
            started,
        )
        .map(|()| Rebound::Switched)
    })();
    if let Some(tree_id) = attached_tree {
        release_profile_build_lease(executor, events, &tree_id)?;
    }
    if matches!(branched, Ok(Rebound::Switched)) {
        note_reread_instructions(events, instructions_before, opened);
    }
    branched
}

/// The attached agent's enrolled instruction sources, captured before a
/// rebind detaches it so the switched branch can be compared against them.
pub(super) fn instruction_sources(
    opened: &AttachedCodingSession,
) -> Option<Vec<plakous::agent::ResolvedContextSourceFact>> {
    opened
        .agent()
        .map(|agent| agent.harness_plan().instructions().sources().to_vec())
}

/// After a branch switched, names each `AGENTS.md` its target re-read.
pub(super) fn note_reread_instructions<P: plakous_host_protocol::ProtocolExtension>(
    events: &Sender<P>,
    before: Option<Vec<plakous::agent::ResolvedContextSourceFact>>,
    opened: &AttachedCodingSession,
) {
    if let (Some(before), Some(agent)) = (before, opened.agent())
        && let Some(note) =
            reread_instructions_note(&before, agent.harness_plan().instructions().sources())
    {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Settings, note));
    }
}

/// The same note for a branch that was saved but not attached, read from its
/// stored plan. A note that cannot be read is omitted.
pub(super) fn note_reread_instructions_at<P: plakous_host_protocol::ProtocolExtension>(
    executor: &ApplicationExecutor,
    config: &HostConfig<P>,
    events: &Sender<P>,
    before: Option<Vec<plakous::agent::ResolvedContextSourceFact>>,
    target: plakous::sessions::SessionLocator,
) {
    let Some(before) = before else { return };
    let Ok(sessions) = config.state_roots.runtime_sessions() else {
        return;
    };
    if let Ok(Ok(after)) = executor.block_on(sessions.created_instruction_sources(target))
        && let Some(note) = reread_instructions_note(&before, &after)
    {
        let _ = events.notice(TranscriptRow::info(NoticeScope::Settings, note));
    }
}

/// What a branch that recompiled for its target re-read from disk: each
/// enrolled `AGENTS.md` whose bytes differ from the source's, plus any the
/// target gained or lost. `None` when the instructions are unchanged.
fn reread_instructions_note(
    before: &[plakous::agent::ResolvedContextSourceFact],
    after: &[plakous::agent::ResolvedContextSourceFact],
) -> Option<String> {
    let label = plakous::prompts::instructions::instruction_source_scope_label;
    let previous = |id: &str| before.iter().find(|source| source.source_id() == id);
    let mut changes = Vec::new();
    for source in after {
        match previous(source.source_id()) {
            Some(old) if old.content_digest() == source.content_digest() => {}
            Some(_) => changes.push(format!("{} changed", label(source.source_id()))),
            None => changes.push(format!("{} added", label(source.source_id()))),
        }
    }
    for source in before {
        if !after
            .iter()
            .any(|kept| kept.source_id() == source.source_id())
        {
            changes.push(format!("{} removed", label(source.source_id())));
        }
    }
    (!changes.is_empty())
        .then(|| format!("AGENTS.md re-read for this branch: {}", changes.join(", ")))
}

pub(super) fn validate_rebind_effort(
    ready_catalog: &[plakous_host_protocol::views::ProviderSection],
    account: &str,
    model: &str,
    effort: Option<&str>,
) -> CliResult<Option<String>> {
    let (_, explicit) = effort_from_config(effort);
    if effort.is_some() && !explicit {
        return Err(CliFailure::new("rebind target effort is invalid"));
    }
    let entry = ready_catalog
        .iter()
        .find(|section| section.ready && section.account == account)
        .and_then(|section| section.entries.iter().find(|entry| entry.id == model))
        .ok_or_else(|| CliFailure::new("rebind target is absent from the exact Ready catalog"))?;
    if entry.effort_options.is_empty() {
        if explicit {
            return Err(CliFailure::new(
                "rebind target does not admit a reasoning effort",
            ));
        }
        return Ok(None);
    }
    if !explicit && entry.effort_options.iter().any(|option| option == "medium") {
        return Ok(Some("medium".to_owned()));
    }
    if !explicit {
        return Err(CliFailure::new(
            "reasoning rebind target requires an explicit effort because medium is not admitted",
        ));
    }
    let effort = effort.ok_or_else(|| CliFailure::new("rebind target effort is missing"))?;
    if !entry.effort_options.iter().any(|option| option == effort) {
        return Err(CliFailure::new(
            "rebind target effort is absent from the exact Ready catalog",
        ));
    }
    Ok(Some(effort.to_owned()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests {
    use crate::runtime::test_kit::TestExtension;
    type HostEvent = crate::surface_protocol::HostEvent<TestExtension>;
    type Sender = crate::runtime::Sender<TestExtension>;
    type UserAction = crate::surface_protocol::UserAction<TestExtension>;

    use super::*;
    use crate::runtime::*;

    use crate::binding::CompositionSelection as ProductComposition;
    use crate::binding::TargetSelection;
    use crate::interrupt::{InterruptWatch, InterruptWatchState};
    use crate::runtime::startup::api_runtime_for_record;
    use crate::runtime::test_support::*;

    #[test]
    fn live_rebind_discard_removes_only_its_request_without_a_notice() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        let opened = executor
            .block_on(startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        let requests = ["plak:rebind:refused", "plak:fork:other"].map(|operation| {
            sessions::projector::WorkflowRequest {
                operation_id: operation.into(),
                source_locator: opened.locator(),
                kind: sessions::SessionWorkflowKind::Fork,
            }
        });
        for request in &requests {
            config
                .store
                .submit_workflow_request(&mut config.session, request)
                .unwrap();
        }
        abandon_prepared_rebind(&executor, &mut config, &requests[0].operation_id).unwrap();
        let intents = config.store.load_intents(&config.session.id).unwrap();
        assert!(
            intents
                .workflow_request(&requests[0].operation_id)
                .is_none()
        );
        assert!(
            intents
                .workflow_request(&requests[1].operation_id)
                .is_some()
        );
        assert_eq!(
            intents.notices,
            [] as [sessions::projector::OperatorNotice; 0]
        );
        assert!(
            config
                .store
                .submit_workflow_request(&mut config.session, &requests[0])
                .is_err(),
            "discarded identity must refuse identical replay"
        );
    }

    fn instruction_fact(id: &str, content: &str) -> plakous::agent::ResolvedContextSourceFact {
        plakous::agent::ResolvedContextSourceFact::new(id, content, "observed", "test").unwrap()
    }

    #[test]
    fn a_recompiled_branch_names_each_agents_md_it_re_read() {
        let before = [
            instruction_fact("plak.agents.global.v1", "global"),
            instruction_fact(
                "plak.agents.workspace-root.v1",
                "root as the session started",
            ),
            instruction_fact("plak.agents.workspace-0123abcd.v1", "nested"),
        ];
        assert_eq!(reread_instructions_note(&before, &before), None);
        let after = [
            instruction_fact("plak.agents.global.v1", "global"),
            instruction_fact("plak.agents.workspace-root.v1", "root after the edit"),
            instruction_fact("plak.agents.workspace-89abcdef.v1", "new nested"),
        ];
        assert_eq!(
            reread_instructions_note(&before, &after).as_deref(),
            Some(
                "AGENTS.md re-read for this branch: workspace root changed, nested directory \
                 added, nested directory removed"
            )
        );
    }

    #[test]
    fn a_treeless_profile_is_not_already_active_when_topology_is_admitted() {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = fresh_injected_attached_host(root.path());
        let roster = crate::orchestration::RosterInput::unpinned(&config.model_id);
        assert!(!profile_cards_active(&config, &roster).unwrap());
    }

    #[test]
    fn unreadable_topology_refuses_even_an_injected_rebind_target() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.topology_admission = Some(Arc::new(|| Err(CliFailure::new("named config error"))));
        let (_, actions) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let result = resolve_rebind_target(
            &ApplicationExecutor::new().unwrap(),
            InterruptWatch::new(&actions, &state),
            &config,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            "target",
            config.session.composition,
            &TargetSelection::Native,
            "target",
        );
        assert!(
            result
                .err()
                .is_some_and(|error| error.message() == "named config error")
        );
    }
    use crate::sessions::{self, QueueItem as SessionQueueItem, SessionRecord};
    use plakous::agent::ReasoningEffort;
    use plakous::application::ApplicationExecutor;
    use std::cell::RefCell;

    struct TopologyApiCredentials;
    struct NoInferenceSource(plakous::openai::StaticPlatformCredentialSource);
    impl plakous::openai::PlatformCredentialSource for NoInferenceSource {
        fn credential_authority(&self) -> &plakous::openai::CredentialAuthorityId {
            self.0.credential_authority()
        }
        fn api_key(
            &self,
            _: &plakous::openai::CredentialReference,
            _: &plakous::openai::EndpointOrigin,
        ) -> Result<plakous::openai::OpenAiApiKey, plakous::openai::OpenAiAuthError> {
            panic!("topology lifecycle must not request inference credentials")
        }
        fn generation_grant(
            &self,
            reference: &plakous::openai::CredentialReference,
            origin: &plakous::openai::EndpointOrigin,
        ) -> Result<plakous::openai::OpenAiApiKeyGenerationGrant, plakous::openai::OpenAiAuthError>
        {
            self.0.generation_grant(reference, origin)
        }
    }
    impl crate::platform::PlatformCredentials for TopologyApiCredentials {
        fn openai(&self) -> CliResult<Arc<dyn plakous::openai::PlatformCredentialSource>> {
            use plakous::openai::*;
            Ok(Arc::new(NoInferenceSource(
                StaticPlatformCredentialSource::new(
                    OpenAiApiKey::new("synthetic-topology-lifecycle-key").unwrap(),
                    CredentialReference::application("plak/openai").unwrap(),
                    CredentialAuthorityId::new("offline/topology-lifecycle").unwrap(),
                ),
            )))
        }
        fn anthropic(&self) -> CliResult<Arc<dyn plakous::anthropic::AnthropicApiKeySource>> {
            Err(CliFailure::new("unselected provider"))
        }
        fn xai(&self) -> CliResult<Arc<dyn plakous::xai::XaiPlatformApiKeySource>> {
            Err(CliFailure::new("unselected provider"))
        }
    }

    fn actual_api_topology_host(
        root: &std::path::Path,
        executor: &ApplicationExecutor,
    ) -> (HostConfig<TestExtension>, AttachedCodingSession) {
        actual_api_topology_host_with(root, executor, None)
    }

    /// [`actual_api_topology_host`] whose root was created with `frozen`.
    fn actual_api_topology_host_with(
        root: &std::path::Path,
        executor: &ApplicationExecutor,
        frozen: Option<crate::frozen_tools::FrozenRootTools>,
    ) -> (HostConfig<TestExtension>, AttachedCodingSession) {
        let (mut config, _) = fresh_injected_attached_host(root);
        config.session.frozen_tools = frozen;
        config.test_injection = None;
        config.state_roots = config
            .state_roots
            .with_test_platform_credentials(Arc::new(TopologyApiCredentials));
        config.store = config.state_roots.session_records();
        let provider = crate::binding::PlatformProvider::OpenAi;
        config.target = HostTargetAuthority::Platform {
            provider,
            subscriptions: config.state_roots.native_subscriptions().unwrap(),
        };
        config.session.account = Some(provider.account().into());
        config.session.model = Some("gpt-5.6-luna".into());
        config.session.effort = Some("low".into());
        config.session.composition = ProductComposition::Direct;
        config.session.target_selection = TargetSelection::Platform { provider };
        config.model_id = "gpt-5.6-luna".into();
        config.effort = Some(ReasoningEffort::Low);
        let parent = crate::agent::api_topology_parent(
            config.product_home.clone(),
            config.session.id.clone(),
            &config.session,
            config.cwd.clone(),
            config.state_roots.clone(),
            config.model_id.clone(),
            config.effort,
            crate::binding::ApiTarget::Platform { provider },
            info::plan_state_authority(&*config.store, &config.session).unwrap(),
            false,
            crate::agent::NATIVE_RUN_MAX_MODEL_ROUNDS,
            true,
        );
        let mut parent = parent;
        parent.tool_selection = startup::root_tool_selection(&config.session);
        let opened = executor
            .block_on(crate::agent::build_api_root_coding_session(
                parent,
                "offline-topology-root",
                None,
            ))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        (config, opened)
    }

    fn retained_api_parent(tree_id: &str) -> crate::agent::build::ApiTopologyParent {
        let hub = crate::topology_runtime::lookup(tree_id).unwrap();
        let retained = hub.rejoin_spec().unwrap();
        let parent = retained
            .downcast_ref::<crate::agent::SpawnParent>()
            .unwrap();
        match parent {
            crate::agent::SpawnParent::Api(parent) => parent.clone(),
            crate::agent::SpawnParent::Native(_) => panic!("API tree retained a native parent"),
        }
    }

    fn topology_tool_names(tree_id: &str) -> Vec<String> {
        let parent = crate::agent::SpawnParent::Api(retained_api_parent(tree_id));
        crate::agent::build::topology_extensions_if_enabled(
            &parent,
            "gpt-5.6-luna",
            crate::topology_runtime::TopologyHubAttach::LookupOnly,
            true,
            &mut None,
        )
        .unwrap()
        .names()
        .map(ToString::to_string)
        .collect()
    }

    #[test]
    fn production_api_rebind_projects_complete_staging_lineage() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
        let source_id = config.session.id.clone();
        let hub = crate::topology_runtime::lookup(&source_id).unwrap();
        let operation = "plak:rebind:recover-topology";
        let mut target = SessionRecord::new(config.cwd.clone());
        target.id = sessions::projector::WorkflowRequest {
            operation_id: operation.to_owned(),
            source_locator: opened.locator(),
            kind: sessions::SessionWorkflowKind::Fork,
        }
        .successor_product_id();
        target.create_operation_id = format!("plak:create:{}", target.id);
        target.account = Some("openai".into());
        target.model = Some("gpt-5.6-sol".into());
        target.effort = Some("medium".into());
        target.composition = ProductComposition::Direct;
        target.target_selection = config.session.target_selection.clone();
        target.source_product_session_id = Some(source_id.clone());
        target.source_workflow_operation_id = Some(operation.into());
        target.validate_for_store().unwrap();
        let runtime = executor
            .block_on(api_runtime_for_record(
                &config,
                &target,
                false,
                crate::topology_runtime::TopologyHubAttach::LookupOnly,
                None,
                &mut None,
            ))
            .unwrap()
            .unwrap();
        let binding = runtime.target_binding();
        let selection = sessions::RebindSelection {
            provider: "openai".into(),
            account: "openai".into(),
            model: "gpt-5.6-sol".into(),
            effort: Some("medium".into()),
            composition: ProductComposition::Direct,
            target_selection: target.target_selection.clone(),
            protocol: binding.protocol().as_str().into(),
            route: binding.route().as_str().into(),
            requested_model: binding.model().as_str().into(),
            selected_configuration: serde_json::to_string(
                &binding.selected_provider_configuration(),
            )
            .unwrap(),
            effective_configuration: serde_json::to_string(
                &binding.effective_provider_configuration(),
            )
            .unwrap(),
            account_binding: serde_json::to_string(&binding.account_binding()).unwrap(),
        };
        let request = sessions::projector::WorkflowRequest {
            operation_id: operation.into(),
            source_locator: opened.locator(),
            kind: sessions::SessionWorkflowKind::Rebind(Box::new(selection)),
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();

        let (events, _rx) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let source_locator = opened.locator();
        let target_id = target.id.clone();
        complete_rebind(
            &executor,
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            operation,
            source_locator,
            ResolvedRebindTarget {
                route: RebindRoute::Api(Box::new(target)),
                effort: Some(ReasoningEffort::Medium),
                effort_explicit: true,
                binding,
            },
            runtime,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(config.session.id, target_id);
        assert_eq!(
            config.session.source_product_session_id.as_deref(),
            Some(source_id.as_str())
        );
        assert_eq!(
            config.session.source_workflow_operation_id.as_deref(),
            Some(operation)
        );
        config.session.validate_for_store().unwrap();
        assert_eq!(retained_api_parent(&source_id).model, "gpt-5.6-sol");
        assert_eq!(
            config.source_releases.settle(&source_id, || {}),
            source_release::Settlement::Released
        );
        let source = config.store.load(&source_id).unwrap();
        let projected = executor
            .block_on(project_session(opened.sessions(), &config.store, &source))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(projected.pending_workflows, [] as [String; 0]);
        assert_eq!(
            projected.head.branch_id(),
            source.plakous_locator.unwrap().branch_id()
        );
        assert!(Arc::ptr_eq(
            &hub,
            &crate::topology_runtime::lookup(&source_id).unwrap()
        ));
        executor.block_on(opened.release()).unwrap().unwrap();
        assert!(crate::topology_runtime::lookup(&source_id).is_none());
    }

    #[test]
    fn reopening_with_an_unadmitted_rebind_withdraws_once_without_branching() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, injection) = fresh_injected_attached_host(root.path());
        let opened = executor
            .block_on(startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        let source_locator = opened.locator();
        config.session.bind_plakous_locator(source_locator).unwrap();
        config.store.save(&mut config.session).unwrap();
        let source_id = config.session.id.clone();
        let binding = &injection.rebind_target;
        let request = sessions::projector::WorkflowRequest {
            operation_id: "plak:rebind:unadmitted".into(),
            source_locator,
            kind: sessions::SessionWorkflowKind::Rebind(Box::new(sessions::RebindSelection {
                provider: "codex".into(),
                account: "codex".into(),
                model: "offline-loopback-v2".into(),
                effort: Some("medium".into()),
                composition: config.session.composition,
                target_selection: config.session.target_selection.clone(),
                protocol: binding.protocol().as_str().into(),
                route: binding.route().as_str().into(),
                requested_model: binding.model().as_str().into(),
                selected_configuration: serde_json::to_string(
                    &binding.selected_provider_configuration(),
                )
                .unwrap(),
                effective_configuration: serde_json::to_string(
                    &binding.effective_provider_configuration(),
                )
                .unwrap(),
                account_binding: serde_json::to_string(&binding.account_binding()).unwrap(),
            })),
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();
        executor.block_on(opened.release()).unwrap().unwrap();
        let store = Arc::clone(&config.store);
        let (actions, events, worker) = spawn_host_worker(config).unwrap();
        loop {
            if matches!(
                events.recv_timeout(Duration::from_secs(10)).unwrap(),
                HostEvent::AgentAttached(true)
            ) {
                break;
            }
        }
        let stored = store.load(&source_id).unwrap();
        assert_eq!(
            stored.plakous_locator.unwrap().branch_id(),
            source_locator.branch_id()
        );
        let intents = store.load_intents(&source_id).unwrap();
        assert_eq!(
            intents.workflow_requests,
            [] as [sessions::projector::WorkflowRequest; 0]
        );
        let notice = TranscriptRow::info(NoticeScope::Recovery, request.withdrawal_note());
        assert_eq!(
            intents
                .notices
                .iter()
                .filter(|row| row.row == notice)
                .count(),
            1
        );
        assert!(store.load(&request.successor_product_id()).is_err());
        let mut stored = store.load(&source_id).unwrap();
        assert_eq!(
            store.withdraw_unadmitted_workflows(&mut stored).unwrap(),
            [] as [TranscriptRow; 0]
        );

        actions.send(UserAction::Quit).unwrap();
        worker.join().unwrap().unwrap();
    }

    /// A plain `/fork` lands on the instruction files as they are now and
    /// keeps the source's history: unchanged files fork exactly and say
    /// nothing, an `AGENTS.md` written since is re-read and named.
    #[test]
    fn plain_fork_rereads_changed_agents_md_and_names_it() {
        use crate::runtime::fork::handle_fork;
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
        let fork = |config: &mut HostConfig<TestExtension>, opened: &mut AttachedCodingSession| {
            let source_id = config.session.id.clone();
            let (events, notes_rx) = host_event_sink();
            let (_actions, receiver) =
                crate::surface_protocol::user_action_channel::<TestExtension>();
            let state = RefCell::new(InterruptWatchState::default());
            handle_fork(
                &executor,
                InterruptWatch::new(&receiver, &state),
                config,
                opened,
                &mut Vec::new(),
                &events,
                None,
            )
            .unwrap();
            assert_ne!(config.session.id, source_id);
            notes_rx
                .try_iter()
                .filter_map(|event| match event {
                    HostEvent::Row(row) => row.text().map(str::to_owned),
                    _ => None,
                })
                .filter(|note| note.contains("AGENTS.md"))
                .collect::<Vec<_>>()
        };
        let instructions = |opened: &AttachedCodingSession| {
            instruction_sources(opened)
                .unwrap()
                .iter()
                .map(|source| source.content().to_owned())
                .collect::<Vec<_>>()
        };
        let before = instructions(&opened);
        assert_eq!(fork(&mut config, &mut opened), Vec::<String>::new());
        assert_eq!(instructions(&opened), before);

        std::fs::write(
            config.session.workspace_root.join("AGENTS.md"),
            "written before the fork",
        )
        .unwrap();
        assert_eq!(
            fork(&mut config, &mut opened),
            ["AGENTS.md re-read for this branch: workspace root added"]
        );
        assert!(
            instructions(&opened)
                .iter()
                .any(|content| content.contains("written before the fork")),
            "{:?}",
            instructions(&opened)
        );
        executor.block_on(opened.release()).unwrap().unwrap();
    }

    fn row_texts(
        receiver: &crate::surface_protocol::HostEventReceiver<TestExtension>,
    ) -> Vec<String> {
        receiver
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(row) => row.text().map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    /// Reopen projects a recorded fork; an unrecorded request is withdrawn,
    /// never compiled or replayed after a crash.
    fn prepared_fork_across_open(
        executor: &ApplicationExecutor,
        mut config: HostConfig<TestExtension>,
        mut opened: AttachedCodingSession,
        canonical_published: bool,
        _names_reread_instructions: bool,
    ) {
        let source_id = config.session.id.clone();
        let source_locator = executor.block_on(opened.detach()).unwrap().unwrap();
        config.session.bind_plakous_locator(source_locator).unwrap();
        config.store.save(&mut config.session).unwrap();
        let request = sessions::projector::WorkflowRequest {
            operation_id: "plak:fork:prepared-before-open".into(),
            source_locator,
            kind: sessions::SessionWorkflowKind::Fork,
        };
        config
            .store
            .submit_workflow_request(&mut config.session, &request)
            .unwrap();
        std::fs::write(
            config.session.workspace_root.join("AGENTS.md"),
            "written while the fork was prepared",
        )
        .unwrap();
        let published = canonical_published.then(|| {
            let runtime = fork::fork_runtime_binding(executor, &config, &mut Vec::new()).unwrap();
            let outcome = executor
                .block_on(opened.fork(
                    SessionOperationId::new(&request.operation_id).unwrap(),
                    runtime,
                ))
                .unwrap()
                .unwrap();
            let plakous::sessions::ForkSessionOutcome::Forked { target, .. } = outcome else {
                panic!("the canonical fork did not publish: {outcome:?}");
            };
            target
        });
        executor.block_on(opened.release()).unwrap().unwrap();
        let reopened = executor
            .block_on(startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        assert_eq!(
            config.session.id, source_id,
            "reopen keeps its requested source"
        );
        let notices = config
            .store
            .withdraw_unadmitted_workflows(&mut config.session)
            .unwrap();
        if let Some(target) = published {
            assert_eq!(notices, [] as [TranscriptRow; 0]);
            let successor = config.store.load(&request.successor_product_id()).unwrap();
            assert_eq!(successor.plakous_locator, Some(target));
            let views = config.store.load_views(&source_id).unwrap().unwrap();
            assert!(
                views.state.workflows[&request.operation_id]
                    .settled
                    .is_some()
            );
        } else {
            assert_eq!(notices.len(), 1);
            assert_eq!(notices[0].text(), Some(request.withdrawal_note()));
            assert_eq!(
                config
                    .store
                    .withdraw_unadmitted_workflows(&mut config.session)
                    .unwrap(),
                [] as [TranscriptRow; 0]
            );
            assert_eq!(
                config
                    .store
                    .load_intents(&source_id)
                    .unwrap()
                    .workflow_requests,
                [] as [sessions::projector::WorkflowRequest; 0]
            );
        }

        executor.block_on(reopened.release()).unwrap().unwrap();
    }

    #[test]
    fn prepared_fork_recovers_before_open_in_a_fresh_process() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = actual_api_topology_host(root.path(), &executor);
        prepared_fork_across_open(&executor, config, opened, false, true);
    }

    #[test]
    fn published_fork_recovers_before_open_in_a_fresh_process() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = actual_api_topology_host(root.path(), &executor);
        prepared_fork_across_open(&executor, config, opened, true, true);
    }

    fn native_injected_host(
        root: &std::path::Path,
        executor: &ApplicationExecutor,
    ) -> (HostConfig<TestExtension>, AttachedCodingSession) {
        let (mut config, _) = fresh_injected_attached_host(root);
        let opened = executor
            .block_on(startup::build_session_from_config(
                &mut config,
                &mut Vec::new(),
            ))
            .unwrap()
            .unwrap();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        config.store.save(&mut config.session).unwrap();
        (config, opened)
    }

    #[test]
    fn prepared_native_fork_recovers_before_open_in_a_fresh_process() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = native_injected_host(root.path(), &executor);
        prepared_fork_across_open(&executor, config, opened, false, false);
    }

    #[test]
    fn published_native_fork_recovers_before_open_in_a_fresh_process() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = native_injected_host(root.path(), &executor);
        prepared_fork_across_open(&executor, config, opened, true, false);
    }

    /// A fork saved without attaching names the `AGENTS.md` it re-read from
    /// the plan it was stored with.
    #[test]
    fn a_saved_fork_names_the_agents_md_it_reread() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, mut opened) = actual_api_topology_host(root.path(), &executor);
        let before = instruction_sources(&opened);
        std::fs::write(
            config.session.workspace_root.join("AGENTS.md"),
            "written before the fork",
        )
        .unwrap();
        let source = executor.block_on(opened.detach()).unwrap().unwrap();
        let runtime = fork::fork_runtime_binding(&executor, &config, &mut Vec::new()).unwrap();
        let sessions = config.state_roots.runtime_sessions().unwrap();
        let outcome = executor
            .block_on(
                sessions.fork(plakous::sessions::ForkSessionRequest::recompiling(
                    SessionOperationId::new("plak:fork:saved-note").unwrap(),
                    source,
                    runtime,
                )),
            )
            .unwrap()
            .unwrap();
        let plakous::sessions::ForkSessionOutcome::Forked { target, .. } = outcome else {
            panic!("the fork did not publish: {outcome:?}");
        };
        let (events, notes_rx) = host_event_sink();
        note_reread_instructions_at(&executor, &config, &events, before, target);
        executor.block_on(opened.release()).unwrap().unwrap();
        assert_eq!(
            row_texts(&notes_rx),
            ["AGENTS.md re-read for this branch: workspace root added"]
        );
    }

    #[test]
    fn refused_fork_does_not_strand_the_session() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, opened) = native_injected_host(root.path(), &executor);
        prepared_fork_across_open(&executor, config, opened, false, false);
    }

    /// `[roster] subagents` turned off after the session started: a plain
    /// `/fork` recompiles the session's own tree and still lands.
    #[test]
    fn plain_fork_after_subagents_were_turned_off_lands() {
        use crate::runtime::fork::handle_fork;
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
        config.topology_enabled = false;
        let source_id = config.session.id.clone();
        let (events, _notes) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let forked = handle_fork(
            &executor,
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            None,
        );
        let message = forked
            .as_ref()
            .err()
            .map(|error| error.message().to_owned());
        executor.block_on(opened.release()).unwrap().unwrap();
        assert!(forked.is_ok(), "/fork failed: {message:?}");
        assert_ne!(config.session.id, source_id);
    }

    /// Fork now onto a direct API root branches onto that account's route
    /// with a tree of its own, names the `AGENTS.md` it re-read, and leaves
    /// no build lease behind.
    #[test]
    fn fork_now_onto_an_api_root_branches_onto_its_own_tree() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (mut config, mut opened) = actual_api_topology_host(root.path(), &executor);
        let source_id = config.session.id.clone();
        std::fs::write(
            config.session.workspace_root.join("AGENTS.md"),
            "written before the fork",
        )
        .unwrap();
        let (events, notes_rx) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let rebound = handle_rebind(
            &executor,
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "openai",
            "gpt-5.6-sol",
            Some("medium"),
            None,
            // Direct: plakous-default binds native-ptc here, whose sidecar this
            // test does not install.
            Some(ProductComposition::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("gpt-5.6-sol"),
        )
        .unwrap();
        assert_eq!(rebound, Rebound::Switched);
        let target_id = config.session.id.clone();
        assert_ne!(target_id, source_id);
        assert_eq!(
            config.session.source_product_session_id.as_deref(),
            Some(source_id.as_str())
        );
        assert_eq!(
            config.session.target_selection,
            TargetSelection::Platform {
                provider: crate::binding::PlatformProvider::OpenAi
            }
        );
        assert_eq!(config.session.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(config.session.effort.as_deref(), Some("medium"));
        assert_eq!(config.session.composition, ProductComposition::Direct);
        assert!(matches!(
            config.target,
            HostTargetAuthority::Platform { .. }
        ));
        assert_eq!(config.model_id, "gpt-5.6-sol");
        assert_eq!(
            durable_tree_id(&config.topology_rows(), &config.session),
            target_id
        );
        let names = topology_tool_names(&target_id);
        assert!(!names.is_empty(), "{names:?}");
        let notes = notes_rx
            .try_iter()
            .filter_map(|event| match event {
                HostEvent::Row(row) => row.text().map(str::to_owned),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            notes
                .iter()
                .any(|note| note == "AGENTS.md re-read for this branch: workspace root added"),
            "{notes:?}"
        );
        executor.block_on(opened.release()).unwrap().unwrap();
        assert!(crate::topology_runtime::lookup(&target_id).is_none());
    }

    /// The root's tools belong to the session: a rebind onto another model
    /// keeps the MCP records frozen at creation, so canonical tool authority
    /// is unchanged, nothing is listed again, and the successor binds them.
    #[test]
    fn a_rebind_keeps_the_sessions_frozen_mcp_tools_without_listing_again() {
        let server = std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(std::path::Path::parent)
            .unwrap()
            .join("mcp-fake-server");
        assert!(
            server.is_file(),
            "run the workspace tests so the fixture is built"
        );
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let list_log = root.path().join("mcp-lists.log");
        let template = crate::binding::RootTemplateFact {
            name: "lead".to_owned(),
            account: "openai".to_owned(),
            model: "gpt-5.6-luna".to_owned(),
            tools: crate::tool_plan::TemplateToolSelection::parse(["mcp:fake"]).unwrap(),
            tool_sections: std::collections::BTreeMap::from([(
                "mcp_servers.fake".to_owned(),
                format!(
                    "[mcp_servers.fake]\ntransport = \"stdio\"\ncommand = {:?}\n\
                     args = [\"--list-log\", {:?}]\nenv = {{ }}\nsecret_env = {{ }}\n\
                     startup_seconds = 20\ncall_seconds = 120\n",
                    server.to_str().unwrap(),
                    list_log.to_str().unwrap()
                ),
            )]),
        };
        let unlisted = crate::frozen_tools::FrozenRootTools::at_creation(
            Some(template),
            Some("openai"),
            Some("gpt-5.6-luna"),
        );
        let tools = unlisted.selection();
        let workspace = root.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let records = crate::mcp_binding::bind_mcp_tools(
            &tools.selection,
            &tools.tool_sections,
            &workspace,
            crate::agent::ProductStateRoots::with_doubles(
                root.path().join("h"),
                root.path().join("s"),
            )
            .named_secrets(),
            None,
        )
        .unwrap()
        .records;
        let frozen = unlisted.with_mcp_tools(records);
        let (mut config, mut opened) =
            actual_api_topology_host_with(root.path(), &executor, Some(frozen.clone()));
        assert_eq!(std::fs::read_to_string(&list_log).unwrap(), "list\n");

        let (events, _notes) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let rebound = handle_rebind(
            &executor,
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "openai",
            "gpt-5.6-sol",
            Some("medium"),
            None,
            Some(ProductComposition::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("gpt-5.6-sol"),
        )
        .unwrap();
        assert_eq!(rebound, Rebound::Switched);
        let successor = config.store.load(&config.session.id).unwrap();
        assert_eq!(successor.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(successor.frozen_tools.as_ref(), Some(&frozen));
        let registrations = crate::agent::injected_root_tool_registrations(
            &frozen,
            &successor,
            &config.state_roots,
        )
        .unwrap();
        assert!(
            registrations
                .iter()
                .any(|registration| registration.name().as_str() == "mcp__fake__echo"),
            "the successor binds the frozen records"
        );
        assert_eq!(
            std::fs::read_to_string(&list_log).unwrap(),
            "list\n",
            "neither the rebind nor a later build lists again"
        );
        executor.block_on(opened.release()).unwrap().unwrap();
    }

    /// A rebind whose target route cannot serve one of the session's tools
    /// refuses with one line naming the tool and the remedy, before any
    /// durable work.
    #[test]
    fn a_rebind_to_a_route_without_a_frozen_tool_refuses_up_front() {
        let root = tempfile::tempdir().unwrap();
        let mut config = code_mode_source_with_a_prefix(root.path());
        config.session.frozen_tools = Some(crate::frozen_tools::FrozenRootTools::new(Some(
            crate::binding::RootTemplateFact {
                name: "lead".to_owned(),
                account: "claude".to_owned(),
                model: "m".to_owned(),
                tools: crate::tool_plan::TemplateToolSelection::parse(["web_fetch"]).unwrap(),
                tool_sections: std::collections::BTreeMap::new(),
            },
        )));
        let mut opened = memory_attached_stub();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();
        let (events, _rx) = host_event_sink();
        let (_actions, receiver) = crate::surface_protocol::user_action_channel::<TestExtension>();
        let state = RefCell::new(InterruptWatchState::default());
        let error = handle_rebind(
            &ApplicationExecutor::new().unwrap(),
            InterruptWatch::new(&receiver, &state),
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            Some(crate::binding::CompositionSelection::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
        )
        .unwrap_err();
        assert_eq!(
            error.message(),
            "this session keeps its tools across a model change, and web_fetch is not available \
             on codex; remedy: /clear, or start a new session on that model"
        );
        assert!(
            config
                .test_injection
                .as_ref()
                .unwrap()
                .rebind_builds
                .lock()
                .unwrap()
                .is_empty(),
            "no target runtime was built"
        );
        assert!(
            !matches!(
                config.store.load_intents(&config.session.id),
                Ok(intents) if !intents.workflow_requests.is_empty()
            ),
            "no workflow request was written"
        );
    }

    #[test]
    fn production_api_open_failures_do_not_steal_or_leak_root_leases() {
        for failure in [
            "occupancy",
            "runtime-db",
            "model-admission",
            "canonical-open",
        ] {
            let root = tempfile::tempdir().unwrap();
            let executor = ApplicationExecutor::new().unwrap();
            let (config, opened) = actual_api_topology_host(root.path(), &executor);
            let tree_id = config.session.id.clone();
            let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
            let mut parent = retained_api_parent(&tree_id);
            match failure {
                "occupancy" => {
                    parent.cwd = root.path().join(".plak/worktrees/not-a-directory");
                    std::fs::create_dir_all(parent.cwd.parent().unwrap()).unwrap();
                    std::fs::write(&parent.cwd, b"fixture").unwrap();
                }
                "runtime-db" => {
                    let product = root.path().join("bad-runtime");
                    std::fs::create_dir_all(product.join("runtime.sqlite3")).unwrap();
                    parent.state_roots = crate::agent::ProductStateRoots::with_doubles(
                        product,
                        root.path().join("bad-state"),
                    )
                    .with_test_platform_credentials(Arc::new(TopologyApiCredentials));
                }
                "model-admission" => parent.model = "unsupported-model".into(),
                _ => parent.model = "gpt-5.6-sol".into(),
            }
            let result = executor
                .block_on(crate::agent::open_api_root_coding_session(
                    parent,
                    opened.locator(),
                    None,
                ))
                .unwrap();
            assert!(result.is_err(), "{failure}");
            assert!(
                Arc::ptr_eq(&hub, &crate::topology_runtime::lookup(&tree_id).unwrap()),
                "{failure} stole the source lease"
            );
            assert_eq!(retained_api_parent(&tree_id).model, "gpt-5.6-luna");
            executor.block_on(opened.release()).unwrap().unwrap();
            assert!(
                crate::topology_runtime::lookup(&tree_id).is_none(),
                "{failure} leaked an acquired lease"
            );
        }
    }

    #[test]
    fn production_api_detached_source_restoration_retires_previous_root_lease() {
        let root = tempfile::tempdir().unwrap();
        let executor = ApplicationExecutor::new().unwrap();
        let (config, mut opened) = actual_api_topology_host(root.path(), &executor);
        let tree_id = config.session.id.clone();
        let hub = crate::topology_runtime::lookup(&tree_id).unwrap();
        let names = topology_tool_names(&tree_id);
        let locator = executor.block_on(opened.detach()).unwrap().unwrap();
        assert!(Arc::ptr_eq(
            &hub,
            &crate::topology_runtime::lookup(&tree_id).unwrap()
        ));
        restore_detached_source(&executor, &config, &mut opened, locator, &mut Vec::new()).unwrap();
        assert!(Arc::ptr_eq(
            &hub,
            &crate::topology_runtime::lookup(&tree_id).unwrap()
        ));
        assert_eq!(topology_tool_names(&tree_id), names);
        assert!(opened.agent().is_some());
        executor.block_on(opened.release()).unwrap().unwrap();
        assert!(
            crate::topology_runtime::lookup(&tree_id).is_none(),
            "detached prior owner leaked a root lease"
        );
    }

    /// An effort-only change is not a model switch: a profile fork onto the
    /// same account and model must not report a switch between a model and
    /// itself, nor the caveat about the previous model's reasoning.
    #[test]
    fn an_effort_only_rebind_is_not_reported_as_a_model_switch() {
        let before = RebindBefore {
            account: Some("codex"),
            model: "gpt-5.6-luna",
            effort: Some(ReasoningEffort::Low),
        };
        let raised = RebindBefore {
            effort: Some(ReasoningEffort::High),
            ..before
        };
        let note = rebind_note(before, raised);
        assert!(
            note.starts_with("Effort changed \u{b7} codex \u{b7} gpt-5.6-luna \u{b7} high"),
            "{note}"
        );
        assert!(!note.contains("Model switched"), "{note}");
        assert!(
            !note.contains("native reasoning from the previous model"),
            "the caveat does not apply when the model has not moved: {note}"
        );

        let other_model = RebindBefore {
            model: "gpt-5.3-codex-spark",
            ..before
        };
        let switched = rebind_note(before, other_model);
        assert!(
            switched.starts_with("Model switched \u{b7} codex \u{b7} gpt-5.3-codex-spark"),
            "{switched}"
        );
        assert!(switched.contains("native reasoning from the previous model"));

        // A cross-provider move on the same model name is still a switch.
        let other_account = RebindBefore {
            account: Some("openrouter"),
            ..before
        };
        assert!(rebind_note(before, other_account).starts_with("Model switched"));
    }

    #[test]
    fn rebind_boundary_requires_empty_queue_and_no_unresolved_work() {
        let root = tempfile::tempdir().unwrap();
        let session = SessionRecord::new(root.path().to_path_buf());
        assert!(session_rebind_boundary_ready(&session));
        let mut queued = session.clone();
        queued.pending_queue.push(SessionQueueItem::new("later"));
        assert!(!session_rebind_boundary_ready(&queued));
    }

    #[test]
    fn rebind_boundary_uses_submission_facts_without_delivery_receipts() {
        use crate::sessions::projector::{
            Delivery, ProductIntents, SessionViews, Submission, SubmissionKind,
        };

        let mut session = SessionRecord::new("/work".into());
        let item = SessionQueueItem::new("submitted");
        let intents = ProductIntents {
            submissions: vec![Submission {
                command_id: "rebind-boundary-command".into(),
                kind: SubmissionKind::RootStart,
                queue_item_id: item.id,
                queue_item_revision: item.revision,
                target_turn_id: None,
                canonical_command: item.text,
                submitted_at_ms: item.submitted_at_ms,
            }],
            ..ProductIntents::default()
        };
        let mut views = SessionViews::empty();
        session.queue_commands = sessions::QueueCommands::from_join(&intents, Some(&views));
        assert!(!session_rebind_boundary_ready(&session));

        for delivery in [
            Delivery::Accepted,
            Delivery::Rejected {
                error: "busy".into(),
            },
        ] {
            views
                .state
                .deliveries
                .insert("rebind-boundary-command".into(), delivery);
            session.queue_commands = sessions::QueueCommands::from_join(&intents, Some(&views));
            assert!(session_rebind_boundary_ready(&session));
        }
    }

    #[test]
    fn rebind_defaults_reasoning_to_medium_when_admitted() {
        let catalog = vec![plakous_host_protocol::views::ProviderSection {
            account: "codex".into(),
            label: "Codex".into(),
            ready: true,
            entries: vec![
                plakous_host_protocol::views::ModelEntry {
                    id: "plain-non-reasoning".into(),
                    tag: String::new(),
                    reasoning: false,
                    effort_options: Vec::new(),
                    extras: Vec::new(),
                    unavailable_reason: None,
                },
                plakous_host_protocol::views::ModelEntry {
                    id: "gpt-reasoning".into(),
                    tag: String::new(),
                    reasoning: true,
                    effort_options: vec!["low".into(), "medium".into()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                },
            ],
        }];
        assert_eq!(
            validate_rebind_effort(&catalog, "codex", "plain-non-reasoning", None).unwrap(),
            None
        );
        assert_eq!(
            validate_rebind_effort(&catalog, "codex", "gpt-reasoning", None).unwrap(),
            Some("medium".into())
        );
        assert_eq!(
            validate_rebind_effort(&catalog, "codex", "gpt-reasoning", Some("low")).unwrap(),
            Some("low".into())
        );
        assert!(
            validate_rebind_effort(&catalog, "codex", "gpt-reasoning", Some("invalid")).is_err()
        );
        assert!(
            validate_rebind_effort(&catalog, "codex", "plain-non-reasoning", Some("medium"))
                .is_err()
        );
    }

    #[test]
    fn restored_paused_tree_epoch_blocks_rebind_for_session_tree_id() {
        let root = tempfile::tempdir().unwrap();
        let (config, _) = host_config_with_tree_epoch_rebind_block(root.path());
        assert!(tree_epoch_blocks_rebind(
            &config.topology_rows(),
            &config.session
        ));
    }

    #[test]
    fn handle_rebind_fails_closed_when_tree_epoch_has_live_descendants() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        install_live_descendant_on_hub(
            &durable_tree_id(&config.topology_rows(), &config.session),
            &config.cwd,
        );
        let mut opened = memory_attached_stub();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();

        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let (_actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let error = handle_rebind(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
        )
        .unwrap_err();
        assert_eq!(
            error.message(),
            crate::agent_tree::ProductAgentTree::TREE_EPOCH_REBIND_LIVE_DESCENDANTS
        );
        assert!(no_unsettled_rebind_requests(&config));
    }

    /// A Codex Code Mode (native-ptc) prefix is replayable onto every
    /// presentation: the kernels re-project `exec`/`wait` cells at dispatch
    /// (Responses/Chat carry the original string input, Messages wraps it as
    /// `{"input": ...}` — see the kernel tests), so the host admits the rebind
    /// like any other. A refusal here ("cannot replay a Codex Code Mode
    /// (native-ptc) prefix onto another presentation") would make a profile
    /// fork after a Code Mode turn a dead end. The memory stub cannot complete
    /// the branch, so only the admission is asserted.
    #[test]
    fn a_code_mode_prefix_is_admitted_for_a_direct_target() {
        let root = tempfile::tempdir().unwrap();
        let mut config = code_mode_source_with_a_prefix(root.path());
        assert_eq!(config.resolved_composition_label(), "native-ptc");
        let mut opened = memory_attached_stub();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();

        let (events, rx) = host_event_sink();
        let (_actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let result = handle_rebind(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            Some(crate::binding::CompositionSelection::Direct),
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
        );
        let mut notes = Vec::new();
        if let Err(error) = &result {
            notes.push(error.message().to_owned());
        }
        while let Ok(event) = rx.try_recv() {
            if let HostEvent::Row(TranscriptRow::Notice(
                plakous_host_protocol::views::transcript::NoticeRow { text: note, .. },
            )) = event
            {
                notes.push(note);
            }
        }
        assert!(
            notes
                .iter()
                .all(|note| !note.contains("Code Mode") && !note.contains("native-ptc")),
            "no Code Mode prefix refusal: {notes:?}"
        );
        let error = result.unwrap_err();
        assert!(
            error
                .message()
                .contains("projector could not read the feed"),
            "unexpected refusal: {error}"
        );
        assert!(
            !config
                .test_injection
                .as_ref()
                .unwrap()
                .rebind_builds
                .lock()
                .unwrap()
                .is_empty(),
            "the target runtime must have compiled before the stub's missing feed refuses publication"
        );
    }

    #[test]
    fn interrupt_refuses_rebind_before_any_branch_is_created() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        config.ready_catalog = plakous_host_protocol::views::ReadyCatalog::new(vec![
            plakous_host_protocol::views::ProviderSection {
                account: "codex".into(),
                label: "Codex".into(),
                ready: true,
                entries: vec![plakous_host_protocol::views::ModelEntry {
                    id: "offline-loopback-v2".into(),
                    tag: "offline".into(),
                    reasoning: true,
                    effort_options: vec!["medium".into()],
                    extras: Vec::new(),
                    unavailable_reason: None,
                }],
            },
        ]);
        let mut opened = memory_attached_stub();
        let source = opened.locator();
        config.session.bind_plakous_locator(source).unwrap();
        let (events, _rx) = host_event_sink();
        let (actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        actions_tx.send(UserAction::Interrupt).unwrap();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);

        let error = handle_rebind(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
        )
        .expect_err("an interrupted rebind must not create a branch");

        assert!(
            error.message().contains("interrupted"),
            "rebind refusal must name the interrupt: {}",
            error.message()
        );
        // the exact source binding and model survive untouched.
        assert_eq!(config.session.plakous_locator, Some(source));
        assert_eq!(config.session.model.as_deref(), Some("offline-loopback-v1"));
        assert!(no_unsettled_rebind_requests(&config));
    }

    #[test]
    fn handle_rebind_fails_closed_when_tree_epoch_blocks_rebind() {
        let root = tempfile::tempdir().unwrap();
        let (mut config, _) = fresh_injected_attached_host(root.path());
        test_kit::seed_restored_paused_tree(
            &config.topology_rows(),
            &config.session.id,
            &config.cwd,
        )
        .unwrap();
        let mut opened = memory_attached_stub();
        config
            .session
            .bind_plakous_locator(opened.locator())
            .unwrap();

        let (tx, _rx) = crate::surface_protocol::host_event_channel::<TestExtension>();
        let events = Sender::over(tx);
        let (_actions_tx, actions_rx) =
            crate::surface_protocol::user_action_channel::<TestExtension>();
        let interrupt_state = RefCell::new(InterruptWatchState::default());
        let watch = InterruptWatch::new(&actions_rx, &interrupt_state);
        let error = handle_rebind(
            &ApplicationExecutor::new().unwrap(),
            watch,
            &mut config,
            &mut opened,
            &mut Vec::new(),
            &events,
            "codex",
            "offline-loopback-v2",
            Some("medium"),
            None,
            None,
            None,
            &crate::orchestration::RosterInput::unpinned("offline-loopback-v2"),
        )
        .unwrap_err();
        assert_eq!(
            error.message(),
            crate::agent_tree::ProductAgentTree::TREE_EPOCH_REBIND_PAUSED
        );
        assert!(no_unsettled_rebind_requests(&config));
    }
}
