use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_model::{
    DigestDomain, ToolConcurrency, ToolCorrelation, ToolDefinition, ToolInputContract, ToolName,
    ToolOutputContract,
};
use serde_json::json;

use super::{
    Arc, AttemptId, CallId, Digest, EFFECT_DISPATCH_INTENT_VERSION, EffectAuthorization,
    EffectAuthorizationGrantSource, EffectDecision, EffectDispatchIntent, ExecutionEnvironment,
    RunId, ToolEffect, ToolInvocation, ToolPolicy, ToolPolicyDecision, ToolPolicyRevision,
    ToolRuntime, TurnId,
};
use plakous_tools::{
    EFFECT_AUTHORIZATION_BINDING_VERSION, EffectDecisionAuditId, EffectDecisionPrompt,
    ExecutionEnvironmentConfig, ExecutionEnvironmentDescriptor, ExecutionEnvironmentScope,
    RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolFuture, ToolInvocationRequest,
    ToolOutcome, ToolPolicyFuture, ToolRegistration, ToolRegistryBuilder,
};

struct FixedTool(ToolOutcome);

struct CountingPolicy {
    evaluations: Arc<AtomicUsize>,
    revision: ToolPolicyRevision,
}

impl ToolPolicy for CountingPolicy {
    fn revision(&self) -> ToolPolicyRevision {
        self.revision.clone()
    }

    fn evaluate(&self, _request: plakous_tools::ToolPolicyRequest) -> ToolPolicyFuture {
        self.evaluations.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ToolPolicyDecision::Allowed) })
    }
}

impl ToolExecutor for FixedTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        let outcome = self.0.clone();
        Box::pin(async move { Ok(outcome) })
    }
}

fn environment_descriptor(
    run_id: RunId,
) -> Result<ExecutionEnvironmentDescriptor, Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    Ok(environment.descriptor().clone())
}

#[test]
fn authorization_and_effect_intent_formats_bind_environment_descriptor()
-> Result<(), Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("mutate")?,
        "1.0.0",
        "Mutates a fixed test resource.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::Mutating,
    )?;
    let tool = Arc::new(FixedTool(ToolOutcome::success_text("ok")?));
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.mutate")?,
        ToolExecutorId::new("test.mutate.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.mutate.v1"])?,
        definition,
        tool,
    ))?;
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000001")?;
    let invocation = builder.freeze().prepare(ToolInvocationRequest::new(
        run_id,
        TurnId::from_str("00000000-0000-0000-0000-000000000002")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000003")?,
        CallId::from_str("00000000-0000-0000-0000-000000000004")?,
        ToolName::new("mutate")?,
        ToolCorrelation::default(),
        Arc::<str>::from(r#"{"value":"fixed"}"#),
    ))?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment_descriptor(run_id)?,
        ToolPolicyRevision::new("test.fixed-policy", 7)?,
    )?;
    assert_eq!(binding.digest().domain(), DigestDomain::EffectAuthorization);
    assert_eq!(
        binding.digest().version(),
        EFFECT_AUTHORIZATION_BINDING_VERSION
    );
    assert_eq!(binding.environment_descriptor().scope().run_id(), run_id);

    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let intent = EffectDispatchIntent::for_invocation(&invocation, &grant)?;
    assert_eq!(intent.digest().domain(), DigestDomain::EffectAuthorization);
    assert_eq!(intent.digest().version(), EFFECT_DISPATCH_INTENT_VERSION);
    Ok(())
}

#[test]
fn exact_pending_restore_skips_policy_and_rejects_replay_or_drift()
-> Result<(), Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("read")?,
        "1.0.0",
        "Reads a fixed test resource.",
        ToolInputContract::structured(json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {"value": {"type": "string"}},
            "required": ["value"],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let tool = Arc::new(FixedTool(ToolOutcome::success_text("ok")?));
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.read")?,
        ToolExecutorId::new("test.read.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.read.v1"])?,
        definition,
        tool,
    ))?;
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000011")?;
    let workspace = tempfile::tempdir()?;
    let environment = Arc::new(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?);
    let evaluations = Arc::new(AtomicUsize::new(0));
    let revision = ToolPolicyRevision::new("test.restore-policy", 1)?;
    let runtime = ToolRuntime::new(
        builder.freeze(),
        Arc::clone(&environment),
        Arc::new(CountingPolicy {
            evaluations: Arc::clone(&evaluations),
            revision: revision.clone(),
        }),
    );
    let request = ToolInvocationRequest::new(
        run_id,
        TurnId::from_str("00000000-0000-0000-0000-000000000012")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000013")?,
        CallId::from_str("00000000-0000-0000-0000-000000000014")?,
        ToolName::new("read")?,
        ToolCorrelation::default(),
        Arc::<str>::from(r#"{"value":"fixed"}"#),
    );
    let invocation = runtime.prepare(request.clone())?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        revision,
    )?;
    let pending = runtime.restore_pending_effect_decision(
        invocation,
        binding.digest(),
        EffectDecisionPrompt::new("restore"),
    )?;
    assert_eq!(pending.invocation().turn_id(), request.turn_id());
    assert_eq!(evaluations.load(Ordering::SeqCst), 0);

    let duplicate = runtime.restore_pending_effect_decision(
        runtime.prepare(request.clone())?,
        binding.digest(),
        EffectDecisionPrompt::new("restore"),
    );
    assert!(duplicate.is_err());

    let resolved = runtime.resolve_effect_decision(
        pending,
        EffectDecision::allow(
            binding.digest(),
            EffectDecisionAuditId::new("restore:allow")?,
        ),
    )?;
    assert!(matches!(resolved, EffectAuthorization::Allowed(_)));
    assert_eq!(evaluations.load(Ordering::SeqCst), 0);

    let deny_request = ToolInvocationRequest::new(
        run_id,
        request.turn_id(),
        request.attempt_id(),
        CallId::from_str("00000000-0000-0000-0000-000000000015")?,
        ToolName::new("read")?,
        ToolCorrelation::default(),
        Arc::<str>::from(r#"{"value":"deny"}"#),
    );
    let deny_invocation = runtime.prepare(deny_request)?;
    let deny_binding = plakous_tools::internal::authorization_binding(
        &deny_invocation,
        environment.descriptor().clone(),
        runtime.policy_revision().clone(),
    )?;
    let deny_pending = runtime.restore_pending_effect_decision(
        deny_invocation,
        deny_binding.digest(),
        EffectDecisionPrompt::new("restore deny"),
    )?;
    let denied = runtime.resolve_effect_decision(
        deny_pending,
        EffectDecision::deny(
            deny_binding.digest(),
            plakous_tools::ToolPolicyDenial::new("operator denied"),
        ),
    )?;
    assert!(matches!(denied, EffectAuthorization::Denied(_)));
    assert_eq!(evaluations.load(Ordering::SeqCst), 0);

    let drifted = runtime.restore_pending_effect_decision(
        runtime.prepare(request)?,
        Digest::sha256(DigestDomain::EffectAuthorization, 6, &[b"drift"])?,
        EffectDecisionPrompt::new("restore"),
    );
    assert!(drifted.is_err());
    assert_eq!(evaluations.load(Ordering::SeqCst), 0);
    Ok(())
}
