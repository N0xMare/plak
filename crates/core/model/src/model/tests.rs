use std::collections::BTreeMap;
use std::num::NonZeroU32;

use super::*;
use crate::{
    BranchId, Digest, DigestDomain, ModelAdapterProfileId, ModelId, ProtocolId,
    ResolvedModelDescriptor, SessionId,
};

fn authority_descriptor() -> Result<ResolvedModelDescriptor, crate::IdentityError> {
    Ok(ResolvedModelDescriptor::new(
        &crate::StandardIntegrationAuthority::release_owned(),
        ProtocolId::new("test-protocol")?,
        RouteId::new("test-route")?,
        ModelId::new("test-model")?,
        None,
        None,
        ModelAdapterProfileId::new("test-adapter")?,
        crate::CapabilitySet::new(BTreeMap::new(), BTreeMap::new()),
    ))
}

fn authority_binding(label: &'static [u8]) -> Result<Digest, crate::DigestError> {
    Digest::sha256(DigestDomain::ModelBinding, 1, &[label])
}

fn assert_object_safe(_: Option<&dyn LanguageModel>) {}

#[test]
fn language_model_is_object_safe() {
    assert_object_safe(None);
}

#[test]
fn cancellation_future_is_woken() {
    let token = CancellationToken::new();
    let second = token.clone();
    token.cancel();
    futures::executor::block_on(second.cancelled());
    assert!(second.is_cancelled());
}

#[test]
fn authority_identity_is_clock_independent_but_generation_exact()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = authority_descriptor()?;
    let account = authority_binding(b"account-a")?;
    let credential = authority_binding(b"credential-a")?;
    let first = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        7,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(10, 0)?),
    )?;
    let later = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        7,
        Timestamp::new(2, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    assert_eq!(first.stable_identity()?, later.stable_identity()?);
    assert_eq!(
        first.authority_class_binding()?,
        later.authority_class_binding()?
    );
    assert!(first.same_selected_authority(&later));
    later.validate(&descriptor, Timestamp::new(3, 0)?)?;

    let rotated = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        8,
        Timestamp::new(2, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    assert_ne!(first.stable_identity()?, rotated.stable_identity()?);
    assert_eq!(
        first.authority_class_binding()?,
        rotated.authority_class_binding()?
    );
    assert!(!first.same_selected_authority(&rotated));

    let foreign_account = RuntimeModelAuthority::new(
        &descriptor,
        authority_binding(b"account-b")?,
        credential,
        7,
        Timestamp::new(2, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?;
    assert_ne!(
        first.authority_class_binding()?,
        foreign_account.authority_class_binding()?
    );
    assert_eq!(
        first.validate(&descriptor, Timestamp::new(10, 0)?),
        Err(RuntimeModelAuthorityError::Stale)
    );
    Ok(())
}

#[test]
fn admission_witness_is_bounded_integrity_checked_and_execution_exact()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = authority_descriptor()?;
    let account = authority_binding(b"account-a")?;
    let credential = authority_binding(b"credential-a")?;
    let witness = RuntimeAdmissionWitness::new(
        "test.selected-catalog-row",
        1,
        br#"{"model":"test-model","context_length":4096}"#.to_vec(),
    )?;
    assert_eq!(witness.kind(), "test.selected-catalog-row");
    assert_eq!(witness.revision(), 1);
    assert!(!format!("{witness:?}").contains("context_length"));

    let base = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        7,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(10, 0)?),
    )?;
    let admitted = base.clone().with_admission_witness(witness.clone());
    let refreshed = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        7,
        Timestamp::new(2, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?
    .with_admission_witness(witness.clone());
    assert!(admitted.same_selected_authority(&refreshed));
    assert_eq!(admitted.stable_identity()?, refreshed.stable_identity()?);
    assert_eq!(
        admitted.authority_class_binding()?,
        base.authority_class_binding()?,
        "provider receipt evidence must not create an authority-class cycle"
    );
    assert_ne!(admitted.stable_identity()?, base.stable_identity()?);

    let changed = RuntimeModelAuthority::new(
        &descriptor,
        account,
        credential,
        7,
        Timestamp::new(2, 0)?,
        Some(Timestamp::new(20, 0)?),
    )?
    .with_admission_witness(RuntimeAdmissionWitness::new(
        "test.selected-catalog-row",
        1,
        br#"{"model":"test-model","context_length":4095}"#.to_vec(),
    )?);
    assert!(!admitted.same_selected_authority(&changed));
    assert_ne!(admitted.stable_identity()?, changed.stable_identity()?);

    let encoded = serde_json::to_value(&witness)?;
    assert_eq!(
        serde_json::from_value::<RuntimeAdmissionWitness>(encoded.clone())?,
        witness
    );
    let mut tampered = encoded;
    tampered["payload"][0] = serde_json::json!(0);
    assert!(serde_json::from_value::<RuntimeAdmissionWitness>(tampered).is_err());
    assert_eq!(
        RuntimeAdmissionWitness::new("", 1, vec![1]),
        Err(RuntimeModelAuthorityError::Invalid)
    );
    assert_eq!(
        RuntimeAdmissionWitness::new("test", 0, vec![1]),
        Err(RuntimeModelAuthorityError::Invalid)
    );
    assert_eq!(
        RuntimeAdmissionWitness::new("test", 1, vec![0; MAX_RUNTIME_ADMISSION_WITNESS_BYTES + 1],),
        Err(RuntimeModelAuthorityError::Invalid)
    );
    Ok(())
}

#[test]
fn cache_affinity_roundtrip_recomputes_binding_and_redacts_debug()
-> Result<(), Box<dyn std::error::Error>> {
    let affinity = ModelCacheAffinity::from_runtime_entropy(
        SessionId::try_from(uuid::Uuid::from_u128(10))?,
        BranchId::try_from(uuid::Uuid::from_u128(11))?,
        3,
        [7_u8; 32],
    )?;
    assert_eq!(affinity.identity().domain(), DigestDomain::CacheAffinity);
    assert!(!format!("{affinity:?}").contains("070707"));
    let encoded = serde_json::to_value(&affinity)?;
    assert_eq!(
        serde_json::from_value::<ModelCacheAffinity>(encoded.clone())?,
        affinity
    );
    let mut tampered = encoded;
    tampered["epoch"] = serde_json::json!(4);
    assert!(serde_json::from_value::<ModelCacheAffinity>(tampered).is_err());
    Ok(())
}

#[test]
fn release_verified_output_hard_cap_binds_stable_exact_scope()
-> Result<(), Box<dyn std::error::Error>> {
    let descriptor = authority_descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        authority_binding(b"account-a")?,
        authority_binding(b"credential-a")?,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(10, 0)?),
    )?;
    let context = ModelPreparationContext::new(authority, Timestamp::new(2, 0)?);
    let certificate = Digest::sha256(DigestDomain::ContextBudget, 1, &[b"hard-cap"])?;
    let limit = PreparedOutputLimit::release_verified_hard_cap(
        NonZeroU32::new(64).ok_or("nonzero output")?,
        certificate,
        &descriptor,
        &context,
        descriptor.protocol(),
        "lowering-1",
        "projection-1",
        "accounting-1",
    )?;
    assert!(matches!(
        limit.source(),
        PreparedOutputLimitSource::ReleaseVerifiedHardCap {
            certificate: found,
            ..
        } if *found == certificate
    ));
    assert_eq!(
        serde_json::from_slice::<PreparedOutputLimit>(&serde_json::to_vec(&limit)?)?,
        limit
    );
    Ok(())
}

#[test]
fn active_family_artifact_roundtrips_tamper_checks_and_stays_redacted()
-> Result<(), Box<dyn std::error::Error>> {
    const SECRET_BYTES: &[u8] = b"OPAQUE_FAMILY_SECRET_SENTINEL";
    let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"])?;
    let artifact = ActiveFamilyArtifact::new("responses", 3, 3, binding, SECRET_BYTES)?;
    assert_eq!(artifact.family(), "responses");
    assert_eq!(artifact.schema_revision(), 3);
    assert_eq!(artifact.codec_revision(), 3);
    assert_eq!(artifact.binding(), binding);
    assert_eq!(artifact.bytes(), SECRET_BYTES);
    assert_eq!(artifact.integrity().domain(), DigestDomain::Blob);

    let encoded = serde_json::to_value(&artifact)?;
    assert_eq!(
        serde_json::from_value::<ActiveFamilyArtifact>(encoded.clone())?,
        artifact
    );
    let debug = format!("{artifact:?}");
    assert!(debug.contains("<redacted;"));
    assert!(!debug.contains("OPAQUE_FAMILY_SECRET_SENTINEL"));

    let mut tampered = encoded;
    tampered["bytes"][0] = serde_json::json!(0);
    assert!(serde_json::from_value::<ActiveFamilyArtifact>(tampered).is_err());
    Ok(())
}

#[test]
fn active_family_artifact_enforces_revision_domain_and_size_bounds()
-> Result<(), Box<dyn std::error::Error>> {
    let binding = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"binding"])?;
    let wrong = Digest::sha256(DigestDomain::RequestPlan, 1, &[b"binding"])?;
    assert_eq!(
        ActiveFamilyArtifact::new("responses", 0, 1, binding, [1_u8]),
        Err(FamilyArtifactError::ZeroRevision)
    );
    assert_eq!(
        ActiveFamilyArtifact::new("responses", 1, 1, wrong, [1_u8]),
        Err(FamilyArtifactError::InvalidDigest { field: "binding" })
    );
    assert_eq!(
        ActiveFamilyArtifact::new("responses", 1, 1, binding, Vec::<u8>::new()),
        Err(FamilyArtifactError::EmptyBytes)
    );
    assert_eq!(
        ActiveFamilyArtifact::new(
            "responses",
            1,
            1,
            binding,
            vec![0_u8; MAX_FAMILY_ARTIFACT_BYTES + 1],
        ),
        Err(FamilyArtifactError::BytesTooLarge {
            maximum: MAX_FAMILY_ARTIFACT_BYTES,
        })
    );
    assert!(ActiveFamilyArtifact::new("provider name", 1, 1, binding, [1_u8]).is_err());
    Ok(())
}
