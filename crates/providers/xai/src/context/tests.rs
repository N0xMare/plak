#![allow(
    clippy::too_many_lines,
    reason = "Recompute every live Grok wire occurrence against the enrolled input bound and disprove a guessed output cap."
)]
use super::*;
use serde_json::Value;

fn contract(
    model: &str,
    reported: u64,
) -> Result<Option<ContextContract>, Box<dyn std::error::Error>> {
    Ok(build_context_contract(
        model,
        reported,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?,
        Timestamp::new(1_700_000_000, 0)?,
        Timestamp::new(1_700_003_600, 0)?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"catalog"])?,
    )?)
}

#[test]
fn only_enrolled_literals_with_a_reported_window_have_a_contract()
-> Result<(), Box<dyn std::error::Error>> {
    for model in [XAI_GROK_BUILD_MODEL, "grok-latest", "grok-4.7-latest"] {
        assert!(contract(model, 500_000)?.is_none(), "{model}");
    }
    // A catalog row with no window gets no guessed one.
    assert!(contract(XAI_GROK_47_CODING_MODEL, 0)?.is_none());
    Ok(())
}

/// The window is the catalog's, in both directions: grok-4.7 reports
/// 256k and runs at 256k; a larger report is not cut to a reviewed cap.
#[test]
fn grok_47_window_is_the_catalog_window() -> Result<(), Box<dyn std::error::Error>> {
    let facts: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/context/grok-4.7-enrollment.json"
    ))?;
    assert_eq!(facts["revision"], XAI_GROK_47_ENROLLMENT_REVISION);
    assert_eq!(facts["model"], XAI_GROK_47_CODING_MODEL);
    for (model, reported) in [
        (XAI_GROK_47_CODING_MODEL, 256_000),
        (XAI_GROK_47_CODING_MODEL, 1_000_000),
        (XAI_GROK_47_FAST_CODING_MODEL, 500_000),
    ] {
        let contract = contract(model, reported)?.ok_or("missing Grok 4.7 contract")?;
        assert_eq!(
            contract.capacity().rule(),
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(reported),
                maximum_projected_input: reported,
                auto_compact_input: None,
            }
        );
        assert!(matches!(contract.capacity().evidence_class(),
            plakous_model::ContextEvidenceClass::ReferenceClientEnvelope { catalog_receipt, .. }
            if *catalog_receipt == Digest::sha256(DigestDomain::ModelBinding, 1, &[b"catalog"])?));
    }
    Ok(())
}

#[test]
fn only_pinned_effective_row_builds_reference_client_envelope_input_contract()
-> Result<(), Box<dyn std::error::Error>> {
    let account = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"account"])?;
    let credential = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential"])?;
    let observed_at = Timestamp::new(1_700_000_000, 0)?;
    let expires_at = Timestamp::new(1_700_003_600, 0)?;
    let receipt = Digest::sha256(DigestDomain::ModelBinding, 1, &[b"catalog"])?;
    let contract = build_context_contract(
        XAI_GROK_PRIMARY_CODING_MODEL,
        500_000,
        account,
        credential,
        observed_at,
        expires_at,
        receipt,
    )?
    .ok_or("grok-4.5 omitted reference-client-envelope contract")?;
    assert_eq!(
        contract.capacity().rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(500_000),
            maximum_projected_input: 500_000,
            auto_compact_input: Some(400_000),
        }
    );
    assert!(matches!(
        contract.capacity().evidence_class(),
        plakous_model::ContextEvidenceClass::ReferenceClientEnvelope {
            catalog_receipt,
            reference_revision
        } if *catalog_receipt == receipt && reference_revision == GROK_REFERENCE_REVISION
    ));
    assert!(matches!(
        contract.measurements()[0].kind(),
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt }
            if catalog_receipt == receipt
    ));
    let grok_46 = build_context_contract(
        XAI_GROK_46_CODING_MODEL,
        500_000,
        account,
        credential,
        observed_at,
        expires_at,
        receipt,
    )?
    .ok_or("grok-4.6 omitted reference-client-envelope contract")?;
    assert_eq!(
        grok_46.capacity().rule(),
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(500_000),
            maximum_projected_input: 500_000,
            auto_compact_input: Some(400_000),
        }
    );
    // The pinned resolver has env/user/remote precedence and only a final
    // 85% fallback for grok-build-0.1; no immutable effective row proves it.
    assert!(
        build_context_contract(
            XAI_GROK_BUILD_MODEL,
            256_000,
            account,
            credential,
            observed_at,
            expires_at,
            receipt,
        )?
        .is_none()
    );
    Ok(())
}

#[test]
fn the_grok_build_compact_trigger_follows_the_catalog_window()
-> Result<(), Box<dyn std::error::Error>> {
    for reported in [499_999, 750_000] {
        let contract = contract(XAI_GROK_PRIMARY_CODING_MODEL, reported)?
            .ok_or("catalog evidence lost enrollment")?;
        assert_eq!(
            contract.capacity().rule(),
            CapacityRule::ProviderReservedInput {
                reported_context_window: Some(reported),
                maximum_projected_input: reported,
                auto_compact_input: Some(reported * 80 / 100),
            }
        );
    }
    Ok(())
}

#[test]
fn checked_in_live_observations_recompute_input_bounds_and_disprove_output_cap()
-> Result<(), Box<dyn std::error::Error>> {
    let corpus: Value = serde_json::from_slice(PROOF_CORPUS)?;
    let observations = corpus
        .get("live_observations")
        .and_then(Value::as_array)
        .ok_or("Grok corpus omitted live observations")?;
    assert_ne!(observations.as_slice(), [] as [Value; 0]);
    assert_eq!(
        corpus.get("route").and_then(Value::as_str),
        Some("xai.grok-subscription.responses.2026-07")
    );
    // This recorded corpus predates tool-result image admission. Its wire
    // shapes retain the same coefficients under the expanded estimator.
    assert_eq!(
        corpus.get("accounting_revision").and_then(Value::as_str),
        Some("responses-wire-accounting-v3")
    );
    assert_eq!(
        corpus.get("estimator_revision").and_then(Value::as_str),
        Some("grok-responses-v2")
    );
    for observation in observations {
        let wire_size = observation
            .get("wire_size")
            .and_then(Value::as_u64)
            .ok_or("live observation omitted wire size")?;
        let occurrences = observation
            .get("occurrences")
            .and_then(Value::as_object)
            .ok_or("live observation omitted occurrences")?;
        let mut recomputed = wire_size + FIXED_MARGIN;
        for (name, occurrence) in [
            ("message", ResponsesWireOccurrence::Message),
            ("input_text", ResponsesWireOccurrence::InputText),
            ("output_text", ResponsesWireOccurrence::OutputText),
            ("refusal", ResponsesWireOccurrence::Refusal),
            ("function_call", ResponsesWireOccurrence::FunctionCall),
            (
                "function_call_output",
                ResponsesWireOccurrence::FunctionCallOutput,
            ),
            ("function_tool", ResponsesWireOccurrence::FunctionTool),
            ("schema_node", ResponsesWireOccurrence::SchemaNode),
            ("reasoning_replay", ResponsesWireOccurrence::ReasoningReplay),
            (
                "reasoning_summary",
                ResponsesWireOccurrence::ReasoningSummary,
            ),
            ("opaque_replay", ResponsesWireOccurrence::OpaqueReplay),
        ] {
            recomputed += occurrences
                .get(name)
                .and_then(Value::as_u64)
                .ok_or("live observation omitted an occurrence count")?
                * occurrence_margin(occurrence);
        }
        assert_eq!(
            observation.get("input_upper_bound").and_then(Value::as_u64),
            Some(recomputed)
        );
        let usage = observation
            .get("usage")
            .and_then(Value::as_object)
            .ok_or("live observation omitted usage")?;
        assert!(
            usage
                .get("input_tokens")
                .and_then(Value::as_u64)
                .is_some_and(|tokens| tokens <= recomputed)
        );
        let output_maximum = observation
            .get("output_maximum_field")
            .and_then(Value::as_u64)
            .ok_or("live observation omitted output maximum")?;
        let output_tokens = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .ok_or("live observation omitted output usage")?;
        let reasoning_tokens = usage
            .get("reasoning_tokens")
            .and_then(Value::as_u64)
            .ok_or("live observation omitted reasoning usage")?;
        assert!(reasoning_tokens <= output_tokens);
        assert_eq!(
            observation
                .get("total_output_reservation_holds")
                .and_then(Value::as_bool),
            Some(output_tokens <= output_maximum)
        );
    }
    for model in [XAI_GROK_BUILD_MODEL, XAI_GROK_PRIMARY_CODING_MODEL] {
        assert!(observations.iter().any(|observation| {
            observation.get("model").and_then(Value::as_str) == Some(model)
                && observation
                    .get("usage")
                    .and_then(|usage| usage.get("output_tokens"))
                    .and_then(Value::as_u64)
                    .zip(
                        observation
                            .get("output_maximum_field")
                            .and_then(Value::as_u64),
                    )
                    .is_some_and(|(output, maximum)| output > maximum)
        }));
    }
    Ok(())
}
