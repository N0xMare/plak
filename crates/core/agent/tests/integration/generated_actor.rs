//! Bounded generated actor command and safe model-fault schedules.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use plakous_agent::internal::{
    Agent, AgentBuilder, AgentConfig, AgentErrorKind, AgentEventKind, AgentRequestPolicy,
    AgentViewReducer, ControlErrorKind, EventDelivery, Turn, TurnControl, TurnResultOutcome,
    TurnViewState, ViewRequest,
};
use plakous_model::{
    AttemptId, BlockCompleted, BlockStart, BlockStarted, CachePolicy, CanonicalHistory, Capability,
    CompiledProviderConfiguration, ConfiguredLanguageModel, ContentBlock, ConversationItem,
    ConversationView, Digest, DigestDomain, FinishDetails, FinishReason, FixedClock,
    HistorySegment, HistorySegmentKind, ModelError, ModelErrorKind, ModelEvent, ModelRequest,
    ModelResponse, OutputBoundDisposition, OutputLocation, ReasoningPolicy, RequestMetadata,
    ResolvedModelDescriptor, ResponseStarted, RunId, RuntimeModelAuthority, SequenceIdGenerator,
    TextDelta, TextPhase, TurnId, ValidatedRequestPlan, ValidationMode, WorkingProjection,
};
use plakous_tools::{FrozenToolRegistry, ToolRegistryBuilder, WorkspacePolicy};
use proptest::prelude::*;
use proptest::test_runner::{RngSeed, TestCaseError};
use uuid::Uuid;

use crate::support::{
    self, DescriptorIds, Fixture, MAX_OUTPUT_TOKENS, TestResult, assistant_text_item,
    generation_limits, id, timestamp, user_text_item, workspace,
};

const FIXTURE: Fixture = Fixture {
    label: "generated-actor",
    provider_semantics: b"generated-actor-semantics-v1",
};
const DESCRIPTOR: DescriptorIds = DescriptorIds {
    protocol: "test.generated.protocol",
    route: "test.generated.route",
    model: "test.generated.model",
    family: "test.generated",
    profile: "test.generated.profile",
};

const CASE_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_secs(1);
const SCRIPT_COUNT: usize = 48;

fn runtime_authority(descriptor: &ResolvedModelDescriptor) -> TestResult<RuntimeModelAuthority> {
    support::runtime_authority(&FIXTURE, descriptor)
}

fn provider_configuration(
    descriptor: &ResolvedModelDescriptor,
) -> TestResult<CompiledProviderConfiguration> {
    support::provider_configuration(
        &FIXTURE,
        descriptor,
        &runtime_authority(descriptor)?,
        &[],
        CachePolicy::Disabled,
        OutputBoundDisposition::Enforced(
            NonZeroU32::new(MAX_OUTPUT_TOKENS).ok_or("nonzero generated actor output bound")?,
        ),
    )
}

fn descriptor() -> TestResult<ResolvedModelDescriptor> {
    support::descriptor(&FIXTURE, &DESCRIPTOR, &[Capability::Streaming])
}

fn scripted_model(
    descriptor: ResolvedModelDescriptor,
    scenarios: impl IntoIterator<Item = plakous_test_support::ModelScenario>,
) -> TestResult<plakous_test_support::ScriptedModel> {
    support::scripted_model(&FIXTURE, descriptor, scenarios)
}

fn identity_sources() -> TestResult<(Arc<FixedClock>, Arc<SequenceIdGenerator>)> {
    support::identity_sources(256)
}

#[derive(Clone, Copy, Debug)]
enum Command {
    Prompt,
    Cancel,
    Steer,
    FollowUp,
    Attach,
}

impl Command {
    const fn from_code(code: u8) -> Self {
        match code % 5 {
            0 => Self::Prompt,
            1 => Self::Cancel,
            2 => Self::Steer,
            3 => Self::FollowUp,
            _ => Self::Attach,
        }
    }
}

struct AdmittedTurn {
    id: TurnId,
    control: TurnControl,
    turn: Turn,
}

impl AdmittedTurn {
    fn new(turn: Turn) -> Self {
        Self {
            id: turn.id(),
            control: turn.control(),
            turn,
        }
    }
}

struct Observation {
    reducer: AgentViewReducer,
    terminals: HashMap<TurnId, usize>,
    model_starts: HashMap<TurnId, usize>,
    steering_queued: usize,
    steering_applied: usize,
}

fn generated_config(cases: u32, seed: u64) -> ProptestConfig {
    ProptestConfig {
        cases,
        max_shrink_iters: 64,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(seed),
        ..ProptestConfig::default()
    }
}

fn agent_builder(
    model: Arc<plakous_test_support::ScriptedModel>,
    tools: FrozenToolRegistry,
    workspace: Arc<WorkspacePolicy>,
) -> TestResult<AgentBuilder> {
    let authority = runtime_authority(model.descriptor())?;
    let configuration = provider_configuration(model.descriptor())?;
    let configured = ConfiguredLanguageModel::new(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        model,
        configuration,
        authority,
    )?;
    Ok(
        AgentBuilder::new(configured, tools, workspace).request_policy(AgentRequestPolicy::new(
            generation_limits()?,
            ReasoningPolicy::default(),
        )),
    )
}

fn event_identity_source() -> TestResult<Arc<SequenceIdGenerator>> {
    Ok(Arc::new(SequenceIdGenerator::new(
        (10_000_u128..=14_000).map(Uuid::from_u128),
    )?))
}

fn model_error(kind: ModelErrorKind, label: &str) -> ModelError {
    ModelError::new(kind, format!("generated safe fault: {label}"))
}

fn fault_steps(code: u8) -> TestResult<Vec<plakous_test_support::ScenarioStep>> {
    Ok(match code % 4 {
        0 => vec![plakous_test_support::ScenarioStep::Error(model_error(
            ModelErrorKind::Unavailable,
            "stream unavailable",
        ))],
        1 => vec![plakous_test_support::ScenarioStep::Eof],
        2 => vec![
            plakous_test_support::ScenarioStep::DelayPolls(2),
            plakous_test_support::ScenarioStep::Error(model_error(
                ModelErrorKind::ResourceLimit,
                "bounded resource exhaustion",
            )),
        ],
        _ => vec![plakous_test_support::ScenarioStep::Emit(
            ModelEvent::ResponseStarted(ResponseStarted::new(
                None,
                Digest::sha256(DigestDomain::RequestPlan, 1, &[b"wrong-plan-binding"])?,
            )),
        )],
    })
}

fn fault_scenario(
    code: u8,
    first_barrier: Option<&plakous_test_support::ScenarioBarrier>,
) -> TestResult<plakous_test_support::ModelScenario> {
    if let Some(barrier) = first_barrier {
        return Ok(plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Barrier(barrier.clone()),
            fault_steps(code)?,
        ));
    }
    Ok(match code % 5 {
        0 => plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Error(model_error(
                ModelErrorKind::Unavailable,
                "start unavailable",
            )),
            Vec::new(),
        ),
        1 => plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::DelayPolls(2),
            fault_steps(code)?,
        ),
        _ => plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::Any,
            plakous_test_support::ScenarioStart::Immediate,
            fault_steps(code)?,
        ),
    })
}

fn fault_scripts(
    codes: &[u8],
    first_barrier: &plakous_test_support::ScenarioBarrier,
) -> TestResult<Vec<plakous_test_support::ModelScenario>> {
    let mut scenarios = Vec::with_capacity(SCRIPT_COUNT);
    scenarios.push(fault_scenario(
        codes.first().copied().unwrap_or_default(),
        Some(first_barrier),
    )?);
    for index in 1_u8..u8::try_from(SCRIPT_COUNT)? {
        let code = codes
            .get(usize::from(index) % codes.len())
            .copied()
            .unwrap_or(index);
        scenarios.push(fault_scenario(code.wrapping_add(index), None)?);
    }
    Ok(scenarios)
}

async fn observe_agent(
    agent: &Agent,
) -> TestResult<tokio::task::JoinHandle<Result<Observation, String>>> {
    let attachment = agent.attach(ViewRequest::metadata()).await?;
    let (snapshot, mut events) = attachment.into_parts();
    Ok(tokio::spawn(async move {
        let mut observation = Observation {
            reducer: AgentViewReducer::from_snapshot(snapshot),
            terminals: HashMap::new(),
            model_starts: HashMap::new(),
            steering_queued: 0,
            steering_applied: 0,
        };
        loop {
            let delivery = events
                .recv_delivery()
                .await
                .map_err(|error| format!("generated observer ended early: {error}"))?;
            let closed = if let EventDelivery::Event(event) = &delivery {
                if let Some(turn_id) = event.turn_id() {
                    match event.kind() {
                        AgentEventKind::TurnCompleted { .. }
                        | AgentEventKind::TurnFailed { .. } => {
                            *observation.terminals.entry(turn_id).or_default() += 1;
                        }
                        AgentEventKind::ModelStarted => {
                            *observation.model_starts.entry(turn_id).or_default() += 1;
                        }
                        AgentEventKind::SteeringQueued { .. } => {
                            observation.steering_queued += 1;
                        }
                        AgentEventKind::SteeringApplied => {
                            observation.steering_applied += 1;
                        }
                        _ => {}
                    }
                }
                matches!(event.kind(), AgentEventKind::AgentClosed)
            } else {
                false
            };
            observation
                .reducer
                .apply_delivery(&delivery)
                .map_err(|error| format!("generated reducer rejected a delivery: {error}"))?;
            if closed {
                return Ok(observation);
            }
        }
    }))
}

async fn wait_for_first_dispatch(model: &plakous_test_support::ScriptedModel) -> TestResult {
    tokio::time::timeout(POLL_TIMEOUT, async {
        while model.captured_plans().is_empty() {
            // Let an idle paused runtime advance to the dispatch deadline.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await?;
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn a_schedule_without_dispatch_hits_the_virtual_poll_deadline() -> TestResult {
    let model = scripted_model(descriptor()?, [])?;
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(CASE_TIMEOUT, wait_for_first_dispatch(&model))
        .await?
        .err()
        .ok_or("a schedule without dispatch must time out")?;
    assert!(
        error
            .downcast_ref::<tokio::time::error::Elapsed>()
            .is_some()
    );
    assert_eq!(started.elapsed(), POLL_TIMEOUT);
    assert_eq!(model.captured_plans(), [] as [ValidatedRequestPlan; 0]);
    Ok(())
}

fn assert_unique_dispatch(plans: &[ValidatedRequestPlan]) -> TestResult {
    let mut digests = HashSet::new();
    let mut attempts = HashSet::new();
    for plan in plans {
        if !digests.insert(plan.digest()) {
            return Err("a prepared request plan was dispatched more than once".into());
        }
        let metadata = plan.request().metadata();
        if !attempts.insert((metadata.turn_id(), metadata.attempt_id())) {
            return Err("a turn/attempt identity was dispatched more than once".into());
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "the generated actor schedule keeps admission, controls, observation, and joined invariant checks in one auditable case"
)]
async fn run_fault_schedule(
    command_codes: Vec<u8>,
    fault_codes: Vec<u8>,
    target_salt: usize,
    cancel_active: bool,
    shutdown_early: bool,
) -> TestResult {
    let descriptor = descriptor()?;
    let release = plakous_test_support::ScenarioBarrier::new();
    let scripts = fault_scripts(&fault_codes, &release)?;
    let model = Arc::new(scripted_model(descriptor, scripts)?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        model.clone(),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .config(
        AgentConfig::default()
            .with_prompt_queue_capacity(3)
            .with_follow_up_queue_capacity(3)
            .with_steering_queue_capacity(3)
            .with_event_capacity(512),
    )
    .identity_sources(clock, ids)
    .event_identity_source(event_identity_source()?)
    .build()?;
    let observer = observe_agent(&agent).await?;

    let active = AdmittedTurn::new(agent.prompt("active").await?);
    let active_id = active.id;
    let active_control = active.control.clone();
    let mut admitted = vec![active];
    wait_for_first_dispatch(model.as_ref()).await?;

    if active_control.steer("mandatory-steering").await.is_err() {
        return Err("the mandatory active steering command was not admitted".into());
    }
    let follow_up = agent.follow_up(active_id, "mandatory-follow-up").await?;
    admitted.push(AdmittedTurn::new(follow_up));
    let queued = AdmittedTurn::new(agent.prompt("mandatory-queued-cancel").await?);
    let queued_id = queued.id;
    let queued_control = queued.control.clone();
    admitted.push(queued);
    let _ = queued_control.cancel().await?;

    let checkpoint = agent.attach(ViewRequest::metadata()).await?;
    if checkpoint.snapshot().cursor().sequence().get() == 0 {
        return Err("mid-schedule attachment observed no admitted lifecycle".into());
    }
    drop(checkpoint);

    for (index, code) in command_codes.into_iter().enumerate() {
        match Command::from_code(code) {
            Command::Prompt => match agent.prompt(format!("generated-prompt-{index}")).await {
                Ok(turn) => admitted.push(AdmittedTurn::new(turn)),
                Err(error)
                    if matches!(
                        error.kind(),
                        AgentErrorKind::Backpressure | AgentErrorKind::Shutdown
                    ) => {}
                Err(error) => return Err(error.into()),
            },
            Command::Cancel => {
                let target = (index + target_salt) % admitted.len();
                match admitted[target].control.cancel().await {
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            ControlErrorKind::Stale | ControlErrorKind::Shutdown
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Command::Steer => match active_control.steer(format!("steer-{index}")).await {
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        ControlErrorKind::QueueFull
                            | ControlErrorKind::WrongPhase
                            | ControlErrorKind::Shutdown
                    ) => {}
                Err(error) => return Err(error.into()),
            },
            Command::FollowUp => {
                match agent
                    .follow_up(active_id, format!("follow-up-{index}"))
                    .await
                {
                    Ok(turn) => admitted.push(AdmittedTurn::new(turn)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            ControlErrorKind::QueueFull
                                | ControlErrorKind::WrongPhase
                                | ControlErrorKind::Stale
                                | ControlErrorKind::Shutdown
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Command::Attach => {
                let attachment = agent.attach(ViewRequest::metadata()).await?;
                drop(attachment);
            }
        }
    }

    if cancel_active {
        let _ = active_control.cancel().await;
    }
    if !shutdown_early {
        release.release();
        tokio::task::yield_now().await;
    }

    let admitted_ids = admitted.iter().map(|turn| turn.id).collect::<HashSet<_>>();
    if !admitted_ids.contains(&queued_id) {
        return Err("mandatory queued cancellation lost its admitted identity".into());
    }
    agent.shutdown().await?;
    release.release();
    let captured_after_join = model.captured_plans();
    agent.shutdown().await?;
    if model.captured_plans() != captured_after_join {
        return Err("joined shutdown allowed a later model dispatch".into());
    }
    if model.remaining_scenarios() + captured_after_join.len() != SCRIPT_COUNT {
        return Err("a scripted operation was removed without a captured dispatch".into());
    }
    if captured_after_join
        .iter()
        .any(|plan| plan.request().metadata().turn_id() == Some(queued_id))
    {
        return Err("queued cancellation crossed the model dispatch boundary".into());
    }
    assert_unique_dispatch(&captured_after_join)?;

    let rejected = agent
        .prompt("must-not-admit-after-shutdown")
        .await
        .err()
        .ok_or("joined shutdown accepted a new prompt")?;
    if rejected.kind() != AgentErrorKind::Shutdown {
        return Err("post-shutdown admission had the wrong error category".into());
    }
    for admitted_turn in admitted {
        let result = tokio::time::timeout(POLL_TIMEOUT, admitted_turn.turn.result()).await??;
        if result.turn_id() != admitted_turn.id {
            return Err("turn result identity diverged from admission".into());
        }
        match result.outcome() {
            TurnResultOutcome::Completed(response) => {
                if result.response() != Some(response) || result.error().is_some() {
                    return Err("completed result accessors diverged from its outcome".into());
                }
            }
            TurnResultOutcome::Cancelled(error) | TurnResultOutcome::Failed(error) => {
                if result.response().is_some()
                    || result.error() != Some(error)
                    || !matches!(
                        error.kind(),
                        AgentErrorKind::Model
                            | AgentErrorKind::Cancelled
                            | AgentErrorKind::Shutdown
                    )
                {
                    return Err("failed result accessors or terminal category diverged".into());
                }
            }
            TurnResultOutcome::RecoveryRequired(_) => {
                return Err("memory-only generated schedule required recovery".into());
            }
        }
    }

    let observation = tokio::time::timeout(POLL_TIMEOUT, observer).await???;
    if observation
        .terminals
        .keys()
        .copied()
        .collect::<HashSet<_>>()
        != admitted_ids
    {
        return Err("terminal event identities did not match admitted turns".into());
    }
    if observation.terminals.values().any(|count| *count != 1) {
        return Err("an admitted turn did not have exactly one terminal event".into());
    }
    let observed_starts = observation.model_starts.values().sum::<usize>();
    if observed_starts != captured_after_join.len() {
        return Err("model-start events and captured dispatches diverged".into());
    }
    let final_attachment = agent.attach(ViewRequest::metadata()).await?;
    if observation.reducer.snapshot() != final_attachment.snapshot() {
        return Err("live reducer did not converge to a fresh atomic snapshot".into());
    }
    Ok(())
}

fn request_for(
    configuration: &CompiledProviderConfiguration,
    turn: u128,
    attempt: u128,
    history: Vec<ConversationItem>,
) -> TestResult<ModelRequest> {
    Ok(ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::from_items(history)?,
        RequestMetadata::for_turn(
            id::<RunId>(4)?,
            id::<TurnId>(turn)?,
            id::<AttemptId>(attempt)?,
            timestamp()?,
        ),
    )
    .limits(generation_limits()?)
    .build()?)
}

fn segmented_request_for(
    configuration: &CompiledProviderConfiguration,
    turn: u128,
    attempt: u128,
    segments: Vec<(HistorySegmentKind, Vec<ConversationItem>)>,
) -> TestResult<ModelRequest> {
    let history =
        segments
            .into_iter()
            .try_fold(CanonicalHistory::default(), |history, (kind, items)| {
                Ok::<_, Box<dyn std::error::Error>>(
                    history.append(HistorySegment::new(kind, items)?)?,
                )
            })?;
    Ok(ModelRequest::builder(
        configuration.selected_identity(),
        configuration.effective_identity(),
        ConversationView::from_shared(history, WorkingProjection::default()),
        RequestMetadata::for_turn(
            id::<RunId>(4)?,
            id::<TurnId>(turn)?,
            id::<AttemptId>(attempt)?,
            timestamp()?,
        ),
    )
    .limits(generation_limits()?)
    .build()?)
}

fn success_scenario(
    descriptor: &ResolvedModelDescriptor,
    request: ModelRequest,
    output: &ConversationItem,
    start: plakous_test_support::ScenarioStart,
) -> TestResult<plakous_test_support::ModelScenario> {
    let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
    let digest = plan.digest();
    let location = OutputLocation::new(0, 0);
    let text = output
        .blocks()
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text()),
            _ => None,
        })
        .ok_or("generated success output omitted text")?;
    let response = ModelResponse::new(
        vec![output.clone()],
        FinishDetails::new(FinishReason::Stop, None),
        None,
        None,
        digest,
    );
    Ok(plakous_test_support::ModelScenario::new(
        plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
        start,
        vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                output.id(),
                location,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                location,
                text.as_bytes(),
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ],
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "the exact identity-bound success schedule keeps all four plakous_test_support::ScriptedModel request boundaries auditable together"
)]
async fn run_success_schedule(follow_first: bool, attach_after_controls: bool) -> TestResult {
    let descriptor = descriptor()?;
    let configuration = provider_configuration(&descriptor)?;
    let release = plakous_test_support::ScenarioBarrier::new();
    let initial = user_text_item(6, "active")?;
    let active_first = assistant_text_item(1_000, "active-first")?;
    let steering = user_text_item(10, "steering")?;
    let active_final = assistant_text_item(1_001, "active-final")?;
    let follow = user_text_item(12, "follow-up")?;
    let follow_output = assistant_text_item(1_002, "follow-result")?;
    let normal = user_text_item(14, "normal")?;
    let normal_output = assistant_text_item(1_003, "normal-result")?;
    let follow_turn = if follow_first { 8 } else { 9 };
    let normal_turn = if follow_first { 9 } else { 8 };

    let scenarios = [
        success_scenario(
            &descriptor,
            request_for(&configuration, 5, 7, vec![initial.clone()])?,
            &active_first,
            plakous_test_support::ScenarioStart::Barrier(release.clone()),
        )?,
        success_scenario(
            &descriptor,
            segmented_request_for(
                &configuration,
                5,
                11,
                vec![
                    (HistorySegmentKind::RecentIntent, vec![initial.clone()]),
                    (HistorySegmentKind::Conversation, vec![active_first.clone()]),
                    (HistorySegmentKind::Steering, vec![steering.clone()]),
                ],
            )?,
            &active_final,
            plakous_test_support::ScenarioStart::Immediate,
        )?,
        success_scenario(
            &descriptor,
            segmented_request_for(
                &configuration,
                follow_turn,
                13,
                vec![
                    (HistorySegmentKind::RecentIntent, vec![initial.clone()]),
                    (HistorySegmentKind::Conversation, vec![active_first.clone()]),
                    (HistorySegmentKind::Steering, vec![steering]),
                    (HistorySegmentKind::Conversation, vec![active_final.clone()]),
                    (HistorySegmentKind::RecentIntent, vec![follow.clone()]),
                ],
            )?,
            &follow_output,
            plakous_test_support::ScenarioStart::Immediate,
        )?,
        success_scenario(
            &descriptor,
            segmented_request_for(
                &configuration,
                normal_turn,
                15,
                vec![
                    (HistorySegmentKind::RecentIntent, vec![initial]),
                    (HistorySegmentKind::Conversation, vec![active_first]),
                    (
                        HistorySegmentKind::Steering,
                        vec![user_text_item(10, "steering")?],
                    ),
                    (HistorySegmentKind::Conversation, vec![active_final]),
                    (HistorySegmentKind::RecentIntent, vec![follow]),
                    (HistorySegmentKind::Conversation, vec![follow_output]),
                    (HistorySegmentKind::RecentIntent, vec![normal]),
                ],
            )?,
            &normal_output,
            plakous_test_support::ScenarioStart::Immediate,
        )?,
    ];
    let model = Arc::new(scripted_model(descriptor, scenarios)?);
    let (_directory, workspace) = workspace()?;
    let (clock, ids) = identity_sources()?;
    let agent = agent_builder(
        model.clone(),
        ToolRegistryBuilder::default().freeze(),
        workspace,
    )?
    .identity_sources(clock, ids)
    .event_identity_source(event_identity_source()?)
    .build()?;
    let observer = observe_agent(&agent).await?;
    let active = AdmittedTurn::new(agent.prompt("active").await?);
    let active_id = active.id;
    wait_for_first_dispatch(model.as_ref()).await?;

    let (follow_turn, normal_turn) = if follow_first {
        (
            AdmittedTurn::new(agent.follow_up(active_id, "follow-up").await?),
            AdmittedTurn::new(agent.prompt("normal").await?),
        )
    } else {
        let normal = AdmittedTurn::new(agent.prompt("normal").await?);
        let follow = AdmittedTurn::new(agent.follow_up(active_id, "follow-up").await?);
        (follow, normal)
    };
    if active.control.steer("steering").await.is_err() {
        return Err("successful generated steering was not admitted".into());
    }
    let admitted_ids = HashSet::from([active.id, follow_turn.id, normal_turn.id]);
    if attach_after_controls {
        let attachment = agent.attach(ViewRequest::metadata()).await?;
        drop(attachment);
    }
    release.release();

    let active_result = active.turn.result().await?;
    if active_result.model_rounds() != 2
        || active_result
            .response()
            .ok_or("active success omitted its response")?
            .output_text()
            != "active-final"
    {
        return Err("steering did not produce the expected second safe-boundary round".into());
    }
    if follow_turn
        .turn
        .result()
        .await?
        .response()
        .ok_or("follow-up success omitted its response")?
        .output_text()
        != "follow-result"
    {
        return Err("follow-up did not retain priority after the active turn".into());
    }
    if normal_turn
        .turn
        .result()
        .await?
        .response()
        .ok_or("normal success omitted its response")?
        .output_text()
        != "normal-result"
    {
        return Err("normal prompt did not run after the admitted follow-up".into());
    }
    agent.shutdown().await?;
    let captured_after_join = model.captured_plans();
    agent.shutdown().await?;
    if model.captured_plans() != captured_after_join {
        return Err("joined success cleanup allowed a later model dispatch".into());
    }
    let rejected = agent
        .prompt("must-not-admit-after-success-shutdown")
        .await
        .err()
        .ok_or("joined success cleanup accepted a new prompt")?;
    if rejected.kind() != AgentErrorKind::Shutdown {
        return Err("successful post-shutdown admission had the wrong category".into());
    }

    let plans = captured_after_join;
    if plans.len() != 4 || model.remaining_scenarios() != 0 {
        return Err("successful control schedule did not consume exactly four dispatches".into());
    }
    assert_unique_dispatch(&plans)?;
    let observation = tokio::time::timeout(POLL_TIMEOUT, observer).await???;
    if observation
        .terminals
        .keys()
        .copied()
        .collect::<HashSet<_>>()
        != admitted_ids
        || observation.terminals.values().any(|count| *count != 1)
        || observation.model_starts.values().sum::<usize>() != plans.len()
        || observation.steering_queued != 1
        || observation.steering_applied != 1
    {
        return Err("successful control lifecycle counts diverged".into());
    }
    let final_attachment = agent.attach(ViewRequest::metadata()).await?;
    if observation.reducer.snapshot() != final_attachment.snapshot() {
        return Err("successful schedule reducer did not converge".into());
    }
    if !observation
        .reducer
        .snapshot()
        .turns()
        .iter()
        .all(|turn| turn.state() != TurnViewState::Running)
    {
        return Err("joined success cleanup retained a running turn".into());
    }
    Ok(())
}

proptest! {
    #![proptest_config(generated_config(16, 0x504c_414b_4b30_0002))]

    #[test]
    fn generated_actor_commands_and_safe_faults_have_total_joined_terminals(
        commands in prop::collection::vec(any::<u8>(), 4..10),
        faults in prop::collection::vec(any::<u8>(), 4..12),
        target_salt in 0usize..16,
        cancel_active in any::<bool>(),
        shutdown_early in any::<bool>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        runtime
            .block_on(async {
                tokio::time::timeout(
                    CASE_TIMEOUT,
                    run_fault_schedule(
                        commands,
                        faults,
                        target_salt,
                        cancel_active,
                        shutdown_early,
                    ),
                )
                .await?
            })
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
    }
}

proptest! {
    #![proptest_config(generated_config(4, 0x504c_414b_4b30_0055))]

    #[test]
    fn generated_success_permutations_apply_steering_and_prioritize_follow_up(
        follow_first in any::<bool>(),
        attach_after_controls in any::<bool>(),
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
        runtime
            .block_on(async {
                tokio::time::timeout(
                    CASE_TIMEOUT,
                    run_success_schedule(follow_first, attach_after_controls),
                )
                .await?
            })
            .map_err(|error| TestCaseError::fail(error.to_string()))?;
    }
}
