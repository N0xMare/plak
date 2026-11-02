//! Agent control, bounded observation, and terminal-result workflow.

use plakous::agent::{
    Agent, AgentError, AgentViewSnapshot, CommandId, EventSubscription, Turn, TurnCommand,
    TurnResult, TurnSubmission, ViewAttachment, ViewError, ViewRequest,
};

pub(crate) fn surface_is_nameable() {
    let _ = std::any::type_name::<AgentViewSnapshot>();
    let _ = std::any::type_name::<EventSubscription>();
    let _ = std::any::type_name::<TurnResult>();
    let _ = std::any::type_name::<plakous::tools::JobConfig>();
    let _ = std::any::type_name::<plakous::tools::JobStatus>();
    let _ = std::any::type_name::<plakous::tools::JobStopReason>();
    let _ = std::any::type_name::<plakous::tools::JobView>();
    let _ = [
        plakous::tools::JobStatus::Running,
        plakous::tools::JobStatus::Exited(None),
        plakous::tools::JobStatus::Stopped(plakous::tools::JobStopReason::User),
        plakous::tools::JobStatus::OutcomeUnknown,
    ];
    let _ = [
        plakous::tools::JobStopReason::Model,
        plakous::tools::JobStopReason::User,
        plakous::tools::JobStopReason::SessionExit,
        plakous::tools::JobStopReason::AttemptEnded,
        plakous::tools::JobStopReason::HardCap,
    ];
    let _ = std::any::type_name::<plakous::tools::JobHostCapacity>();
    let _ = std::any::type_name::<plakous::tools::JobOwner>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessSessions>();
    let _ = std::any::type_name::<plakous::tools::JoinedSessionsPort>();
    let _ = std::any::type_name::<plakous::tools::LocalWorkspaceProbe>();
    let _ = std::any::type_name::<&dyn plakous::tools::WorkspaceProbe>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessSessionId>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessStartRequest>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessCommand>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessShellProfile>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessTerminalMode>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessStatus>();
    let _ = std::any::type_name::<plakous::tools::JoinedProcessInputDisposition>();
    let _ = HostRegistry;
    let _ = plakous::agent::AgentEventKind::JobPromoted { number: 1 };
    let _ = plakous::agent::AgentEventKind::JobFinished { number: 1 };
}

#[allow(dead_code)]
pub(crate) async fn jobs(
    builder: plakous::agent::AgentBuilder,
    host: std::sync::Arc<dyn plakous::tools::JobHost>,
    agent: &Agent,
) -> Result<Vec<plakous::tools::JobView>, Box<dyn std::error::Error>> {
    let config = plakous::tools::JobConfig::new(30, 10)?;
    let _builder = builder.job_host(host.clone()).job_config(config);
    let _changed = agent.record_job_terminals().await?;
    let _runtime = plakous::sessions::SessionRuntimeBinding::new(_builder)
        .with_job_host(host)
        .with_job_counter(agent)?;
    Ok(agent.jobs())
}

struct HostRegistry;

impl plakous::tools::JobHost for HostRegistry {
    fn service(
        &self,
        _: plakous::model::SessionId,
        _: plakous::model::BranchId,
    ) -> Option<plakous::tools::JoinedSessionsPort> {
        None
    }

    fn attach(
        &self,
        _owner: plakous::tools::JobOwner,
        fresh: plakous::tools::JoinedSessionsPort,
    ) -> Result<plakous::tools::JoinedSessionsPort, plakous::tools::ExecutionBackendError> {
        fresh.stop_foreground()?;
        Ok(fresh)
    }
}

#[allow(dead_code)]
pub(crate) async fn control_observe_and_finish(
    agent: &Agent,
    id: CommandId,
    command: TurnCommand,
    view: ViewRequest,
) -> Result<(ViewAttachment, TurnSubmission), Box<dyn std::error::Error>> {
    let _acknowledgement = agent.command(command).await?;
    let attachment = agent.attach(view).await.map_err(box_view_error)?;
    let submission = agent.start(id, "named facade workflow").await?;
    Ok((attachment, submission))
}

#[allow(dead_code)]
pub(crate) async fn finish(turn: Turn) -> Result<TurnResult, AgentError> {
    turn.result().await
}

fn box_view_error(error: ViewError) -> Box<dyn std::error::Error> {
    Box::new(error)
}

/// The complete public attach contract, without a library presentation model.
#[allow(dead_code)]
fn attach_contract(
    attachment: ViewAttachment,
    event: &plakous::agent::AgentEvent,
    delivery: &plakous::agent::EventDelivery,
) -> Result<AgentViewSnapshot, plakous::agent::ReducerError> {
    use plakous::agent::{AgentViewReducer, AgentViewState, TurnViewState};
    let (snapshot, _tail) = attachment.into_parts();
    let _cursor: plakous::agent::EventCursor = snapshot.cursor();
    let _source: AgentViewState = snapshot.state();
    for turn in snapshot.turns() {
        let _identity: plakous::model::TurnId = turn.turn_id();
        let _lifecycle: TurnViewState = turn.state();
    }
    for pending in snapshot.pending_effect_decisions() {
        let _identity = pending.identity();
        let _reference: &plakous::agent::EffectDecisionRef = pending.decision();
        let _description = pending.prompt();
        let _deadline: plakous::model::Timestamp = pending.expires_at();
    }
    let mut reducer = AgentViewReducer::from_snapshot(snapshot);
    reducer.apply(event)?;
    reducer.apply_delivery(delivery)?;
    let _incomplete = reducer.snapshot().presentation_incomplete();
    Ok(reducer.into_snapshot())
}

#[allow(dead_code)]
fn name_agent_error(error: AgentError) -> AgentError {
    error
}

#[allow(dead_code)]
async fn turn_loss_is_explicit(events: &mut plakous::agent::TurnEvents)
    -> Result<plakous::agent::EventDelivery, plakous::agent::TurnEventError> {
    events.recv_delivery().await
}
