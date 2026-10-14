//! Exact supported Custom-tool inventory and registration workflow.
//!
//! Driver scheduling and admitted-effect transitions are intentionally absent
//! even while transitional implementation items remain publicly nameable.

use std::sync::Arc;

use plakous_model::{
    ToolConcurrency, ToolDefinition, ToolEffect, ToolInputContract, ToolName, ToolOutputContract,
};
use plakous_tools::{
    FrozenToolRegistry, RegisteredToolId, ToolContext, ToolExecutor, ToolExecutorId, ToolFuture,
    ToolInvocation, ToolOutcome, ToolRegistration, ToolRegistryBuilder,
};

struct ConsumerTool;

impl ToolExecutor for ConsumerTool {
    fn execute(self: Arc<Self>, _invocation: ToolInvocation, context: ToolContext) -> ToolFuture {
        let _preempt = context.preempt().clone();
        let _none = plakous_tools::ToolPreempt::None;
        let _steer = plakous_tools::ToolPreempt::Steer;
        let _background = plakous_tools::ToolPreempt::Background;
        Box::pin(async { ToolOutcome::success_text("consumer tool") })
    }
}

fn register_custom_tool() -> Result<FrozenToolRegistry, Box<dyn std::error::Error>> {
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
        plakous_tools::ToolExecutorBindingIdentity::new(1, &[b"consumer.tool.v1"])?,
        definition,
        Arc::new(ConsumerTool),
    );
    let mut builder = ToolRegistryBuilder::default();
    builder.register(registration)?;
    Ok(builder.freeze())
}

fn main() {
    let registry = register_custom_tool().expect("custom tool registers");
    assert_eq!(registry.len(), 1);
}
