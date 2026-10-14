//! Provider-neutral execution used by deterministic conformance suites.

use std::sync::Arc;

use futures_util::StreamExt;

use plakous_model::{
    LanguageModel, ModelCallContext, ModelCallRecoveryError, ModelCallRecoveryRequest,
    ModelCallRecoveryResult, ModelError, ModelEvent, ModelPreparationContext,
    ModelPreparationError, ModelRequest, ModelResponse, ResolvedModelDescriptor, ValidationMode,
};

/// Runs and validates one read-only provider-call recovery classification.
///
/// This helper never prepares or starts a model call. Ambiguous, unsupported,
/// and unavailable results remain classifications and cannot authorize replay.
///
/// # Errors
///
/// Returns provider recovery construction errors or rejects a result bound to
/// another request, plan, response, usage fact, or evidence receipt.
pub async fn recover_model_operation(
    model: Arc<dyn LanguageModel>,
    request: ModelCallRecoveryRequest,
) -> Result<ModelCallRecoveryResult, ModelCallRecoveryError> {
    let result = model.recover_call(request.clone()).await?;
    result.validate_for_request(&request)?;
    Ok(result)
}

/// Runs one prepared model operation to its self-contained terminal aggregate.
///
/// This helper owns no retry, tool, ledger, or transport policy. It exists so
/// every [`LanguageModel`] implementation can be exercised through the exact
/// public seam with identical terminal and EOF classification. Nonterminal
/// events are bounded observations, not construction authority for the result;
/// observation loss or a projector gap therefore cannot invalidate an exact
/// terminal aggregate.
///
/// # Errors
///
/// Returns preparation/start/stream errors unchanged, rejects a terminal bound
/// to another prepared plan or any event after terminal, and returns an
/// incomplete-response error for EOF before the unique terminal aggregate.
pub async fn run_model_operation(
    model: Arc<dyn LanguageModel>,
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    context: ModelCallContext,
) -> Result<ModelResponse, ModelError> {
    let prepared_at = request.metadata().created_at();
    let plan = descriptor
        .validate_request(request, ValidationMode::Strict)
        .map_err(ModelError::from)?;
    let plan_digest = plan.digest();
    let authority = Arc::clone(&model)
        .revalidate_runtime_authority(prepared_at)
        .await
        .map_err(ModelPreparationError::from)
        .map_err(ModelError::from)?;
    let prepared = Arc::clone(&model)
        .prepare_call(plan, ModelPreparationContext::new(authority, prepared_at))
        .map_err(ModelError::from)?;
    let mut stream = prepared.start(context).await?;
    let mut completed = None;

    while let Some(event) = stream.next().await {
        let event = event?;
        if completed.is_some() {
            return Err(ModelError::new(
                plakous_model::ModelErrorKind::InvalidEvent,
                "model operation produced an event after its terminal aggregate",
            ));
        }
        let ModelEvent::ResponseCompleted(response) = event else {
            continue;
        };
        response.validate_for_commit()?;
        if response.plan_digest() != plan_digest {
            return Err(ModelError::new(
                plakous_model::ModelErrorKind::InvalidEvent,
                "model terminal aggregate does not match the prepared request plan",
            ));
        }
        completed = Some(response);
    }
    completed.ok_or_else(ModelError::incomplete)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
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
        let freshness =
            EvidenceFreshness::new(Timestamp::new(1, 0).expect("observed timestamp"), None)
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
        CountObservation::new("scripted-counter", "scripted-exact-v1", 64)
            .expect("context observation")
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
        let credential =
            Digest::sha256(DigestDomain::ModelBinding, 1, &[b"conformance-credential"])?;
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
    fn shared_runner_classifies_complete_error_eof_and_cancel()
    -> Result<(), Box<dyn std::error::Error>> {
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
}
