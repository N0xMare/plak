//! Every tool call's backend operation carries the line head its environment
//! runs for, the call and attempt that key it, and the call's deadline.

use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;

use plakous_model::{
    AttemptId, BranchId, CallId, CancellationToken, Clock, FixedClock, LineId, RunId, Timestamp,
    ToolConcurrency, ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract, ToolName,
    ToolOutputContract, TurnId,
};
use plakous_tools::{
    EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, LineOwner, OperationClass, ProgressSink, RegisteredToolId,
    ToolContext, ToolExecutor, ToolExecutorId, ToolFuture, ToolInvocation, ToolInvocationRequest,
    ToolOutcome, ToolPolicyRevision, ToolRegistration, ToolRegistryBuilder, operation_context,
};

struct Unused;

impl ToolExecutor for Unused {
    fn execute(self: Arc<Self>, _: ToolInvocation, _: ToolContext) -> ToolFuture {
        Box::pin(async { ToolOutcome::success_text("unused") })
    }
}

fn context(
    environment: ExecutionEnvironment,
    deadline: Option<Timestamp>,
) -> Result<ToolContext, Box<dyn Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("probe")?,
        "1.0.0",
        "Probes the operation context.",
        ToolInputContract::structured(serde_json::json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "properties": {},
            "required": [],
            "additionalProperties": false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("test.probe")?,
        ToolExecutorId::new("test.probe.v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"test.probe.v1"])?,
        definition,
        Arc::new(Unused),
    ))?;
    let run_id = RunId::from_str("00000000-0000-0000-0000-000000000701")?;
    let call_id = CallId::from_str("00000000-0000-0000-0000-000000000704")?;
    let invocation = builder.freeze().prepare(ToolInvocationRequest::new(
        run_id,
        TurnId::from_str("00000000-0000-0000-0000-000000000702")?,
        AttemptId::from_str("00000000-0000-0000-0000-000000000703")?,
        call_id,
        ToolName::new("probe")?,
        ToolCorrelation::default(),
        Arc::<str>::from("{}"),
    ))?;
    let binding = plakous_tools::internal::authorization_binding(
        &invocation,
        environment.descriptor().clone(),
        ToolPolicyRevision::new("test.operation-context", 1)?,
    )?;
    let grant = plakous_tools::internal::authorization_grant(
        binding,
        EffectAuthorizationGrantSource::Policy,
    );
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(90, 0)?));
    let (progress, _receiver) = ProgressSink::channel(4, call_id, Arc::clone(&clock))?;
    Ok(plakous_tools::internal::tool_context(
        invocation.run_id(),
        invocation.turn_id(),
        invocation.attempt_id(),
        CancellationToken::new(),
        deadline,
        clock,
        Arc::new(environment),
        grant,
        progress,
    ))
}

fn environment(workspace: &tempfile::TempDir) -> Result<ExecutionEnvironment, Box<dyn Error>> {
    Ok(ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(RunId::from_str("00000000-0000-0000-0000-000000000701")?),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?)
}

#[test]
fn a_tool_calls_operation_carries_its_line_owner_key_and_deadline() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let line = LineId::from_str("00000000-0000-0000-0000-000000000710")?;
    let branch = BranchId::from_str("00000000-0000-0000-0000-000000000711")?;
    let owner = LineOwner::new(line, 3, branch, 9);
    let deadline = Timestamp::new(120, 0)?;
    let context = context(
        environment(&workspace)?.with_line_owner(owner),
        Some(deadline),
    )?;

    let operation = operation_context(&context);

    assert_eq!(operation.owner(), Some(&owner));
    assert_eq!(
        operation.class(),
        OperationClass::EffectCall {
            call_id: context.call_id(),
            attempt_id: context.attempt_id(),
        }
    );
    assert_eq!(operation.budget(), Some(deadline));
    Ok(())
}

#[test]
fn an_environment_without_a_line_owner_starts_ownerless_operations() -> Result<(), Box<dyn Error>> {
    let workspace = tempfile::tempdir()?;
    let context = context(environment(&workspace)?, None)?;
    let operation = operation_context(&context);
    assert_eq!(operation.owner(), None);
    assert_eq!(operation.budget(), None);
    assert!(matches!(
        operation.class(),
        OperationClass::EffectCall { .. }
    ));
    Ok(())
}
