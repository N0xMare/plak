//! Exact compaction requests, admission and source-cache affinity.

use crate::runtime::AgentBuilder;
use crate::runtime::admission::sealed_runtime_admission;
use crate::runtime::compaction::fold::{compact_diagnostic, measure_compaction_successor};
use crate::runtime::compaction::placeholder::{
    collect_compaction_reduction_candidates, replace_compaction_result_body,
};
use crate::runtime::compaction::record::{compaction_attempt_id, compaction_uuid};
use crate::runtime::compaction::{
    COMPACTION_INSTRUCTION, DurableCompactionIdentity, DurableCompactionRequestRecord,
    ExactCompactionRuntime, NATIVE_COMPACTION_NAMESPACE, NATIVE_COMPACTION_TRIGGER_V1,
};
use crate::runtime::continuation::runtime_manifest_error;
use crate::runtime::durable::branch::validate_durable_builder;
use crate::runtime::observation::budget::observation_bytes_from_headroom;
use crate::{
    AgentError, AgentErrorKind, AgentSessionEndpointError, ContextEvaluationAudit,
    RuntimeCheckpoint, RuntimeDurability, RuntimeManifest, SessionWorkflowId,
};
use plakous_model::{
    Actor, CanonicalHistory, CapacityRule, Clock, CompiledProviderConfiguration, ContentBlock,
    ContextAdmissionRequirement, ContextAnchor, ContextStatus, ContextStatusKind, ConversationItem,
    ConversationView, Digest, DigestDomain, EffectiveProviderConfigurationId, EntryId,
    GenerationLimits, HistorySegment, HistorySegmentKind, LanguageModel,
    LocalDurabilityDisposition, ModelCacheAffinity, ModelRequest, NamespaceId, NativeOptions,
    Provenance, ReasoningPolicy, RequestMetadata, RunId, SelectedProviderConfigurationId,
    SemanticCompactionCut, StableRequestPrefix, Support, TextBlock, ToolChoice, WorkingProjection,
    context_occupancy_tokens, semantic_compaction_output_bound,
};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

/// Fresh compaction has no replay authority. Keep portable semantics, never
/// the unbound opaque envelope or private reasoning it cannot lower.
pub(in crate::runtime) fn portable_compaction_history(
    history: &CanonicalHistory,
) -> Result<CanonicalHistory, AgentError> {
    use plakous_model::{ContentBlock, HistorySegment, HistorySegmentKind, OpaqueFallback};

    let mut segments = Vec::new();
    for segment in history.segments() {
        let mut items = Vec::new();
        for item in segment.items() {
            let blocks = item
                .blocks()
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Opaque(opaque) => match opaque.fallback() {
                        OpaqueFallback::SemanticSummary(text) => TextBlock::new(text.clone(), None)
                            .ok()
                            .map(ContentBlock::Text),
                        _ => None,
                    },
                    ContentBlock::Reasoning(_) => None,
                    _ => Some(block.clone()),
                })
                .collect::<Vec<_>>();
            if !blocks.is_empty() {
                items.push(
                    ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
                        .map_err(|error| {
                            AgentError::new(AgentErrorKind::Internal, error.to_string())
                        })?,
                );
            }
        }
        if items.is_empty() {
            continue;
        }
        let kind = if segment.kind() == HistorySegmentKind::ReasoningReplay {
            HistorySegmentKind::Conversation
        } else {
            segment.kind()
        };
        segments.push(
            HistorySegment::new(kind, items)
                .map_err(|error| AgentError::new(AgentErrorKind::Internal, error.to_string()))?,
        );
    }
    CanonicalHistory::new(segments)
        .map_err(|error| AgentError::new(AgentErrorKind::Internal, error.to_string()))
}
use std::time::Duration;
pub(in crate::runtime) fn native_compaction_facet_enrolled(
    configuration: &CompiledProviderConfiguration,
) -> bool {
    let Ok(namespace) = NamespaceId::new(NATIVE_COMPACTION_NAMESPACE) else {
        return false;
    };
    configuration
        .effective()
        .descriptor()
        .capabilities()
        .native_namespace(&namespace)
        .is_some_and(|fact| fact.support() == Support::Native)
}

/// The model's total context window. A reference envelope without a reported
/// window states its own input maximum as the only window.
pub(in crate::runtime) fn total_context_window_tokens(rule: CapacityRule) -> Option<u64> {
    match rule {
        CapacityRule::ProviderReservedInput {
            reported_context_window,
            maximum_projected_input,
            ..
        } => Some(reported_context_window.unwrap_or(maximum_projected_input)),
        CapacityRule::SharedWindow { total }
        | CapacityRule::SharedWindowWithOutputCap { total, .. } => Some(total.lower_bound()),
        CapacityRule::Separate { input, .. } => Some(input.lower_bound()),
        _ => None,
    }
}

/// Working envelope: the tighter of reported window and admitted input.
pub(in crate::runtime) fn compaction_fence_base_tokens(rule: CapacityRule) -> Option<u64> {
    let window = total_context_window_tokens(rule)?;
    let ceiling = match rule {
        CapacityRule::ProviderReservedInput {
            maximum_projected_input,
            ..
        } => maximum_projected_input,
        _ => window,
    };
    Some(window.min(ceiling).max(1))
}

pub(in crate::runtime) fn occupancy_gauge_percent(occupied: u64, total_window: u64) -> Option<u8> {
    if total_window == 0 {
        return None;
    }
    let occupied = u128::from(occupied);
    let window = u128::from(total_window);
    u8::try_from(((occupied * 100 + window / 2) / window).min(100)).ok()
}

/// Occupied tokens at the tool-group boundary: the context measure of the
/// post-tool live head, whose whole-request `estimate` also counts committed
/// nested local-composition observations that cannot enter canonical history
/// (`local_programmatic` is rejected at the provider wire).
///
/// `None` until a completed round has anchored the measure: an unanchored
/// whole-request estimate never schedules a compaction by itself (spec 01,
/// 10.3), so only an overflow disposition can act before then.
pub(in crate::runtime) fn tool_group_fence_occupied_tokens(
    estimate: Option<u64>,
    anchor: Option<ContextAnchor>,
    nested_committed: u64,
) -> Option<u64> {
    let anchor = anchor?;
    let estimate = estimate?.saturating_add(nested_committed);
    Some(context_occupancy_tokens(estimate, Some(anchor)))
}

/// Re-anchors on a completed model round's provider input. A round without a
/// provider count or a request estimate keeps the previous anchor: the growth
/// since that older request is still the estimate's difference.
pub(in crate::runtime) fn apply_completed_model_round_occupancy(
    anchor: &mut Option<ContextAnchor>,
    nested_committed: &mut u64,
    provider_input: Option<u64>,
    request_estimate: Option<u64>,
) {
    let Some(next) = provider_input
        .zip(request_estimate)
        .and_then(|(provider, estimate)| ContextAnchor::new(provider, estimate))
    else {
        return;
    };
    *anchor = Some(next);
    *nested_committed = 0;
}

/// Whether the tool-group boundary should schedule a compact: on an overflow
/// disposition, on exhausted observation headroom, or once the anchored
/// occupancy reaches the compaction trigger. The ladder decides which rung acts.
pub(in crate::runtime) fn should_schedule_mid_turn_compaction_with_trigger(
    kind: &ContextStatusKind,
    occupied: Option<u64>,
    trigger: Option<u64>,
) -> bool {
    matches!(
        kind,
        ContextStatusKind::WouldOverflow(_) | ContextStatusKind::ProtectedOverflow(_)
    ) || matches!(kind, ContextStatusKind::Fits { headroom }
        if observation_bytes_from_headroom(*headroom) == 0)
        || (matches!(kind, ContextStatusKind::Fits { .. })
            && match (occupied, trigger) {
                (Some(occupied), Some(trigger)) => occupied >= trigger,
                _ => false,
            })
}

/// The harness summary's generation limits: the turn's bound narrowed to
/// the compaction bound for its reasoning, with no stop sequences.
fn compaction_generation_limits(
    turn: &GenerationLimits,
    reasoning: ReasoningPolicy,
) -> Result<GenerationLimits, AgentError> {
    let bound = semantic_compaction_output_bound(turn.max_output_tokens(), reasoning);
    GenerationLimits::new(Some(bound), Vec::new()).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "compaction output limit is invalid",
        )
    })
}

pub(in crate::runtime) fn harness_compaction_prefix(
    source: &StableRequestPrefix,
    selected: SelectedProviderConfigurationId,
    effective: EffectiveProviderConfigurationId,
) -> Result<StableRequestPrefix, AgentError> {
    let limits = compaction_generation_limits(source.limits(), source.reasoning())?;
    // same system/prefix as the live turn (cache affinity).
    // The compaction instruction is only the trailing user item.
    let mut prefix = StableRequestPrefix::new(
        selected,
        effective,
        source.processing_tier().clone(),
        source.instructions().to_vec(),
        source.tools().to_vec(),
        source.hosted_tools().to_vec(),
        source.output().clone(),
        source.reasoning(),
        ToolChoice::None,
        source.cache(),
        limits,
        source.native().clone(),
        source.resolved_harness_plan_identity(),
    )
    .and_then(|prefix| {
        prefix.with_context_admission_requirement(source.context_admission_requirement())
    })
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "harness compaction request prefix is invalid",
        )
    })?;
    if let Some(projection) = source.tool_projection_v2() {
        prefix = prefix
            .with_tool_projection_v2(projection.clone())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "harness compaction tool projection is invalid",
                )
            })?;
    }
    Ok(prefix)
}

pub(in crate::runtime) fn native_compaction_dispatch_prefix(
    source: &StableRequestPrefix,
) -> Result<StableRequestPrefix, AgentError> {
    let namespace = NamespaceId::new(NATIVE_COMPACTION_NAMESPACE).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "native compaction namespace is invalid",
        )
    })?;
    let mut entries = BTreeMap::new();
    entries.insert(
        namespace,
        serde_json::Value::String(NATIVE_COMPACTION_TRIGGER_V1.to_owned()),
    );
    let mut prefix = StableRequestPrefix::new(
        source.selected_provider_configuration_id(),
        source.effective_provider_configuration_id(),
        source.processing_tier().clone(),
        source.instructions().to_vec(),
        source.tools().to_vec(),
        source.hosted_tools().to_vec(),
        source.output().clone(),
        source.reasoning(),
        source.tool_choice().clone(),
        source.cache(),
        source.limits().clone(),
        NativeOptions::new(entries),
        source.resolved_harness_plan_identity(),
    )
    .and_then(|prefix| {
        prefix.with_context_admission_requirement(source.context_admission_requirement())
    })
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "native compaction request prefix is invalid",
        )
    })?;
    if let Some(projection) = source.tool_projection_v2() {
        prefix = prefix
            .with_tool_projection_v2(projection.clone())
            .map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "native compaction tool projection is invalid",
                )
            })?;
    }
    Ok(prefix)
}

pub(in crate::runtime) fn harness_compaction_input(
    cut: &SemanticCompactionCut,
    workflow_id: SessionWorkflowId,
) -> Result<ConversationView, AgentError> {
    let entry_id = EntryId::try_from(compaction_uuid(workflow_id, b"compact-user-instruction")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let item = ConversationItem::new(
        entry_id,
        Actor::User,
        vec![ContentBlock::Text(
            TextBlock::new(COMPACTION_INSTRUCTION, None).map_err(|_| {
                AgentError::new(
                    AgentErrorKind::Internal,
                    "compaction user instruction is invalid",
                )
            })?,
        )],
        Provenance::UserInput,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "compaction user instruction item is invalid",
        )
    })?;
    let segment =
        HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item]).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "compaction user instruction segment is invalid",
            )
        })?;
    // sealed cut plus one trailing user on this request, not
    // session working. Reconstruct uses this same function.
    let history = cut.summary_input().history().append(segment).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Internal,
            "compaction user instruction history is invalid",
        )
    })?;
    Ok(ConversationView::from_shared(
        history,
        WorkingProjection::default(),
    ))
}

/// Number of eligible selected-span result bodies, in source order. This is
/// the maximum depth, not the number of inputs (R0 is also a candidate).
pub(in crate::runtime) fn harness_compaction_reduction_count(cut: &SemanticCompactionCut) -> usize {
    collect_compaction_reduction_candidates(cut).len()
}

/// R0 is the original harness input. Rk replaces exactly the first k eligible
/// selected-span bodies; retained history and canonical source are unchanged.
/// This is an offline request transformation, not a placeholder publication.
pub(in crate::runtime) fn harness_compaction_input_at_depth(
    cut: &SemanticCompactionCut,
    workflow_id: SessionWorkflowId,
    depth: usize,
) -> Result<ConversationView, AgentError> {
    let input = harness_compaction_input(cut, workflow_id)?;
    if depth == 0 {
        return Ok(input);
    }
    let candidates = collect_compaction_reduction_candidates(cut);
    let selected = candidates.get(..depth).ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Internal,
            "summary reduction depth is out of range",
        )
    })?;
    let mut history = input.history().clone();
    for candidate in selected {
        history = replace_compaction_result_body(&history, candidate)?;
    }
    Ok(ConversationView::from_shared(
        history,
        WorkingProjection::default(),
    ))
}

/// Validates against the CLOSED source-derived sequence, including R0 for old
/// requests. Never recognizes a reduction by trusting a marker or replacement
/// text. Incremental rewriting avoids rebuilding earlier depths on recovery.
/// No provider measurement or dispatch is performed here.
pub(in crate::runtime) fn validate_harness_compaction_input(
    cut: &SemanticCompactionCut,
    workflow_id: SessionWorkflowId,
    input: &ConversationView,
) -> Result<Option<usize>, AgentError> {
    let mut expected = harness_compaction_input(cut, workflow_id)?;
    if *input == expected {
        return Ok(Some(0));
    }
    for (index, candidate) in collect_compaction_reduction_candidates(cut)
        .iter()
        .enumerate()
    {
        let history = replace_compaction_result_body(expected.history(), candidate)?;
        expected = ConversationView::from_shared(history, WorkingProjection::default());
        if *input == expected {
            return Ok(Some(index + 1));
        }
    }
    Ok(None)
}

pub(in crate::runtime) fn context_status_audit(
    status: &ContextStatus,
) -> Result<ContextEvaluationAudit, AgentError> {
    let measurement = status.measurement().ok_or_else(|| {
        AgentError::new(
            AgentErrorKind::Model,
            "context evaluation did not provide an authoritative measurement",
        )
    })?;
    ContextEvaluationAudit::new(
        measurement.identity(),
        status.identity(),
        measurement.bound().upper_bound(),
    )
    .map_err(runtime_manifest_error)
}

pub(in crate::runtime) fn compact_runtime_endpoint_error(
    error: &AgentError,
) -> AgentSessionEndpointError {
    if error.kind() == AgentErrorKind::OutcomeUnknown {
        return AgentSessionEndpointError::OutcomeUnknown;
    }
    let reason = compact_diagnostic(error);
    compact_runtime_endpoint_reason(reason)
}

pub(in crate::runtime) fn compact_runtime_endpoint_rejected(
    reason: impl fmt::Display,
) -> AgentSessionEndpointError {
    compact_runtime_endpoint_reason(reason.to_string())
}

fn compact_runtime_endpoint_reason(reason: String) -> AgentSessionEndpointError {
    if reason.is_empty() {
        // This helper also handles errors after admission; silence is not a
        // proof that the workflow never committed.
        AgentSessionEndpointError::OutcomeUnknown
    } else {
        AgentSessionEndpointError::Rejected { reason }
    }
}

pub(in crate::runtime) async fn exact_compaction_runtime(
    builder: AgentBuilder,
    checkpoint: &RuntimeCheckpoint,
    _run_id: RunId,
) -> Result<ExactCompactionRuntime, AgentError> {
    validate_durable_builder(&builder)?;
    let _admission = sealed_runtime_admission(&builder).await?;
    let manifest = checkpoint.manifest();
    validate_compaction_provider_configuration(
        &builder.provider_configuration,
        manifest,
        builder.provider_configuration.effective().descriptor(),
    )?;
    if native_compaction_facet_enrolled(&builder.provider_configuration) {
        let descriptor = builder.provider_configuration.effective().descriptor();
        let binding = manifest.binding();
        if builder.instructions.len() > 256
            || binding.protocol() != descriptor.protocol()
            || binding.route() != descriptor.route()
            || binding.requested_model() != descriptor.requested_model()
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "native compaction runtime is not the exact source coding configuration",
            ));
        }
    } else {
        let source = manifest.canonical().stable_prefix();
        let compact_limits = compaction_generation_limits(source.limits(), source.reasoning())?;
        if builder
            .request_policy
            .generation_limits()
            .max_output_tokens()
            != compact_limits.max_output_tokens()
            || builder.request_policy.context_admission_requirement()
                != manifest
                    .canonical()
                    .stable_prefix()
                    .context_admission_requirement()
            || manifest.binding().model_binding()
                != builder
                    .provider_configuration
                    .effective()
                    .descriptor()
                    .semantic_binding_digest()
                    .map_err(|_| {
                        AgentError::new(AgentErrorKind::InvalidInput, "model binding failed")
                    })?
        {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "compaction runtime is not the exact driver-owned operation configuration",
            ));
        }
    }
    crate::recovery_scope::require_recoverable(checkpoint).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "compaction source scope is not recoverable by this build",
        )
    })?;
    Ok(ExactCompactionRuntime {
        model: builder.model,
        provider_configuration: builder.provider_configuration,
        clock: builder.clock,
        model_operation_timeout: builder.config.model_operation_timeout,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "successor audit keeps persisted source and current measurement authority explicit"
)]
pub(super) async fn native_successor_audit(
    model: Arc<dyn LanguageModel>,
    manifest: &RuntimeManifest,
    identity: &DurableCompactionIdentity,
    request: &DurableCompactionRequestRecord,
    current_configuration: &CompiledProviderConfiguration,
    clock: &dyn Clock,
    authority_timeout: Duration,
    successor: &CanonicalHistory,
) -> Result<(ContextEvaluationAudit, Option<u64>), AgentError> {
    let source_context = manifest.canonical().context().retained_for_exact_resume();
    let Some(next_epoch) = source_context.affinity().epoch().checked_add(1) else {
        return unmeasured_successor(successor);
    };
    let Ok(target_affinity) = ModelCacheAffinity::from_runtime_entropy(
        source_context.affinity().session(),
        source_context.affinity().branch(),
        next_epoch,
        request.target_affinity_entropy,
    ) else {
        return unmeasured_successor(successor);
    };
    let Ok(attempt_id) = compaction_attempt_id(identity.workflow_id, b"successor-measurement")
    else {
        return unmeasured_successor(successor);
    };
    let Ok(successor_request) = ModelRequest::from_parts(
        manifest.canonical().stable_prefix().clone(),
        ConversationView::from_shared(successor.clone(), WorkingProjection::default()),
        RequestMetadata::for_session_workflow(
            request.request.metadata().run_id(),
            identity.workflow_id,
            attempt_id,
            request.request.metadata().created_at(),
        ),
    ) else {
        return unmeasured_successor(successor);
    };
    match measure_compaction_successor(
        model,
        manifest,
        current_configuration,
        successor_request,
        target_affinity,
        clock,
        authority_timeout,
    )
    .await
    {
        Ok((status, audit)) => Ok((audit, status.unanchored_occupancy_tokens())),
        Err(_) => unmeasured_successor(successor),
    }
}

fn unmeasured_successor(
    successor: &CanonicalHistory,
) -> Result<(ContextEvaluationAudit, Option<u64>), AgentError> {
    Ok((unmeasured_native_compact_audit(successor.identity())?, None))
}

pub(in crate::runtime) fn unmeasured_native_compact_audit(
    source_projection: Digest,
) -> Result<ContextEvaluationAudit, AgentError> {
    let measurement = Digest::sha256(
        DigestDomain::TokenMeasurement,
        2,
        &[
            b"plakous-agent/native-compact-unmeasured-v1",
            source_projection.bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "native compact unmeasured audit failed",
        )
    })?;
    let status = Digest::sha256(
        DigestDomain::ContextAdmission,
        2,
        &[
            b"plakous-agent/native-compact-unmeasured-status-v1",
            source_projection.bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "native compact unmeasured audit failed",
        )
    })?;
    ContextEvaluationAudit::new(measurement, status, 0).map_err(runtime_manifest_error)
}

pub(super) fn validate_compaction_provider_configuration(
    configuration: &CompiledProviderConfiguration,
    source: &RuntimeManifest,
    descriptor: &plakous_model::ResolvedModelDescriptor,
) -> Result<(), AgentError> {
    if configuration.effective().descriptor() != descriptor {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "compaction provider configuration is not exact for the source operation",
        ));
    }
    compaction_runtime_matches_source_operation(
        configuration,
        source.provider_configuration(),
        source.durability(),
        source
            .canonical()
            .stable_prefix()
            .context_admission_requirement(),
    )
}

pub(in crate::runtime) fn compaction_runtime_matches_source_operation(
    configuration: &CompiledProviderConfiguration,
    source_configuration: &CompiledProviderConfiguration,
    source_durability: RuntimeDurability,
    source_context_requirement: ContextAdmissionRequirement,
) -> Result<(), AgentError> {
    let behavior = configuration.effective().behavior();
    let expected_durability = match source_durability {
        RuntimeDurability::Required => LocalDurabilityDisposition::DurableRequired,
        RuntimeDurability::BestEffort => LocalDurabilityDisposition::BestEffort,
        RuntimeDurability::Disabled => LocalDurabilityDisposition::Ephemeral,
    };
    let same_route = configuration.selected().route() == source_configuration.selected().route();
    let native = native_compaction_facet_enrolled(configuration)
        && same_route
        && behavior.local_durability == expected_durability;
    // Historical observation timestamps may refresh without changing the selected
    // operation. Current descriptor freshness is admitted before this comparison.
    let same_semantic_descriptor = configuration
        .effective()
        .descriptor()
        .semantic_binding_digest()
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error.to_string()))?
        == source_configuration
            .effective()
            .descriptor()
            .semantic_binding_digest()
            .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error.to_string()))?;
    let harness = same_route
        && configuration.selected_identity() == source_configuration.selected_identity()
        && configuration.effective_identity() == source_configuration.effective_identity()
        && same_semantic_descriptor
        && configuration.effective().authority() == source_configuration.effective().authority()
        && behavior.local_durability == expected_durability
        && behavior.context_evidence_requirement == source_context_requirement;
    if !(native || harness) {
        return Err(AgentError::new(
            AgentErrorKind::InvalidInput,
            "compaction provider configuration is not exact for the source operation",
        ));
    }
    Ok(())
}
