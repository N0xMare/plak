//! Small cross-package proof that an exposed portable identity survives agent admission.

use std::str::FromStr;
use std::sync::Arc;

use plakous_agent::internal::{EffectAuthorization, ToolExecutionControl, ToolRuntime};
use plakous_model::{
    AttemptId, CallId, CancellationToken, Clock, FixedClock, RunId, Timestamp, ToolConcurrency,
    ToolCorrelation, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
    TurnId,
};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentConfig, ExecutionEnvironmentScope, ProgressSink,
    ReadOnlyToolPolicy, RegisteredToolId, ToolExecutorId, ToolExposure, ToolExposureMap,
    ToolInvocationRequest, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
};
use serde_json::json;

#[tokio::test]
async fn admitted_exposure_dispatches_the_exact_canonical_executor()
-> Result<(), Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("read")?,
        "1.0.0",
        "read",
        ToolInputContract::structured(json!({
            "$schema":"https://json-schema.org/draft/2020-12/schema",
            "type":"object", "properties":{}, "required":[],
            "additionalProperties":false
        }))?,
        ToolOutputContract::portable_model_content(),
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let executor = Arc::new(plakous_test_support::ScriptedTool::new([
        ToolOutcome::success_text("read-result"),
    ]));
    let mut builder = ToolRegistryBuilder::default();
    builder.register(ToolRegistration::new(
        RegisteredToolId::new("read-id")?,
        ToolExecutorId::new("read-executor-v1")?,
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"read-executor-v1"])?,
        definition,
        executor.clone(),
    ))?;
    let registry = builder.freeze();
    let canonical = registry
        .identity(&ToolName::new("read")?)
        .cloned()
        .ok_or("canonical identity missing")?;
    let exposure = ToolExposureMap::new(
        registry.clone(),
        [ToolExposure::new(canonical.clone(), "read_file")?],
    )?;

    let run = RunId::from_str("00000000-0000-0000-0000-000000000011")?;
    let turn = TurnId::from_str("00000000-0000-0000-0000-000000000012")?;
    let attempt = AttemptId::from_str("00000000-0000-0000-0000-000000000013")?;
    let call = CallId::from_str("00000000-0000-0000-0000-000000000014")?;
    let invocation = exposure.prepare(ToolInvocationRequest::new(
        run,
        turn,
        attempt,
        call,
        ToolName::new("read_file")?,
        ToolCorrelation::new(None, None, plakous_model::ToolCallerLineage::Direct)?,
        Arc::<str>::from("{}"),
    ))?;
    let workspace = tempfile::tempdir()?;
    let environment = ExecutionEnvironment::host(
        ExecutionEnvironmentScope::Run(run),
        workspace.path(),
        ".",
        ExecutionEnvironmentConfig::default(),
    )?;
    let runtime = ToolRuntime::new(
        registry,
        Arc::new(environment),
        Arc::new(ReadOnlyToolPolicy),
    );
    let EffectAuthorization::Allowed(authorized) = runtime.authorize(invocation).await? else {
        return Err("read-only exposure was not admitted".into());
    };
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(77, 0)?));
    let (progress, _receiver) = ProgressSink::channel(2, call, Arc::clone(&clock))?;
    let outcome = runtime
        .execute(
            authorized,
            ToolExecutionControl::new(CancellationToken::new(), None, clock, progress),
        )
        .await?;

    assert_eq!(outcome.parts().len(), 1);
    let captured = executor.captured();
    assert_eq!(captured.len(), 1);
    let binding = captured[0].exposure().ok_or("exposure binding missing")?;
    assert_eq!(binding.canonical_identity(), &canonical);
    assert_eq!(binding.exposed_name().as_str(), "read_file");
    assert_eq!(binding.map_digest(), exposure.digest());
    Ok(())
}
