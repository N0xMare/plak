use super::*;
#[test]
fn unreachable_retry_names_the_network_wait_without_changing_incomplete_text() {
    let retry = ModelRetry::new(
        5,
        12,
        std::time::Duration::from_secs(16),
        ModelRetryCause::Connect,
        false,
        "transport detail",
    )
    .with_loss(ModelRetryLoss {
        transport_phase: Some(ModelRetryPhase::Connect),
        ..ModelRetryLoss::default()
    });
    assert_eq!(
        retry.human_cause(),
        "provider unreachable (could not connect to the provider (nothing was sent)); waiting for network, try 5 in 16s"
    );
    let incomplete = retry.with_loss(ModelRetryLoss {
        transport_phase: Some(ModelRetryPhase::RequestWrite),
        ..ModelRetryLoss::default()
    });
    assert!(!incomplete.human_cause().contains("waiting for network"));
    let declined = incomplete.with_loss(ModelRetryLoss {
        transport_phase: Some(ModelRetryPhase::Connect),
        declined_reason: Some(ModelRetryDeclineReason::Budget),
        ..ModelRetryLoss::default()
    });
    assert!(!declined.human_cause().contains("waiting for network"));
}

use crate::{
    AttemptId, DataClassification, DigestDomain, IdentityError, MAX_FAMILY_ARTIFACT_BYTES,
    MAX_MODEL_PROVIDER_REQUEST_ID_BYTES, ModelOperationRoot, ProtocolId, ReplayKindId, ReplayScope,
    RunId, TurnId,
};
use proptest::prelude::*;
use uuid::Uuid;

#[test]
fn retry_request_logging_facts_are_ephemeral_and_exclude_queries() {
    let plain = ModelRetry::new(
        2,
        4,
        std::time::Duration::ZERO,
        ModelRetryCause::RecordRejectedInTransit,
        false,
        "peer_rejected_record",
    );
    let logged =
        plain
            .clone()
            .with_request("fixture.invalid", "/v1/messages?private=hidden", 4_194_304);
    assert_eq!(
        logged.request(),
        Some(("fixture.invalid", "/v1/messages", 4_194_304))
    );
    assert_eq!(
        serde_json::to_value(&plain).unwrap(),
        serde_json::to_value(&logged).unwrap()
    );
    let decoded: ModelRetry =
        serde_json::from_value(serde_json::to_value(logged).unwrap()).unwrap();
    assert_eq!(decoded.request(), None);
    assert_eq!(decoded.cause().kind(), "record_rejected_in_transit");
}

#[test]
fn replay_loss_counts_bytes_and_completed_items_without_retaining_content() {
    let mut loss = ModelRetryLoss::default();
    let location = OutputLocation::new(0, 0);
    loss.observe(&ModelEvent::TextDelta(TextDelta::new(
        location,
        vec![b'a', b'b', b'c'],
    )));
    loss.observe(&ModelEvent::ReasoningDelta(ReasoningDelta::new(
        location,
        vec![0xc3, 0xa9],
    )));
    loss.observe(&ModelEvent::BlockCompleted(BlockCompleted::new(location)));
    assert_eq!(loss.assistant_text_bytes, 3);
    assert_eq!(loss.reasoning_bytes, 2);
    assert_eq!(loss.completed_items, 1);
}

#[test]
fn historical_projection_diagnostic_identity_is_not_serialized()
-> Result<(), Box<dyn std::error::Error>> {
    let identity = Digest::sha256(DigestDomain::HistoryProjection, 1, &[b"transition"])?;
    let generic = ModelDiagnostic::new("configuration transition");
    let typed = ModelDiagnostic::historical_projection(identity, "configuration transition");
    assert_eq!(generic.history_projection_identity(), None);
    assert_eq!(typed.history_projection_identity(), Some(identity));
    let wire = serde_json::to_value(&typed)?;
    assert_eq!(
        wire,
        serde_json::json!({"message": "configuration transition"})
    );
    assert_eq!(wire, serde_json::to_value(&generic)?);
    let restored: ModelDiagnostic = serde_json::from_value(wire)?;
    assert_eq!(restored, generic);
    assert_eq!(restored.history_projection_identity(), None);
    Ok(())
}

fn entry(value: u128) -> Result<EntryId, IdentityError> {
    EntryId::try_from(Uuid::from_u128(value))
}

fn call(value: u128) -> Result<crate::CallId, IdentityError> {
    crate::CallId::try_from(Uuid::from_u128(value))
}

fn plan_digest() -> Result<Digest, crate::DigestError> {
    Digest::sha256(DigestDomain::RequestPlan, 1, &[b"test-plan"])
}

fn artifact_digest(
    domain: DigestDomain,
    label: &'static [u8],
) -> Result<Digest, crate::DigestError> {
    Digest::sha256(domain, 1, &[label])
}

fn artifact_producer(
    plan: Digest,
    output_items: u32,
) -> Result<FamilyArtifactProducerFacts, Box<dyn std::error::Error>> {
    Ok(FamilyArtifactProducerFacts::new(
        artifact_digest(DigestDomain::RequestPlan, b"request")?,
        artifact_digest(DigestDomain::RequestPlan, b"model-input")?,
        plan,
        Some(artifact_digest(
            DigestDomain::CacheAffinity,
            b"cache-affinity",
        )?),
        RunId::try_from(Uuid::from_u128(11))?,
        ModelOperationRoot::Turn(TurnId::try_from(Uuid::from_u128(12))?),
        AttemptId::try_from(Uuid::from_u128(13))?,
        artifact_digest(DigestDomain::CanonicalItemPrefix, b"source")?,
        2,
        output_items,
        artifact_digest(DigestDomain::CanonicalItemPrefix, b"covered")?,
        2_u32
            .checked_add(output_items)
            .ok_or_else(|| std::io::Error::other("coverage overflow"))?,
        artifact_digest(DigestDomain::ProviderOperation, b"physical")?,
        artifact_digest(DigestDomain::ProviderOperation, b"terminal")?,
    )?)
}

fn artifact_candidate(
    plan: Digest,
    output_items: u32,
    bytes: impl Into<Arc<[u8]>>,
) -> Result<SealedFamilyArtifactCandidate, Box<dyn std::error::Error>> {
    Ok(SealedFamilyArtifactCandidate::new(
        "responses",
        3,
        3,
        artifact_digest(DigestDomain::ModelBinding, b"binding")?,
        artifact_producer(plan, output_items)?,
        bytes,
    )?)
}

fn artifact_output_item() -> Result<ConversationItem, Box<dyn std::error::Error>> {
    Ok(ConversationItem::new(
        entry(14)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("artifact output", None)?)],
        Provenance::ModelOutput,
    )?)
}

fn structured_format() -> Result<OutputFormat, Box<dyn std::error::Error>> {
    Ok(OutputFormat::structured(serde_json::json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {"answer": {"type": "string"}},
        "required": ["answer"],
        "additionalProperties": false
    }))?)
}

fn output_response(
    items: Vec<Vec<ContentBlock>>,
) -> Result<ModelResponse, Box<dyn std::error::Error>> {
    output_response_with_finish(items, FinishReason::Stop)
}

/// Landing records each runtime-owned call's canonical name beside the
/// exact name the model called, and its runtime owner; a provider-hosted
/// call keeps its own name and owner.
#[test]
fn landing_records_canonical_names_and_owners_and_keeps_exposed_names()
-> Result<(), Box<dyn std::error::Error>> {
    let call = |id: u128, name: &str, owner: ToolExecutionOwner| {
        ToolCallBlock::new(
            crate::CallId::try_from(Uuid::from_u128(id))?,
            ToolName::new(name)?,
            owner,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )
        .map(ContentBlock::ToolCall)
        .map_err(Into::<Box<dyn std::error::Error>>::into)
    };
    let hosted = ToolExecutionOwner::ProviderHosted {
        provider_kind: crate::NamespaceId::new("hosted.search")?,
    };
    let github = McpServerId::new("github")?;
    let mcp = ToolExecutionOwner::Mcp {
        server: github.clone(),
    };
    let response = output_response(vec![vec![
        call(1, "edit", ToolExecutionOwner::Local)?,
        call(2, "write", mcp.clone())?,
        call(3, "web_search", hosted.clone())?,
        call(4, "mcp__github__create_issue", ToolExecutionOwner::Local)?,
    ]])?
    .with_canonical_tool_calls(|call| {
        let name = ToolName::new(format!("canonical_{}", call.exposed_name().as_str()))
            .unwrap_or_else(|_| call.exposed_name().clone());
        let server = call
            .exposed_name()
            .as_str()
            .starts_with("mcp__")
            .then(|| github.clone());
        (name, server)
    });
    let calls = response.output()[0]
        .blocks()
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolCall(call) => Some((
                call.canonical_name().as_str().to_owned(),
                call.exposed_name().as_str().to_owned(),
                call.owner().clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        calls,
        [
            (
                "canonical_edit".to_owned(),
                "edit".to_owned(),
                ToolExecutionOwner::Local
            ),
            (
                "canonical_write".to_owned(),
                "write".to_owned(),
                ToolExecutionOwner::Local
            ),
            ("web_search".to_owned(), "web_search".to_owned(), hosted),
            (
                "canonical_mcp__github__create_issue".to_owned(),
                "mcp__github__create_issue".to_owned(),
                mcp
            ),
        ]
    );
    Ok(())
}

fn output_response_with_finish(
    items: Vec<Vec<ContentBlock>>,
    finish: FinishReason,
) -> Result<ModelResponse, Box<dyn std::error::Error>> {
    let output = items
        .into_iter()
        .enumerate()
        .map(|(index, blocks)| {
            ConversationItem::new(
                entry(100 + u128::try_from(index)?)?,
                Actor::Assistant,
                blocks,
                Provenance::ModelOutput,
            )
            .map_err(Into::into)
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;
    Ok(ModelResponse::new(
        output,
        FinishDetails::new(finish, None),
        None,
        None,
        plan_digest()?,
    ))
}

fn replay_envelope(payload: &str) -> Result<ReplayEnvelope, Box<dyn std::error::Error>> {
    Ok(ReplayEnvelope::new(
        ProtocolId::new("test-protocol")?,
        None,
        ReplayScope::Protocol,
        ReplayKindId::new("reasoning-signature")?,
        1,
        DataClassification::Sensitive,
        RawValue::from_string(payload.to_owned())?,
    )?)
}

fn assembler_with_block(
    block: BlockStart,
) -> Result<(ModelResponseAssembler, Digest, OutputLocation), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        block,
    )))?;
    Ok((assembler, digest, location))
}

fn complete_text_response(
    assembler: &mut ModelResponseAssembler,
    digest: Digest,
    id: u128,
) -> Result<ModelResponse, Box<dyn std::error::Error>> {
    let item = ConversationItem::new(
        entry(id)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("complete", None)?)],
        Provenance::ModelOutput,
    )?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        item.id(),
        OutputLocation::new(0, 0),
        BlockStart::Complete(item.blocks()[0].clone()),
    )))?;
    let response = ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    assembler
        .push(ModelEvent::ResponseCompleted(response.clone()))?
        .ok_or_else(|| std::io::Error::other("terminal aggregate was not returned"))?;
    Ok(response)
}

fn assert_invalid(result: Result<Option<ModelResponse>, ModelError>) {
    assert_eq!(
        result.err().as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
}

fn metadata_assembly_result(
    started_metadata: ModelResponseMetadata,
    completed_metadata: ModelResponseMetadata,
) -> Result<Result<Option<ModelResponse>, ModelError>, Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let item = ConversationItem::new(
        entry(91)?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("metadata", None)?)],
        Provenance::ModelOutput,
    )?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(
        ResponseStarted::new(None, digest).with_metadata(started_metadata),
    ))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        item.id(),
        OutputLocation::new(0, 0),
        BlockStart::Complete(item.blocks()[0].clone()),
    )))?;
    let response = ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    )
    .with_metadata(completed_metadata);
    Ok(assembler.push(ModelEvent::ResponseCompleted(response)))
}

#[test]
fn family_artifact_candidate_roundtrips_with_durable_facts_and_redacted_debug()
-> Result<(), Box<dyn std::error::Error>> {
    const SECRET_BYTES: &[u8] = b"SEALED_FAMILY_SECRET_SENTINEL";
    let plan = plan_digest()?;
    let candidate = artifact_candidate(plan, 1, SECRET_BYTES)?;
    let response = ModelResponse::new(
        vec![artifact_output_item()?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan,
    )
    .with_family_artifact_candidate(candidate.clone())?;

    assert_eq!(response.family_artifact_candidate(), Some(&candidate));
    assert_eq!(candidate.family(), "responses");
    assert_eq!(candidate.schema_revision(), 3);
    assert_eq!(candidate.codec_revision(), 3);
    assert_eq!(candidate.bytes(), SECRET_BYTES);
    assert_eq!(candidate.integrity().domain(), DigestDomain::Blob);
    assert_eq!(candidate.producer().plan(), plan);
    assert_eq!(
        candidate.producer().cache_affinity().map(Digest::domain),
        Some(DigestDomain::CacheAffinity)
    );
    assert_eq!(candidate.producer().source_projection_items(), 2);
    assert_eq!(candidate.producer().output_items(), 1);
    assert_eq!(candidate.producer().covered_projection_items(), 3);

    let encoded = serde_json::to_value(&response)?;
    assert!(
        encoded["family_artifact_candidate"]["producer"]
            .get("physical_operation")
            .is_some()
    );
    assert!(
        encoded["family_artifact_candidate"]["producer"]
            .get("terminal_operation")
            .is_some()
    );
    assert!(
        encoded["family_artifact_candidate"]["producer"]
            .get("cache_affinity")
            .is_some()
    );
    assert_eq!(
        serde_json::from_value::<ModelResponse>(encoded.clone())?,
        response
    );
    let mut missing_affinity = encoded;
    missing_affinity["family_artifact_candidate"]["producer"]
        .as_object_mut()
        .ok_or_else(|| std::io::Error::other("producer facts are not an object"))?
        .remove("cache_affinity");
    assert!(serde_json::from_value::<ModelResponse>(missing_affinity).is_err());

    let candidate_debug = format!("{candidate:?}");
    let response_debug = format!("{response:?}");
    assert!(candidate_debug.contains("<redacted;"));
    assert!(!candidate_debug.contains("SEALED_FAMILY_SECRET_SENTINEL"));
    assert!(!response_debug.contains("SEALED_FAMILY_SECRET_SENTINEL"));
    Ok(())
}

#[test]
fn family_artifact_candidate_rejects_tamper_bounds_and_foreign_response_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let plan = plan_digest()?;
    assert!(artifact_producer(plan, 0).is_err());
    let candidate = artifact_candidate(plan, 1, [1_u8, 2, 3])?;
    let mut encoded = serde_json::to_value(&candidate)?;
    encoded["bytes"][0] = serde_json::json!(9);
    assert!(serde_json::from_value::<SealedFamilyArtifactCandidate>(encoded).is_err());

    let mut fact_tamper = serde_json::to_value(&candidate)?;
    fact_tamper["producer"]["physical_operation"]["bytes"][0] = serde_json::json!(9);
    assert!(serde_json::from_value::<SealedFamilyArtifactCandidate>(fact_tamper).is_err());

    assert!(matches!(
        SealedFamilyArtifactCandidate::new(
            "responses",
            3,
            3,
            artifact_digest(DigestDomain::ModelBinding, b"binding")?,
            artifact_producer(plan, 1)?,
            vec![0_u8; MAX_FAMILY_ARTIFACT_BYTES + 1],
        ),
        Err(FamilyArtifactError::BytesTooLarge { .. })
    ));

    let item = artifact_output_item()?;
    let foreign_plan = artifact_digest(DigestDomain::RequestPlan, b"foreign-plan")?;
    let foreign = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan,
    )
    .with_family_artifact_candidate(artifact_candidate(foreign_plan, 1, [1_u8])?);
    assert!(matches!(
        foreign,
        Err(FamilyArtifactError::ForeignResponsePlan)
    ));

    let wrong_count = ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan,
    )
    .with_family_artifact_candidate(artifact_candidate(plan, 2, [1_u8])?);
    assert!(matches!(
        wrong_count,
        Err(FamilyArtifactError::ResponseOutputMismatch)
    ));

    let wrong_domain = artifact_digest(DigestDomain::Blob, b"foreign-request")?;
    assert!(matches!(
        FamilyArtifactProducerFacts::new(
            wrong_domain,
            artifact_digest(DigestDomain::RequestPlan, b"model-input")?,
            plan,
            None,
            RunId::try_from(Uuid::from_u128(11))?,
            ModelOperationRoot::Turn(TurnId::try_from(Uuid::from_u128(12))?),
            AttemptId::try_from(Uuid::from_u128(13))?,
            artifact_digest(DigestDomain::CanonicalItemPrefix, b"source")?,
            2,
            1,
            artifact_digest(DigestDomain::CanonicalItemPrefix, b"covered")?,
            3,
            artifact_digest(DigestDomain::ProviderOperation, b"physical")?,
            artifact_digest(DigestDomain::ProviderOperation, b"terminal")?,
        ),
        Err(FamilyArtifactError::InvalidDigest { field: "request" })
    ));
    let wrong_affinity = artifact_digest(DigestDomain::ModelBinding, b"foreign-affinity")?;
    assert!(matches!(
        FamilyArtifactProducerFacts::new(
            artifact_digest(DigestDomain::RequestPlan, b"request")?,
            artifact_digest(DigestDomain::RequestPlan, b"model-input")?,
            plan,
            Some(wrong_affinity),
            RunId::try_from(Uuid::from_u128(11))?,
            ModelOperationRoot::Turn(TurnId::try_from(Uuid::from_u128(12))?),
            AttemptId::try_from(Uuid::from_u128(13))?,
            artifact_digest(DigestDomain::CanonicalItemPrefix, b"source")?,
            2,
            1,
            artifact_digest(DigestDomain::CanonicalItemPrefix, b"covered")?,
            3,
            artifact_digest(DigestDomain::ProviderOperation, b"physical")?,
            artifact_digest(DigestDomain::ProviderOperation, b"terminal")?,
        ),
        Err(FamilyArtifactError::InvalidDigest {
            field: "cache affinity"
        })
    ));
    Ok(())
}

#[test]
fn response_metadata_preserves_legacy_empty_serde_and_validates_nonempty_values()
-> Result<(), Box<dyn std::error::Error>> {
    const REQUEST_ID: &str = "request-id-debug-sentinel";

    let digest = plan_digest()?;
    let started = ResponseStarted::new(None, digest);
    let response = ModelResponse::new(
        vec![ConversationItem::new(
            entry(90)?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("complete", None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let started_json = serde_json::to_value(&started)?;
    let response_json = serde_json::to_value(&response)?;
    assert!(started_json.get("metadata").is_none());
    assert!(response_json.get("metadata").is_none());
    assert_eq!(
        serde_json::from_value::<ResponseStarted>(started_json)?,
        started
    );
    assert_eq!(
        serde_json::from_value::<ModelResponse>(response_json)?,
        response
    );

    let metadata = ModelResponseMetadata::new(
        Some(ModelId::new("provider-model")?),
        Some(ModelProviderRequestId::new(REQUEST_ID)?),
    );
    assert_eq!(
        metadata.actual_model().map(ModelId::as_str),
        Some("provider-model")
    );
    assert_eq!(metadata.provider_request_id(), Some(REQUEST_ID));
    assert!(!metadata.is_empty());
    let enriched_started = started.clone().with_metadata(metadata.clone());
    let enriched_response = response.clone().with_metadata(metadata.clone());
    assert_eq!(enriched_started.metadata(), &metadata);
    assert_eq!(enriched_response.metadata(), &metadata);
    assert_eq!(
        serde_json::from_str::<ResponseStarted>(&serde_json::to_string(&enriched_started)?)?,
        enriched_started
    );
    assert_eq!(
        serde_json::from_str::<ModelResponse>(&serde_json::to_string(&enriched_response)?)?,
        enriched_response
    );

    assert!(ModelId::new("not a model").is_err());
    assert!(ModelProviderRequestId::new("Bearer-secret").is_err());
    assert!(
        ModelProviderRequestId::new("x".repeat(MAX_MODEL_PROVIDER_REQUEST_ID_BYTES + 1)).is_err()
    );

    let mut invalid_model = serde_json::to_value(&metadata)?;
    invalid_model["actual_model"] = serde_json::json!("not a model");
    assert!(serde_json::from_value::<ModelResponseMetadata>(invalid_model).is_err());
    let mut invalid_request_id = serde_json::to_value(&metadata)?;
    invalid_request_id["provider_request_id"] = serde_json::json!("Bearer-secret");
    assert!(serde_json::from_value::<ModelResponseMetadata>(invalid_request_id).is_err());
    let mut oversized_request_id = serde_json::to_value(&metadata)?;
    oversized_request_id["provider_request_id"] =
        serde_json::json!("x".repeat(MAX_MODEL_PROVIDER_REQUEST_ID_BYTES + 1));
    assert!(serde_json::from_value::<ModelResponseMetadata>(oversized_request_id).is_err());
    Ok(())
}

#[test]
fn response_metadata_debug_redacts_provider_request_id() -> Result<(), Box<dyn std::error::Error>> {
    const REQUEST_ID: &str = "request-id-debug-sentinel";

    let metadata = ModelResponseMetadata::empty()
        .with_actual_model(ModelId::new("provider-model")?)
        .with_provider_request_id(ModelProviderRequestId::new(REQUEST_ID)?);
    let started = ResponseStarted::new(None, plan_digest()?).with_metadata(metadata.clone());
    let response = ModelResponse::new(
        vec![ConversationItem::new(
            entry(92)?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("complete", None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        plan_digest()?,
    )
    .with_metadata(metadata.clone());
    for debug in [
        format!("{metadata:?}"),
        format!("{started:?}"),
        format!("{response:?}"),
        format!("{:?}", ModelEvent::ResponseStarted(started)),
        format!("{:?}", ModelEvent::ResponseCompleted(response)),
    ] {
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(REQUEST_ID));
    }
    Ok(())
}

#[test]
fn assembler_accepts_only_monotonic_response_metadata() -> Result<(), Box<dyn std::error::Error>> {
    let actual_a = ModelId::new("actual-a")?;
    let actual_b = ModelId::new("actual-b")?;
    let request_a = ModelProviderRequestId::new("request-a")?;
    let request_b = ModelProviderRequestId::new("request-b")?;

    let accepted = [
        (
            ModelResponseMetadata::empty(),
            ModelResponseMetadata::empty(),
        ),
        (
            ModelResponseMetadata::empty().with_actual_model(actual_a.clone()),
            ModelResponseMetadata::empty().with_actual_model(actual_a.clone()),
        ),
        (
            ModelResponseMetadata::empty().with_actual_model(actual_a.clone()),
            ModelResponseMetadata::empty()
                .with_actual_model(actual_a.clone())
                .with_provider_request_id(request_a.clone()),
        ),
        (
            ModelResponseMetadata::empty().with_provider_request_id(request_a.clone()),
            ModelResponseMetadata::empty().with_provider_request_id(request_a.clone()),
        ),
        (
            ModelResponseMetadata::empty(),
            ModelResponseMetadata::new(Some(actual_a.clone()), Some(request_a.clone())),
        ),
    ];
    for (started, completed) in accepted {
        assert!(metadata_assembly_result(started, completed)?.is_ok());
    }

    let rejected = [
        (
            ModelResponseMetadata::empty().with_actual_model(actual_a.clone()),
            ModelResponseMetadata::empty(),
        ),
        (
            ModelResponseMetadata::empty().with_actual_model(actual_a.clone()),
            ModelResponseMetadata::empty().with_actual_model(actual_b),
        ),
        (
            ModelResponseMetadata::empty().with_provider_request_id(request_a.clone()),
            ModelResponseMetadata::empty(),
        ),
        (
            ModelResponseMetadata::empty().with_provider_request_id(request_a),
            ModelResponseMetadata::empty().with_provider_request_id(request_b),
        ),
    ];
    for (started, completed) in rejected {
        assert_invalid(metadata_assembly_result(started, completed)?);
    }
    Ok(())
}

#[test]
fn response_omits_and_rejects_obsolete_aggregate_authority_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Stop, None),
        None,
        Some(String::from("response-id")),
        plan_digest()?,
    );
    let encoded = serde_json::to_value(&response)?;
    assert!(
        !encoded
            .as_object()
            .is_some_and(|wire| wire.contains_key("replay"))
    );
    for (field, value) in [
        (
            "continuation_update",
            serde_json::json!({"replay": [], "expires_at": null}),
        ),
        (
            "continuation",
            serde_json::json!({"replay": [], "expires_at": null}),
        ),
        ("replay", serde_json::json!([])),
    ] {
        let mut wire = serde_json::to_value(&response)?;
        wire[field] = value;
        assert!(serde_json::from_value::<ModelResponse>(wire).is_err());
    }
    Ok(())
}

#[test]
fn empty_refusal_is_valid_in_complete_and_streamed_responses()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Refusal, None),
        None,
        None,
        digest,
    );
    response.validate_for_commit()?;
    let decoded: ModelResponse = serde_json::from_value(serde_json::to_value(&response)?)?;
    assert_eq!(decoded, response);
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assert_eq!(
        assembler.push(ModelEvent::ResponseCompleted(response.clone()))?,
        Some(response)
    );
    Ok(())
}

#[test]
fn empty_length_is_valid_for_output_limit_classification() -> Result<(), Box<dyn std::error::Error>>
{
    let digest = plan_digest()?;
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Length, None),
        None,
        None,
        digest,
    );
    response.validate_for_commit()?;
    let decoded: ModelResponse = serde_json::from_value(serde_json::to_value(&response)?)?;
    assert_eq!(decoded, response);
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assert_eq!(
        assembler.push(ModelEvent::ResponseCompleted(response.clone()))?,
        Some(response)
    );
    Ok(())
}

#[test]
fn empty_refusal_revokes_complete_provisional_blocks_only() -> Result<(), Box<dyn std::error::Error>>
{
    for (finish, closed, accepted) in [
        (FinishReason::Refusal, true, true),
        (FinishReason::Length, true, true),
        (FinishReason::Stop, true, false),
        (FinishReason::ToolCalls, true, false),
        (FinishReason::Refusal, false, false),
        (FinishReason::Length, false, false),
    ] {
        let (mut assembler, digest, location) =
            assembler_with_block(BlockStart::Text { phase: None })?;
        assembler.push(ModelEvent::TextDelta(TextDelta::new(
            location,
            b"provisional",
        )))?;
        if closed {
            assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
        }
        let response = ModelResponse::new(
            Vec::new(),
            FinishDetails::new(finish, None),
            None,
            None,
            digest,
        );
        assert_eq!(
            assembler
                .push(ModelEvent::ResponseCompleted(response))
                .is_ok(),
            accepted
        );
    }
    Ok(())
}

#[test]
fn refusal_can_revoke_unfinished_local_arguments_but_not_remote_inputs()
-> Result<(), Box<dyn std::error::Error>> {
    for (owner, reason, accepted) in [
        (ToolExecutionOwner::Local, FinishReason::Refusal, true),
        (ToolExecutionOwner::Local, FinishReason::Length, true),
        (
            ToolExecutionOwner::Mcp {
                server: McpServerId::new("test-server")?,
            },
            FinishReason::Length,
            true,
        ),
        (ToolExecutionOwner::Local, FinishReason::ToolCalls, false),
        (ToolExecutionOwner::Local, FinishReason::Stop, false),
        (
            ToolExecutionOwner::ProviderHosted {
                provider_kind: crate::NamespaceId::new("remote")?,
            },
            FinishReason::Refusal,
            false,
        ),
        (
            ToolExecutionOwner::ProviderHosted {
                provider_kind: crate::NamespaceId::new("remote")?,
            },
            FinishReason::Length,
            false,
        ),
    ] {
        let (mut assembler, digest, location) = assembler_with_block(BlockStart::ToolCall {
            call_id: call(1)?,
            tool: ToolName::new("probe")?,
            owner,
            state: ToolCallState::Requested,
            correlation: ToolCorrelation::default(),
        })?;
        assembler.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
            location,
            b"{invalid",
        )))?;
        let response = ModelResponse::new(
            Vec::new(),
            FinishDetails::new(reason, None),
            None,
            None,
            digest,
        );
        assert_eq!(
            assembler
                .push(ModelEvent::ResponseCompleted(response))
                .is_ok(),
            accepted
        );
    }
    Ok(())
}

#[test]
fn refusal_and_length_revoke_only_unexecuted_client_proposals()
-> Result<(), Box<dyn std::error::Error>> {
    for finish in [FinishReason::Refusal, FinishReason::Length] {
        for owner in [
            ToolExecutionOwner::Local,
            ToolExecutionOwner::Mcp {
                server: McpServerId::new("test-server")?,
            },
            ToolExecutionOwner::ProviderHosted {
                provider_kind: crate::NamespaceId::new("remote")?,
            },
        ] {
            for state in [
                ToolCallState::Requested,
                ToolCallState::Pending,
                ToolCallState::Completed,
                ToolCallState::Failed,
            ] {
                let block = ContentBlock::ToolCall(ToolCallBlock::new(
                    call(1)?,
                    ToolName::new("probe")?,
                    owner.clone(),
                    state,
                    ToolCorrelation::default(),
                    "{}",
                )?);
                let (mut assembler, digest, _) = assembler_with_block(BlockStart::Complete(block))?;
                let response = ModelResponse::new(
                    Vec::new(),
                    FinishDetails::new(finish, None),
                    None,
                    None,
                    digest,
                );
                let result = assembler.push(ModelEvent::ResponseCompleted(response.clone()));
                if state == ToolCallState::Requested
                    && !matches!(owner, ToolExecutionOwner::ProviderHosted { .. })
                {
                    assert_eq!(result?, Some(response));
                } else {
                    assert_invalid(result);
                }
            }
        }
    }
    Ok(())
}

#[test]
fn discarded_refusal_still_binds_identity_usage_and_effects()
-> Result<(), Box<dyn std::error::Error>> {
    for (owner, changed_id, changed_usage, accepted) in [
        (ToolExecutionOwner::Local, false, false, true),
        (ToolExecutionOwner::Local, true, false, false),
        (ToolExecutionOwner::Local, false, true, false),
        (
            ToolExecutionOwner::ProviderHosted {
                provider_kind: crate::NamespaceId::new("remote")?,
            },
            false,
            false,
            false,
        ),
    ] {
        let block = ContentBlock::ToolCall(ToolCallBlock::new(
            call(1)?,
            ToolName::new("probe")?,
            owner,
            ToolCallState::Requested,
            ToolCorrelation::default(),
            "{}",
        )?);
        let (mut assembler, digest, _) = assembler_with_block(BlockStart::Complete(block))?;
        let usage = Usage::new(3, 2, 0, 0, 0);
        assembler.push(ModelEvent::Usage(UsageUpdate::new(usage)))?;
        let response = ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Refusal, None),
            if changed_usage { None } else { Some(usage) },
            changed_id.then(|| "foreign".to_owned()),
            digest,
        );
        assert_eq!(
            assembler
                .push(ModelEvent::ResponseCompleted(response))
                .is_ok(),
            accepted
        );
    }
    Ok(())
}

#[test]
fn assembler_rejects_empty_successful_output() -> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    let error = assembler
        .push(ModelEvent::ResponseCompleted(ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            digest,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    Ok(())
}

#[test]
fn structured_output_requires_one_valid_json_text_block() -> Result<(), Box<dyn std::error::Error>>
{
    let format = structured_format()?;
    let text = |value| TextBlock::new(value, None).map(ContentBlock::Text);

    output_response(vec![vec![text(r#"{"answer":"yes"}"#)?]])?.validate_output_format(&format)?;

    let rejected = [
        output_response(vec![vec![text(r#"{"answer":1}"#)?]])?,
        output_response(vec![vec![text(r#"{"answer":"yes",}"#)?]])?,
        output_response(vec![vec![text(r#"{"answer":"yes"}{"answer":"no"}"#)?]])?,
        output_response(vec![vec![text(r#"{"answer":"yes"}"#)?, text("extra")?]])?,
        output_response_with_finish(
            vec![vec![text(r#"{"answer":"yes"}"#)?]],
            FinishReason::Length,
        )?,
        output_response(vec![
            vec![text(r#"{"answer":"yes"}"#)?],
            vec![text(r#"{"answer":"no"}"#)?],
        ])?,
        output_response(vec![vec![ContentBlock::Refusal(RefusalBlock::new(
            "cannot comply",
        )?)]])?,
    ];
    for response in rejected {
        let error = response.validate_output_format(&format).err();
        assert_eq!(
            error.as_ref().map(ModelError::kind),
            Some(ModelErrorKind::InvalidEvent)
        );
        assert_eq!(
            error.as_ref().map(ToString::to_string).as_deref(),
            Some("structured assistant output was invalid")
        );
    }

    output_response(vec![vec![ContentBlock::Refusal(RefusalBlock::new(
        "ordinary text behavior",
    )?)]])?
    .validate_output_format(&OutputFormat::Text)?;
    Ok(())
}

fn assemble_fragmented_text(
    text: &str,
    chunk_sizes: &[usize],
) -> Result<ModelResponse, Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let item_id = entry(1)?;
    let location = OutputLocation::new(0, 0);
    let expected = ModelResponse::new(
        vec![ConversationItem::new(
            item_id,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        item_id,
        location,
        BlockStart::Text { phase: None },
    )))?;
    let bytes = text.as_bytes();
    let mut offset = 0;
    for requested in chunk_sizes {
        if offset == bytes.len() {
            break;
        }
        let width = requested.saturating_add(1);
        let end = offset.saturating_add(width).min(bytes.len());
        assembler.push(ModelEvent::TextDelta(TextDelta::new(
            location,
            bytes[offset..end].to_vec(),
        )))?;
        offset = end;
    }
    if offset != bytes.len() {
        assembler.push(ModelEvent::TextDelta(TextDelta::new(
            location,
            bytes[offset..].to_vec(),
        )))?;
    }
    assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    let result = assembler
        .push(ModelEvent::ResponseCompleted(expected.clone()))?
        .ok_or_else(|| std::io::Error::other("terminal aggregate was not returned"))?;
    assembler.finish_eof()?;
    Ok(result)
}

fn assemble_fragmented_pending_reasoning(
    text: &str,
    chunk_sizes: &[usize],
) -> Result<(ModelResponse, ReplayEnvelope), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let item_id = entry(1)?;
    let location = OutputLocation::new(0, 0);
    let replay = replay_envelope(r#"{"signature":"exact-late-binding"}"#)?;
    let expected = ModelResponse::new(
        vec![ConversationItem::new(
            item_id,
            Actor::Assistant,
            vec![ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::Visible,
                Some(text.to_owned()),
                Some(replay.clone()),
            )?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        item_id,
        location,
        BlockStart::ReasoningPending { replay: None },
    )))?;
    assembler.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
        location,
        replay.clone(),
    )))?;
    assembler.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    ))?;
    let bytes = text.as_bytes();
    let mut offset = 0;
    for requested in chunk_sizes {
        if offset == bytes.len() {
            break;
        }
        let width = requested.saturating_add(1);
        let end = offset.saturating_add(width).min(bytes.len());
        assembler.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
            location,
            bytes[offset..end].to_vec(),
        )))?;
        offset = end;
    }
    if offset != bytes.len() {
        assembler.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
            location,
            bytes[offset..].to_vec(),
        )))?;
    }
    assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    let result = assembler
        .push(ModelEvent::ResponseCompleted(expected))?
        .ok_or_else(|| std::io::Error::other("terminal aggregate was not returned"))?;
    assembler.finish_eof()?;
    Ok((result, replay))
}

proptest! {
    #[test]
    fn arbitrary_utf8_fragmentation_preserves_text(
        text in ".{1,256}",
        chunk_sizes in proptest::collection::vec(0_usize..32, 0..64),
    ) {
        let result = assemble_fragmented_text(&text, &chunk_sizes)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        prop_assert_eq!(result.output_text(), text);
    }

    #[test]
    fn arbitrary_pending_reasoning_deltas_resolve_visible_with_exact_replay(
        text in ".{1,256}",
        chunk_sizes in proptest::collection::vec(0_usize..32, 0..64),
    ) {
        let (result, replay) = assemble_fragmented_pending_reasoning(&text, &chunk_sizes)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let Some(item) = result.output().first() else {
            return Err(TestCaseError::fail("missing assembled item"));
        };
        let Some(ContentBlock::Reasoning(reasoning)) = item.blocks().first() else {
            return Err(TestCaseError::fail("missing assembled reasoning block"));
        };
        prop_assert_eq!(reasoning.visibility(), ReasoningVisibility::Visible);
        prop_assert_eq!(reasoning.text(), Some(text.as_str()));
        prop_assert_eq!(reasoning.replay(), Some(&replay));
    }

    #[test]
    fn arbitrary_event_sequences_never_succeed_more_than_once(
        codes in proptest::collection::vec(any::<u8>(), 0..128),
    ) {
        let digest = plan_digest()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let other = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"other"])
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let raw = RawValue::from_string(String::from("{}"))
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let replay = replay_envelope(r#"{"signature":"sequence"}"#)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let response = ModelResponse::new(
            Vec::new(),
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            digest,
        );
        let mut assembler = ModelResponseAssembler::new(digest)
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        let mut successes = 0_usize;
        let mut failed = false;
        for code in codes {
            let event = match code % 8 {
                0 => ModelEvent::ProviderEvent(ProviderEvent::new("unknown", raw.clone())),
                1 => ModelEvent::ResponseStarted(ResponseStarted::new(None, digest)),
                2 => ModelEvent::Usage(UsageUpdate::new(Usage::new(1, 0, 0, 0, 0))),
                3 => ModelEvent::ResponseCompleted(response.clone()),
                4 => ModelEvent::BlockCompleted(BlockCompleted::new(OutputLocation::new(0, 0))),
                5 => ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                    OutputLocation::new(0, 0),
                    replay.clone(),
                )),
                6 => ModelEvent::ReasoningVisibilityBound(ReasoningVisibilityBound::new(
                    OutputLocation::new(0, 0),
                    ReasoningVisibility::Visible,
                )),
                _ => ModelEvent::ResponseStarted(ResponseStarted::new(None, other)),
            };
            match assembler.push(event) {
                Ok(Some(_)) => {
                    prop_assert!(!failed);
                    successes = successes.saturating_add(1);
                }
                Ok(None) => {}
                Err(_) => failed = true,
            }
        }
        prop_assert!(successes <= 1);
    }
}

#[test]
fn interleaved_fragmented_blocks_assemble_in_index_order() -> Result<(), Box<dyn std::error::Error>>
{
    let first = entry(1)?;
    let second = entry(2)?;
    let text = OutputLocation::new(0, 0);
    let tool = OutputLocation::new(0, 1);
    let next_text = OutputLocation::new(1, 0);
    let digest = plan_digest()?;
    let expected_items = vec![
        ConversationItem::new(
            first,
            Actor::Assistant,
            vec![
                ContentBlock::Text(TextBlock::new("café", Some(TextPhase::Final))?),
                ContentBlock::ToolCall(ToolCallBlock::new(
                    call(3)?,
                    ToolName::new("lookup")?,
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    ToolCorrelation::default(),
                    "{\"key\":1}",
                )?),
            ],
            Provenance::ModelOutput,
        )?,
        ConversationItem::new(
            second,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new("done", None)?)],
            Provenance::ModelOutput,
        )?,
    ];
    let expected = ModelResponse::new(
        expected_items,
        FinishDetails::new(FinishReason::ToolCalls, None),
        Some(Usage::new(10, 4, 0, 2, 0)),
        Some(String::from("response-1")),
        digest,
    );
    let events = [
        ModelEvent::ResponseStarted(ResponseStarted::new(
            Some(String::from("response-1")),
            digest,
        )),
        ModelEvent::BlockStarted(BlockStarted::new(
            first,
            text,
            BlockStart::Text {
                phase: Some(TextPhase::Final),
            },
        )),
        ModelEvent::BlockStarted(BlockStarted::new(
            first,
            tool,
            BlockStart::ToolCall {
                call_id: call(3)?,
                tool: ToolName::new("lookup")?,
                owner: ToolExecutionOwner::Local,
                state: ToolCallState::Requested,
                correlation: ToolCorrelation::default(),
            },
        )),
        ModelEvent::TextDelta(TextDelta::new(text, vec![b'c', b'a', b'f', 0xc3])),
        ModelEvent::ToolInputDelta(ToolInputDelta::new(tool, b"{\"key\":".to_vec())),
        ModelEvent::BlockStarted(BlockStarted::new(
            second,
            next_text,
            BlockStart::Text { phase: None },
        )),
        ModelEvent::TextDelta(TextDelta::new(text, vec![0xa9])),
        ModelEvent::TextDelta(TextDelta::new(next_text, b"done".to_vec())),
        ModelEvent::ToolInputDelta(ToolInputDelta::new(tool, b"1}".to_vec())),
        ModelEvent::BlockCompleted(BlockCompleted::new(tool)),
        ModelEvent::BlockCompleted(BlockCompleted::new(text)),
        ModelEvent::BlockCompleted(BlockCompleted::new(next_text)),
        ModelEvent::Usage(UsageUpdate::new(Usage::new(10, 4, 0, 2, 0))),
        ModelEvent::ResponseCompleted(expected.clone()),
    ];
    let mut assembler = ModelResponseAssembler::new(digest)?;
    let mut result = None;
    for event in events {
        if let Some(response) = assembler.push(event)? {
            assert!(result.is_none());
            result = Some(response);
        }
    }
    assert_eq!(result, Some(expected));
    assembler.finish_eof()?;
    Ok(())
}

#[test]
fn premature_eof_is_incomplete() -> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    let error = assembler.finish_eof().err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::IncompleteResponse)
    );
    Ok(())
}

#[test]
fn assembler_rejects_wrong_plan_binding_and_domain() -> Result<(), Box<dyn std::error::Error>> {
    let expected = plan_digest()?;
    let other = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"other-plan"])?;
    let mut assembler = ModelResponseAssembler::new(expected)?;
    let error = assembler
        .push(ModelEvent::ResponseStarted(ResponseStarted::new(
            None, other,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );

    let wrong_domain = Digest::sha256(DigestDomain::Blob, 1, &[b"not-a-plan"])?;
    let error = ModelResponseAssembler::new(wrong_domain).err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidRequest)
    );
    Ok(())
}

#[test]
fn provider_event_before_start_is_forward_compatible() -> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assert!(
        assembler
            .push(ModelEvent::ProviderEvent(ProviderEvent::new(
                "future-preamble",
                RawValue::from_string(String::from("{}"))?,
            )))?
            .is_none()
    );
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    let response = complete_text_response(&mut assembler, digest, 42)?;
    assert_eq!(response.output_text(), "complete");
    Ok(())
}

#[test]
fn invalid_terminal_assembly_cannot_be_repaired() -> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Text { phase: None },
    )))?;
    assembler.push(ModelEvent::TextDelta(TextDelta::new(
        location,
        b"partial".to_vec(),
    )))?;
    let response = ModelResponse::new(
        Vec::new(),
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    assert!(
        assembler
            .push(ModelEvent::ResponseCompleted(response))
            .is_err()
    );
    assert!(
        assembler
            .push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))
            .is_err()
    );
    Ok(())
}

#[test]
fn complete_blocks_and_event_counts_obey_explicit_limits() -> Result<(), Box<dyn std::error::Error>>
{
    let digest = plan_digest()?;
    let limits = AssemblerLimits::new(
        NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::new(1).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::new(1).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::new(3).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::new(16).unwrap_or(NonZeroUsize::MIN),
    );
    let mut assembler = ModelResponseAssembler::with_limits(digest, limits)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    let error = assembler
        .push(ModelEvent::BlockStarted(BlockStarted::new(
            entry(1)?,
            OutputLocation::new(0, 0),
            BlockStart::Complete(ContentBlock::Text(TextBlock::new("four", None)?)),
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );

    let event_limit = AssemblerLimits::new(
        NonZeroUsize::new(1).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
    );
    let mut assembler = ModelResponseAssembler::with_limits(digest, event_limit)?;
    assembler.push(ModelEvent::ProviderEvent(ProviderEvent::new(
        "preamble",
        RawValue::from_string(String::from("{}"))?,
    )))?;
    assert!(
        assembler
            .push(ModelEvent::ResponseStarted(ResponseStarted::new(
                None, digest,
            )))
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_tool_json_fails_terminally() -> Result<(), Box<dyn std::error::Error>> {
    let location = OutputLocation::new(0, 0);
    let digest = plan_digest()?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::ToolCall {
            call_id: call(2)?,
            tool: ToolName::new("lookup")?,
            owner: ToolExecutionOwner::Local,
            state: ToolCallState::Requested,
            correlation: ToolCorrelation::default(),
        },
    )))?;
    assembler.push(ModelEvent::ToolInputDelta(ToolInputDelta::new(
        location,
        b"{".to_vec(),
    )))?;
    let error = assembler
        .push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    assert!(
        assembler
            .push(ModelEvent::ProviderEvent(ProviderEvent::new(
                "ignored",
                RawValue::from_string(String::from("{}"))?,
            )))
            .is_err()
    );
    Ok(())
}

#[test]
fn duplicate_block_completion_is_terminal() -> Result<(), Box<dyn std::error::Error>> {
    let location = OutputLocation::new(0, 0);
    let digest = plan_digest()?;
    let mut assembler = ModelResponseAssembler::new(digest)?;
    assembler.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    assembler.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Text { phase: None },
    )))?;
    assembler.push(ModelEvent::TextDelta(TextDelta::new(
        location,
        b"done".to_vec(),
    )))?;
    assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    let error = assembler
        .push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    Ok(())
}

#[test]
fn pending_reasoning_without_text_resolves_omitted_with_exact_replay()
-> Result<(), Box<dyn std::error::Error>> {
    let replay = replay_envelope(r#"{"signature":"omitted-reasoning"}"#)?;
    let (mut assembler, digest, location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    assembler.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
        location,
        replay.clone(),
    )))?;
    assembler.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    ))?;
    assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;

    let expected = ModelResponse::new(
        vec![ConversationItem::new(
            entry(1)?,
            Actor::Assistant,
            vec![ContentBlock::Reasoning(ReasoningBlock::new(
                ReasoningVisibility::Omitted,
                None,
                Some(replay.clone()),
            )?)],
            Provenance::ModelOutput,
        )?],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let response = assembler
        .push(ModelEvent::ResponseCompleted(expected))?
        .ok_or_else(|| std::io::Error::other("terminal aggregate was not returned"))?;
    let Some(ContentBlock::Reasoning(reasoning)) = response.output()[0].blocks().first() else {
        return Err(std::io::Error::other("missing reasoning block").into());
    };
    assert_eq!(reasoning.visibility(), ReasoningVisibility::Omitted);
    assert_eq!(reasoning.text(), None);
    assert_eq!(reasoning.replay(), Some(&replay));
    Ok(())
}

#[test]
fn reasoning_visibility_event_round_trips_with_typed_accessors()
-> Result<(), Box<dyn std::error::Error>> {
    let location = OutputLocation::new(2, 3);
    let binding = ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible);
    assert_eq!(binding.location(), location);
    assert_eq!(binding.visibility(), ReasoningVisibility::Visible);

    let event = ModelEvent::ReasoningVisibilityBound(binding);
    let encoded = serde_json::to_string(&event)?;
    let decoded: ModelEvent = serde_json::from_str(&encoded)?;
    assert_eq!(decoded, event);
    assert!(format!("{binding:?}").contains("Visible"));

    let pending = BlockStart::ReasoningPending { replay: None };
    let encoded = serde_json::to_string(&pending)?;
    let decoded: BlockStart = serde_json::from_str(&encoded)?;
    assert_eq!(decoded, pending);
    Ok(())
}

#[test]
fn pending_reasoning_requires_visibility_before_completion_terminally()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut assembler, _digest, location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    assert_invalid(assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location))));
    assert_invalid(assembler.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    )));
    Ok(())
}

#[test]
fn pending_reasoning_rejects_delta_before_visibility_terminally()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut assembler, _digest, location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    assert_invalid(
        assembler.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
            location,
            b"too-early".to_vec(),
        ))),
    );
    assert_invalid(assembler.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    )));
    Ok(())
}

#[test]
fn bound_hidden_reasoning_rejects_deltas_terminally() -> Result<(), Box<dyn std::error::Error>> {
    for visibility in [
        ReasoningVisibility::Omitted,
        ReasoningVisibility::Redacted,
        ReasoningVisibility::OpaqueReplay,
    ] {
        let (mut assembler, _digest, location) =
            assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
        assembler.push(ModelEvent::ReasoningVisibilityBound(
            ReasoningVisibilityBound::new(location, visibility),
        ))?;
        assert_invalid(
            assembler.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                location,
                b"must-stay-hidden".to_vec(),
            ))),
        );
        assert_invalid(assembler.push(ModelEvent::BlockCompleted(BlockCompleted::new(location))));
    }
    Ok(())
}

#[test]
fn pending_reasoning_bytes_defensively_block_visibility_binding_terminally()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut assembler, _digest, location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    let AssemblyState::Open(open) = &mut assembler.state else {
        return Err(std::io::Error::other("assembler was not open").into());
    };
    let Some(PartialBlock::Reasoning { bytes, .. }) = open.blocks.get_mut(&location) else {
        return Err(std::io::Error::other("pending reasoning block was missing").into());
    };
    bytes.extend_from_slice(b"impossible-through-public-grammar");

    assert_invalid(assembler.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    )));
    assert_invalid(
        assembler.push(ModelEvent::Usage(UsageUpdate::new(Usage::new(
            1, 0, 0, 0, 0,
        )))),
    );
    Ok(())
}

#[test]
fn reasoning_visibility_rejects_duplicate_known_and_replacement_terminally()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut duplicate, _digest, location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    duplicate.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    ))?;
    assert_invalid(duplicate.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    )));
    assert_invalid(
        duplicate.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
            location,
            b"cannot-repair".to_vec(),
        ))),
    );

    let (mut replacement, _digest, location) = assembler_with_block(BlockStart::Reasoning {
        visibility: ReasoningVisibility::Visible,
        replay: None,
    })?;
    assert_invalid(replacement.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    )));
    assert_invalid(replacement.push(ModelEvent::BlockCompleted(BlockCompleted::new(location))));
    Ok(())
}

#[test]
fn reasoning_visibility_rejects_wrong_kind_location_closed_and_post_terminal()
-> Result<(), Box<dyn std::error::Error>> {
    let (mut unknown, _digest, _location) =
        assembler_with_block(BlockStart::ReasoningPending { replay: None })?;
    assert_invalid(unknown.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(OutputLocation::new(0, 1), ReasoningVisibility::Visible),
    )));

    let (mut wrong_kind, _digest, location) =
        assembler_with_block(BlockStart::Text { phase: None })?;
    assert_invalid(wrong_kind.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    )));

    let (mut closed, _digest, location) = assembler_with_block(BlockStart::Reasoning {
        visibility: ReasoningVisibility::Omitted,
        replay: None,
    })?;
    closed.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    assert_invalid(closed.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    )));

    let digest = plan_digest()?;
    let mut terminal = ModelResponseAssembler::new(digest)?;
    terminal.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    complete_text_response(&mut terminal, digest, 43)?;
    assert_invalid(terminal.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(OutputLocation::new(0, 0), ReasoningVisibility::Omitted),
    )));
    Ok(())
}

#[test]
fn reasoning_visibility_binding_counts_as_an_event_without_block_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);
    let event_limits = AssemblerLimits::new(
        NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
    );
    let mut event_limited = ModelResponseAssembler::with_limits(digest, event_limits)?;
    event_limited.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    event_limited.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::ReasoningPending { replay: None },
    )))?;
    assert_invalid(event_limited.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Omitted),
    )));

    let byte_limits = AssemblerLimits::new(
        NonZeroUsize::new(5).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
    );
    let mut byte_limited = ModelResponseAssembler::with_limits(digest, byte_limits)?;
    byte_limited.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    byte_limited.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::ReasoningPending { replay: None },
    )))?;
    byte_limited.push(ModelEvent::ReasoningVisibilityBound(
        ReasoningVisibilityBound::new(location, ReasoningVisibility::Visible),
    ))?;
    byte_limited.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
        location,
        b"x".to_vec(),
    )))?;
    byte_limited.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    Ok(())
}

#[test]
fn late_reasoning_replay_event_round_trips_and_redacts_debug()
-> Result<(), Box<dyn std::error::Error>> {
    let location = OutputLocation::new(2, 3);
    let binding =
        ReasoningReplayBound::new(location, replay_envelope(r#"{"secret":"do-not-log"}"#)?);
    assert_eq!(binding.location(), location);
    assert_eq!(
        binding.replay().payload().get(),
        r#"{"secret":"do-not-log"}"#
    );

    let encoded = serde_json::to_string(&binding)?;
    let decoded: ReasoningReplayBound = serde_json::from_str(&encoded)?;
    assert_eq!(decoded, binding);

    let debug = format!("{binding:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("do-not-log"));
    Ok(())
}

#[test]
fn late_reasoning_replay_rejects_duplicate_and_replacement_terminally()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);
    let first = replay_envelope(r#"{"signature":"first"}"#)?;
    let replacement = replay_envelope(r#"{"signature":"replacement"}"#)?;

    let mut duplicate = ModelResponseAssembler::new(digest)?;
    duplicate.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    duplicate.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Reasoning {
            visibility: ReasoningVisibility::Summary,
            replay: None,
        },
    )))?;
    duplicate.push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
        location,
        first.clone(),
    )))?;
    let error = duplicate
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            first.clone(),
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    assert!(
        duplicate
            .push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
                location,
                b"summary".to_vec(),
            )))
            .is_err()
    );

    let mut initially_bound = ModelResponseAssembler::new(digest)?;
    initially_bound.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    initially_bound.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Reasoning {
            visibility: ReasoningVisibility::OpaqueReplay,
            replay: Some(first),
        },
    )))?;
    let error = initially_bound
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            replacement,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    assert!(
        initially_bound
            .push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))
            .is_err()
    );
    Ok(())
}

#[test]
fn late_reasoning_replay_rejects_wrong_kind_location_and_order()
-> Result<(), Box<dyn std::error::Error>> {
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);

    let mut unknown = ModelResponseAssembler::new(digest)?;
    unknown.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    let error = unknown
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            replay_envelope(r#"{"case":"unknown"}"#)?,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    assert!(
        unknown
            .push(ModelEvent::Usage(UsageUpdate::new(Usage::new(
                1, 0, 0, 0, 0
            ))))
            .is_err()
    );

    let mut wrong_kind = ModelResponseAssembler::new(digest)?;
    wrong_kind.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    wrong_kind.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Text { phase: None },
    )))?;
    let error = wrong_kind
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            replay_envelope(r#"{"case":"wrong-kind"}"#)?,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );

    let mut closed = ModelResponseAssembler::new(digest)?;
    closed.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    closed.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Reasoning {
            visibility: ReasoningVisibility::Omitted,
            replay: None,
        },
    )))?;
    closed.push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))?;
    let error = closed
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            replay_envelope(r#"{"case":"closed"}"#)?,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );

    let mut terminal = ModelResponseAssembler::new(digest)?;
    terminal.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    complete_text_response(&mut terminal, digest, 44)?;
    let error = terminal
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location,
            replay_envelope(r#"{"case":"terminal"}"#)?,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    Ok(())
}

#[test]
fn late_reasoning_replay_counts_events_and_payload_bytes() -> Result<(), Box<dyn std::error::Error>>
{
    let digest = plan_digest()?;
    let location = OutputLocation::new(0, 0);
    let event_limits = AssemblerLimits::new(
        NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::new(1024).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
    );
    let mut event_limited = ModelResponseAssembler::with_limits(digest, event_limits)?;
    event_limited.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    event_limited.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Reasoning {
            visibility: ReasoningVisibility::Omitted,
            replay: None,
        },
    )))?;
    assert!(
        event_limited
            .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
                location,
                replay_envelope(r#"{"limit":"event"}"#)?,
            ),))
            .is_err()
    );

    let replay = replay_envelope(r#"{"limit":"payload"}"#)?;
    let payload_bytes = replay.payload().get().len();
    let byte_limits = AssemblerLimits::new(
        NonZeroUsize::new(8).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
        NonZeroUsize::MIN,
        NonZeroUsize::new(payload_bytes).unwrap_or(NonZeroUsize::MIN),
        NonZeroUsize::MIN,
    );
    let mut byte_limited = ModelResponseAssembler::with_limits(digest, byte_limits)?;
    byte_limited.push(ModelEvent::ResponseStarted(ResponseStarted::new(
        None, digest,
    )))?;
    byte_limited.push(ModelEvent::BlockStarted(BlockStarted::new(
        entry(1)?,
        location,
        BlockStart::Reasoning {
            visibility: ReasoningVisibility::Summary,
            replay: None,
        },
    )))?;
    byte_limited.push(ModelEvent::ReasoningDelta(ReasoningDelta::new(
        location,
        b"x".to_vec(),
    )))?;
    let error = byte_limited
        .push(ModelEvent::ReasoningReplayBound(ReasoningReplayBound::new(
            location, replay,
        )))
        .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::InvalidEvent)
    );
    assert!(
        byte_limited
            .push(ModelEvent::BlockCompleted(BlockCompleted::new(location)))
            .is_err()
    );
    Ok(())
}

#[test]
fn unknown_event_debug_is_redacted() -> Result<(), Box<dyn std::error::Error>> {
    let event = ProviderEvent::new(
        "future-event",
        RawValue::from_string(String::from("{\"sensitive\":true}"))?,
    );
    let debug = format!("{event:?}");
    assert!(debug.contains("<redacted>"));
    assert!(!debug.contains("sensitive"));
    Ok(())
}
