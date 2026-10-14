//! Build, prompt, command, control, and terminal result through the component.

use plakous_agent::{
    Agent, AgentBuilder, AgentError, CommandId, Turn, TurnCommand, TurnResult, TurnSubmission,
};

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<AgentBuilder>();
    let _ = std::any::type_name::<TurnResult>();
    let _ = std::any::type_name::<plakous_agent::JobView>();
    let _ = std::any::type_name::<plakous_agent::JobOwner>();
    let _ = plakous_agent::AgentEventKind::JobPromoted { number: 1 };
    let _ = plakous_agent::AgentEventKind::JobFinished { number: 1 };
}

#[allow(dead_code)]
pub(crate) async fn jobs(
    builder: AgentBuilder,
    host: std::sync::Arc<dyn plakous_agent::JobHost>,
    agent: &Agent,
) -> Result<Vec<plakous_agent::JobView>, AgentError> {
    let _builder = builder
        .job_host(host)
        .job_config(plakous_agent::JobConfig::default());
    let _changed = agent.record_job_terminals().await?;
    Ok(agent.jobs())
}

#[allow(dead_code)]
pub(crate) fn build(builder: AgentBuilder) -> Result<Agent, AgentError> {
    builder.build()
}

#[allow(dead_code)]
pub(crate) async fn prompt(agent: &Agent) -> Result<Turn, AgentError> {
    agent.prompt("component workflow").await
}

#[allow(dead_code)]
pub(crate) async fn exit_without_join(agent: &Agent) -> Result<(), AgentError> {
    agent.begin_abandon();
    agent.abandon_and_release().await
}

#[allow(dead_code)]
pub(crate) async fn command(
    agent: &Agent,
    id: CommandId,
    command: TurnCommand,
) -> Result<TurnSubmission, Box<dyn std::error::Error>> {
    let _ = id;
    Ok(agent.command(command).await?)
}
