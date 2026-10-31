use super::*;

#[test]
fn public_constructor_redacts_and_bounds_diagnostics() {
    let error = AgentError::new(
        AgentErrorKind::Internal,
        format!("authorization: Bearer secret-value {}", "x".repeat(4_000)),
    );
    let display = error.to_string();
    let debug = format!("{error:?}");
    assert!(!display.contains("secret-value"));
    assert!(!debug.contains("secret-value"));
    assert!(display.len() <= 2_048);
}

#[test]
fn model_and_tool_errors_propagate_only_safe_diagnostics() {
    const SECRET_SENTINEL: &str = "SECRET_SENTINEL_DO_NOT_RETAIN";
    let raw = format!(
        "authorization: Bearer {SECRET_SENTINEL} {}",
        "🦀".repeat(2_048)
    );
    let model = ModelError::new(ModelErrorKind::Protocol, raw.clone());
    let tool = ToolRuntimeError::new(ToolRuntimeErrorKind::Execution, raw);

    for error in [
        AgentError::from_model(model.clone()),
        AgentError::from_tool(&tool),
    ] {
        assert!(!error.message().contains(SECRET_SENTINEL));
        assert!(!error.to_string().contains(SECRET_SENTINEL));
        assert!(!format!("{error:?}").contains(SECRET_SENTINEL));
        assert!(error.message().len() <= plakous_model::MAX_SAFE_DIAGNOSTIC_BYTES);
    }
}

#[test]
fn model_errors_preserve_safe_provider_neutral_context() -> Result<(), Box<dyn std::error::Error>> {
    let context = ModelErrorContext::new(
        plakous_model::ProtocolId::new("example.responses")?,
        plakous_model::RouteId::new("example.platform")?,
        plakous_model::ModelId::new("test-model")?,
        plakous_model::ModelErrorPhase::Connect,
        ModelRetryability::Exhausted,
    )?
    .with_http_status(plakous_model::ModelHttpStatus::new(503)?);
    let model = ModelError::contextual(
        ModelErrorKind::Transport,
        "transport retries exhausted",
        context.clone(),
    )?
    .with_source("connection reset after bounded retry");

    let agent = AgentError::from_model(model.clone());
    assert_eq!(agent.model_kind(), Some(ModelErrorKind::Transport));
    assert_eq!(agent.model_error(), Some(&model));
    assert_eq!(agent.model_context(), Some(&context));
    assert_eq!(
        agent
            .model_context()
            .and_then(ModelErrorContext::http_status)
            .map(plakous_model::ModelHttpStatus::get),
        Some(503)
    );
    assert!(std::error::Error::source(&agent).is_some());
    Ok(())
}

#[test]
fn provider_hosted_effect_risk_requires_recovery_even_for_transport_kind()
-> Result<(), Box<dyn std::error::Error>> {
    let context = ModelErrorContext::new(
        plakous_model::ProtocolId::new("example.responses")?,
        plakous_model::RouteId::new("example.platform")?,
        plakous_model::ModelId::new("test-model")?,
        plakous_model::ModelErrorPhase::Stream,
        ModelRetryability::Unsafe {
            risk: ModelRetryRisk::ProviderHostedEffect,
        },
    )?;
    let model = ModelError::contextual(
        ModelErrorKind::Transport,
        "transport ended after hosted work became possible",
        context,
    )?;

    assert_eq!(
        AgentError::from_model(model).kind(),
        AgentErrorKind::OutcomeUnknown
    );
    Ok(())
}

#[test]
fn paid_recovery_dominates_model_and_cancellation_projection()
-> Result<(), Box<dyn std::error::Error>> {
    let target = plakous_model::ModelErrorTarget::new(
        plakous_model::ProtocolId::new("example.responses")?,
        plakous_model::RouteId::new("example.platform")?,
        plakous_model::ModelId::new("test-model")?,
    )?;
    let recovery = plakous_model::ModelPaidRecovery::new(
        b"typed-recovery-locator".to_vec(),
        plakous_model::ModelPaidTerminalFacts::new(
            1,
            plakous_model::ModelPaidProviderAcceptance::PossiblyAccepted,
            false,
            true,
            true,
        )?,
    )?;
    let model = ModelError::paid_recovery_required(
        "paid result requires reconciliation",
        &target,
        plakous_model::ModelErrorPhase::Cleanup,
        recovery,
    );

    for projected in [
        AgentError::from_model(model.clone()),
        AgentError::from_cancelled_model(model.clone()),
    ] {
        assert_eq!(projected.kind(), AgentErrorKind::OutcomeUnknown);
        assert_eq!(projected.model_kind(), Some(ModelErrorKind::OutcomeUnknown));
        assert_eq!(projected.model_error(), Some(&model));
        assert_eq!(
            projected
                .model_error()
                .and_then(ModelError::paid_recovery)
                .map(plakous_model::ModelPaidRecovery::canonical_recovery_bytes),
            Some(b"typed-recovery-locator".as_slice())
        );
    }
    Ok(())
}

#[test]
fn a_binding_change_is_named_and_never_reconstructed_from_canonical_parts() {
    let change = crate::BindingChange::ToolBinding {
        tool: "shell".to_owned(),
    };
    let error = AgentError::new(AgentErrorKind::InvalidInput, "reopen refused")
        .with_binding_change(change.clone());
    assert_eq!(error.binding_change(), Some(&change));
    assert_eq!(change.to_string(), "executor binding for tool `shell`");
    let decoded = AgentError::from_canonical_parts(error.kind(), error.message(), None, None, None);
    assert_eq!(decoded.binding_change(), None);
    assert_eq!(
        AgentError::new(AgentErrorKind::InvalidInput, "plain").binding_change(),
        None
    );
}
