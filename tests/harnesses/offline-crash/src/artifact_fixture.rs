//! Managed, sealed final responses for the actual-host finalization crash cut.

use std::sync::Arc;

use plakous::model::{
    CapacityCertificate, CapacityRule, ContentBlock, ContextContract, ContextEvidenceSource,
    CountCertificateKind, Digest, DigestDomain, EvidenceFreshness, MeasurementCertificate,
    ModelResponse, OpaqueBlock, OpaqueFallback, ReplayEnvelope, ReplayKindId, ReplayScope,
    ResolvedModelDescriptor, ValidatedRequestPlan,
};
use plakous_model::{
    FamilyArtifactProducerFacts, SealedFamilyArtifactCandidate, StandardIntegrationAuthority,
};

use crate::fixture::{HarnessResult, descriptor_with_projected_input, fixture_time};

pub(super) fn artifact_fixture_descriptor(
    account: &[u8],
    maximum: u64,
) -> HarnessResult<ResolvedModelDescriptor> {
    let base = descriptor_with_projected_input(account, maximum)?;
    let authority = StandardIntegrationAuthority::release_owned();
    let freshness = EvidenceFreshness::new(fixture_time()?, None)?;
    let contract = ContextContract::new(
        base.protocol().clone(),
        "offline-loopback-accounting-v1",
        "offline-loopback-projection-v1",
        "offline-loopback-output-reservation-v1",
        CapacityCertificate::new(
            &authority,
            "offline-loopback-capacity",
            "exact-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: plakous::model::SafeLimit::Exact(maximum),
            },
            None,
            freshness,
        )?,
        vec![MeasurementCertificate::standard(
            &authority,
            "offline-loopback-byte-counter",
            "exact-v1",
            "complete-loopback-wire",
            "byte-tokenizer-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CountCertificateKind::ExactTokenizer,
            None,
            freshness,
        )?],
    )?;
    Ok(ResolvedModelDescriptor::new(
        &authority,
        base.protocol().clone(),
        base.route().clone(),
        base.requested_model().clone(),
        None,
        base.model_family().cloned(),
        base.profile().clone(),
        base.capabilities().clone(),
    )
    .with_context_contract(contract))
}

pub(super) fn seal_fixture_artifact(
    plan: &ValidatedRequestPlan,
    response: &ModelResponse,
    cache_affinity: Option<Digest>,
) -> Result<ModelResponse, plakous::model::ModelError> {
    seal(plan, response, cache_affinity).map_err(|error| {
        plakous::model::ModelError::new(
            plakous::model::ModelErrorKind::InvalidEvent,
            error.to_string(),
        )
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "the fixture keeps exact producer facts adjacent to its native output"
)]
fn seal(
    plan: &ValidatedRequestPlan,
    response: &ModelResponse,
    cache_affinity: Option<Digest>,
) -> HarnessResult<ModelResponse> {
    let mut output = response.output().to_vec();
    let first = output.first().ok_or("missing final output")?;
    let native_text = ReplayEnvelope::new(
        plakous::model::ProtocolId::new("offline.loopback.responses")?,
        None,
        ReplayScope::Protocol,
        ReplayKindId::new("message")?,
        1,
        plakous::model::DataClassification::Sensitive,
        serde_json::value::RawValue::from_string(
            serde_json::json!({"type": "message", "text": "fixture final"}).to_string(),
        )?,
    )?;
    let reasoning_bytes = serde_json::value::RawValue::from_string(
        serde_json::json!({"type": "reasoning", "encrypted_content": "fixture-encrypted-reasoning"}).to_string(),
    )?;
    let envelope = ReplayEnvelope::new(
        plakous::model::ProtocolId::new("offline.loopback.responses")?,
        None,
        ReplayScope::Protocol,
        ReplayKindId::new("reasoning")?,
        1,
        plakous::model::DataClassification::Sensitive,
        reasoning_bytes,
    )?;
    let mut blocks = vec![
        ContentBlock::Reasoning(plakous::model::ReasoningBlock::new(
            plakous::model::ReasoningVisibility::Visible,
            Some("fixture thought".to_owned()),
            Some(envelope.clone()),
        )?),
        ContentBlock::Opaque(OpaqueBlock::new(
            envelope,
            OpaqueFallback::CompatibleReplay,
        )?),
    ];
    blocks.extend(first.blocks().iter().cloned());
    if response.finish().reason() == plakous::model::FinishReason::Stop {
        blocks.push(ContentBlock::Opaque(OpaqueBlock::new(
            native_text,
            OpaqueFallback::CompatibleReplay,
        )?));
    }
    output[0] = plakous::model::ConversationItem::new(
        first.id(),
        first.actor(),
        blocks,
        first.provenance(),
    )?;
    let response = ModelResponse::new(
        output,
        response.finish().clone(),
        response.usage(),
        response.response_id().map(str::to_owned),
        response.plan_digest(),
    )
    .with_metadata(response.metadata().clone());
    let request = plan.request();
    let metadata = request.metadata();
    let source = request
        .input()
        .canonical_item_prefix_identity(request.input().items().len())?;
    let covered = request
        .input()
        .staged_canonical_item_prefix_identity(response.output())?;
    let producer = FamilyArtifactProducerFacts::new(
        request.identity(),
        request.model_input_identity(),
        plan.digest(),
        cache_affinity,
        metadata.run_id(),
        metadata.root(),
        metadata.attempt_id(),
        source.identity(),
        u32::try_from(source.item_count())?,
        u32::try_from(response.output().len())?,
        covered.identity(),
        u32::try_from(covered.item_count())?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"fixture physical",
                metadata.attempt_id().as_uuid().as_bytes(),
            ],
        )?,
        Digest::sha256(
            DigestDomain::ProviderOperation,
            1,
            &[
                b"fixture terminal",
                metadata.attempt_id().as_uuid().as_bytes(),
            ],
        )?,
    )?;
    Ok(
        response.with_family_artifact_candidate(SealedFamilyArtifactCandidate::new(
            "responses",
            plakous_agent::internal::RESPONSES_FAMILY_ARTIFACT_SCHEMA_REVISION,
            plakous_agent::internal::RESPONSES_FAMILY_ARTIFACT_CODEC_REVISION,
            plan.binding_digest(),
            producer,
            Arc::<[u8]>::from(
                format!("fixture-encrypted-native-{}", metadata.attempt_id()).into_bytes(),
            ),
        )?)?,
    )
}
