use std::collections::BTreeMap;

use super::*;
use crate::{
    AttemptId, ConversationView, EffectiveProviderConfigurationId, IdentityError, RequestMetadata,
    RunId, SelectedProviderConfigurationId, Timestamp, TurnId,
};
use uuid::Uuid;

fn descriptor(reasoning: Support) -> Result<ResolvedModelDescriptor, IdentityError> {
    let timestamp = Timestamp::new(1, 0).map_err(|_| IdentityError::InvalidNanoseconds)?;
    let provenance = FactProvenance::new(FactSource::TestFixture, timestamp, None);
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            provenance,
        ),
    );
    features.insert(
        Capability::Reasoning,
        FeatureSupport::new(reasoning, CapabilityConstraints::default(), provenance),
    );
    Ok(ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test-protocol")?,
        RouteId::new("test-route")?,
        ModelId::new("test-model")?,
        None,
        None,
        ModelAdapterProfileId::new("test-profile")?,
        CapabilitySet::new(features, BTreeMap::new()),
    ))
}

fn request(requirement: CapabilityRequirement) -> Result<ModelRequest, Box<dyn std::error::Error>> {
    request_with(requirement, ConversationView::default(), 3)
}

fn request_with(
    requirement: CapabilityRequirement,
    input: ConversationView,
    attempt: u128,
) -> Result<ModelRequest, Box<dyn std::error::Error>> {
    let metadata = RequestMetadata::for_turn(
        RunId::try_from(Uuid::from_u128(1))?,
        TurnId::try_from(Uuid::from_u128(2))?,
        AttemptId::try_from(Uuid::from_u128(attempt))?,
        Timestamp::new(1, 0)?,
    );
    let selected = SelectedProviderConfigurationId::new(Digest::sha256(
        DigestDomain::SelectedProviderConfiguration,
        5,
        &[b"descriptor-test-selected"],
    )?)?;
    let effective = EffectiveProviderConfigurationId::new(Digest::sha256(
        DigestDomain::EffectiveProviderConfiguration,
        5,
        &[b"descriptor-test-effective"],
    )?)?;
    Ok(ModelRequest::builder(selected, effective, input, metadata)
        .reasoning(ReasoningPolicy::Enabled {
            effort: Some(crate::ReasoningEffort::Low),
            requirement,
        })
        .build()?)
}

#[test]
fn strict_rejects_preferred_downgrade() -> Result<(), Box<dyn std::error::Error>> {
    let error = descriptor(Support::Unsupported)?
        .validate_request(
            request(CapabilityRequirement::Preferred)?,
            ValidationMode::Strict,
        )
        .err();
    assert!(matches!(
        error,
        Some(ValidationError::StrictPreference { .. })
    ));
    Ok(())
}

#[test]
fn explicit_mode_records_ordered_downgrade() -> Result<(), Box<dyn std::error::Error>> {
    let plan = descriptor(Support::Unsupported)?.validate_request(
        request(CapabilityRequirement::Preferred)?,
        ValidationMode::AllowDowngrade,
    )?;
    assert_eq!(plan.decisions()[1].outcome(), CapabilityOutcome::Dropped);
    assert_eq!(plan.diagnostics().len(), 1);
    assert_eq!(plan.digest().domain(), DigestDomain::RequestPlan);
    Ok(())
}

/// A view of one committed text segment and one image segment, the image
/// committed or still working.
fn image_view(
    image: &[u8],
    image_committed: bool,
    limits: crate::WorkingProjectionLimits,
) -> Result<ConversationView, Box<dyn std::error::Error>> {
    use crate::{
        Actor, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
        HistorySegmentKind, ImageBlock, MediaSource, Provenance, TextBlock, WorkingProjection,
    };
    let item = |id: u128, block: ContentBlock| {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(id))?,
            Actor::User,
            vec![block],
            Provenance::UserInput,
        )
        .map_err(Box::<dyn std::error::Error>::from)
    };
    let text = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![item(10, ContentBlock::Text(TextBlock::new("look", None)?))?],
    )?;
    let picture = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![item(
            11,
            ContentBlock::Image(ImageBlock::new(
                "image/png",
                MediaSource::bytes(image.to_vec())?,
            )?),
        )?],
    )?;
    let (committed, working) = if image_committed {
        (vec![text, picture], Vec::new())
    } else {
        (vec![text], vec![picture])
    };
    Ok(ConversationView::from_shared(
        CanonicalHistory::new(committed)?,
        WorkingProjection::new(working, limits)?,
    ))
}

#[test]
fn plan_digest_binds_request_content_without_reserializing_it()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = descriptor(Support::Native)?;
    let limits = crate::WorkingProjectionLimits::default();
    let plan = |input: ConversationView, attempt: u128| {
        descriptor
            .validate_request(
                request_with(CapabilityRequirement::Required, input, attempt)?,
                ValidationMode::Strict,
            )
            .map(|plan| plan.digest())
            .map_err(Box::<dyn std::error::Error>::from)
    };
    let image = b"\x89PNG\r\n\x1a\n\x00\x01";
    let base = plan(image_view(image, true, limits)?, 3)?;
    assert_eq!(base.domain(), DigestDomain::RequestPlan);
    assert_eq!(base.version(), 4);
    assert_eq!(
        base.to_string(),
        "0dbb6ad2a1c08772367f672c758845efa803207c36af3e6d59596e1fc8ce8e55",
        "the plan digest is part of the runtime format epoch"
    );
    assert_eq!(plan(image_view(image, true, limits)?, 3)?, base);

    let mut flipped = *image;
    flipped[9] ^= 1;
    assert_ne!(plan(image_view(&flipped, true, limits)?, 3)?, base);
    assert_ne!(plan(image_view(image, false, limits)?, 3)?, base);
    let narrower = crate::WorkingProjectionLimits::new(8, 64, 1024 * 1024)?;
    assert_ne!(
        plan(image_view(image, false, narrower)?, 3)?,
        plan(image_view(image, false, limits)?, 3)?
    );
    assert_ne!(plan(image_view(image, true, limits)?, 4)?, base);

    let request = request_with(
        CapabilityRequirement::Required,
        image_view(image, false, narrower)?,
        3,
    )?;
    let restored: ModelRequest = serde_json::from_slice(&serde_json::to_vec(&request)?)?;
    assert_eq!(
        descriptor
            .validate_request(restored, ValidationMode::Strict)?
            .digest(),
        descriptor
            .validate_request(request, ValidationMode::Strict)?
            .digest()
    );
    Ok(())
}

#[test]
fn required_is_never_dropped() -> Result<(), Box<dyn std::error::Error>> {
    let error = descriptor(Support::Unsupported)?
        .validate_request(
            request(CapabilityRequirement::Required)?,
            ValidationMode::AllowDowngrade,
        )
        .err();
    assert!(matches!(
        error,
        Some(ValidationError::RequiredCapability { .. })
    ));
    Ok(())
}

#[test]
fn same_served_models_bind_only_when_declared() -> Result<(), Box<dyn std::error::Error>> {
    let plain = descriptor(Support::Native)?;
    let encoded = serde_json::to_value(&plain)?;
    assert!(encoded.get("same_served_models").is_none());
    let unchanged = plain.clone().with_same_served_models([]);
    assert_eq!(
        unchanged.semantic_binding_digest()?,
        plain.semantic_binding_digest()?
    );

    let declared = plain.clone().with_same_served_models([
        ModelId::new("served-b")?,
        ModelId::new("served-a")?,
        ModelId::new("served-b")?,
    ]);
    assert_eq!(
        declared
            .same_served_models()
            .iter()
            .map(ModelId::as_str)
            .collect::<Vec<_>>(),
        ["served-a", "served-b"]
    );
    assert_ne!(
        declared.semantic_binding_digest()?,
        plain.semantic_binding_digest()?
    );
    assert!(declared.serves_same_model(&ModelId::new("served-a")?));
    assert!(declared.serves_same_model(plain.requested_model()));
    assert!(!declared.serves_same_model(&ModelId::new("served-c")?));
    assert!(!plain.serves_same_model(&ModelId::new("served-a")?));

    let mut encoded = serde_json::to_value(&declared)?;
    assert_eq!(
        serde_json::from_value::<ResolvedModelDescriptor>(encoded.clone())?,
        declared
    );
    encoded["same_served_models"] = serde_json::json!(["served-b", "served-a"]);
    assert!(serde_json::from_value::<ResolvedModelDescriptor>(encoded).is_err());
    Ok(())
}

#[test]
fn foreign_model_binding_rejects_prepared_plan() -> Result<(), Box<dyn std::error::Error>> {
    let first = descriptor(Support::Native)?;
    let plan = first.validate_request(
        request(CapabilityRequirement::Preferred)?,
        ValidationMode::Strict,
    )?;
    let second = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        first.protocol().clone(),
        RouteId::new("other-route")?,
        first.requested_model().clone(),
        first.reported_model().cloned(),
        first.model_family().cloned(),
        first.profile().clone(),
        first.capabilities().clone(),
    );
    assert_eq!(
        second.validate_plan_binding(&plan),
        Err(ValidationError::ForeignPlan)
    );
    Ok(())
}

#[test]
fn semantic_binding_ignores_fact_freshness_but_binds_behavior()
-> Result<(), Box<dyn std::error::Error>> {
    let original = descriptor(Support::Native)?;
    let mut refreshed_features = original.capabilities().features().clone();
    for fact in refreshed_features.values_mut() {
        *fact = FeatureSupport::new(
            fact.support(),
            fact.constraints().clone(),
            FactProvenance::new(
                FactSource::TestFixture,
                Timestamp::new(20, 0)?,
                Some(Timestamp::new(30, 0)?),
            ),
        );
    }
    let refreshed = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        original.protocol().clone(),
        original.route().clone(),
        original.requested_model().clone(),
        original.reported_model().cloned(),
        original.model_family().cloned(),
        original.profile().clone(),
        CapabilitySet::new(refreshed_features.clone(), BTreeMap::new()),
    );
    assert_ne!(original.binding_digest()?, refreshed.binding_digest()?);
    assert_eq!(
        original.semantic_binding_digest()?,
        refreshed.semantic_binding_digest()?
    );

    let reasoning = refreshed_features
        .get_mut(&Capability::Reasoning)
        .expect("reasoning fact");
    *reasoning = FeatureSupport::new(
        Support::Degraded,
        reasoning.constraints().clone(),
        reasoning.provenance(),
    );
    let changed = ResolvedModelDescriptor::new(
        &StandardIntegrationAuthority::release_owned(),
        original.protocol().clone(),
        original.route().clone(),
        original.requested_model().clone(),
        original.reported_model().cloned(),
        original.model_family().cloned(),
        original.profile().clone(),
        CapabilitySet::new(refreshed_features, BTreeMap::new()),
    );
    assert_ne!(
        original.semantic_binding_digest()?,
        changed.semantic_binding_digest()?
    );
    Ok(())
}

#[test]
fn exact_admission_rejects_substituted_request() -> Result<(), Box<dyn std::error::Error>> {
    let descriptor = descriptor(Support::Native)?;
    let submitted = request(CapabilityRequirement::Preferred)?;
    let substituted = request(CapabilityRequirement::Required)?;
    let plan = descriptor.validate_request(substituted, ValidationMode::Strict)?;
    assert_eq!(
        descriptor.validate_plan_admission(&submitted, &plan),
        Err(ValidationError::SubstitutedRequest)
    );
    Ok(())
}
