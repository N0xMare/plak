use std::collections::BTreeMap;
use std::num::NonZeroU32;

use uuid::Uuid;

use super::*;
use crate::{ExpectedRequest, ModelScenario, ScenarioStart, ScenarioStep, ScriptedModel};
use plakous_model::{
    Actor, AttemptId, BlockStart, BlockStarted, CancellationToken, Capability,
    CapabilityConstraints, CapabilitySet, CapacityCertificate, CapacityRule, ContentBlock,
    ContextContract, ContextEvidenceSource, ConversationItem, ConversationView,
    CountCertificateKind, CountObservation, Digest, DigestDomain, EntryId, EvidenceFreshness,
    FactProvenance, FactSource, FeatureSupport, FinishDetails, FinishReason, GenerationLimits,
    MeasurementCertificate, ModelAdapterProfileId, ModelErrorKind, ModelId, OutputLocation,
    ProtocolId, Provenance, RequestMetadata, ResolvedModelDescriptor, ResponseStarted, RouteId,
    RunId, SafeLimit, Support, TextBlock, Timestamp, TurnId, ValidationMode,
};

fn context_contract() -> ContextContract {
    let freshness = EvidenceFreshness::new(Timestamp::new(1, 0).expect("observed timestamp"), None)
        .expect("context freshness");
    ContextContract::new(
        ProtocolId::new("test-protocol").expect("protocol family"),
        "scripted-accounting-v1",
        "scripted-projection-v1",
        "scripted-reservation-v1",
        CapacityCertificate::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            "scripted-capacity",
            "scripted-capacity-v1",
            ContextEvidenceSource::BuiltInCatalog,
            CapacityRule::SharedWindow {
                total: SafeLimit::Exact(1_000),
            },
            None,
            freshness,
        )
        .expect("capacity certificate"),
        vec![
            MeasurementCertificate::standard(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                "scripted-counter",
                "scripted-exact-v1",
                "scripted-complete-projection",
                "scripted-method-v1",
                ContextEvidenceSource::BuiltInCatalog,
                CountCertificateKind::ExactTokenizer,
                None,
                freshness,
            )
            .expect("measurement certificate"),
        ],
    )
    .expect("context contract")
}

fn context_observation() -> CountObservation {
    CountObservation::new("scripted-counter", "scripted-exact-v1", 64).expect("context observation")
}

fn descriptor() -> Result<ResolvedModelDescriptor, plakous_model::IdentityError> {
    let observed_at = Timestamp::new(1, 0)?;
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::TestFixture, observed_at, None),
        ),
    );
    Ok(ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test-protocol")?,
        RouteId::new("test-route")?,
        ModelId::new("test-model")?,
        None,
        None,
        ModelAdapterProfileId::new("test-profile")?,
        CapabilitySet::new(features, BTreeMap::new()),
    )
    .with_context_contract(context_contract()))
}

fn request() -> Result<ModelRequest, Box<dyn std::error::Error>> {
    request_with_attempt(3)
}

fn request_with_attempt(value: u128) -> Result<ModelRequest, Box<dyn std::error::Error>> {
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"conformance-test-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"conformance-test-effective"],
    )?)?)?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::default(),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(1))?,
            TurnId::try_from(Uuid::from_u128(2))?,
            AttemptId::try_from(Uuid::from_u128(value))?,
            Timestamp::new(1, 0)?,
        ),
    )
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
    .build()?)
}

fn scripted(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = ModelScenario>,
) -> Result<Arc<dyn LanguageModel>, Box<dyn std::error::Error>> {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"conformance-account"])?;
    let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"conformance-credential"])?;
    let authority = plakous_model::RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?;
    Ok(Arc::new(
        ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation()),
    ))
}

fn context(attempt: AttemptId, token: CancellationToken) -> ModelCallContext {
    ModelCallContext::new(attempt, token, None)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one shared-runner regression classifies every terminal stream outcome"
)]
fn shared_runner_classifies_complete_error_eof_and_cancel() -> Result<(), Box<dyn std::error::Error>>
{
    let descriptor = descriptor()?;
    let request = request()?;
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(5))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new("complete", None)?)],
        Provenance::ModelOutput,
    )?;
    let response = ModelResponse::new(
        vec![item.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    let complete = scripted(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::PlanDigest(digest),
            ScenarioStart::Immediate,
            vec![
                ScenarioStep::Emit(ModelEvent::ResponseStarted(ResponseStarted::new(
                    None, digest,
                ))),
                ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                    item.id(),
                    OutputLocation::new(0, 0),
                    BlockStart::Complete(item.blocks()[0].clone()),
                ))),
                ScenarioStep::Emit(ModelEvent::ResponseCompleted(response.clone())),
            ],
        )],
    )?;
    let result = futures::executor::block_on(run_model_operation(
        complete,
        &descriptor,
        request.clone(),
        context(request.metadata().attempt_id(), CancellationToken::new()),
    ))?;
    assert_eq!(result, response);

    let terminal_only = scripted(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::PlanDigest(digest),
            ScenarioStart::Immediate,
            vec![ScenarioStep::Emit(ModelEvent::ResponseCompleted(
                response.clone(),
            ))],
        )],
    )?;
    let result = futures::executor::block_on(run_model_operation(
        terminal_only,
        &descriptor,
        request.clone(),
        context(request.metadata().attempt_id(), CancellationToken::new()),
    ))?;
    assert_eq!(result, response);

    let failure = scripted(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Error(ModelError::new(
                ModelErrorKind::Timeout,
                "timed out",
            ))],
        )],
    )?;
    let error = futures::executor::block_on(run_model_operation(
        failure,
        &descriptor,
        request.clone(),
        context(request.metadata().attempt_id(), CancellationToken::new()),
    ))
    .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::Timeout)
    );

    let eof = scripted(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let error = futures::executor::block_on(run_model_operation(
        eof,
        &descriptor,
        request.clone(),
        context(request.metadata().attempt_id(), CancellationToken::new()),
    ))
    .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::IncompleteResponse)
    );

    let token = CancellationToken::new();
    token.cancel();
    let cancelled = scripted(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            Vec::new(),
        )],
    )?;
    let error = futures::executor::block_on(run_model_operation(
        cancelled,
        &descriptor,
        request.clone(),
        context(request.metadata().attempt_id(), token),
    ))
    .err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::Cancelled)
    );
    Ok(())
}
