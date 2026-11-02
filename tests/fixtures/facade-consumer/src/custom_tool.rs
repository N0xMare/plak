//! A caller-defined local tool implementation through the root facade.

use std::sync::Arc;

use plakous::model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous::tools::{
    FrozenToolRegistry, RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolFuture,
    ToolInvocation, ToolOutcome, ToolRegistration, ToolRegistryBuilder, WaitFact, WaitWake,
};

struct ConsumerTool;

impl ToolExecutor for ConsumerTool {
    fn execute(self: Arc<Self>, invocation: ToolInvocation, _context: ToolContext) -> ToolFuture {
        assert_eq!(invocation.arguments().text(), Some("consumer-input"));
        Box::pin(async { ToolOutcome::success_text("consumer tool") })
    }
}

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<ConsumerTool>();
    let _ = std::any::type_name::<dyn ToolExecutor>();
}

pub(crate) fn register_custom_tool() -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
    let definition = ToolDefinition::new(
        ToolName::new("consumer_tool")?,
        "1.0.0",
        "Return a fixed consumer fixture result.",
        ToolInputContract::utf8(256)?,
        ToolOutputContract::utf8(256)?,
        ToolConcurrency::Serial,
        ToolEffect::ReadOnly,
    )?;
    let registration = ToolRegistration::new(
        RegisteredToolId::new("consumer.tool")?,
        ToolExecutorId::new("consumer.tool.v1")?,
        plakous::tools::ToolExecutorBindingIdentity::new(1, &[b"consumer.tool.v1"])?,
        definition,
        Arc::new(ConsumerTool),
    );
    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration)?;
    Ok(builder.freeze())
}

/// A waiting tool reports what it waited on and how the wait ended as a typed
/// fact beside its result; hosts measure from the fact, never the text.
pub(crate) fn wait_fact_rides_an_outcome() -> Result<(), Box<dyn std::error::Error>> {
    let outcome = ToolOutcome::success_text("waited")?
        .with_wait_fact(WaitFact::new(1, 2, WaitWake::Interrupted));
    let fact = outcome.wait_fact().ok_or("wait fact")?;
    assert_eq!((fact.agents, fact.processes), (1, 2));
    assert_eq!(fact.wake, WaitWake::Interrupted);
    assert!(ToolOutcome::success_text("plain")?.wait_fact().is_none());
    Ok(())
}
