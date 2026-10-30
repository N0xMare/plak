use super::*;

pub(super) fn runtime_context_transition_identity(
    transition: &RuntimeContextTransition,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_runtime_context_transition_fields(&mut writer, transition)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-context-transition-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

pub(in crate::manifest) fn compaction_provenance_identity(
    provenance: &CompactionProvenance,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    write_compaction_provenance_fields(&mut writer, provenance)?;
    Digest::sha256(
        DigestDomain::ContextBudget,
        2,
        &[
            b"plakous-agent/runtime-compaction-provenance-v2",
            &writer.bytes,
        ],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

pub(super) fn runtime_context_state_identity(
    state: &RuntimeContextState,
) -> Result<Digest, RuntimeManifestError> {
    let mut writer = Writer::default();
    writer.id(state.affinity.session().as_uuid());
    writer.id(state.affinity.branch().as_uuid());
    writer.u64(state.affinity.epoch());
    writer.digest(state.affinity.identity())?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(compaction) = &state.compaction {
        writer.digest(compaction.identity)?;
    }
    Digest::sha256(
        DigestDomain::ContextBudget,
        3,
        &[b"plakous-agent/runtime-context-state-v3", &writer.bytes],
    )
    .map_err(|_| RuntimeManifestError::InvalidDigest)
}

fn write_request_anchor(
    writer: &mut Writer,
    anchor: Option<&plakous_model::ContextRequestAnchor>,
) -> Result<(), RuntimeManifestError> {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        let bytes =
            serde_json::to_vec(anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?;
        writer.raw(&bytes, MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    }
    Ok(())
}

fn read_request_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<plakous_model::ContextRequestAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let bytes = reader.raw(MAX_RUNTIME_CONTEXT_STATE_BYTES)?;
    let anchor: plakous_model::ContextRequestAnchor =
        serde_json::from_slice(&bytes).map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&anchor).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    Ok(Some(anchor))
}

fn write_occupancy_anchor(writer: &mut Writer, anchor: Option<ContextAnchor>) {
    writer.optional(anchor.is_some());
    if let Some(anchor) = anchor {
        writer.u64(anchor.provider_input_tokens());
        writer.u64(anchor.request_estimate_tokens());
    }
}

fn read_occupancy_anchor(
    reader: &mut Reader<'_>,
) -> Result<Option<ContextAnchor>, RuntimeManifestError> {
    if !reader.optional()? {
        return Ok(None);
    }
    let provider_input_tokens = reader.u64()?;
    let request_estimate_tokens = reader.u64()?;
    ContextAnchor::new(provider_input_tokens, request_estimate_tokens)
        .map(Some)
        .ok_or(RuntimeManifestError::InvalidContextState)
}

fn write_runtime_context_transition_fields(
    writer: &mut Writer,
    transition: &RuntimeContextTransition,
) -> Result<(), RuntimeManifestError> {
    writer.u8(transition.kind.tag());
    writer.id(transition.source_session.as_uuid());
    writer.id(transition.source_branch.as_uuid());
    writer.id(transition.source_checkpoint.as_uuid());
    writer.digest(transition.source_context)?;
    writer.digest(transition.source_affinity)?;
    writer.u64(transition.source_epoch);
    writer.id(transition.target_session.as_uuid());
    writer.id(transition.target_branch.as_uuid());
    writer.digest(transition.target_affinity)?;
    writer.u64(transition.target_epoch);
    Ok(())
}

fn write_context_evaluation_audit(
    writer: &mut Writer,
    audit: ContextEvaluationAudit,
) -> Result<(), RuntimeManifestError> {
    writer.digest(audit.measurement)?;
    writer.digest(audit.status)?;
    writer.u64(audit.input_tokens);
    Ok(())
}

pub(in crate::manifest) fn write_compaction_provenance_fields(
    writer: &mut Writer,
    provenance: &CompactionProvenance,
) -> Result<(), RuntimeManifestError> {
    writer.id(provenance.source_checkpoint.as_uuid());
    writer.digest(provenance.source_history)?;
    writer.digest(provenance.operation)?;
    writer.digest(provenance.route_binding)?;
    writer.digest(provenance.model_binding)?;
    writer.digest(provenance.harness_plan)?;
    writer.digest(provenance.selection_plan)?;
    writer.u8(provenance.strategy.tag());
    writer.u32(provenance.strategy_revision);
    writer.digest(provenance.source_projection)?;
    writer.u32(provenance.selected_start);
    writer.u32(provenance.selected_end);
    writer.u32(provenance.selected_group_count);
    writer.u32(provenance.selected_segment_count);
    writer.digest(provenance.selected_identity)?;
    writer.u32(provenance.retained_segment_count);
    writer.u32(provenance.trailing_segment_count);
    writer.digest(provenance.retained_tail_identity)?;
    writer.digest(provenance.target_history)?;
    writer.u32(provenance.target_segment_count);
    writer.digest(provenance.target_projection)?;
    write_context_evaluation_audit(writer, provenance.before)?;
    write_context_evaluation_audit(writer, provenance.after)?;
    writer.digest(provenance.summary_segment)?;
    writer.optional(provenance.usage.is_some());
    if let Some(usage) = provenance.usage {
        writer.u64(usage.input_tokens());
        writer.u64(usage.output_tokens());
        writer.u64(usage.reasoning_tokens());
        writer.u64(usage.cached_input_tokens());
        writer.u64(usage.cache_write_tokens());
    }
    writer.digest(provenance.invalidation.source_affinity)?;
    writer.u64(provenance.invalidation.source_epoch);
    writer.digest(provenance.invalidation.target_affinity)?;
    writer.u64(provenance.invalidation.target_epoch);
    Ok(())
}

pub(super) fn encode_runtime_context_state(
    state: &RuntimeContextState,
) -> Result<Vec<u8>, RuntimeManifestError> {
    state.validate_shape()?;
    let mut writer = Writer::default();
    let affinity = serde_json::to_vec(&state.affinity)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    writer.raw(&affinity, 4 * 1024)?;
    write_occupancy_anchor(&mut writer, state.occupancy_anchor);
    write_request_anchor(&mut writer, state.request_anchor.as_ref())?;
    writer.optional(state.transition.is_some());
    if let Some(transition) = &state.transition {
        write_runtime_context_transition_fields(&mut writer, transition)?;
        writer.digest(transition.identity)?;
    }
    writer.optional(state.compaction.is_some());
    if let Some(provenance) = &state.compaction {
        write_compaction_provenance_fields(&mut writer, provenance)?;
        writer.digest(provenance.identity)?;
    }
    writer.digest(state.identity)?;
    let integrity = root_integrity(b"runtime-context-state", &writer.bytes)?;
    encode_envelope(
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        &writer.bytes,
        integrity,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
    )
}

pub(super) fn decode_runtime_context_state(
    bytes: &[u8],
) -> Result<RuntimeContextState, RuntimeManifestError> {
    let body = decode_envelope(
        bytes,
        CONTEXT_STATE_MAGIC,
        RUNTIME_CONTEXT_STATE_CODEC_REVISION,
        MAX_RUNTIME_CONTEXT_STATE_BYTES,
        b"runtime-context-state",
    )?;
    let mut reader = Reader::new(body);
    let affinity_bytes = reader.raw(4 * 1024)?;
    let affinity: ModelCacheAffinity = serde_json::from_slice(&affinity_bytes)
        .map_err(|_| RuntimeManifestError::InvalidContextState)?;
    if serde_json::to_vec(&affinity).map_err(|_| RuntimeManifestError::InvalidContextState)?
        != affinity_bytes.as_ref()
    {
        return Err(RuntimeManifestError::InvalidContextState);
    }
    let occupancy_anchor = read_occupancy_anchor(&mut reader)?;
    let request_anchor = read_request_anchor(&mut reader)?;
    let transition = reader
        .optional()?
        .then(|| read_runtime_context_transition(&mut reader))
        .transpose()?;
    let compaction = reader
        .optional()?
        .then(|| read_compaction_provenance(&mut reader))
        .transpose()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    reader.finish()?;
    RuntimeContextState::from_recorded(
        affinity,
        occupancy_anchor,
        request_anchor,
        transition,
        compaction,
        identity,
    )
}

fn read_runtime_context_transition(
    reader: &mut Reader<'_>,
) -> Result<RuntimeContextTransition, RuntimeManifestError> {
    let kind = RuntimeContextTransitionKind::from_tag(reader.u8()?)?;
    let source_session = reader.session_id()?;
    let source_branch = reader.branch_id()?;
    let source_checkpoint = reader.checkpoint_id()?;
    let source_context = reader.digest(Some(DigestDomain::ContextBudget))?;
    let source_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let source_epoch = reader.u64()?;
    let target_session = reader.session_id()?;
    let target_branch = reader.branch_id()?;
    let target_affinity = reader.digest(Some(DigestDomain::CacheAffinity))?;
    let target_epoch = reader.u64()?;
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    RuntimeContextTransition::from_recorded(
        RuntimeContextTransition {
            kind,
            source_session,
            source_branch,
            source_checkpoint,
            source_context,
            source_affinity,
            source_epoch,
            target_session,
            target_branch,
            target_affinity,
            target_epoch,
            identity,
        },
        identity,
    )
}

fn read_context_evaluation_audit(
    reader: &mut Reader<'_>,
) -> Result<ContextEvaluationAudit, RuntimeManifestError> {
    ContextEvaluationAudit::new(
        reader.digest(Some(DigestDomain::TokenMeasurement))?,
        reader.digest(Some(DigestDomain::ContextAdmission))?,
        reader.u64()?,
    )
}

pub(in crate::manifest) fn read_compaction_provenance(
    reader: &mut Reader<'_>,
) -> Result<CompactionProvenance, RuntimeManifestError> {
    let source_checkpoint = reader.checkpoint_id()?;
    let source_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let operation = reader.digest(Some(DigestDomain::ProviderOperation))?;
    let route_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let model_binding = reader.digest(Some(DigestDomain::ModelBinding))?;
    let harness_plan = reader.digest(Some(DigestDomain::ResolvedHarnessPlan))?;
    let selection_plan = reader.digest(Some(DigestDomain::ContextPlan))?;
    let strategy = match reader.u8()? {
        1 => RuntimeCompactionStrategy::SemanticSummaryPrefixV1,
        2 => RuntimeCompactionStrategy::NativeResponsesTriggerV1,
        3 => RuntimeCompactionStrategy::ToolResultPlaceholdersV1,
        4 => RuntimeCompactionStrategy::ReplayOmissionV1,
        tag => {
            return Err(RuntimeManifestError::UnknownTag {
                field: "runtime compaction strategy",
                tag,
            });
        }
    };
    let strategy_revision = reader.u32()?;
    let source_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let selected_start = reader.u32()?;
    let selected_end = reader.u32()?;
    let selected_group_count = reader.u32()?;
    let selected_segment_count = reader.u32()?;
    let selected_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let retained_segment_count = reader.u32()?;
    let trailing_segment_count = reader.u32()?;
    let retained_tail_identity = reader.digest(Some(DigestDomain::ContextPlan))?;
    let target_history = reader.digest(Some(DigestDomain::CanonicalHistory))?;
    let target_segment_count = reader.u32()?;
    let target_projection = reader.digest(Some(DigestDomain::HistoryProjection))?;
    let before = read_context_evaluation_audit(reader)?;
    let after = read_context_evaluation_audit(reader)?;
    let summary_segment = reader.digest(Some(DigestDomain::CanonicalHistorySegment))?;
    let usage = reader
        .optional()?
        .then(|| {
            Ok::<_, RuntimeManifestError>(Usage::new(
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
                reader.u64()?,
            ))
        })
        .transpose()?;
    let invalidation = CompactionInvalidation {
        source_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        source_epoch: reader.u64()?,
        target_affinity: reader.digest(Some(DigestDomain::CacheAffinity))?,
        target_epoch: reader.u64()?,
    };
    let identity = reader.digest(Some(DigestDomain::ContextBudget))?;
    CompactionProvenance::from_recorded(
        CompactionProvenance {
            source_checkpoint,
            source_history,
            operation,
            route_binding,
            model_binding,
            harness_plan,
            selection_plan,
            strategy,
            strategy_revision,
            source_projection,
            selected_start,
            selected_end,
            selected_group_count,
            selected_segment_count,
            selected_identity,
            retained_segment_count,
            trailing_segment_count,
            retained_tail_identity,
            target_history,
            target_segment_count,
            target_projection,
            before,
            after,
            summary_segment,
            usage,
            invalidation,
            identity,
        },
        identity,
    )
}
