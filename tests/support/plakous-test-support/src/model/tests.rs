use std::collections::BTreeMap;
use std::num::NonZeroU32;
use uuid::Uuid;

use futures_util::{FutureExt, StreamExt};

use super::*;
use plakous_model::{
    Actor, AttemptId, CancellationToken, Capability, CapabilityConstraints, CapabilitySet,
    CapacityCertificate, CapacityRule, ContentBlock, ContextContract, ContextEvidenceSource,
    ConversationItem, ConversationView, CountCertificateKind, DigestDomain, EntryId,
    EvidenceFreshness, FactProvenance, FactSource, FeatureSupport, FinishDetails, FinishReason,
    GenerationLimits, MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES,
    MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION, MeasurementCertificate, ModelAdapterProfileId,
    ModelCallRecoveryOutcome, ModelErrorContext, ModelErrorPhase, ModelId, ModelRetryability,
    ModelStreamItem, PREPARED_CALL_RECORD_SEMANTIC_REVISION, PROVIDER_OPERATION_SEMANTIC_REVISION,
    ProtocolId, Provenance, ProviderCallRecoveryLocatorStage, RequestMetadata, RouteId, RunId,
    SafeLimit, Support, TextBlock, Timestamp, TurnId, Usage, ValidationMode,
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

fn request(value: u128) -> Result<ModelRequest, Box<dyn std::error::Error>> {
    let selected = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"scripted-test-selected"],
    )?)?)?;
    let effective = serde_json::from_value(serde_json::to_value(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"scripted-test-effective"],
    )?)?)?;
    Ok(ModelRequest::builder(
        selected,
        effective,
        ConversationView::default(),
        RequestMetadata::for_turn(
            RunId::try_from(Uuid::from_u128(value))?,
            TurnId::try_from(Uuid::from_u128(value + 1))?,
            AttemptId::try_from(Uuid::from_u128(value + 2))?,
            Timestamp::new(1, 0)?,
        ),
    )
    .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
    .build()?)
}

fn authority(
    descriptor: &ResolvedModelDescriptor,
) -> Result<RuntimeModelAuthority, Box<dyn std::error::Error>> {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"script-account"])?;
    let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"script-credential"])?;
    Ok(RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        1,
        Timestamp::new(1, 0)?,
        None,
    )?)
}

fn model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = ModelScenario>,
) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
    let authority = authority(&descriptor)?;
    Ok(Arc::new(
        ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation()),
    ))
}

fn recovery_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = ModelScenario>,
    recovery: ScriptedRecoveryState,
) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
    let authority = authority(&descriptor)?;
    Ok(Arc::new(
        ScriptedModel::new(descriptor, scenarios)
            .with_runtime_authority(authority)
            .with_context_observation(context_observation())
            .with_call_recovery(recovery),
    ))
}

fn restarted_recovery_model(
    descriptor: &ResolvedModelDescriptor,
    recovery: &ScriptedRecoveryState,
) -> Result<Arc<ScriptedModel>, Box<dyn std::error::Error>> {
    let snapshot = recovery.snapshot()?;
    recovery_model(
        descriptor.clone(),
        [],
        ScriptedRecoveryState::from_snapshot(&snapshot)?,
    )
}

fn response(plan: Digest, label: &str) -> Result<ModelResponse, Box<dyn std::error::Error>> {
    let item = ConversationItem::new(
        EntryId::try_from(Uuid::from_u128(900))?,
        Actor::Assistant,
        vec![ContentBlock::Text(TextBlock::new(label, None)?)],
        Provenance::ModelOutput,
    )?;
    Ok(ModelResponse::new(
        vec![item],
        FinishDetails::new(FinishReason::Stop, None),
        Some(Usage::new(10, 2, 0, 0, 0)),
        None,
        plan,
    ))
}

fn prepare(
    model: &Arc<ScriptedModel>,
    request: ModelRequest,
) -> Result<(AttemptId, Digest, PreparedModelCall), Box<dyn std::error::Error>> {
    let attempt = request.metadata().attempt_id();
    let plan = model
        .descriptor()
        .validate_request(request, ValidationMode::Strict)?;
    let digest = plan.digest();
    let authority = model
        .runtime_authority
        .clone()
        .ok_or("scripted test authority missing")?;
    let prepared = Arc::clone(model).prepare_call(
        plan,
        ModelPreparationContext::new(authority, Timestamp::new(1, 0)?),
    )?;
    Ok((attempt, digest, prepared))
}

fn context(attempt: AttemptId, token: CancellationToken) -> ModelCallContext {
    ModelCallContext::new(attempt, token, None)
}

#[test]
fn expected_request_mismatch_is_typed_and_redacted() -> Result<(), Box<dyn std::error::Error>> {
    let model = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Exact(Box::new(request(10)?)),
            ScenarioStart::Immediate,
            vec![],
        )],
    )?;
    let (attempt, _, prepared) = prepare(&model, request(20)?)?;
    let result =
        futures::executor::block_on(prepared.start(context(attempt, CancellationToken::new())));
    let error = result.err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::ExpectedRequestMismatch)
    );
    assert!(
        !error
            .as_ref()
            .is_some_and(|error| error.message().contains("ConversationView"))
    );
    assert_eq!(model.remaining_scenarios(), 1);
    Ok(())
}

#[test]
fn cancellation_before_connect_is_terminal() -> Result<(), Box<dyn std::error::Error>> {
    let token = CancellationToken::new();
    token.cancel();
    let model = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![],
        )],
    )?;
    let (attempt, _, prepared) = prepare(&model, request(10)?)?;
    let result = futures::executor::block_on(prepared.start(context(attempt, token)));
    assert_eq!(
        result.err().as_ref().map(ModelError::kind),
        Some(ModelErrorKind::Cancelled)
    );
    Ok(())
}

#[test]
fn drop_before_start_has_no_dispatch_or_scenario_consumption()
-> Result<(), Box<dyn std::error::Error>> {
    let model = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let (_, _, prepared) = prepare(&model, request(10)?)?;
    drop(prepared);
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    assert_eq!(model.remaining_scenarios(), 1);
    Ok(())
}

#[test]
fn prepared_call_is_non_clone_and_consumed_once() -> Result<(), Box<dyn std::error::Error>> {
    trait AmbiguousIfClone<A> {
        fn marker() {}
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<u8> for T {}
    let _ = <PreparedModelCall as AmbiguousIfClone<_>>::marker;

    let model = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let (attempt, _, prepared) = prepare(&model, request(10)?)?;
    let _stream =
        futures::executor::block_on(prepared.start(context(attempt, CancellationToken::new())))?;
    assert_eq!(model.captured_records().len(), 1);
    assert_eq!(model.remaining_scenarios(), 0);
    Ok(())
}

#[test]
fn prepared_output_limit_can_be_smaller_than_the_request_bound()
-> Result<(), Box<dyn std::error::Error>> {
    let wire = NonZeroU32::new(64).ok_or("nonzero wire limit")?;
    let descriptor = descriptor()?;
    let request = request(10)?;
    assert_eq!(
        request.limits().max_output_tokens().map(NonZeroU32::get),
        Some(128)
    );
    let overridden = Arc::new(
        ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority(&descriptor)?)
            .with_context_observation(context_observation())
            .with_prepared_output_limit(wire),
    );
    let (_, _, prepared) = prepare(&overridden, request.clone())?;
    assert_eq!(
        prepared
            .record()
            .projection()
            .output_limit()
            .map(PreparedOutputLimit::tokens),
        Some(wire)
    );
    let baseline = Arc::new(
        ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority(&descriptor)?)
            .with_context_observation(context_observation()),
    );
    let (_, _, prepared) = prepare(&baseline, request)?;
    assert_eq!(
        prepared
            .record()
            .projection()
            .output_limit()
            .map(PreparedOutputLimit::tokens)
            .map(NonZeroU32::get),
        Some(128)
    );
    Ok(())
}

#[test]
fn inert_record_roundtrip_cannot_dispatch() -> Result<(), Box<dyn std::error::Error>> {
    let expected = request(10)?;
    let model = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Exact(Box::new(expected.clone())),
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let plan = model
        .descriptor()
        .validate_request(expected.clone(), ValidationMode::Strict)?;
    let (attempt, digest, prepared) = prepare(&model, expected)?;
    let encoded = serde_json::to_vec(prepared.record())?;
    let decoded: PreparedCallRecord = serde_json::from_slice(&encoded)?;
    let cloned = decoded.clone();
    assert_eq!(decoded, cloned);
    decoded.revalidate(model.descriptor(), &plan, Timestamp::new(1, 0)?)?;
    assert_eq!(model.captured_records(), [] as [PreparedCallRecord; 0]);
    assert_eq!(model.remaining_scenarios(), 1);
    let _stream =
        futures::executor::block_on(prepared.start(context(attempt, CancellationToken::new())))?;
    assert_eq!(model.captured_plans()[0].digest(), digest);
    Ok(())
}

#[test]
fn start_rejects_foreign_plan_before_consuming_scenario() -> Result<(), Box<dyn std::error::Error>>
{
    let first_descriptor = descriptor()?;
    let second_descriptor = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        first_descriptor.protocol().clone(),
        RouteId::new("other-route")?,
        first_descriptor.requested_model().clone(),
        None,
        None,
        first_descriptor.profile().clone(),
        first_descriptor.capabilities().clone(),
    );
    let foreign_plan = second_descriptor.validate_request(request(10)?, ValidationMode::Strict)?;
    let model = model(
        first_descriptor,
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let authority = model
        .runtime_authority
        .clone()
        .ok_or("scripted test authority missing")?;
    let result = Arc::clone(&model).prepare_call(
        foreign_plan,
        ModelPreparationContext::new(authority, Timestamp::new(1, 0)?),
    );
    assert!(matches!(result, Err(ModelPreparationError::Validation(_))));
    assert_eq!(model.remaining_scenarios(), 1);
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one contract test keeps locator bounds, scope, stage, redaction, and tamper cases together"
)]
fn recovery_locator_is_exact_versioned_redacted_and_tamper_evident()
-> Result<(), Box<dyn std::error::Error>> {
    let ordinary = model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
    )?;
    let (_, _, ordinary_prepared) = prepare(&ordinary, request(10)?)?;
    assert_eq!(
        ordinary_prepared.record().recovery_request(),
        Err(ModelCallRecoveryError::MissingLocator)
    );

    let shared = ScriptedRecoveryState::new();
    let exact_request = request(20)?;
    let recoverable = recovery_model(
        descriptor()?,
        [ModelScenario::new(
            ExpectedRequest::Exact(Box::new(exact_request.clone())),
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
        shared,
    )?;
    let (_, _, prepared) = prepare(&recoverable, exact_request)?;
    let record = prepared.record();
    let recovery = record.recovery_request()?;
    let locator = record.recovery_locator().ok_or("locator missing")?;
    assert_eq!(
        locator.availability_stage(),
        ProviderCallRecoveryLocatorStage::Prepared
    );
    assert_eq!(
        locator.operation_root(),
        record.projection().operation_root()
    );
    assert_eq!(
        record.semantic_revision(),
        PREPARED_CALL_RECORD_SEMANTIC_REVISION
    );
    assert_eq!(
        record.identity().version(),
        PROVIDER_OPERATION_SEMANTIC_REVISION
    );
    assert_eq!(recovery.prepared_call(), record.identity());
    assert_eq!(recovery.request(), record.projection().request());
    assert_eq!(recovery.plan(), record.projection().plan());
    assert_eq!(recovery.projection(), record.projection().identity());
    let exact_plan = recoverable
        .descriptor()
        .validate_request(request(20)?, ValidationMode::Strict)?;
    let expected_wire = serde_json::to_vec(&(
        exact_plan.request().model_input_identity(),
        exact_plan.decisions(),
        locator.provider_material(),
    ))?;
    assert_eq!(
        record.projection().wire_digest(),
        Digest::sha256(
            DigestDomain::Blob,
            1,
            &[b"plakous-model/scripted-wire-v1", &expected_wire],
        )?
    );
    let debug = format!("{locator:?}");
    assert!(debug.contains("redacted bytes"));
    assert!(!debug.contains(&format!("{:?}", locator.provider_material())));

    let learned = ProviderCallRecoveryLocator::learned_after_dispatch(
        record.projection(),
        NonZeroU32::new(2).ok_or("provider revision")?,
        [0_u8, 0xff, 0x80, 7],
    )?;
    assert_eq!(
        learned.availability_stage(),
        ProviderCallRecoveryLocatorStage::LearnedAfterDispatch
    );
    assert_eq!(learned.provider_material(), [0_u8, 0xff, 0x80, 7]);
    assert_eq!(
        record
            .recovery_request_with_locator(learned.clone())?
            .locator(),
        &learned
    );
    assert!(matches!(
        ModelStreamItem::RecoveryLocatorBound(learned.clone()),
        ModelStreamItem::RecoveryLocatorBound(bound) if bound == learned
    ));
    assert_eq!(
        ProviderCallRecoveryLocator::seal(record.projection(), NonZeroU32::MIN, [],),
        Err(ModelCallRecoveryError::InvalidProviderMaterial)
    );
    assert_eq!(
        ProviderCallRecoveryLocator::seal(
            record.projection(),
            NonZeroU32::MIN,
            vec![1; MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES + 1],
        ),
        Err(ModelCallRecoveryError::InvalidProviderMaterial)
    );
    let learned_value = serde_json::to_value(&learned)?;
    assert_eq!(
        serde_json::from_value::<ProviderCallRecoveryLocator>(learned_value.clone())?,
        learned
    );
    let mut stage_tamper = learned_value.clone();
    stage_tamper["availability_stage"] = serde_json::json!("Prepared");
    assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(stage_tamper).is_err());
    let mut root_tamper = learned_value.clone();
    root_tamper["operation_root"] = serde_json::json!({
        "SessionWorkflow": vec![3_u8; 32]
    });
    assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(root_tamper).is_err());
    let mut oversized = learned_value;
    oversized["provider_material"] =
        serde_json::json!(vec![1_u8; MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES + 1]);
    assert!(serde_json::from_value::<ProviderCallRecoveryLocator>(oversized).is_err());

    let encoded = serde_json::to_value(record)?;
    let decoded: PreparedCallRecord = serde_json::from_value(encoded.clone())?;
    assert_eq!(decoded, *record);
    decoded.recovery_request()?.validate_for_record(&decoded)?;

    let mut receipt = encoded.clone();
    let changed = receipt["recovery_locator"]["provider_material"][0]
        .as_u64()
        .map_or(1, |value| (value + 1) % 256);
    receipt["recovery_locator"]["provider_material"][0] = serde_json::json!(changed);
    assert!(serde_json::from_value::<PreparedCallRecord>(receipt).is_err());
    let mut revision = encoded.clone();
    revision["semantic_revision"] = serde_json::json!(3);
    assert!(serde_json::from_value::<PreparedCallRecord>(revision).is_err());
    let mut request_scope = encoded.clone();
    request_scope["recovery_locator"]["request"]["bytes"][0] = serde_json::json!(255);
    assert!(serde_json::from_value::<PreparedCallRecord>(request_scope).is_err());
    let mut plan_scope = encoded.clone();
    plan_scope["recovery_locator"]["plan"]["bytes"][0] = serde_json::json!(255);
    assert!(serde_json::from_value::<PreparedCallRecord>(plan_scope).is_err());
    let mut projection_scope = encoded.clone();
    projection_scope["recovery_locator"]["projection"]["bytes"][0] = serde_json::json!(255);
    assert!(serde_json::from_value::<PreparedCallRecord>(projection_scope).is_err());
    let mut provider_revision = encoded;
    provider_revision["recovery_locator"]["provider_revision"] = serde_json::json!(2);
    assert!(serde_json::from_value::<PreparedCallRecord>(provider_revision).is_err());

    let foreign = ResolvedModelDescriptor::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        recoverable.descriptor().protocol().clone(),
        RouteId::new("foreign-route")?,
        recoverable.descriptor().requested_model().clone(),
        None,
        None,
        recoverable.descriptor().profile().clone(),
        recoverable.descriptor().capabilities().clone(),
    );
    assert!(recovery.validate_for_descriptor(&foreign).is_err());
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one restart matrix proves every closed scripted recovery classification and no duplicate start"
)]
fn scripted_recovery_survives_restart_and_starts_each_locator_at_most_once()
-> Result<(), Box<dyn std::error::Error>> {
    let shared = ScriptedRecoveryState::new();
    let descriptor = descriptor()?;

    let not_dispatched_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
        shared.clone(),
    )?;
    let (_, _, not_dispatched) = prepare(&not_dispatched_model, request(100)?)?;
    let not_dispatched_request = not_dispatched.record().recovery_request()?;
    let restarted = restarted_recovery_model(&descriptor, &shared)?;
    let recovered = futures::executor::block_on(crate::recover_model_operation(
        restarted,
        not_dispatched_request.clone(),
    ))?;
    assert!(matches!(
        recovered.outcome(),
        ModelCallRecoveryOutcome::ProvenNotDispatched
    ));
    assert_eq!(shared.start_count(&not_dispatched_request), 0);
    let unsupported = model(descriptor.clone(), [])?;
    let unavailable_in_ordinary_script =
        futures::executor::block_on(unsupported.recover_call(not_dispatched_request.clone()))?;
    assert!(matches!(
        unavailable_in_ordinary_script.outcome(),
        ModelCallRecoveryOutcome::Unsupported
    ));

    let cancelled_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
        shared.clone(),
    )?;
    let (cancelled_attempt, _, cancelled) = prepare(&cancelled_model, request(200)?)?;
    let cancelled_request = cancelled.record().recovery_request()?;
    let token = CancellationToken::new();
    token.cancel();
    let error =
        futures::executor::block_on(cancelled.start(context(cancelled_attempt, token))).err();
    assert_eq!(
        error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::Cancelled)
    );
    let restarted = restarted_recovery_model(&descriptor, &shared)?;
    let recovered = futures::executor::block_on(
        Arc::clone(&restarted).recover_call(cancelled_request.clone()),
    )?;
    assert!(matches!(
        recovered.outcome(),
        ModelCallRecoveryOutcome::ProvenNotDispatched
    ));
    assert_eq!(shared.start_count(&cancelled_request), 0);

    let barrier = ScenarioBarrier::new();
    let ambiguous_input = request(300)?;
    let ambiguous_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Exact(Box::new(ambiguous_input.clone())),
            ScenarioStart::Barrier(barrier),
            vec![ScenarioStep::Eof],
        )],
        shared.clone(),
    )?;
    let (ambiguous_attempt, _, ambiguous) = prepare(&ambiguous_model, ambiguous_input.clone())?;
    let ambiguous_request = ambiguous.record().recovery_request()?;
    assert!(
        ambiguous
            .start(context(ambiguous_attempt, CancellationToken::new()))
            .now_or_never()
            .is_none()
    );
    let restarted = restarted_recovery_model(&descriptor, &shared)?;
    let recovered = futures::executor::block_on(
        Arc::clone(&restarted).recover_call(ambiguous_request.clone()),
    )?;
    assert!(matches!(
        recovered.outcome(),
        ModelCallRecoveryOutcome::Ambiguous
    ));
    assert_eq!(shared.start_count(&ambiguous_request), 1);

    let duplicate_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
        shared.clone(),
    )?;
    let (duplicate_attempt, _, duplicate) = prepare(&duplicate_model, ambiguous_input)?;
    let duplicate_error = futures::executor::block_on(
        duplicate.start(context(duplicate_attempt, CancellationToken::new())),
    )
    .err();
    assert_eq!(
        duplicate_error.as_ref().map(ModelError::kind),
        Some(ModelErrorKind::OutcomeUnknown)
    );
    assert_eq!(shared.start_count(&ambiguous_request), 1);

    let completed_input = request(400)?;
    let completed_plan = descriptor
        .validate_request(completed_input.clone(), ValidationMode::Strict)?
        .digest();
    let completed_response = response(completed_plan, "completed")?;
    let completed_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::PlanDigest(completed_plan),
            ScenarioStart::Immediate,
            vec![ScenarioStep::Emit(ModelEvent::ResponseCompleted(
                completed_response.clone(),
            ))],
        )],
        shared.clone(),
    )?;
    let (completed_attempt, _, completed) = prepare(&completed_model, completed_input)?;
    let completed_request = completed.record().recovery_request()?;
    futures::executor::block_on(async {
        let mut stream = completed
            .start(context(completed_attempt, CancellationToken::new()))
            .await?;
        while let Some(event) = stream.next().await {
            event?;
        }
        Ok::<(), ModelError>(())
    })?;
    let restarted = restarted_recovery_model(&descriptor, &shared)?;
    let recovered = futures::executor::block_on(
        Arc::clone(&restarted).recover_call(completed_request.clone()),
    )?;
    match recovered.outcome() {
        ModelCallRecoveryOutcome::Completed(response) => {
            assert_eq!(response.as_ref(), &completed_response);
        }
        outcome => panic!("unexpected completed recovery outcome: {outcome:?}"),
    }
    assert_eq!(shared.start_count(&completed_request), 1);

    let failed_input = request(500)?;
    let definite = ModelError::new(ModelErrorKind::Protocol, "definite scripted failure");
    let failed_model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Error(definite.clone())],
        )],
        shared.clone(),
    )?;
    let (failed_attempt, _, failed) = prepare(&failed_model, failed_input)?;
    let failed_request = failed.record().recovery_request()?;
    let observed = futures::executor::block_on(async {
        let mut stream = failed
            .start(context(failed_attempt, CancellationToken::new()))
            .await?;
        stream.next().await.transpose()
    });
    assert_eq!(
        observed.err().as_ref().map(ModelError::kind),
        Some(ModelErrorKind::Protocol)
    );
    let restarted = restarted_recovery_model(&descriptor, &shared)?;
    let recovered = futures::executor::block_on(restarted.recover_call(failed_request.clone()))?;
    match recovered.outcome() {
        ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
            assert_eq!(error.as_ref(), &definite);
        }
        outcome => panic!("unexpected failed recovery outcome: {outcome:?}"),
    }
    assert_eq!(shared.start_count(&failed_request), 1);
    Ok(())
}

#[test]
fn recovered_terminal_values_are_exact_and_tamper_evident() -> Result<(), Box<dyn std::error::Error>>
{
    let shared = ScriptedRecoveryState::new();
    let descriptor = descriptor()?;
    let input = request(600)?;
    let plan = descriptor
        .validate_request(input.clone(), ValidationMode::Strict)?
        .digest();
    let model = recovery_model(
        descriptor.clone(),
        [ModelScenario::new(
            ExpectedRequest::Any,
            ScenarioStart::Immediate,
            vec![ScenarioStep::Eof],
        )],
        shared,
    )?;
    let (_, _, prepared) = prepare(&model, input)?;
    let request = prepared.record().recovery_request()?;
    let completed = ModelCallRecoveryResult::completed(
        &request,
        response(plan, "exact")?,
        Some(Usage::new(10, 2, 0, 0, 0)),
        None,
    )?;
    let encoded = serde_json::to_value(&completed)?;
    let decoded: ModelCallRecoveryResult = serde_json::from_value(encoded.clone())?;
    decoded.validate_for_request(&request)?;
    assert_eq!(
        decoded.identity().version(),
        PROVIDER_OPERATION_SEMANTIC_REVISION
    );
    assert_eq!(
        encoded["semantic_revision"],
        serde_json::json!(MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION)
    );
    let mut tampered = encoded;
    tampered["usage"]["output_tokens"] = serde_json::json!(3);
    assert!(serde_json::from_value::<ModelCallRecoveryResult>(tampered).is_err());

    let foreign = response(
        Digest::sha256(DigestDomain::RequestPlan, 99, &[b"foreign-plan"])?,
        "foreign",
    )?;
    assert!(ModelCallRecoveryResult::completed(&request, foreign, None, None).is_err());
    assert!(
        ModelCallRecoveryResult::definitely_failed(
            &request,
            ModelError::new(ModelErrorKind::OutcomeUnknown, "ambiguous"),
            None,
            None,
        )
        .is_err()
    );
    let nonterminal = ModelError::contextual(
        ModelErrorKind::Protocol,
        "not proved permanent",
        ModelErrorContext::new(
            descriptor.protocol().clone(),
            descriptor.route().clone(),
            descriptor.requested_model().clone(),
            ModelErrorPhase::Terminal,
            ModelRetryability::Unknown,
        )?,
    )?;
    assert!(ModelCallRecoveryResult::definitely_failed(&request, nonterminal, None, None).is_err());
    Ok(())
}
