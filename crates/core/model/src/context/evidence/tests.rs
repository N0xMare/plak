use super::super::tests::{authority, timestamp};
use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one adversarial matrix proves every recomputed evidence-class forgery fails"
)]
fn forged_recomputed_wire_cannot_bypass_evidence_class_invariants() {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
    let authority_binding = authority(b"authority");
    let receipt = authority(b"receipt");
    let valid_capacity = CapacityCertificate::reference_client_envelope(
        &StandardIntegrationAuthority::release_owned(),
        "reference-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        authority_binding,
        freshness,
        receipt,
        "reference-v1",
    )
    .expect("capacity");

    let mut forged_capacity = serde_json::to_value(&valid_capacity).expect("capacity wire");
    forged_capacity["non_guarantee_disposition"] = serde_json::Value::Null;
    let forged_capacity_content = CapacityCertificateContent {
        evidence_id: "reference-capacity",
        revision: "v1",
        source: ContextEvidenceSource::AuthenticatedDiscovery,
        rule: CapacityRule::ProviderReservedInput {
            reported_context_window: Some(125),
            maximum_projected_input: 100,
            auto_compact_input: None,
        },
        authority_binding: Some(authority_binding),
        freshness,
        evidence_class: valid_capacity.evidence_class(),
        non_guarantee_disposition: None,
    };
    forged_capacity["identity"] = serde_json::to_value(
        semantic_digest(DigestDomain::ContextBudget, 3, &forged_capacity_content)
            .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<CapacityCertificate>(forged_capacity).is_err());

    let valid_measurement = MeasurementCertificate::standard(
        &StandardIntegrationAuthority::release_owned(),
        "reference-measurement",
        "v1",
        "complete-input-bound",
        "v1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
            catalog_receipt: receipt,
        },
        Some(authority_binding),
        freshness,
    )
    .expect("measurement");
    let mut forged_measurement =
        serde_json::to_value(&valid_measurement).expect("measurement wire");
    forged_measurement["source"] =
        serde_json::to_value(ContextEvidenceSource::BuiltInCatalog).expect("source wire");
    forged_measurement["authority_binding"] = serde_json::Value::Null;
    let forged_measurement_content = MeasurementCertificateContent {
        certificate_id: "reference-measurement",
        revision: "v1",
        method: "complete-input-bound",
        method_revision: "v1",
        source: ContextEvidenceSource::BuiltInCatalog,
        kind: valid_measurement.kind(),
        authority_binding: None,
        freshness,
    };
    forged_measurement["identity"] = serde_json::to_value(
        semantic_digest(
            DigestDomain::TokenMeasurement,
            3,
            &forged_measurement_content,
        )
        .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<MeasurementCertificate>(forged_measurement).is_err());

    let valid_contract = ContextContract::new(
        ProtocolId::new("responses-family").expect("protocol"),
        "accounting-v1",
        "projection-v1",
        "no-output-v1",
        valid_capacity,
        vec![valid_measurement],
    )
    .expect("contract");
    let mut forged_contract = serde_json::to_value(&valid_contract).expect("contract wire");
    forged_contract["measurements"] = serde_json::json!([]);
    let empty_measurements: Vec<MeasurementCertificate> = Vec::new();
    let forged_contract_content = ContextContractContent {
        protocol_family: valid_contract.protocol_family(),
        accounting_revision: valid_contract.accounting_revision(),
        projection_revision: valid_contract.projection_revision(),
        reservation_revision: valid_contract.reservation_revision(),
        capacity: valid_contract.capacity(),
        measurements: &empty_measurements,
    };
    forged_contract["identity"] = serde_json::to_value(
        semantic_digest(DigestDomain::ContextBudget, 4, &forged_contract_content)
            .expect("forged identity"),
    )
    .expect("identity wire");
    assert!(serde_json::from_value::<ContextContract>(forged_contract).is_err());
}
