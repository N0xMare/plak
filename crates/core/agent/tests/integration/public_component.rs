//! Positive compile inventory for the curated single-agent component.

#![allow(
    clippy::expect_used,
    reason = "reviewed literals fail fast if the public construction contract drifts"
)]

use plakous_agent::{
    Agent, AgentBuilder, AgentError, AgentEvent, AgentRequestPolicy, AgentViewReducer,
    AgentViewSnapshot, CancellationStatus, CommandAck, CommandId, ContextSourceCatalog,
    ContextSourceResolver, ContextSourceSelector, ControlError, EffectDecisionChoice,
    EffectDecisionStatus, EventSubscription, HarnessProfileId, HarnessProfileSpecBuilder,
    InstructionCompositionStrategy, RegisteredContextSource, ResumeOutcome, SteeringStatus,
    TelemetryEmitter, TelemetryObservation, Turn, TurnCommand, TurnControl, TurnEvents, TurnResult,
    TurnResultHandle, TurnSubmission, TurnUsage, ViewAttachment, ViewError, ViewRequest, WeakAgent,
    compile_explicit_profile,
};

fn hold<T: ?Sized>(_: &T) {}

#[test]
fn construction_context_profile_and_observation_values_are_nameable() {
    let selector = ContextSourceSelector::new("repo.agents").expect("selector");
    let profile = HarnessProfileSpecBuilder::new()
        .id(HarnessProfileId::new("component.profile").expect("profile id"))
        .base_instructions("base")
        .instruction_strategy(InstructionCompositionStrategy::BaseThenSources)
        .instruction_sources(vec![selector])
        .build()
        .expect("profile");
    let mut catalog = ContextSourceCatalog::new();
    catalog
        .insert(
            RegisteredContextSource::new("repo.agents", "exact bytes", "1", "component")
                .expect("source"),
        )
        .expect("catalog insertion");
    let sources = ContextSourceResolver::new(catalog)
        .resolve(profile.instruction_sources())
        .expect("resolved context");

    assert_eq!(sources[0].content(), "exact bytes");
    hold(&std::any::type_name::<AgentRequestPolicy>());
    hold(&std::any::type_name::<AgentEvent>());
    hold(&std::any::type_name::<AgentViewReducer>());
    hold(&std::any::type_name::<AgentViewSnapshot>());
    hold(&std::any::type_name::<EventSubscription>());
    hold(&std::any::type_name::<TelemetryEmitter>());
    hold(&std::any::type_name::<TelemetryObservation>());
    hold(&std::any::type_name::<TurnUsage>());
    hold(&std::any::type_name::<WeakAgent>());
}

#[test]
fn build_turn_control_observe_and_result_workflow_is_nameable() {
    fn build(builder: AgentBuilder) -> Result<Agent, AgentError> {
        builder.build()
    }

    async fn prompt(agent: &Agent) -> Result<Turn, AgentError> {
        agent.prompt("prompt").await
    }

    async fn start(agent: &Agent, command_id: CommandId) -> Result<TurnSubmission, AgentError> {
        agent.start(command_id, "prompt").await
    }

    async fn command(agent: &Agent, command: TurnCommand) -> Result<TurnSubmission, ControlError> {
        agent.command(command).await
    }

    async fn control(
        control: &TurnControl,
        command_id: CommandId,
        reference: plakous_agent::EffectDecisionRef,
        decision: EffectDecisionChoice,
    ) -> (
        Result<SteeringStatus, ControlError>,
        Result<CancellationStatus, ControlError>,
        Result<CommandAck, ControlError>,
    ) {
        (
            control.steer("steer").await,
            control.cancel().await,
            control
                .resolve_effect_decision_with_id(command_id, reference, decision)
                .await,
        )
    }

    async fn observe(
        mut turn: Turn,
    ) -> (
        Result<TurnEvents, ViewError>,
        Result<TurnResult, AgentError>,
    ) {
        (turn.take_events(), turn.result().await)
    }

    async fn attach(agent: &Agent, request: ViewRequest) -> Result<ViewAttachment, ViewError> {
        agent.attach(request).await
    }

    async fn resume(
        agent: &Agent,
        attachment: &ViewAttachment,
        request: ViewRequest,
    ) -> Result<ResumeOutcome, ViewError> {
        agent.resume(attachment.snapshot().cursor(), request).await
    }

    async fn duplicate_result(
        handle: TurnResultHandle,
    ) -> Result<std::sync::Arc<TurnResult>, AgentError> {
        handle.result().await
    }

    fn acknowledge(ack: &CommandAck) -> u64 {
        ack.sequence()
    }

    let compile_profile = compile_explicit_profile;
    hold(&std::any::type_name_of_val(&compile_profile));
    let _ = (
        build,
        prompt,
        start,
        command,
        control,
        observe,
        attach,
        resume,
        duplicate_result,
        acknowledge,
    );
    hold(&std::any::type_name::<EffectDecisionStatus>());
}
