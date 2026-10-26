//! Reference-envelope counts must remain usable when fresh compaction cannot reuse wire items.

use super::*;

fn reference_descriptor() -> ResolvedModelDescriptor {
    let freshness = EvidenceFreshness::new(timestamp(1), Some(timestamp(100))).expect("freshness");
    let account = authority_class(authority(b"account"));
    let receipt = authority(b"reference-receipt");
    let capacity = CapacityCertificate::reference_client_envelope(
        &StandardIntegrationAuthority::release_owned(),
        "reference-capacity",
        "v1",
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(50_000),
            maximum_projected_input: 47_500,
            auto_compact_input: Some(45_000),
        },
        account,
        freshness,
        receipt,
        "reference-v1",
    )
    .expect("reference capacity");
    let certificate = MeasurementCertificate::standard(
        &StandardIntegrationAuthority::release_owned(),
        "reference-counter",
        "v1",
        "complete-input",
        "v1",
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound {
            catalog_receipt: receipt,
        },
        Some(account),
        freshness,
    )
    .expect("reference measurement");
    descriptor(
        CapacityRule::SharedWindow {
            total: SafeLimit::Exact(1),
        },
        measurement_certificate(CountCertificateKind::ExactTokenizer),
    )
    .with_context_contract(
        ContextContract::new(
            ProtocolId::new("responses-family").expect("family"),
            "accounting-v1",
            "projection-v1",
            "reservation-v1",
            capacity,
            vec![certificate],
        )
        .expect("reference contract"),
    )
}

fn summary_status(
    descriptor: &ResolvedModelDescriptor,
    input: &ContextWireInput,
    measured: u64,
    anchor: Option<&ContextRequestAnchor>,
) -> ContextStatus {
    let request = request(None);
    prepared_status(
        descriptor,
        &request,
        authority(b"account"),
        Some(CountObservation::new("reference-counter", "v1", measured).expect("count")),
        timestamp(2),
    )
    .evaluate_wire_input(
        &request,
        input,
        anchor,
        descriptor.semantic_binding_digest().expect("binding"),
        true,
    )
    .expect("summary admission")
}

#[test]
fn inflated_encrypted_replay_and_fresh_portable_summary_still_admit() {
    let descriptor = reference_descriptor();
    let source = serde_json::json!({"input": [
        {"type":"reasoning", "encrypted_content":"A".repeat(114_000)},
        {"role":"user", "content":"x".repeat(60_000)}
    ]});
    let native = wire_input(&source);
    let reference_tokens = serde_json::to_vec(&source).expect("wire").len().div_ceil(4) as u64;
    let provider_input = 26_000;
    assert!(
        reference_tokens > provider_input * 16 / 10 && reference_tokens < provider_input * 17 / 10
    );
    assert!(native.upper_bound() > 47_500);
    // A fresh native request keeps the ciphertext; its admitted reference
    // envelope is still below the window even without a reusable item anchor.
    let status = summary_status(&descriptor, &native, reference_tokens, None);
    assert!(
        matches!(status.kind(), ContextStatusKind::Fits { .. }),
        "{:?}",
        status.kind()
    );
    assert!(
        !status.uses_unanchored_estimate(),
        "a required bound is not estimate-only admission"
    );
    let anchor = ContextRequestAnchor::new(
        descriptor.semantic_binding_digest().expect("binding"),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        crate::Usage::new(provider_input, 0, 0, 0, 0),
        native,
    )
    .expect("successful response");
    // Removing private replay and rendering the remaining content as a fresh
    // summarizer input invalidates item digests, not the route's counter.
    let portable = serde_json::json!({"input":[{"role":"user","content":[{"type":"input_text","text":"x".repeat(60_000)}]}]});
    let input = wire_input(&portable);
    assert!(input.upper_bound() > 47_500);
    let measured = serde_json::to_vec(&portable)
        .expect("wire")
        .len()
        .div_ceil(4) as u64;
    let status = summary_status(&descriptor, &input, measured, Some(&anchor));
    assert!(
        matches!(status.kind(), ContextStatusKind::Fits { .. }),
        "{:?}",
        status.kind()
    );
    assert_eq!(
        status.non_guarantee_disposition(),
        Some(ContextNonGuaranteeDisposition::ReferenceClientOperatingEnvelope)
    );
    let overflow = summary_status(&descriptor, &input, 48_000, None);
    assert!(
        !matches!(overflow.kind(), ContextStatusKind::Fits { .. }),
        "the real reference ceiling still applies"
    );
}

#[test]
fn incident_regime_native_refuses_but_depth_one_portable_summary_admits() {
    let descriptor = reference_descriptor();
    let ceiling = 47_500;
    let provider_input = ceiling * 92 / 100;
    let reference_tokens = provider_input * 166 / 100;
    assert!(reference_tokens > ceiling);
    // The successful ordinary request and fresh native lowering render the
    // same replay differently, so the fresh items miss the request anchor.
    let original = wire_input(&serde_json::json!({"input":[
        {"type":"reasoning", "encrypted_content":"A".repeat(228_168)},
        {"role":"assistant", "content":"x".repeat(62_000)}
    ]}));
    let anchor = ContextRequestAnchor::new(
        descriptor.semantic_binding_digest().expect("binding"),
        descriptor.route().clone(),
        descriptor.requested_model().clone(),
        crate::Usage::new(provider_input, 0, 0, 0, 0),
        original,
    )
    .expect("successful response");
    let native = wire_input(&serde_json::json!({"input":[
        {"type":"reasoning", "encrypted_content":"A".repeat(228_168), "id":"fresh"},
        {"role":"assistant", "content":[{"type":"output_text", "text":"x".repeat(62_000)}]}
    ]}));
    let refused = summary_status(&descriptor, &native, reference_tokens, Some(&anchor));
    assert!(!matches!(refused.kind(), ContextStatusKind::Fits { .. }));
    // Depth one drops private replay, not the visible source text. Its fresh
    // rendering still exceeds the byte-only bound but fits the reference one.
    let portable = serde_json::json!({"input":[{"role":"user","content":[{"type":"input_text","text":"x".repeat(62_000)}]}]});
    let measured = serde_json::to_vec(&portable)
        .expect("wire")
        .len()
        .div_ceil(4) as u64;
    let portable = wire_input(&portable);
    assert!(portable.upper_bound() > ceiling);
    let admitted = summary_status(&descriptor, &portable, measured, Some(&anchor));
    assert!(matches!(admitted.kind(), ContextStatusKind::Fits { .. }));
    assert!(!admitted.uses_unanchored_estimate());
}

#[test]
fn reference_envelope_does_not_replace_the_dialects_image_token_ceiling() {
    let descriptor = reference_descriptor();
    let input = ContextWireInput::measure_wire(
        &serde_json::to_vec(&serde_json::json!({"input":[{"role":"user","content":[{"type":"input_image","image_url":"data:image/png;base64,AAAA"}]}]})).expect("wire"),
        Some(50_000),
    ).expect("bounded image");
    let status = summary_status(&descriptor, &input, 100, None);
    assert!(!matches!(status.kind(), ContextStatusKind::Fits { .. }));
}
