use std::num::NonZeroU32;

use super::*;

#[test]
fn attempt_summary_survives_a_bounded_diagnostic() {
    let suffix = " [4 requests; last: DNS lookup failed, nothing sent; retry declined: deadline]";
    let error =
        ModelError::new(ModelErrorKind::Transport, "x".repeat(2048)).with_attempt_summary(suffix);
    assert!(error.message().ends_with(suffix));
    assert!(error.message().len() <= crate::MAX_SAFE_DIAGNOSTIC_BYTES);
}

#[test]
fn protected_history_preparation_is_typed_persisted_and_nonretryable()
-> Result<(), Box<dyn std::error::Error>> {
    let error = ModelError::from(ModelPreparationError::rejected(
        ModelPreparationErrorKind::ProtectedHistory,
        "native history cannot be replayed",
    ));
    assert_eq!(error.kind(), ModelErrorKind::ProtectedHistory);
    let encoded = serde_json::to_vec(&error)?;
    assert_eq!(serde_json::from_slice::<ModelError>(&encoded)?, error);
    let context = ModelErrorContext::new(
        ProtocolId::new("test.responses")?,
        RouteId::new("test.route")?,
        ModelId::new("test-model")?,
        ModelErrorPhase::Preparation,
        ModelRetryability::Eligible { retry_after: None },
    )?;
    assert_eq!(
        ModelError::contextual(ModelErrorKind::ProtectedHistory, "protected", context),
        Err(ModelErrorContextError::ContradictoryRetryDisposition)
    );
    Ok(())
}

#[test]
fn existing_incomplete_record_decodes_and_reencodes_without_new_fields()
-> Result<(), serde_json::Error> {
    let record = serde_json::json!({
        "kind": "IncompleteResponse",
        "message": "stream ended before a terminal aggregate",
    });
    let error: ModelError = serde_json::from_value(record.clone())?;
    assert_eq!(error.kind(), ModelErrorKind::IncompleteResponse);
    assert_eq!(error.context(), None);
    assert_eq!(serde_json::to_value(&error)?, record);
    for kind in [
        ModelErrorKind::ContentFilteredIncomplete,
        ModelErrorKind::Policy,
    ] {
        let error = ModelError::new(kind, "content-filtered incomplete");
        assert_eq!(
            serde_json::from_value::<ModelError>(serde_json::to_value(&error)?)?,
            error
        );
    }
    Ok(())
}

#[test]
fn served_by_validates_provider_display_names() -> Result<(), crate::IdentityError> {
    assert!(ServedBy::new("").is_err());
    assert!(ServedBy::new("x".repeat(65)).is_err());
    assert!(ServedBy::new("café").is_err());
    assert_eq!(ServedBy::new("x".repeat(64))?.as_str().len(), 64);
    let spaced = ServedBy::new("Google AI Studio")?;
    assert_eq!(spaced.as_str(), "Google AI Studio");
    Ok(())
}

#[test]
fn model_error_deserialization_sanitizes_message() -> Result<(), serde_json::Error> {
    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    let raw = serde_json::json!({
        "kind": "Protocol",
        "message": format!(
            "authorization: Bearer {SECRET_SENTINEL} {}",
            "🦀".repeat(2_048)
        ),
    });
    let error: ModelError = serde_json::from_value(raw)?;

    assert_eq!(error.kind(), ModelErrorKind::Protocol);
    assert_eq!(error.context(), None);
    assert!(!error.message().contains(SECRET_SENTINEL));
    assert!(!error.to_string().contains(SECRET_SENTINEL));
    assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
    assert!(error.message().len() <= crate::MAX_SAFE_DIAGNOSTIC_BYTES);

    let serialized = serde_json::to_string(&error)?;
    assert!(!serialized.contains(SECRET_SENTINEL));
    assert_eq!(serde_json::from_str::<ModelError>(&serialized)?, error);
    Ok(())
}

#[test]
fn model_error_context_is_typed_validated_and_credential_safe()
-> Result<(), Box<dyn std::error::Error>> {
    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    assert_eq!(
        ModelHttpStatus::new(99),
        Err(ModelErrorContextError::InvalidHttpStatus(99))
    );
    assert!(serde_json::from_str::<ModelHttpStatus>("600").is_err());
    assert_eq!(
        ModelProviderCode::new(format!("sk-{SECRET_SENTINEL}")),
        Err(ModelErrorContextError::InvalidProviderCode)
    );
    assert_eq!(
        ModelProviderRequestId::new(format!("Bearer/{SECRET_SENTINEL}")),
        Err(ModelErrorContextError::InvalidProviderRequestId)
    );
    assert_eq!(
        ModelErrorContext::new(
            ProtocolId::new("example.responses")?,
            RouteId::new(format!("sk-{SECRET_SENTINEL}"))?,
            ModelId::new("test-model")?,
            ModelErrorPhase::FirstEvent,
            ModelRetryability::Permanent,
        ),
        Err(ModelErrorContextError::UnsafeTargetIdentity)
    );

    let context = ModelErrorContext::new(
        ProtocolId::new("example.responses")?,
        RouteId::new("example.platform")?,
        ModelId::new("test-model")?,
        ModelErrorPhase::FirstEvent,
        ModelRetryability::Exhausted,
    )?
    .with_http_status(ModelHttpStatus::new(429)?)
    .with_provider_code(ModelProviderCode::new("rate_limit_exceeded")?)
    .with_provider_request_id(ModelProviderRequestId::new("req_test-123")?);
    let error = ModelError::contextual(
        ModelErrorKind::RateLimited,
        "bounded retry exhausted",
        context,
    )?
    .with_source(format!("authorization: Bearer {SECRET_SENTINEL}"));

    let retained = error.context().ok_or("missing context")?;
    assert_eq!(retained.protocol().as_str(), "example.responses");
    assert_eq!(retained.route().as_str(), "example.platform");
    assert_eq!(retained.model().as_str(), "test-model");
    assert_eq!(retained.phase(), ModelErrorPhase::FirstEvent);
    assert_eq!(retained.retryability(), ModelRetryability::Exhausted);
    assert_eq!(retained.http_status().map(ModelHttpStatus::get), Some(429));
    assert_eq!(retained.provider_code(), Some("rate_limit_exceeded"));
    assert_eq!(retained.provider_request_id(), Some("req_test-123"));
    assert!(
        !error
            .source_diagnostic()
            .map_or("", RedactedModelErrorSource::message)
            .contains(SECRET_SENTINEL)
    );

    let serialized = serde_json::to_string(&error)?;
    assert!(!serialized.contains(SECRET_SENTINEL));
    assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
    assert_eq!(serde_json::from_str::<ModelError>(&serialized)?, error);

    let fallback_target = ModelErrorTarget::new(
        ProtocolId::new("fallback.protocol")?,
        RouteId::new("fallback.route")?,
        ModelId::new("fallback-model")?,
    )?;
    let mut preserved = error.clone();
    preserved.ensure_context(&fallback_target, ModelErrorPhase::Cleanup);
    preserved.ensure_context(&fallback_target, ModelErrorPhase::Operation);
    assert_eq!(preserved, error);

    let mut once = error.clone();
    let replacement = ModelErrorContext::new(
        ProtocolId::new("example.responses")?,
        RouteId::new("example.other")?,
        ModelId::new("test-model")?,
        ModelErrorPhase::Terminal,
        ModelRetryability::Permanent,
    )?;
    assert_eq!(
        once.attach_context(replacement),
        Err(ModelErrorContextError::ContextAlreadySet)
    );
    assert_eq!(once, error);

    let contradictory = ModelErrorContext::new(
        ProtocolId::new("example.responses")?,
        RouteId::new("example.platform")?,
        ModelId::new("test-model")?,
        ModelErrorPhase::Authentication,
        ModelRetryability::Eligible { retry_after: None },
    )?;
    assert_eq!(
        ModelError::contextual(
            ModelErrorKind::Authentication,
            "authentication rejected",
            contradictory,
        ),
        Err(ModelErrorContextError::ContradictoryRetryDisposition)
    );
    Ok(())
}

#[test]
fn model_error_deserialization_rejects_unsafe_context() {
    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    let unsafe_target = serde_json::json!({
        "kind": "Transport",
        "message": "transport failed",
        "context": {
            "protocol": "example.responses",
            "route": format!("sk-{SECRET_SENTINEL}"),
            "model": "test-model",
            "phase": "Connect",
            "retryability": "Permanent"
        }
    });
    assert!(serde_json::from_value::<ModelError>(unsafe_target).is_err());

    let contradictory_retry = serde_json::json!({
        "kind": "Authentication",
        "message": "authentication rejected",
        "context": {
            "protocol": "example.responses",
            "route": "example.platform",
            "model": "test-model",
            "phase": "Authentication",
            "retryability": { "Eligible": { "retry_after": null } }
        }
    });
    assert!(serde_json::from_value::<ModelError>(contradictory_retry).is_err());
}

#[test]
fn dispatch_rejection_construction_and_codec_reject_ambiguous_or_terminal_facts()
-> Result<(), Box<dyn std::error::Error>> {
    let rejection = ModelDispatchRejection::new(
        ModelDispatchRejectionClass::Admission,
        ModelDispatchRejectionPhase::Revalidation,
    );
    assert_eq!(
        ModelError::new(ModelErrorKind::OutcomeUnknown, "ambiguous")
            .with_dispatch_rejection(rejection),
        Err(ModelErrorContextError::ContradictoryDispatchRejection)
    );

    let target = paid_target()?;
    let terminal = ModelError::contextual(
        ModelErrorKind::Protocol,
        "terminal protocol failure",
        ModelErrorContext::new(
            target.protocol().clone(),
            target.route().clone(),
            target.model().clone(),
            ModelErrorPhase::Terminal,
            ModelRetryability::Permanent,
        )?,
    )?;
    assert_eq!(
        terminal.with_dispatch_rejection(rejection),
        Err(ModelErrorContextError::ContradictoryDispatchRejection)
    );

    let definite = ModelError::contextual(
        ModelErrorKind::ResourceLimit,
        "admission rejected",
        ModelErrorContext::new(
            target.protocol().clone(),
            target.route().clone(),
            target.model().clone(),
            ModelErrorPhase::Readiness,
            ModelRetryability::Permanent,
        )?,
    )?
    .with_dispatch_rejection(rejection)?;
    let canonical = serde_json::to_value(&definite)?;
    assert_eq!(
        serde_json::from_value::<ModelError>(canonical.clone())?,
        definite
    );

    let mut ambiguous = canonical.clone();
    ambiguous["kind"] = serde_json::json!("OutcomeUnknown");
    assert!(serde_json::from_value::<ModelError>(ambiguous).is_err());

    let mut terminal_wire = canonical;
    terminal_wire["context"]["phase"] = serde_json::json!("Terminal");
    assert!(serde_json::from_value::<ModelError>(terminal_wire).is_err());

    let mut paid = serde_json::to_value(ModelError::paid_recovery_required(
        "paid request requires recovery",
        &target,
        ModelErrorPhase::Terminal,
        paid_recovery_fixture()?,
    ))?;
    paid["dispatch_rejection"] = serde_json::to_value(rejection)?;
    assert!(serde_json::from_value::<ModelError>(paid).is_err());
    Ok(())
}

#[test]
fn outcome_unknown_rejects_retry_authorizing_dispositions() -> Result<(), Box<dyn std::error::Error>>
{
    for retryability in [
        ModelRetryability::Eligible { retry_after: None },
        ModelRetryability::Exhausted,
    ] {
        let context = ModelErrorContext::new(
            ProtocolId::new("example.responses")?,
            RouteId::new("example.platform")?,
            ModelId::new("test-model")?,
            ModelErrorPhase::Terminal,
            retryability,
        )?;
        assert_eq!(
            ModelError::contextual(
                ModelErrorKind::OutcomeUnknown,
                "paid request outcome requires explicit recovery",
                context,
            ),
            Err(ModelErrorContextError::ContradictoryRetryDisposition)
        );
    }

    let unsafe_context = ModelErrorContext::new(
        ProtocolId::new("example.responses")?,
        RouteId::new("example.platform")?,
        ModelId::new("test-model")?,
        ModelErrorPhase::Terminal,
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
    )?;
    let error = ModelError::contextual(
        ModelErrorKind::OutcomeUnknown,
        "paid request outcome requires explicit recovery",
        unsafe_context,
    )?;
    assert_eq!(error.kind(), ModelErrorKind::OutcomeUnknown);
    assert_eq!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        })
    );
    Ok(())
}

fn paid_terminal_facts() -> Result<ModelPaidTerminalFacts, ModelErrorContextError> {
    ModelPaidTerminalFacts::new(2, ModelPaidProviderAcceptance::Accepted, true, true, true)
}

fn paid_recovery_fixture() -> Result<ModelPaidRecovery, ModelErrorContextError> {
    ModelPaidRecovery::new(
        br#"{"schema":"plak.paid-recovery.test.v1","opaque":[0,255]}"#.to_vec(),
        paid_terminal_facts()?,
    )
}

fn paid_target() -> Result<ModelErrorTarget, Box<dyn std::error::Error>> {
    Ok(ModelErrorTarget::new(
        ProtocolId::new("example.responses")?,
        RouteId::new("example.platform")?,
        ModelId::new("test-model")?,
    )?)
}

#[test]
fn paid_recovery_round_trip_preserves_exact_bytes_facts_and_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let recovery = paid_recovery_fixture()?;
    let expected_bytes = recovery.canonical_recovery_bytes().to_vec();
    let expected_facts = recovery.terminal_facts();
    let error = ModelError::paid_recovery_required(
        "paid request outcome requires exact recovery",
        &paid_target()?,
        ModelErrorPhase::Terminal,
        recovery,
    );

    assert_eq!(error.kind(), ModelErrorKind::OutcomeUnknown);
    assert_eq!(
        error.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Permanent)
    );
    let retained = error.paid_recovery().ok_or("missing paid recovery")?;
    assert_eq!(
        retained.requirement(),
        ModelPaidRecoveryRequirement::PaidRecoveryRequired
    );
    assert_eq!(retained.canonical_recovery_bytes(), expected_bytes);
    assert_eq!(retained.terminal_facts(), expected_facts);
    assert_eq!(expected_facts.physical_attempts(), 2);
    assert_eq!(
        expected_facts.provider_acceptance(),
        ModelPaidProviderAcceptance::Accepted
    );
    assert!(expected_facts.visible_output());
    assert!(expected_facts.hosted_effect_capable());
    assert!(expected_facts.hosted_effects_possible());

    let encoded = serde_json::to_vec(&error)?;
    let decoded: ModelError = serde_json::from_slice(&encoded)?;
    assert_eq!(decoded, error);
    assert_eq!(
        decoded
            .paid_recovery()
            .ok_or("missing decoded recovery")?
            .canonical_recovery_bytes(),
        expected_bytes
    );
    assert_eq!(serde_json::to_vec(&decoded)?, encoded);
    Ok(())
}

#[test]
fn paid_recovery_bytes_are_strictly_bounded_and_canonically_encoded()
-> Result<(), Box<dyn std::error::Error>> {
    assert_eq!(
        ModelPaidRecoveryBytes::new(Vec::new()),
        Err(ModelErrorContextError::EmptyPaidRecovery)
    );
    let maximum = vec![0xa5; MAX_MODEL_PAID_RECOVERY_BYTES];
    let retained = ModelPaidRecoveryBytes::new(maximum.clone())?;
    assert_eq!(retained.as_bytes(), maximum);
    let encoded = serde_json::to_string(&retained)?;
    assert_eq!(encoded.len(), MAX_MODEL_PAID_RECOVERY_BYTES * 2 + 2);
    assert_eq!(
        serde_json::from_str::<ModelPaidRecoveryBytes>(&encoded)?,
        retained
    );
    assert_eq!(
        ModelPaidRecoveryBytes::new(vec![0; MAX_MODEL_PAID_RECOVERY_BYTES + 1]),
        Err(ModelErrorContextError::PaidRecoveryTooLarge)
    );

    for malformed in ["\"\"", "\"0\"", "\"gg\"", "\"AF\"", "42", "null"] {
        assert!(
            serde_json::from_str::<ModelPaidRecoveryBytes>(malformed).is_err(),
            "accepted malformed recovery encoding {malformed}"
        );
    }
    let oversized = format!("\"{}\"", "00".repeat(MAX_MODEL_PAID_RECOVERY_BYTES + 1));
    assert!(serde_json::from_str::<ModelPaidRecoveryBytes>(&oversized).is_err());
    Ok(())
}

#[test]
fn paid_recovery_debug_and_display_never_expose_exact_bytes()
-> Result<(), Box<dyn std::error::Error>> {
    const RECOVERY_SENTINEL: &str = "EXACT_RECOVERY_SENTINEL_DO_NOT_LOG";
    let recovery = ModelPaidRecovery::new(
        RECOVERY_SENTINEL.as_bytes().to_vec(),
        paid_terminal_facts()?,
    )?;
    let bytes = ModelPaidRecoveryBytes::new(RECOVERY_SENTINEL.as_bytes().to_vec())?;
    let error = ModelError::paid_recovery_required(
        "paid request requires recovery",
        &paid_target()?,
        ModelErrorPhase::Cleanup,
        recovery.clone(),
    );
    let encoded_sentinel = bytes.encode_hex();

    for rendered in [
        format!("{bytes:?}"),
        bytes.to_string(),
        format!("{recovery:?}"),
        recovery.to_string(),
        format!("{error:?}"),
        error.to_string(),
    ] {
        assert!(!rendered.contains(RECOVERY_SENTINEL));
        assert!(!rendered.contains(&encoded_sentinel));
    }
    Ok(())
}

#[test]
fn paid_terminal_facts_reject_contradictions_and_malformed_wire() {
    for facts in [
        ModelPaidTerminalFacts::new(
            0,
            ModelPaidProviderAcceptance::PossiblyAccepted,
            false,
            false,
            false,
        ),
        ModelPaidTerminalFacts::new(
            1,
            ModelPaidProviderAcceptance::PossiblyAccepted,
            true,
            false,
            false,
        ),
        ModelPaidTerminalFacts::new(1, ModelPaidProviderAcceptance::Accepted, false, false, true),
        ModelPaidTerminalFacts::new(
            1,
            ModelPaidProviderAcceptance::NotAccepted,
            false,
            true,
            true,
        ),
        ModelPaidTerminalFacts::new(
            1,
            ModelPaidProviderAcceptance::PossiblyAccepted,
            false,
            true,
            false,
        ),
    ] {
        assert_eq!(
            facts,
            Err(ModelErrorContextError::InconsistentPaidTerminalFacts)
        );
    }

    let malformed = serde_json::json!({
        "physical_attempts": 0,
        "provider_acceptance": "Accepted",
        "visible_output": false,
        "hosted_effect_capable": false,
        "hosted_effects_possible": false,
    });
    assert!(serde_json::from_value::<ModelPaidTerminalFacts>(malformed).is_err());
    let out_of_range = serde_json::json!({
        "physical_attempts": u64::from(u32::MAX) + 1,
        "provider_acceptance": "NotAccepted",
        "visible_output": false,
        "hosted_effect_capable": false,
        "hosted_effects_possible": false,
    });
    assert!(serde_json::from_value::<ModelPaidTerminalFacts>(out_of_range).is_err());
}

#[test]
fn paid_recovery_attachment_is_one_shot_and_requires_permanent_unknown_context()
-> Result<(), Box<dyn std::error::Error>> {
    let target = paid_target()?;
    let permanent_context = ModelErrorContext::new(
        target.protocol().clone(),
        target.route().clone(),
        target.model().clone(),
        ModelErrorPhase::Terminal,
        ModelRetryability::Permanent,
    )?;
    let mut error = ModelError::contextual(
        ModelErrorKind::OutcomeUnknown,
        "paid outcome unknown",
        permanent_context,
    )?;
    let original = paid_recovery_fixture()?;
    error.attach_paid_recovery(original.clone())?;
    assert_eq!(
        error.attach_paid_recovery(ModelPaidRecovery::new(
            b"different".to_vec(),
            paid_terminal_facts()?,
        )?),
        Err(ModelErrorContextError::PaidRecoveryAlreadySet)
    );
    assert_eq!(error.paid_recovery(), Some(&original));

    let mut wrong_kind = ModelError::contextual(
        ModelErrorKind::Protocol,
        "protocol failure",
        ModelErrorContext::new(
            target.protocol().clone(),
            target.route().clone(),
            target.model().clone(),
            ModelErrorPhase::Terminal,
            ModelRetryability::Permanent,
        )?,
    )?;
    assert_eq!(
        wrong_kind.attach_paid_recovery(paid_recovery_fixture()?),
        Err(ModelErrorContextError::PaidRecoveryRequiresOutcomeUnknown)
    );

    let mut missing_context = ModelError::new(ModelErrorKind::OutcomeUnknown, "unknown");
    assert_eq!(
        missing_context.attach_paid_recovery(paid_recovery_fixture()?),
        Err(ModelErrorContextError::PaidRecoveryRequiresContext)
    );

    let unsafe_context = ModelErrorContext::new(
        target.protocol().clone(),
        target.route().clone(),
        target.model().clone(),
        ModelErrorPhase::Terminal,
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::DuplicateInferenceOrBilling,
        },
    )?;
    let mut unsafe_error = ModelError::contextual(
        ModelErrorKind::OutcomeUnknown,
        "paid outcome unknown",
        unsafe_context,
    )?;
    assert_eq!(
        unsafe_error.attach_paid_recovery(paid_recovery_fixture()?),
        Err(ModelErrorContextError::PaidRecoveryRequiresPermanentRetry)
    );
    Ok(())
}

#[test]
fn paid_recovery_deserialization_rejects_malformed_or_retryable_attachments()
-> Result<(), Box<dyn std::error::Error>> {
    let error = ModelError::paid_recovery_required(
        "paid request requires recovery",
        &paid_target()?,
        ModelErrorPhase::Terminal,
        paid_recovery_fixture()?,
    );
    let canonical = serde_json::to_value(error)?;

    let mut wrong_kind = canonical.clone();
    wrong_kind["kind"] = serde_json::json!("Protocol");
    assert!(serde_json::from_value::<ModelError>(wrong_kind).is_err());

    let mut missing_context = canonical.clone();
    missing_context
        .as_object_mut()
        .ok_or("error fixture is not an object")?
        .remove("context");
    assert!(serde_json::from_value::<ModelError>(missing_context).is_err());

    let mut retryable = canonical.clone();
    retryable["context"]["retryability"] = serde_json::json!({
        "Unsafe": { "risk": "DuplicateInferenceOrBilling" }
    });
    assert!(serde_json::from_value::<ModelError>(retryable).is_err());

    let mut malformed_bytes = canonical.clone();
    malformed_bytes["paid_recovery"]["canonical_recovery_bytes"] = serde_json::json!("AB");
    assert!(serde_json::from_value::<ModelError>(malformed_bytes).is_err());

    let mut missing_marker = canonical.clone();
    missing_marker["paid_recovery"]
        .as_object_mut()
        .ok_or("recovery fixture is not an object")?
        .remove("requirement");
    assert!(serde_json::from_value::<ModelError>(missing_marker).is_err());

    let mut unknown_field = canonical;
    unknown_field["paid_recovery"]["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<ModelError>(unknown_field).is_err());
    Ok(())
}

#[test]
fn model_error_rejects_explicit_null_typo_and_reserved_paid_fields()
-> Result<(), Box<dyn std::error::Error>> {
    let canonical = serde_json::to_value(ModelError::paid_recovery_required(
        "paid request requires recovery",
        &paid_target()?,
        ModelErrorPhase::Terminal,
        paid_recovery_fixture()?,
    ))?;

    let mut explicit_null = canonical.clone();
    explicit_null["paid_recovery"] = serde_json::Value::Null;
    assert!(serde_json::from_value::<ModelError>(explicit_null).is_err());

    for field in ["paid_recovrey", "paid_recovery_v2", "paid_retry"] {
        let mut malformed = canonical.clone();
        malformed[field] = serde_json::json!({"unexpected": true});
        assert!(
            serde_json::from_value::<ModelError>(malformed).is_err(),
            "accepted reserved or misspelled field {field}"
        );
    }
    Ok(())
}

#[test]
fn legacy_and_generic_retry_model_errors_decode_without_paid_marker()
-> Result<(), Box<dyn std::error::Error>> {
    let legacy: ModelError = serde_json::from_value(serde_json::json!({
        "kind": "Protocol",
        "message": "legacy protocol failure"
    }))?;
    assert_eq!(legacy.context(), None);
    assert_eq!(legacy.paid_recovery(), None);

    let generic_retry: ModelError = serde_json::from_value(serde_json::json!({
        "kind": "RateLimited",
        "message": "retry later",
        "context": {
            "protocol": "example.responses",
            "route": "example.platform",
            "model": "test-model",
            "phase": "Terminal",
            "retryability": { "Eligible": { "retry_after": null } }
        }
    }))?;
    assert_eq!(generic_retry.paid_recovery(), None);
    assert!(matches!(
        generic_retry.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Eligible { .. })
    ));
    Ok(())
}

#[test]
fn model_error_uses_one_canonical_map_and_rejects_prototype_sequence()
-> Result<(), Box<dyn std::error::Error>> {
    const CANONICAL_MAP: &str = r#"{
        "kind":"RateLimited",
        "message":"bounded retry exhausted",
        "context":{
            "protocol":"example.responses",
            "route":"example.platform",
            "model":"test-model",
            "phase":"Terminal",
            "retryability":"Exhausted",
            "http_status":429,
            "provider_code":"rate_limit",
            "provider_request_id":"req-current"
        },
        "source":"sanitized source"
    }"#;
    const PROTOTYPE_SEQUENCE: &str = r#"[
        "RateLimited",
        "bounded retry exhausted",
        {
            "protocol":"example.responses",
            "route":"example.platform",
            "model":"test-model",
            "phase":"Terminal",
            "retryability":"Exhausted",
            "http_status":429,
            "provider_code":"rate_limit",
            "provider_request_id":"req-current"
        },
        "sanitized source"
    ]"#;

    let from_map: ModelError = serde_json::from_str(CANONICAL_MAP)?;
    assert_eq!(from_map.kind(), ModelErrorKind::RateLimited);
    assert_eq!(
        from_map.context().map(ModelErrorContext::retryability),
        Some(ModelRetryability::Exhausted)
    );
    assert_eq!(
        from_map
            .source_diagnostic()
            .map(RedactedModelErrorSource::message),
        Some("sanitized source")
    );
    assert_eq!(from_map.paid_recovery(), None);
    assert!(serde_json::from_str::<ModelError>(PROTOTYPE_SEQUENCE).is_err());
    Ok(())
}

#[test]
fn frozen_old_reader_rejects_the_new_paid_recovery_wire_kind()
-> Result<(), Box<dyn std::error::Error>> {
    #[derive(Debug, Deserialize, Eq, PartialEq)]
    enum FrozenOldModelErrorKind {
        OutcomeUnknown,
    }

    #[derive(Deserialize)]
    struct FrozenOldModelError {
        kind: FrozenOldModelErrorKind,
        message: String,
        #[serde(default)]
        context: Option<serde_json::Value>,
        #[serde(default)]
        source: Option<String>,
    }

    const FROZEN_NEW_PAID_V1: &str = r#"{"kind":"PaidRecoveryRequiredV1","message":"paid request outcome requires exact recovery","context":{"protocol":"example.responses","route":"example.platform","model":"test-model","phase":"Terminal","retryability":"Permanent"},"paid_recovery":{"requirement":"PaidRecoveryRequired","canonical_recovery_bytes":"7b22736368656d61223a22706c616b2e706169642d7265636f766572792e746573742e7631222c226f7061717565223a5b302c3235355d7d","terminal_facts":{"physical_attempts":2,"provider_acceptance":"Accepted","visible_output":true,"hosted_effect_capable":true,"hosted_effects_possible":true}}}"#;
    const FROZEN_OLD_OUTCOME_UNKNOWN: &str =
        r#"{"kind":"OutcomeUnknown","message":"legacy outcome unknown"}"#;

    let old = serde_json::from_str::<FrozenOldModelError>(FROZEN_OLD_OUTCOME_UNKNOWN)?;
    assert_eq!(old.kind, FrozenOldModelErrorKind::OutcomeUnknown);
    assert_eq!(old.message, "legacy outcome unknown");
    assert!(old.context.is_none());
    assert!(old.source.is_none());
    assert!(serde_json::from_str::<FrozenOldModelError>(FROZEN_NEW_PAID_V1).is_err());

    let current: ModelError = serde_json::from_str(FROZEN_NEW_PAID_V1)?;
    assert_eq!(current.kind(), ModelErrorKind::OutcomeUnknown);
    assert!(current.paid_recovery().is_some());
    assert_eq!(serde_json::to_string(&current)?, FROZEN_NEW_PAID_V1);
    Ok(())
}

#[test]
fn provider_overflow_phrasings_classify_and_other_rejections_do_not() {
    for (code, message) in [
        (None, "prompt is too long: 270013 tokens > 256000 maximum"),
        (
            None,
            "This endpoint's maximum context length is 131072 tokens. However, you requested about 140000 tokens",
        ),
        (
            None,
            "This model's maximum prompt length is 256000 but the request contains 262144 tokens.",
        ),
        (
            Some("context_length_exceeded"),
            "Your input exceeds the limit.",
        ),
    ] {
        assert!(
            provider_rejection_is_context_overflow(code, message),
            "{code:?} {message}"
        );
    }
    for (code, message) in [
        (None, "tools.0.name: String should match pattern"),
        (
            Some("invalid_request_error"),
            "max_tokens: must be at most 8192",
        ),
        (None, ""),
    ] {
        assert!(!provider_rejection_is_context_overflow(code, message));
    }
}

#[test]
fn only_a_provider_refusal_is_a_provider_context_overflow() {
    let provider = ModelError::new(ModelErrorKind::ContextLimit, "prompt is too long");
    assert!(provider.is_provider_context_overflow());
    let other = ModelError::new(ModelErrorKind::InvalidRequest, "prompt is too long");
    assert!(!other.is_provider_context_overflow());
}

#[test]
fn wire_output_limit_is_absent_by_default_and_round_trips_on_output_limit()
-> Result<(), serde_json::Error> {
    let absent = ModelError::new(ModelErrorKind::OutputLimit, "truncated response");
    assert_eq!(absent.wire_output_limit(), None);
    let encoded = serde_json::to_value(&absent)?;
    assert!(encoded.get("wire_output_limit").is_none());
    assert_eq!(serde_json::from_value::<ModelError>(encoded)?, absent);

    let tokens = NonZeroU32::new(64_000).expect("nonzero");
    let present = ModelError::new(ModelErrorKind::OutputLimit, "truncated response")
        .with_wire_output_limit(tokens);
    assert_eq!(present.wire_output_limit(), Some(tokens));
    let encoded = serde_json::to_string(&present)?;
    assert!(encoded.contains("\"wire_output_limit\":64000"));
    assert_eq!(serde_json::from_str::<ModelError>(&encoded)?, present);
    let mut kept = present.clone();
    kept.fill_missing_wire_output_limit(NonZeroU32::MIN);
    assert_eq!(kept.wire_output_limit(), Some(tokens));

    let foreign = serde_json::json!({
        "kind": "Transport",
        "message": "connection failed",
        "wire_output_limit": 64000,
    });
    assert!(serde_json::from_value::<ModelError>(foreign).is_err());
    let ignored = ModelError::new(ModelErrorKind::Transport, "connection failed")
        .with_wire_output_limit(tokens);
    assert_eq!(ignored.wire_output_limit(), None);
    Ok(())
}
