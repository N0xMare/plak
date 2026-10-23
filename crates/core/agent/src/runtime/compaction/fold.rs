//! Pure successor folds and bounded successor measurement.

use crate::runtime::OperationId;
use crate::runtime::admission::validate_configuration_runtime_authority;
use crate::runtime::compaction::continuation::{is_covered_harness_note, is_user_intent};
use crate::runtime::compaction::record::{
    compaction_attempt_id, compaction_operation_component, compaction_uuid,
};
use crate::runtime::compaction::request::{
    context_status_audit, harness_compaction_prefix, native_successor_audit,
    validate_compaction_provider_configuration, validate_harness_compaction_input,
};
use crate::runtime::compaction::{
    CompactionCompletionError, CompactionSuccessFold, DurableCompactionIdentity,
    DurableCompactionPublicationEvidence, DurableCompactionRequestRecord,
    NATIVE_COMPACTION_RETAINED_MESSAGE_TOKEN_BUDGET,
};
use crate::runtime::continuation::{runtime_component, runtime_manifest_error};
use crate::runtime::driver::model_cleanup::ambiguous_compaction_error;
use crate::runtime::driver::turn_position::summarized_turns;
use crate::{
    AgentError, AgentErrorKind, ContextEvaluationAudit, DurableOperationState,
    DurableOperationTransition, ReconstructiveComponent, RuntimeComponentKind, RuntimeManifest,
    RuntimeOperationLedger, SemanticCompactionPublication,
};
use plakous_model::{
    Actor, CanonicalHistory, Clock, CompactionBlock, CompiledProviderConfiguration, ContentBlock,
    ContextHistoryPlan, ContextStatus, ContextStatusKind, ConversationItem, ConversationView,
    Digest, DigestDomain, EntryId, FinishReason, HistorySegment, HistorySegmentKind, LanguageModel,
    ModelCacheAffinity, ModelError, ModelErrorKind, ModelPreparationContext, ModelPreparationError,
    ModelRequest, ModelResponse, OpaqueFallback, PreparedCallRecord, Provenance, RequestMetadata,
    SemanticCompactionCut, ToolResultPart, ValidationMode, WorkingProjection,
};
use std::sync::Arc;
use std::time::Duration;
pub(in crate::runtime) fn compaction_summary_segment(
    identity: DurableCompactionIdentity,
    response: &ModelResponse,
) -> Result<HistorySegment, AgentError> {
    if let Some(segment) = native_compaction_segment(identity, response)? {
        return Ok(segment);
    }
    // A summary cut off at its output bound silently loses whatever it would
    // have said last, typically the open work and the next step. Publishing it
    // would replace the history with less than it claims to hold.
    if response.finish().reason() == FinishReason::Length {
        return Err(AgentError::new(
            AgentErrorKind::Model,
            "compaction summary was cut off at its output bound",
        ));
    }
    if response.finish().reason() != FinishReason::Stop || response.output().is_empty() {
        return Err(AgentError::new(
            AgentErrorKind::Model,
            "compaction provider returned a non-summary terminal response",
        ));
    }
    // Coding-runtime compact keeps reasoning enabled. Chat-family
    // harness compact may emit a reasoning item plus a text item; fold every
    // assistant text block and ignore reasoning/citation.
    let mut summary = String::new();
    for item in response.output() {
        if item.actor() != Actor::Assistant {
            return Err(AgentError::new(
                AgentErrorKind::Model,
                "compaction provider returned a non-summary terminal response",
            ));
        }
        for block in item.blocks() {
            match block {
                ContentBlock::Text(text) => {
                    if !summary.is_empty() {
                        summary.push('\n');
                    }
                    summary.push_str(text.text());
                }
                ContentBlock::Refusal(refusal) => {
                    if !summary.is_empty() {
                        summary.push('\n');
                    }
                    summary.push_str(refusal.text());
                }
                ContentBlock::Compaction(compaction) => {
                    if !summary.is_empty() {
                        summary.push('\n');
                    }
                    summary.push_str(compaction.summary());
                }
                ContentBlock::Opaque(opaque) => match opaque.fallback() {
                    OpaqueFallback::SemanticSummary(text) => {
                        if !summary.is_empty() {
                            summary.push('\n');
                        }
                        summary.push_str(text);
                    }
                    OpaqueFallback::Omit | OpaqueFallback::CompatibleReplay => {}
                    _ => {
                        return Err(AgentError::new(
                            AgentErrorKind::Model,
                            "compaction provider output must be summary text",
                        ));
                    }
                },
                ContentBlock::Reasoning(_) | ContentBlock::Citation(_) => {}
                _ => {
                    return Err(AgentError::new(
                        AgentErrorKind::Model,
                        "compaction provider output must be summary text",
                    ));
                }
            }
        }
    }
    if summary.is_empty() {
        return Err(AgentError::new(
            AgentErrorKind::Model,
            "compaction provider output must include summary text",
        ));
    }
    let entry_id = EntryId::try_from(compaction_uuid(identity.workflow_id, b"summary-entry")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let item = ConversationItem::new(
        entry_id,
        Actor::Assistant,
        vec![ContentBlock::Compaction(
            CompactionBlock::new(summary).map_err(|_| {
                AgentError::new(AgentErrorKind::Model, "compaction summary is invalid")
            })?,
        )],
        Provenance::ModelOutput,
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Model, "compaction summary is invalid"))?;
    HistorySegment::new(HistorySegmentKind::Compaction, vec![item])
        .map_err(|_| AgentError::new(AgentErrorKind::Model, "compaction summary is invalid"))
}

fn native_compaction_segment(
    identity: DurableCompactionIdentity,
    response: &ModelResponse,
) -> Result<Option<HistorySegment>, AgentError> {
    let mut compaction = None;
    for item in response.output() {
        if item.actor() != Actor::Assistant {
            continue;
        }
        for block in item.blocks() {
            if let ContentBlock::Compaction(block) = block {
                if compaction.is_some() {
                    return Err(AgentError::new(
                        AgentErrorKind::Model,
                        "native compaction returned more than one compaction item",
                    ));
                }
                compaction = Some(block.clone());
            }
        }
    }
    let Some(compaction) = compaction else {
        return Ok(None);
    };
    let entry_id = EntryId::try_from(compaction_uuid(identity.workflow_id, b"native-entry")?)
        .map_err(|error| AgentError::from_identity(&error))?;
    let item = ConversationItem::new(
        entry_id,
        Actor::Assistant,
        vec![ContentBlock::Compaction(compaction)],
        Provenance::ModelOutput,
    )
    .map_err(|_| AgentError::new(AgentErrorKind::Model, "native compaction item is invalid"))?;
    Ok(Some(
        HistorySegment::new(HistorySegmentKind::Compaction, vec![item]).map_err(|_| {
            AgentError::new(AgentErrorKind::Model, "native compaction item is invalid")
        })?,
    ))
}

fn native_compaction_keeps_item(item: &ConversationItem) -> bool {
    matches!(item.actor(), Actor::User | Actor::Developer | Actor::System)
}

fn native_compaction_text_token_upper_bound(text: &str) -> u64 {
    let bytes = u64::try_from(text.len()).unwrap_or(u64::MAX);
    bytes.saturating_add(3) / 4
}

pub(in crate::runtime) fn nested_observation_token_upper_bound(item: &ConversationItem) -> u64 {
    let mut tokens = 0u64;
    for block in item.blocks() {
        tokens = tokens.saturating_add(match block {
            ContentBlock::ToolResult(result) => result
                .parts()
                .iter()
                .map(|part| match part {
                    ToolResultPart::Text(text) => {
                        native_compaction_text_token_upper_bound(text.text())
                    }
                    _ => 1,
                })
                .fold(0, u64::saturating_add),
            ContentBlock::ToolResultUpdate(update) => {
                native_compaction_text_token_upper_bound(update.text().text())
            }
            ContentBlock::Text(text) => native_compaction_text_token_upper_bound(text.text()),
            _ => 1,
        });
    }
    tokens.max(1)
}

pub(super) fn native_compaction_item_token_upper_bound(item: &ConversationItem) -> u64 {
    let mut tokens = 0u64;
    for block in item.blocks() {
        tokens = tokens.saturating_add(match block {
            ContentBlock::Text(text) => {
                native_compaction_text_token_upper_bound(text.text()).max(1)
            }
            _ => 1,
        });
    }
    tokens.max(1)
}

fn native_compaction_segment_token_upper_bound(segment: &HistorySegment) -> u64 {
    segment
        .items()
        .iter()
        .map(native_compaction_item_token_upper_bound)
        .fold(0, u64::saturating_add)
        .max(1)
}

/// Newest-first whole-group keep under the official 64k retained-message budget.
///
/// Older groups that do not fit are omitted from the successor; the source
/// checkpoint is unchanged. An oversized newest group is dropped rather than
/// keeping older groups in its place (official CLI truncates that boundary
/// message; Plak does not split a group).
fn retain_native_compaction_prefix(segments: Vec<HistorySegment>) -> Vec<HistorySegment> {
    let mut remaining = NATIVE_COMPACTION_RETAINED_MESSAGE_TOKEN_BUDGET;
    let mut newest_first = Vec::new();
    for segment in segments.into_iter().rev() {
        if remaining == 0 {
            break;
        }
        let cost = native_compaction_segment_token_upper_bound(&segment);
        if cost > remaining {
            break;
        }
        remaining -= cost;
        newest_first.push(segment);
    }
    newest_first.reverse();
    newest_first
}

/// Records on `summary` the root turns `successor` no longer holds, so the
/// compacted history keeps `source`'s turn position, and rebuilds `successor`
/// around the recorded summary. A source whose position is already unknown
/// leaves both unchanged.
pub(in crate::runtime) fn record_summarized_turns(
    source: &CanonicalHistory,
    summary: HistorySegment,
    successor: CanonicalHistory,
) -> Result<(HistorySegment, CanonicalHistory), AgentError> {
    let Some(turns) = summarized_turns(source, successor.segments().iter()) else {
        return Ok((summary, successor));
    };
    let invalid = || AgentError::new(AgentErrorKind::Model, "compaction summary is invalid");
    let items = summary
        .items()
        .iter()
        .map(|item| {
            let blocks = item
                .blocks()
                .iter()
                .map(|block| match block {
                    ContentBlock::Compaction(compaction) => {
                        ContentBlock::Compaction(compaction.clone().with_summarized_turns(turns))
                    }
                    other => other.clone(),
                })
                .collect();
            ConversationItem::new(item.id(), item.actor(), blocks, item.provenance())
                .map_err(|_| invalid())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let recorded =
        HistorySegment::new(HistorySegmentKind::Compaction, items).map_err(|_| invalid())?;
    let segments = successor
        .segments()
        .iter()
        .map(|segment| {
            if segment.identity() == summary.identity() {
                recorded.clone()
            } else {
                segment.clone()
            }
        })
        .collect();
    let successor = CanonicalHistory::new(segments).map_err(|_| invalid())?;
    Ok((recorded, successor))
}

/// The two successor shapes a native compaction item can publish.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::runtime) enum NativeSuccessorShape {
    /// Retained messages, the compaction item, then the source's protected
    /// current-round tail verbatim.
    WithTail,
    /// Retained messages and the compaction item last.
    WithoutTail,
}

/// Where the source's protected current-round tail starts: the newest tool
/// exchange after the latest user intent (spec 01 §10.1), which the tail keeps
/// with everything after it. `None` when there is no such exchange or the tail
/// would carry a compaction segment.
fn native_protected_tail_start(source: &CanonicalHistory) -> Option<usize> {
    let segments = source.segments();
    let latest_intent = segments
        .iter()
        .enumerate()
        .filter(|(_, segment)| is_user_intent(segment))
        .map(|(position, _)| position)
        .last()?;
    let start = segments
        .iter()
        .enumerate()
        .skip(latest_intent.saturating_add(1))
        .filter(|(_, segment)| segment.kind() == HistorySegmentKind::ToolExchange)
        .map(|(position, _)| position)
        .last()?;
    segments
        .iter()
        .skip(start)
        .all(|segment| segment.kind() != HistorySegmentKind::Compaction)
        .then_some(start)
}

/// Builds one native successor shape from the source and its compaction item.
///
/// Both shapes are pure functions of the source history and the item, so
/// publication and reopen validation derive them identically. `WithTail`
/// returns `None` when the source has no protected current-round tail. No
/// covered harness note before the tail is retained, and none counts against
/// the retained-message budget; process notices are retained. Publication
/// appends the post-compaction and continuation notes after the tail.
pub(in crate::runtime) fn native_compaction_successor(
    source: &CanonicalHistory,
    summary: HistorySegment,
    shape: NativeSuccessorShape,
) -> Result<Option<CanonicalHistory>, AgentError> {
    let tail_start = match shape {
        NativeSuccessorShape::WithTail => {
            let Some(start) = native_protected_tail_start(source) else {
                return Ok(None);
            };
            start
        }
        NativeSuccessorShape::WithoutTail => source.segments().len(),
    };
    let mut segments = Vec::new();
    for segment in source.segments().iter().take(tail_start) {
        if matches!(
            segment.kind(),
            HistorySegmentKind::Compaction
                | HistorySegmentKind::ToolExchange
                | HistorySegmentKind::ReasoningReplay
        ) || is_covered_harness_note(segment)
        {
            continue;
        }
        if segment.items().iter().all(native_compaction_keeps_item) {
            segments.push(segment.clone());
            continue;
        }
        let kept = segment
            .items()
            .iter()
            .filter(|item| native_compaction_keeps_item(item))
            .cloned()
            .collect::<Vec<_>>();
        if kept.is_empty() {
            continue;
        }
        let kind = if kept.iter().all(|item| item.actor() == Actor::User)
            && segment.kind() == HistorySegmentKind::RecentIntent
        {
            HistorySegmentKind::RecentIntent
        } else {
            HistorySegmentKind::Conversation
        };
        segments.push(HistorySegment::new(kind, kept).map_err(|_| {
            AgentError::new(
                AgentErrorKind::Model,
                "native compaction retained prefix is invalid",
            )
        })?);
    }
    let mut segments = retain_native_compaction_prefix(segments);
    segments.push(summary);
    segments.extend(source.segments().iter().skip(tail_start).cloned());
    CanonicalHistory::new(segments).map(Some).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Model,
            "native compaction successor is invalid",
        )
    })
}

pub(super) async fn measure_compaction_successor(
    model: Arc<dyn LanguageModel>,
    source: &RuntimeManifest,
    provider_configuration: &CompiledProviderConfiguration,
    request: ModelRequest,
    affinity: ModelCacheAffinity,
    clock: &dyn Clock,
    authority_timeout: Duration,
) -> Result<(ContextStatus, ContextEvaluationAudit), AgentError> {
    let descriptor = provider_configuration.effective().descriptor().clone();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)
        .map_err(AgentError::from_model)?;
    let checked_at = clock.now();
    let authority = tokio::time::timeout(
        authority_timeout,
        Arc::clone(&model).revalidate_runtime_authority(checked_at),
    )
    .await
    .map_err(|_| {
        AgentError::from_model(ModelError::new(
            ModelErrorKind::Timeout,
            "compaction successor authority revalidation timed out",
        ))
    })?
    .map_err(ModelPreparationError::from)
    .map_err(ModelError::from)
    .map_err(AgentError::from_model)?;
    validate_configuration_runtime_authority(
        provider_configuration,
        &descriptor,
        &authority,
        checked_at,
    )
    .map_err(ModelError::from)
    .map_err(AgentError::from_model)?;
    let mut context =
        ModelPreparationContext::new(authority, checked_at).with_cache_affinity(affinity);
    if let Some(prefix) = crate::runtime::driver::completed_history_prefix(
        source.canonical().history(),
        source.lifecycle(),
        plan.request().input(),
    ) {
        context = context.with_completed_history_prefix(prefix);
    }
    let prepared = match model.prepare_call(plan.clone(), context) {
        Ok(prepared) => prepared,
        // Local admission refusing the successor for capacity is a definite
        // measurement: the successor does not fit, and the caller's fit check
        // turns that into a failed compaction. It is not a publication
        // ambiguity. Any other denial (including one carrying a `Fits` or
        // `Unavailable` status) is an error, so it can never publish.
        Err(error) => {
            let status = match error.context_status() {
                Some(status)
                    if matches!(
                        status.kind(),
                        ContextStatusKind::WouldOverflow(_)
                            | ContextStatusKind::ProtectedOverflow(_)
                    ) =>
                {
                    status.clone()
                }
                _ => return Err(AgentError::from_model(ModelError::from(error))),
            };
            let audit = context_status_audit(&status)?;
            return Ok((status, audit));
        }
    };
    let status = descriptor
        .context_status(&plan, prepared.record().projection(), checked_at)
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Model,
                "compaction successor status is unavailable",
            )
        })?;
    let audit = context_status_audit(&status)?;
    Ok((
        status.with_input_parts(
            prepared
                .record()
                .projection()
                .token_projection()
                .input_parts(),
        ),
        audit,
    ))
}

pub(in crate::runtime) fn compact_diagnostic(error: &AgentError) -> String {
    match error.model_kind() {
        Some(kind) => format!("{kind:?} \"{}\"", error.message()),
        None => error.message().to_owned(),
    }
}

impl CompactionCompletionError {
    pub(in crate::runtime) fn semantic(error: AgentError) -> Self {
        Self::Semantic(error)
    }

    pub(in crate::runtime) fn recovery(error: AgentError) -> Self {
        if error.kind() == AgentErrorKind::OutcomeUnknown {
            Self::RecoveryRequired(error)
        } else {
            Self::RecoveryRequired(AgentError::from_canonical_parts(
                AgentErrorKind::OutcomeUnknown,
                error.message(),
                error.model_error().cloned(),
                error.tool_kind(),
                error.store_error(),
            ))
        }
    }
}

pub(in crate::runtime) fn reconstruct_compaction_cut(
    manifest: &RuntimeManifest,
    identity: DurableCompactionIdentity,
    request: &DurableCompactionRequestRecord,
) -> Result<SemanticCompactionCut, AgentError> {
    let topology = manifest.topology();
    if (topology.session_id, topology.branch_id)
        != (identity.source.session_id, identity.source.branch_id)
        || request.source() != identity.source
        || manifest.canonical().history().identity() != request.source_history
    {
        return Err(ambiguous_compaction_error(
            "compaction completion lost its exact semantic source",
        ));
    }
    let source = manifest.canonical().projection();
    let plan = ContextHistoryPlan::new(&source).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "compaction completion plan is unavailable",
        )
    })?;
    let cut = SemanticCompactionCut::seal(&source, &plan).map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "compaction completion cut is unavailable",
        )
    })?;
    let expected_prefix = harness_compaction_prefix(
        manifest.canonical().stable_prefix(),
        request.provider_configuration.selected_identity(),
        request.provider_configuration.effective_identity(),
    )?;
    if validate_harness_compaction_input(&cut, identity.workflow_id, request.request.input())?
        .is_none()
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "persisted compaction input is not an exact source-derived reduction",
        ));
    }
    let exact_request = ModelRequest::from_parts(
        expected_prefix.clone(),
        request.request.input().clone(),
        request.request.metadata(),
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Session,
            "persisted compaction request cannot be reconstructed",
        )
    })?;
    validate_compaction_provider_configuration(
        &request.provider_configuration,
        manifest,
        request.provider_configuration.effective().descriptor(),
    )?;
    if request.request.selected_provider_configuration_id()
        != request.provider_configuration.selected_identity()
        || request.request.effective_provider_configuration_id()
            != request.provider_configuration.effective_identity()
        || cut.identity() != request.cut
        || cut.source_history_identity() != request.source_history
        || request.request.stable_prefix().canonical_bytes() != expected_prefix.canonical_bytes()
        || request.request.identity() != exact_request.identity()
        || request.request.canonical_bytes() != exact_request.canonical_bytes()
        || manifest
            .canonical()
            .stable_prefix()
            .resolved_harness_plan_identity()
            != Some(request.source_harness_plan)
    {
        return Err(AgentError::new(
            AgentErrorKind::Session,
            "persisted compaction request disagrees with its exact sealed cut",
        ));
    }
    Ok(cut)
}

fn completed_provider_evidence(
    ledger: &RuntimeOperationLedger,
    operation: OperationId,
    response: &ModelResponse,
) -> Result<ReconstructiveComponent, AgentError> {
    let expected = compaction_operation_component(response)?;
    let actual = ledger
        .operation(operation.0)
        .and_then(|candidate| match candidate.state() {
            DurableOperationState::Completed(evidence) => Some(evidence.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            ambiguous_compaction_error("compaction completion lacks proven provider evidence")
        })?;
    if actual != expected {
        return Err(ambiguous_compaction_error(
            "compaction completion response contradicts durable provider evidence",
        ));
    }
    Ok(actual)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the pure fold consumes the closed persisted compaction authority tuple"
)]
pub(super) fn fold_compaction_success(
    manifest: &RuntimeManifest,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    cut: &SemanticCompactionCut,
    summary: &HistorySegment,
    successor: CanonicalHistory,
    response: &ModelResponse,
    before: ContextEvaluationAudit,
    after: ContextEvaluationAudit,
) -> Result<CompactionSuccessFold, AgentError> {
    let provider_evidence = completed_provider_evidence(manifest.ledger(), provider, response)?;
    let compaction_evidence_before_publication =
        completed_provider_evidence(manifest.ledger(), compaction, response)?;
    if provider_evidence != compaction_evidence_before_publication {
        return Err(ambiguous_compaction_error(
            "compaction and provider completion evidence disagree",
        ));
    }
    let operation = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/session-compaction-publication-v1",
            identity.workflow_id.as_bytes(),
            &cut.identity().identity_bytes(),
            &response.plan_digest().identity_bytes(),
            &summary.identity().identity_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "compaction publication identity failed",
        )
    })?;
    let source_context = manifest.canonical().context().retained_for_exact_resume();
    let publication = SemanticCompactionPublication::new(
        request.source_checkpoint,
        operation,
        manifest
            .binding()
            .route_binding_identity()
            .map_err(runtime_manifest_error)?,
        manifest.binding().model_binding(),
        request.source_harness_plan,
        before,
        after,
        response.usage(),
    )
    .map_err(runtime_manifest_error)?;
    let context = source_context
        .successful_semantic_compaction(
            &publication,
            cut,
            summary,
            &successor,
            request.target_affinity_entropy,
        )
        .map_err(runtime_manifest_error)?;
    let compaction_evidence = runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&DurableCompactionPublicationEvidence {
            version: 1,
            workflow_id: identity.workflow_id,
            source_checkpoint: request.source_checkpoint,
            cut: cut.identity(),
            operation,
            successor_history: successor.identity(),
            after_measurement: after.measurement_identity(),
            after_status: after.status_identity(),
            after_input_tokens: after.input_tokens(),
        })
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "compaction publication evidence could not be encoded",
            )
        })?,
    )?;
    let mut ledger = manifest.ledger().clone();
    for (operation_id, evidence) in [
        (compaction, compaction_evidence),
        (provider, provider_evidence),
    ] {
        ledger = ledger
            .transition_operation(
                operation_id.0,
                DurableOperationTransition::new(
                    identity.success_checkpoint,
                    DurableOperationState::Finalized(evidence),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    Ok(CompactionSuccessFold {
        history: successor,
        ledger,
        context,
        occupancy_tokens_after: None,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "native publication consumes the same closed persisted compaction tuple as semantic fold"
)]
pub(super) fn fold_native_compaction_success(
    manifest: &RuntimeManifest,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    summary: &HistorySegment,
    successor: CanonicalHistory,
    response: &ModelResponse,
    after: ContextEvaluationAudit,
) -> Result<CompactionSuccessFold, AgentError> {
    let provider_evidence = completed_provider_evidence(manifest.ledger(), provider, response)?;
    let compaction_evidence_before_publication =
        completed_provider_evidence(manifest.ledger(), compaction, response)?;
    if provider_evidence != compaction_evidence_before_publication {
        return Err(ambiguous_compaction_error(
            "compaction and provider completion evidence disagree",
        ));
    }
    let operation = Digest::sha256(
        DigestDomain::ProviderOperation,
        3,
        &[
            b"plakous-agent/session-native-compaction-publication-v1",
            identity.workflow_id.as_bytes(),
            &request.cut.identity_bytes(),
            &response.plan_digest().identity_bytes(),
            &summary.identity().identity_bytes(),
        ],
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::Identity,
            "native compaction publication identity failed",
        )
    })?;
    let source_context = manifest.canonical().context().retained_for_exact_resume();
    let publication = SemanticCompactionPublication::new(
        request.source_checkpoint,
        operation,
        manifest
            .binding()
            .route_binding_identity()
            .map_err(runtime_manifest_error)?,
        manifest.binding().model_binding(),
        request.source_harness_plan,
        request.before_audit()?,
        after,
        response.usage(),
    )
    .map_err(runtime_manifest_error)?;
    let context = source_context
        .successful_native_compaction(
            &publication,
            summary,
            &successor,
            request.source_history,
            request.cut,
            request.target_affinity_entropy,
            manifest.canonical().history().segments().len(),
        )
        .map_err(runtime_manifest_error)?;
    let compaction_evidence = runtime_component(
        RuntimeComponentKind::Operation,
        serde_json::to_vec(&DurableCompactionPublicationEvidence {
            version: 1,
            workflow_id: identity.workflow_id,
            source_checkpoint: request.source_checkpoint,
            cut: request.cut,
            operation,
            successor_history: successor.identity(),
            after_measurement: after.measurement_identity(),
            after_status: after.status_identity(),
            after_input_tokens: after.input_tokens(),
        })
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::Internal,
                "native compaction publication evidence could not be encoded",
            )
        })?,
    )?;
    let mut ledger = manifest.ledger().clone();
    for (operation_id, evidence) in [
        (compaction, compaction_evidence),
        (provider, provider_evidence),
    ] {
        ledger = ledger
            .transition_operation(
                operation_id.0,
                DurableOperationTransition::new(
                    identity.success_checkpoint,
                    DurableOperationState::Finalized(evidence),
                ),
            )
            .map_err(runtime_manifest_error)?;
    }
    Ok(CompactionSuccessFold {
        history: successor,
        ledger,
        context,
        occupancy_tokens_after: None,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "live and restart completion share the same exact persisted authority tuple"
)]
pub(in crate::runtime) async fn build_compaction_success_fold(
    manifest: &RuntimeManifest,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    prepared: &PreparedCallRecord,
    response: &ModelResponse,
    model: Arc<dyn LanguageModel>,
    current_configuration: &CompiledProviderConfiguration,
    clock: &dyn Clock,
    authority_timeout: Duration,
) -> Result<CompactionSuccessFold, CompactionCompletionError> {
    if response.plan_digest() != prepared.projection().plan() {
        return Err(CompactionCompletionError::recovery(
            ambiguous_compaction_error(
                "compaction response disagrees with its exact prepared plan",
            ),
        ));
    }
    if request.version >= 3 {
        return build_native_success_fold(
            manifest,
            identity,
            compaction,
            provider,
            request,
            response,
            model,
            current_configuration,
            clock,
            authority_timeout,
        )
        .await;
    }
    let cut = reconstruct_compaction_cut(manifest, identity, request)
        .map_err(CompactionCompletionError::recovery)?;
    let summary = compaction_summary_segment(identity, response)
        .map_err(CompactionCompletionError::semantic)?;
    let successor = cut.build_successor(&summary).map_err(|_| {
        CompactionCompletionError::semantic(AgentError::new(
            AgentErrorKind::Model,
            "compaction summary cannot build its exact successor",
        ))
    })?;
    let (summary, successor) =
        record_summarized_turns(manifest.canonical().history(), summary, successor)
            .map_err(CompactionCompletionError::semantic)?;
    let source_context = manifest.canonical().context().retained_for_exact_resume();
    let next_epoch = source_context
        .affinity()
        .epoch()
        .checked_add(1)
        .ok_or_else(|| {
            CompactionCompletionError::semantic(AgentError::new(
                AgentErrorKind::Session,
                "cache epoch overflowed",
            ))
        })?;
    let target_affinity = ModelCacheAffinity::from_runtime_entropy(
        source_context.affinity().session(),
        source_context.affinity().branch(),
        next_epoch,
        request.target_affinity_entropy,
    )
    .map_err(|_| {
        CompactionCompletionError::recovery(AgentError::new(
            AgentErrorKind::Session,
            "cache affinity rotation failed",
        ))
    })?;
    let successor_request = ModelRequest::from_parts(
        manifest.canonical().stable_prefix().clone(),
        ConversationView::from_shared(successor.clone(), WorkingProjection::default()),
        RequestMetadata::for_session_workflow(
            request.request.metadata().run_id(),
            identity.workflow_id,
            compaction_attempt_id(identity.workflow_id, b"successor-measurement")
                .map_err(CompactionCompletionError::recovery)?,
            request.request.metadata().created_at(),
        ),
    )
    .map_err(|_| {
        CompactionCompletionError::recovery(AgentError::new(
            AgentErrorKind::Model,
            "compaction successor request is invalid",
        ))
    })?;
    let (status, after) = measure_compaction_successor(
        Arc::clone(&model),
        manifest,
        current_configuration,
        successor_request,
        target_affinity.clone(),
        clock,
        authority_timeout,
    )
    .await
    .map_err(CompactionCompletionError::recovery)?;
    if !matches!(status.kind(), ContextStatusKind::Fits { .. }) {
        return Err(CompactionCompletionError::semantic(AgentError::new(
            AgentErrorKind::Model,
            "compaction successor does not independently fit",
        )));
    }
    // The admission-time before and this successor audit are not one basis
    // (anchor, prefix, counter). Re-measure the source with the same call
    // the successor just used, and publish that audit as the provenance
    // before so the proof and the logged figures name the same numbers.
    let source_request = ModelRequest::from_parts(
        manifest.canonical().stable_prefix().clone(),
        ConversationView::from_shared(
            manifest.canonical().history().clone(),
            WorkingProjection::default(),
        ),
        RequestMetadata::for_session_workflow(
            request.request.metadata().run_id(),
            identity.workflow_id,
            compaction_attempt_id(identity.workflow_id, b"source-proof-measurement")
                .map_err(CompactionCompletionError::recovery)?,
            request.request.metadata().created_at(),
        ),
    )
    .map_err(|_| {
        CompactionCompletionError::recovery(AgentError::new(
            AgentErrorKind::Model,
            "compaction source proof request is invalid",
        ))
    })?;
    // Classified like the successor measurement above: a capacity denial
    // is a measurement, anything else (authority, infrastructure) fences
    // recovery rather than discarding a paid response as failed.
    let (_source_status, proof_before) = measure_compaction_successor(
        model,
        manifest,
        current_configuration,
        source_request,
        target_affinity,
        clock,
        authority_timeout,
    )
    .await
    .map_err(CompactionCompletionError::recovery)?;
    // A source that overflows on this basis (a capacity denial carries
    // its audit) is above the capacity the successor Fits in, so the
    // strict comparison holds for it as well; reopen checks the same rule.
    if after.input_tokens() >= proof_before.input_tokens() {
        return Err(CompactionCompletionError::semantic(AgentError::new(
            AgentErrorKind::Model,
            "compaction successor did not prove net input shrink",
        )));
    }
    let fold = fold_compaction_success(
        manifest,
        identity,
        compaction,
        provider,
        request,
        &cut,
        &summary,
        successor,
        response,
        proof_before,
        after,
    )
    .map_err(CompactionCompletionError::recovery)?;
    Ok(CompactionSuccessFold {
        occupancy_tokens_after: status.unanchored_occupancy_tokens(),
        ..fold
    })
}

/// Publishes the first acceptable native successor shape from one compaction
/// item: the retained current-round tail when its successor is measurable,
/// Fits and strictly shrinks; otherwise the tail-less successor under today's
/// measurable strict-shrink rule. Both candidates are measured offline; the
/// paid dispatch already happened once.
#[allow(
    clippy::too_many_arguments,
    reason = "native completion consumes the same exact persisted authority tuple"
)]
async fn build_native_success_fold(
    manifest: &RuntimeManifest,
    identity: DurableCompactionIdentity,
    compaction: OperationId,
    provider: OperationId,
    request: &DurableCompactionRequestRecord,
    response: &ModelResponse,
    model: Arc<dyn LanguageModel>,
    current_configuration: &CompiledProviderConfiguration,
    clock: &dyn Clock,
    authority_timeout: Duration,
) -> Result<CompactionSuccessFold, CompactionCompletionError> {
    let source = manifest.canonical().history();
    let item = compaction_summary_segment(identity, response)
        .map_err(CompactionCompletionError::semantic)?;
    let mut selected = None;
    for (shape, attempt_role) in [
        (
            NativeSuccessorShape::WithTail,
            &b"successor-tail-measurement"[..],
        ),
        (
            NativeSuccessorShape::WithoutTail,
            &b"successor-measurement"[..],
        ),
    ] {
        let Some(successor) = native_compaction_successor(source, item.clone(), shape)
            .map_err(CompactionCompletionError::semantic)?
        else {
            continue;
        };
        let (summary, successor) = record_summarized_turns(source, item.clone(), successor)
            .map_err(CompactionCompletionError::semantic)?;
        let audit = native_successor_audit(
            Arc::clone(&model),
            manifest,
            &identity,
            request,
            current_configuration,
            clock,
            authority_timeout,
            &successor,
            attempt_role,
        )
        .await
        .map_err(CompactionCompletionError::semantic)?;
        let shrinks =
            audit.occupancy.is_some() && audit.after.input_tokens() < request.before_input_tokens;
        let acceptable = match shape {
            NativeSuccessorShape::WithTail => shrinks && audit.fits,
            NativeSuccessorShape::WithoutTail => shrinks,
        };
        if acceptable {
            selected = Some((summary, successor, audit));
            break;
        }
    }
    let Some((summary, successor, audit)) = selected else {
        return Err(CompactionCompletionError::semantic(AgentError::new(
            AgentErrorKind::Model,
            "compaction successor did not prove net input shrink",
        )));
    };
    let fold = fold_native_compaction_success(
        manifest,
        identity,
        compaction,
        provider,
        request,
        &summary,
        successor,
        response,
        audit.after,
    )
    .map_err(|error| {
        if error.kind() == AgentErrorKind::OutcomeUnknown {
            CompactionCompletionError::recovery(error)
        } else {
            CompactionCompletionError::semantic(error)
        }
    })?;
    Ok(CompactionSuccessFold {
        occupancy_tokens_after: audit.occupancy,
        ..fold
    })
}
