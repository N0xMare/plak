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

fn measured_token_projection() -> Result<PreparedTokenProjection, Box<dyn std::error::Error>> {
    let descriptor = authority_descriptor()?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        authority_binding(b"account-a")?,
        authority_binding(b"credential-a")?,
        1,
        Timestamp::new(1, 0)?,
        Some(Timestamp::new(100, 0)?),
    )?;
    let context = ModelPreparationContext::new(authority, Timestamp::new(2, 0)?);
    let messages = (0..200)
        .map(|index| serde_json::json!({"role": "user", "content": index.to_string()}))
        .collect::<Vec<_>>();
    let body = serde_json::to_vec(&serde_json::json!({"messages": messages}))?;
    let digest = Digest::sha256(DigestDomain::Blob, 1, &[&body])?;
    Ok(
        PreparedTokenProjection::new(digest, u64::try_from(body.len())?, None)?
            .with_wire_input(&body, None, &context)?,
    )
}

#[test]
fn persisted_token_projection_references_its_items_and_bounds_them_as_embedded()
-> Result<(), Box<dyn std::error::Error>> {
    let tokens = measured_token_projection()?;
    let encoded = serde_json::to_vec(&tokens)?;
    let text = std::str::from_utf8(&encoded)?;
    assert!(!text.contains("conversation_index") && !text.contains("\"wire_input\":"));
    assert!(
        encoded.len() < 2048,
        "{} bytes for 200 items",
        encoded.len()
    );

    // The record bound is exactly the length the item list adds when embedded.
    let mut embedded = serde_json::to_value(&tokens)?;
    let fields = embedded.as_object_mut().ok_or("projection object")?;
    fields.remove("wire_input_identity").ok_or("reference")?;
    fields.insert(
        "wire_input".to_owned(),
        serde_json::to_value(tokens.wire_input())?,
    );
    let embedded = serde_json::to_vec(&embedded)?;
    assert!(embedded.len() > encoded.len() + 200 * 100);
    assert_eq!(tokens.embedded_record_bytes(encoded.len())?, embedded.len());

    let decoded: PreparedTokenProjection = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, tokens);
    assert_eq!(decoded.identity(), tokens.identity());
    assert_eq!(serde_json::to_vec(&decoded)?, encoded);
    assert!(decoded.wire_input().is_none());
    assert!(decoded.is_recorded() && !tokens.is_recorded());
    assert_eq!(decoded.embedded_record_bytes(7)?, 7);
    Ok(())
}

#[test]
fn persisted_token_projection_refuses_an_inconsistent_reference()
-> Result<(), Box<dyn std::error::Error>> {
    let tokens = measured_token_projection()?;
    let encoded = serde_json::to_value(&tokens)?;
    let unmeasured = PreparedTokenProjection::new(tokens.wire_digest(), tokens.wire_size(), None)?;
    let blob = Digest::sha256(DigestDomain::Blob, 1, &[b"not a measurement"])?;
    let old_version = Digest::sha256(DigestDomain::TokenMeasurement, 4, &[b"items"])?;
    let other_domain = Digest::sha256(DigestDomain::ContextAdmission, 5, &[b"items"])?;
    let anchor = Digest::sha256(DigestDomain::TokenMeasurement, 5, &[b"anchor"])?;
    let cases = [
        // A measured identity cannot pass as an unmeasured projection's.
        ("wire_input_identity", serde_json::Value::Null),
        ("wire_input_identity", serde_json::to_value(blob)?),
        ("wire_input_identity", serde_json::to_value(old_version)?),
        ("wire_input_identity", serde_json::to_value(other_domain)?),
        ("identity", serde_json::to_value(unmeasured.identity())?),
        ("request_anchor_identity", serde_json::to_value(blob)?),
        // The identity, the item reference and the anchor reference name
        // distinct objects; a swap that makes two coincide is refused.
        ("identity", encoded["wire_input_identity"].clone()),
        (
            "request_anchor_identity",
            encoded["wire_input_identity"].clone(),
        ),
        ("request_anchor_identity", encoded["identity"].clone()),
    ];
    for (field, value) in cases {
        let mut tampered = encoded.clone();
        tampered[field] = value;
        assert!(
            serde_json::from_value::<PreparedTokenProjection>(tampered).is_err(),
            "{field}"
        );
    }
    // Alone, a measured projection cannot re-derive its identity; the
    // enclosing record's identity authenticates these fields.
    let accepted = serde_json::from_value::<PreparedTokenProjection>({
        let mut anchored = encoded.clone();
        anchored["request_anchor_identity"] = serde_json::to_value(anchor)?;
        anchored
    })?;
    assert!(accepted.is_recorded());

    // An unmeasured projection carries no anchor or bound authority.
    let plain = serde_json::to_value(&unmeasured)?;
    assert!(serde_json::from_value::<PreparedTokenProjection>(plain.clone()).is_ok());
    for (field, value) in [
        ("request_anchor_identity", serde_json::to_value(anchor)?),
        ("require_input_bound", serde_json::Value::Bool(true)),
    ] {
        let mut tampered = plain.clone();
        tampered[field] = value;
        assert!(
            serde_json::from_value::<PreparedTokenProjection>(tampered).is_err(),
            "unmeasured {field}"
        );
    }
    Ok(())
}

#[test]
fn the_record_bound_admits_exactly_its_maximum() {
    assert!(!recovery::exceeds_record_bound(
        MAX_PREPARED_CALL_RECORD_BYTES
    ));
    assert!(recovery::exceeds_record_bound(
        MAX_PREPARED_CALL_RECORD_BYTES + 1
    ));
}

#[test]
fn an_unmeasured_projection_counts_its_absent_list_as_the_embedded_encoding_did()
-> Result<(), Box<dyn std::error::Error>> {
    let tokens = PreparedTokenProjection::new(
        Digest::sha256(DigestDomain::Blob, 1, &[b"unmeasured body"])?,
        15,
        None,
    )?;
    let encoded = serde_json::to_vec(&tokens)?;
    let mut embedded = serde_json::to_value(&tokens)?;
    let fields = embedded.as_object_mut().ok_or("projection object")?;
    fields.remove("wire_input_identity").ok_or("reference")?;
    fields.insert("wire_input".to_owned(), serde_json::Value::Null);
    assert_eq!(
        tokens.embedded_record_bytes(encoded.len())?,
        serde_json::to_vec(&embedded)?.len()
    );
    Ok(())
}
