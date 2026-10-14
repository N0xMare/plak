//! Neutral application-edge execution and caller-supplied cancellation.
//!
//! This module exists so a facade-only application can drive Plakous futures
//! without depending directly on Plakous's async executor. It owns no agent,
//! model, provider, session, retry, or tool-loop behavior.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::io;
use std::time::Duration;

use plakous_agent::{
    Agent, AgentError, AgentEvent, ControlError, RUNTIME_EXECUTOR_STACK_BYTES, TurnResult,
    TurnResultOutcome,
};
use tokio::runtime::{Builder, Handle, Runtime};

/// Worker count for the application-edge runtime.
///
/// Tokio's default is one worker per logical CPU. A CLI/TUI on a many-core
/// host would otherwise spawn a hundred-plus threads during attach, which
/// dominates ready time and RSS. Four workers cover the single-agent loop,
/// HTTP, and a few overlapping `spawn_blocking` tools.
const APPLICATION_WORKER_THREADS: usize = 4;

/// Quit cannot sit on Tokio's default `Runtime` drop, which waits forever for
/// `spawn_blocking` (SQLite, HTTP). Leftover blocking work is leaked and the
/// process reaps it; unfinished writes reconcile from the checkpoint.
const APPLICATION_EXECUTOR_SHUTDOWN: Duration = Duration::from_millis(250);

/// An application-edge executor for one top-level synchronous process.
///
/// Library consumers that already run inside an async executor should call the
/// ordinary async Plakous APIs and do not need this type.
pub struct ApplicationExecutor {
    runtime: Option<Runtime>,
}

impl ApplicationExecutor {
    /// Constructs one multi-thread Tokio executor with time and I/O enabled,
    /// whose threads carry [`RUNTIME_EXECUTOR_STACK_BYTES`] of stack.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationExecutorError::AlreadyRunning`] when called from a
    /// Tokio task, or [`ApplicationExecutorError::Construction`] when the
    /// executor cannot allocate its runtime resources.
    pub fn new() -> Result<Self, ApplicationExecutorError> {
        if Handle::try_current().is_ok() {
            return Err(ApplicationExecutorError::AlreadyRunning);
        }
        let runtime = Builder::new_multi_thread()
            .worker_threads(APPLICATION_WORKER_THREADS)
            // Worker and blocking threads both poll the agent's turn loop; its
            // unoptimized poll frames outgrow the 2 MiB `std::thread` default
            // (see `RUNTIME_EXECUTOR_STACK_BYTES`). Virtual reservation only.
            .thread_stack_size(RUNTIME_EXECUTOR_STACK_BYTES)
            .enable_all()
            .build()
            .map_err(ApplicationExecutorError::Construction)?;
        Ok(Self {
            runtime: Some(runtime),
        })
    }

    /// Drives one application future to completion.
    ///
    /// This method is only valid at the synchronous application edge.
    ///
    /// # Errors
    ///
    /// Returns [`ApplicationExecutorError::AlreadyRunning`] instead of nesting
    /// a blocking executor inside an active Tokio task.
    pub fn block_on<F>(&self, future: F) -> Result<F::Output, ApplicationExecutorError>
    where
        F: Future,
    {
        if Handle::try_current().is_ok() {
            return Err(ApplicationExecutorError::AlreadyRunning);
        }
        match &self.runtime {
            Some(runtime) => Ok(runtime.block_on(future)),
            None => Err(ApplicationExecutorError::AlreadyRunning),
        }
    }
}

impl Drop for ApplicationExecutor {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(APPLICATION_EXECUTOR_SHUTDOWN);
        }
    }
}

impl fmt::Debug for ApplicationExecutor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationExecutor")
            .finish_non_exhaustive()
    }
}

/// Failure to construct or safely enter an application executor.
#[derive(Debug)]
#[non_exhaustive]
pub enum ApplicationExecutorError {
    /// A blocking application executor was requested from an active Tokio task.
    AlreadyRunning,
    /// Tokio could not construct the executor.
    Construction(io::Error),
}

impl fmt::Display for ApplicationExecutorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => {
                formatter.write_str("application executor cannot be nested in a Tokio task")
            }
            Self::Construction(_) => {
                formatter.write_str("application executor construction failed")
            }
        }
    }
}

impl Error for ApplicationExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyRunning => None,
            Self::Construction(error) => Some(error),
        }
    }
}

/// Terminal result of one application-owned agent turn.
pub enum TurnDriveOutcome {
    /// The model/tool loop committed a complete result.
    Completed(Box<TurnResult>),
    /// The supplied signal prevented admission or cancelled and joined the exact turn.
    Interrupted,
}

impl fmt::Debug for TurnDriveOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Completed(result) => formatter.debug_tuple("Completed").field(&result).finish(),
            Self::Interrupted => formatter.write_str("Interrupted"),
        }
    }
}

/// Failure while presenting and joining one application-owned turn.
#[derive(Debug)]
#[non_exhaustive]
pub enum TurnDriveError {
    /// Prompt admission or terminal agent execution failed.
    Agent(AgentError),
    /// Exact-turn cancellation could not be acknowledged.
    Control(ControlError),
    /// The presentation callback failed.
    Presentation(io::Error),
    /// Ctrl-C registration or delivery failed.
    Signal(io::Error),
    /// Agent shutdown could not join cleanly.
    Shutdown(AgentError),
}

impl fmt::Display for TurnDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Agent(_) => "agent turn failed",
            Self::Control(_) => "turn cancellation failed",
            Self::Presentation(_) => "turn presentation failed",
            Self::Signal(_) => "interrupt handling failed",
            Self::Shutdown(_) => "agent shutdown failed",
        })
    }
}

impl Error for TurnDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Agent(error) | Self::Shutdown(error) => Some(error),
            Self::Control(error) => Some(error),
            Self::Presentation(error) | Self::Signal(error) => Some(error),
        }
    }
}

/// Runs, presents, interrupts, and joins one prompt on an owned agent.
///
/// The callback receives available content-projected causal events in order.
/// Observation attachment, lag, closure, or failure never delays or replaces
/// the authoritative turn result. The cancellation signal is polled before prompt admission. If
/// admission wins, the signal requests exact-turn cancellation and the runner
/// continues to await the result. Agent shutdown is always joined before
/// returning.
///
/// # Errors
///
/// Returns a typed presentation, signal, control, agent, or shutdown failure
/// after requesting bounded cleanup.
#[allow(
    clippy::too_many_lines,
    reason = "admission, exact-turn settlement, cancellation, and shutdown stay visible in one ownership path"
)]
pub async fn run_once_until_signal<F, S>(
    agent: Agent,
    prompt: impl Into<String>,
    signal: S,
    mut present: F,
) -> Result<TurnDriveOutcome, TurnDriveError>
where
    F: FnMut(&AgentEvent) -> io::Result<()>,
    S: Future<Output = io::Result<()>>,
{
    tokio::pin!(signal);
    let prompt = agent.prompt(prompt);
    tokio::pin!(prompt);
    let prompt_result = tokio::select! {
        biased;
        signal_result = &mut signal => {
            if let Err(error) = agent.shutdown().await {
                return Err(TurnDriveError::Shutdown(error));
            }
            return match signal_result {
                Ok(()) => Ok(TurnDriveOutcome::Interrupted),
                Err(error) => Err(TurnDriveError::Signal(error)),
            };
        }
        prompt_result = &mut prompt => prompt_result,
    };
    let mut turn = match prompt_result {
        Ok(turn) => turn,
        Err(error) => {
            return match agent.shutdown().await {
                Ok(()) => Err(TurnDriveError::Agent(error)),
                Err(shutdown) => Err(TurnDriveError::Shutdown(shutdown)),
            };
        }
    };
    let mut events = turn.take_events().ok();
    let control = turn.control();
    let result = turn.result();
    tokio::pin!(result);
    let mut signal_active = true;
    let mut cancellation_requested = false;
    let mut cancellation_attempted = false;
    let loop_result = loop {
        tokio::select! {
            biased;
            result_value = &mut result => break Ok(result_value),
            signal_result = &mut signal, if signal_active => {
                signal_active = false;
                if let Err(error) = signal_result {
                    break Err(TurnDriveError::Signal(error));
                }
                cancellation_attempted = true;
                match control.cancel().await {
                    Ok(_) => {
                        cancellation_requested = true;
                    }
                    Err(error) => break Err(TurnDriveError::Control(error)),
                }
            }
            event_result = async {
                match events.as_mut() {
                    Some(events) => Some(events.recv().await),
                    None => None,
                }
            }, if events.is_some() => {
                let Some(event_result) = event_result else {
                    continue;
                };
                match event_result {
                    Ok(event) => {
                        let observation_ended = matches!(
                            event.kind(),
                            plakous_agent::AgentEventKind::TurnCompleted { .. }
                                | plakous_agent::AgentEventKind::TurnFailed { .. }
                        );
                        if let Err(error) = present(&event) {
                            break Err(TurnDriveError::Presentation(error));
                        }
                        if observation_ended {
                            events = None;
                        }
                    }
                    Err(_) => events = None,
                }
            }
        }
    };
    if loop_result.is_err() && !cancellation_attempted {
        let _ = control.cancel().await;
    }
    agent.shutdown().await.map_err(TurnDriveError::Shutdown)?;
    let result = loop_result?.map_err(TurnDriveError::Agent)?;
    match result.outcome() {
        TurnResultOutcome::Completed(_) => Ok(TurnDriveOutcome::Completed(Box::new(result))),
        TurnResultOutcome::Cancelled(_) if cancellation_requested => {
            Ok(TurnDriveOutcome::Interrupted)
        }
        TurnResultOutcome::Cancelled(error)
        | TurnResultOutcome::Failed(error)
        | TurnResultOutcome::RecoveryRequired(error) => Err(TurnDriveError::Agent(error.clone())),
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::num::NonZeroU32;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    use plakous_agent::{AgentBuilder, AgentConfig, AgentErrorKind, AgentRequestPolicy};
    use plakous_model::{
        Actor, AttemptId, BlockCompleted, BlockStart, BlockStarted, CachePolicy, Capability,
        CapabilityConstraints, CapabilitySet, CompiledProviderConfiguration,
        ConfiguredLanguageModel, ContentBlock, ContextAdmissionRequirement, ConversationItem,
        ConversationView, CredentialSelectionId, EffectiveProviderConfiguration, EntryId,
        FactProvenance, FactSource, FamilyPolicyEnvelope, FeatureSupport, FinishDetails,
        FinishReason, FixedClock, GenerationLimits, LocalDurabilityDisposition,
        ModelAdapterProfileId, ModelError, ModelErrorKind, ModelEvent, ModelFamilyId, ModelId,
        ModelImplementationIdentity, ModelRequest, ModelResponse, OutputBoundDisposition,
        OutputLocation, OutputPolicy, ParallelToolDisposition, ProtocolId, Provenance,
        ProviderAuthorityClassId, ProviderBehaviorConfiguration, ProviderProfileRevisionId,
        ProviderRouteSelection, ProviderSemanticEvidence, ProviderStorageDisposition,
        ReasoningEffortDisposition, ReasoningIncludeDisposition, ReasoningSummaryDisposition,
        ReplayDisposition, RequestMetadata, ResolvedModelDescriptor,
        ResolvedProviderAuthorityBinding, ResponseStarted, ResponseStateDisposition,
        RetentionDisposition, RouteId, RunId, RuntimeModelAuthority, SelectedProviderConfiguration,
        SequenceIdGenerator, Support, TextBlock, TextDelta, TextPhase, Timestamp, ToolChoice,
        TurnId, ValidationMode,
    };
    use plakous_tools::{ToolRegistryBuilder, WorkspacePolicy};

    use super::*;
    use crate::test_model;

    type TestResult<T = ()> = Result<T, Box<dyn Error>>;

    fn id<T>(value: u128) -> TestResult<T>
    where
        T: FromStr,
        T::Err: Error + 'static,
    {
        Ok(format!("{value:032x}").parse()?)
    }

    fn identity_source(start: u128) -> TestResult<Arc<SequenceIdGenerator>> {
        let values = (start..start.saturating_add(64))
            .map(|value| id::<RunId>(value).map(RunId::as_uuid))
            .collect::<TestResult<Vec<_>>>()?;
        Ok(Arc::new(SequenceIdGenerator::new(values)?))
    }

    fn timestamp() -> TestResult<Timestamp> {
        Ok(Timestamp::new(1, 0)?)
    }

    fn descriptor() -> TestResult<ResolvedModelDescriptor> {
        let mut features = BTreeMap::new();
        features.insert(
            Capability::Streaming,
            FeatureSupport::new(
                Support::Native,
                CapabilityConstraints::default(),
                FactProvenance::new(FactSource::TestFixture, timestamp()?, None),
            ),
        );
        Ok(test_model::with_context_contract(
            ResolvedModelDescriptor::new(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                ProtocolId::new("test.protocol")?,
                RouteId::new("test.route")?,
                ModelId::new("test.model")?,
                Some(ModelId::new("test.model")?),
                Some(ModelFamilyId::new("test.family")?),
                ModelAdapterProfileId::new("test.profile")?,
                CapabilitySet::new(features, BTreeMap::new()),
            ),
            timestamp()?,
        )?)
    }

    fn runtime_authority(
        descriptor: &ResolvedModelDescriptor,
    ) -> TestResult<RuntimeModelAuthority> {
        test_model::runtime_authority(descriptor, timestamp()?)
    }

    fn compiled_configuration(
        descriptor: &ResolvedModelDescriptor,
    ) -> TestResult<CompiledProviderConfiguration> {
        let behavior = ProviderBehaviorConfiguration {
            processing_tier: plakous_model::ProcessingTierDisposition::Standard,
            tool_mode: plakous_model::ToolPresentationMode::Direct,
            reasoning_effort: ReasoningEffortDisposition::Disabled,
            reasoning_summary: ReasoningSummaryDisposition::Omit,
            reasoning_include: ReasoningIncludeDisposition::Omit,
            replay: ReplayDisposition::Disabled,
            cache: CachePolicy::Disabled,
            tools: Vec::new(),
            hosted_tools: Vec::new(),
            tool_choice: ToolChoice::Auto,
            parallel_tools: ParallelToolDisposition::Disabled,
            output: OutputPolicy::default(),
            output_bound: OutputBoundDisposition::Enforced(
                NonZeroU32::new(128).ok_or("nonzero application output bound")?,
            ),
            storage: ProviderStorageDisposition::OmittedRouteContract,
            retention: RetentionDisposition::RouteContract {
                profile: ModelAdapterProfileId::new("test.retention")?,
                revision: ProviderProfileRevisionId::new("test-retention-v1")?,
            },
            response_state: ResponseStateDisposition::Stateless,
            context_evidence_requirement: ContextAdmissionRequirement::AnyBounded,
            local_durability: LocalDurabilityDisposition::Ephemeral,
        };
        let selected = SelectedProviderConfiguration::new(
            ProviderRouteSelection::new(
                descriptor.protocol().clone(),
                descriptor.route().clone(),
                descriptor.profile().clone(),
                ProviderProfileRevisionId::new("test-integration-v1")?,
                ModelAdapterProfileId::new("test.auth")?,
                ProviderProfileRevisionId::new("test-auth-v1")?,
                ProviderAuthorityClassId::new("test-authority")?,
                CredentialSelectionId::new("test-credential-selection")?,
                descriptor.requested_model().clone(),
            ),
            behavior.clone(),
        )?;
        let authority = runtime_authority(descriptor)?;
        let stable_authority = ResolvedProviderAuthorityBinding::new(
            descriptor,
            &authority,
            ProviderSemanticEvidence::new(1, b"application-test-semantic-v1".to_vec())?,
        )?;
        let family_policy = FamilyPolicyEnvelope::new(
            descriptor
                .model_family()
                .cloned()
                .expect("test model family"),
            1,
            1,
            1,
            b"application-test-family-policy-v1".to_vec(),
        )?;
        let effective = EffectiveProviderConfiguration::compile(
            &selected,
            descriptor.clone(),
            ModelImplementationIdentity::release_owned(
                &plakous_model::StandardIntegrationAuthority::release_owned(),
                descriptor,
                1,
            )?,
            stable_authority,
            behavior,
            family_policy,
        )?;
        Ok(CompiledProviderConfiguration::new(selected, effective)?)
    }

    fn expected_request(
        descriptor: &ResolvedModelDescriptor,
        prompt: &str,
    ) -> TestResult<ModelRequest> {
        let configuration = compiled_configuration(descriptor)?;
        let user = ConversationItem::new(
            id::<EntryId>(6)?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new(prompt, None)?)],
            Provenance::UserInput,
        )?;
        Ok(ModelRequest::builder(
            configuration.selected_identity(),
            configuration.effective_identity(),
            ConversationView::new(vec![user]),
            RequestMetadata::for_turn(
                id::<RunId>(4)?,
                id::<TurnId>(5)?,
                id::<AttemptId>(7)?,
                timestamp()?,
            ),
        )
        .limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?)
        .build()?)
    }

    fn completion_scenario(
        descriptor: &ResolvedModelDescriptor,
        prompt: &str,
    ) -> TestResult<plakous_test_support::ModelScenario> {
        completion_scenario_with_barrier(descriptor, prompt, None)
    }

    fn completion_scenario_with_barrier(
        descriptor: &ResolvedModelDescriptor,
        prompt: &str,
        barrier: Option<plakous_test_support::ScenarioBarrier>,
    ) -> TestResult<plakous_test_support::ModelScenario> {
        let request = expected_request(descriptor, prompt)?;
        let plan = descriptor.validate_request(request.clone(), ValidationMode::Strict)?;
        let digest = plan.digest();
        let item = ConversationItem::new(
            id::<EntryId>(100)?,
            Actor::Assistant,
            vec![ContentBlock::Text(TextBlock::new(
                "café",
                Some(TextPhase::Final),
            )?)],
            Provenance::ModelOutput,
        )?;
        let location = OutputLocation::new(0, 0);
        let response = ModelResponse::new(
            vec![item.clone()],
            FinishDetails::new(FinishReason::Stop, None),
            None,
            None,
            digest,
        );
        let mut steps = vec![
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseStarted(
                ResponseStarted::new(None, digest),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockStarted(BlockStarted::new(
                item.id(),
                location,
                BlockStart::Text {
                    phase: Some(TextPhase::Final),
                },
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                location,
                vec![b'c', b'a', b'f', 0xc3],
            ))),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::TextDelta(TextDelta::new(
                location,
                vec![0xa9],
            ))),
        ];
        if let Some(barrier) = barrier {
            steps.push(plakous_test_support::ScenarioStep::Barrier(barrier));
        }
        steps.extend([
            plakous_test_support::ScenarioStep::Emit(ModelEvent::BlockCompleted(
                BlockCompleted::new(location),
            )),
            plakous_test_support::ScenarioStep::Emit(ModelEvent::ResponseCompleted(response)),
        ]);
        Ok(plakous_test_support::ModelScenario::new(
            plakous_test_support::ExpectedRequest::ExactBeforeRuntimeHarnessPlan(Box::new(request)),
            plakous_test_support::ScenarioStart::Immediate,
            steps,
        ))
    }

    fn agent(model: Arc<plakous_test_support::ScriptedModel>) -> TestResult<Agent> {
        agent_with_config(model, AgentConfig::default())
    }

    fn agent_with_config(
        model: Arc<plakous_test_support::ScriptedModel>,
        config: AgentConfig,
    ) -> TestResult<Agent> {
        let clock = Arc::new(FixedClock::new(timestamp()?));
        let identities = identity_source(1)?;
        let event_identities = identity_source(1_000)?;
        let workspace = Arc::new(WorkspacePolicy::host(".", ".")?);
        let descriptor = model.descriptor().clone();
        let configuration = compiled_configuration(&descriptor)?;
        let authority = runtime_authority(&descriptor)?;
        let configured_model = ConfiguredLanguageModel::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            model,
            configuration,
            authority,
        )?;
        Ok(AgentBuilder::new(
            configured_model,
            ToolRegistryBuilder::default().freeze(),
            workspace,
        )
        .config(config)
        .request_policy(
            AgentRequestPolicy::default()
                .with_generation_limits(GenerationLimits::new(NonZeroU32::new(128), Vec::new())?),
        )
        .identity_sources(clock, identities)
        .event_identity_source(event_identities)
        .build()?)
    }

    fn assert_terminal_agent_error(
        model_kind: ModelErrorKind,
        expected_agent_kind: AgentErrorKind,
    ) -> TestResult {
        let model = Arc::new(test_model::scripted_model(
            descriptor()?,
            [plakous_test_support::ModelScenario::new(
                plakous_test_support::ExpectedRequest::Any,
                plakous_test_support::ScenarioStart::Immediate,
                vec![plakous_test_support::ScenarioStep::Error(ModelError::new(
                    model_kind,
                    "injected terminal model error",
                ))],
            )],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                run_once_until_signal(
                    agent,
                    "terminal error",
                    std::future::pending::<io::Result<()>>(),
                    |_| Ok(()),
                ),
            )
            .await?;

            let Err(TurnDriveError::Agent(error)) = outcome else {
                return Err(format!("terminal outcome was not an agent error: {outcome:?}").into());
            };
            assert_eq!(error.kind(), expected_agent_kind);
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn executor_drives_a_future_and_rejects_nested_construction() -> TestResult {
        let executor = ApplicationExecutor::new()?;
        let result = executor.block_on(async {
            assert!(matches!(
                ApplicationExecutor::new(),
                Err(ApplicationExecutorError::AlreadyRunning)
            ));
            42_u8
        })?;
        assert_eq!(result, 42);
        Ok(())
    }

    #[test]
    fn executor_debug_is_content_free() -> TestResult {
        let executor = ApplicationExecutor::new()?;
        assert_eq!(format!("{executor:?}"), "ApplicationExecutor { .. }");
        Ok(())
    }

    #[test]
    fn executor_drop_does_not_wait_out_spawn_blocking() -> TestResult {
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_secs(30)));
            tokio::time::sleep(Duration::from_millis(20)).await;
        })?;
        let started = std::time::Instant::now();
        drop(executor);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "Runtime drop must not join leftover spawn_blocking: {:?}",
            started.elapsed()
        );
        Ok(())
    }

    #[test]
    fn result_before_terminal_event_returns_without_drain_or_hang() -> TestResult {
        const PROMPT: &str = "hello";
        let descriptor = descriptor()?;
        let model = Arc::new(test_model::scripted_model(
            descriptor.clone(),
            [completion_scenario(&descriptor, PROMPT)?],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let mut presented = Vec::new();
            let mut delta_count = 0_usize;
            let mut aggregate_count = 0_usize;
            let mut terminal_count = 0_usize;
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                run_once_until_signal(
                    agent,
                    PROMPT,
                    std::future::pending::<io::Result<()>>(),
                    |event| {
                        if matches!(
                            event.kind(),
                            plakous_agent::AgentEventKind::TurnCompleted { .. }
                                | plakous_agent::AgentEventKind::TurnFailed { .. }
                        ) {
                            terminal_count = terminal_count.saturating_add(1);
                        }
                        if let Some(model_event) = event.kind().model_event() {
                            match model_event {
                                ModelEvent::TextDelta(delta) => {
                                    delta_count = delta_count.saturating_add(1);
                                    presented.extend_from_slice(delta.bytes());
                                }
                                ModelEvent::ResponseCompleted(_) => {
                                    aggregate_count = aggregate_count.saturating_add(1);
                                }
                                _ => {}
                            }
                        }
                        Ok(())
                    },
                ),
            )
            .await??;

            let TurnDriveOutcome::Completed(result) = outcome else {
                return Err("completed script was interrupted".into());
            };
            assert_eq!(presented, "café".as_bytes());
            assert_eq!(delta_count, 2);
            assert_eq!(aggregate_count, 0);
            assert_eq!(terminal_count, 0, "result waited for a terminal event");
            assert_eq!(
                result.response().expect("completed response").output_text(),
                "café"
            );
            assert_eq!(model.captured_plans().len(), 1);
            assert_eq!(model.remaining_scenarios(), 0);
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn event_before_result_is_presented_without_becoming_terminal_authority() -> TestResult {
        const PROMPT: &str = "event first";
        let descriptor = descriptor()?;
        let barrier = plakous_test_support::ScenarioBarrier::new();
        let model = Arc::new(test_model::scripted_model(
            descriptor.clone(),
            [completion_scenario_with_barrier(
                &descriptor,
                PROMPT,
                Some(barrier.clone()),
            )?],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let mut event_preceded_result = false;
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                run_once_until_signal(
                    agent,
                    PROMPT,
                    std::future::pending::<io::Result<()>>(),
                    |event| {
                        if matches!(event.kind().model_event(), Some(ModelEvent::TextDelta(_))) {
                            event_preceded_result = true;
                            barrier.release();
                        }
                        Ok(())
                    },
                ),
            )
            .await??;

            let TurnDriveOutcome::Completed(result) = outcome else {
                return Err("event-first script was interrupted".into());
            };
            assert!(
                event_preceded_result,
                "result bypassed the blocked model event"
            );
            assert_eq!(
                result.response().expect("completed response").output_text(),
                "café"
            );
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn observation_error_before_result_is_non_authoritative_and_does_not_hang() -> TestResult {
        const PROMPT: &str = "observation loss";
        let descriptor = descriptor()?;
        let barrier = plakous_test_support::ScenarioBarrier::new();
        let model = Arc::new(test_model::scripted_model(
            descriptor.clone(),
            [completion_scenario_with_barrier(
                &descriptor,
                PROMPT,
                Some(barrier.clone()),
            )?],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent_with_config(
                Arc::clone(&model),
                AgentConfig::default().with_event_byte_capacity(512),
            )?;
            let weak = agent.downgrade();
            let release = tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                barrier.release();
            });
            let mut presented_model_events = 0_usize;
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                run_once_until_signal(
                    agent,
                    PROMPT,
                    std::future::pending::<io::Result<()>>(),
                    |event| {
                        presented_model_events = presented_model_events
                            .saturating_add(usize::from(event.kind().model_event().is_some()));
                        Ok(())
                    },
                ),
            )
            .await??;
            release.await?;

            let TurnDriveOutcome::Completed(result) = outcome else {
                return Err("observation-loss script was interrupted".into());
            };
            assert_eq!(
                presented_model_events, 0,
                "oversized observation unexpectedly crossed the one-envelope bound"
            );
            assert_eq!(
                result.response().expect("completed response").output_text(),
                "café"
            );
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn ready_result_ignores_late_observation_error_without_hanging() -> TestResult {
        const PROMPT: &str = "ready result";
        let descriptor = descriptor()?;
        let model = Arc::new(test_model::scripted_model(
            descriptor.clone(),
            [completion_scenario(&descriptor, PROMPT)?],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent_with_config(
                Arc::clone(&model),
                AgentConfig::default().with_event_byte_capacity(512),
            )?;
            let weak = agent.downgrade();
            let outcome = tokio::time::timeout(
                Duration::from_secs(1),
                run_once_until_signal(
                    agent,
                    PROMPT,
                    std::future::pending::<io::Result<()>>(),
                    |_| Ok(()),
                ),
            )
            .await??;

            let TurnDriveOutcome::Completed(result) = outcome else {
                return Err("ready-result script was interrupted".into());
            };
            assert_eq!(
                result.response().expect("completed response").output_text(),
                "café"
            );
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn failed_terminal_result_is_returned_as_agent_error() -> TestResult {
        assert_terminal_agent_error(ModelErrorKind::Unavailable, AgentErrorKind::Model)
    }

    #[test]
    fn recovery_required_terminal_result_is_returned_as_agent_error() -> TestResult {
        assert_terminal_agent_error(
            ModelErrorKind::OutcomeUnknown,
            AgentErrorKind::OutcomeUnknown,
        )
    }

    #[test]
    fn unsolicited_cancelled_terminal_result_is_not_reported_as_interrupt() -> TestResult {
        assert_terminal_agent_error(ModelErrorKind::Cancelled, AgentErrorKind::Cancelled)
    }

    #[test]
    fn ready_interrupt_prevents_admission_and_joins_all_owned_work() -> TestResult {
        let model = Arc::new(test_model::scripted_model(
            descriptor()?,
            [plakous_test_support::ModelScenario::new(
                plakous_test_support::ExpectedRequest::Any,
                plakous_test_support::ScenarioStart::AwaitCancellation,
                Vec::new(),
            )],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let mut presented = 0_usize;
            let outcome =
                run_once_until_signal(agent, "interrupt me", std::future::ready(Ok(())), |_| {
                    presented = presented.saturating_add(1);
                    Ok(())
                })
                .await?;

            assert!(matches!(outcome, TurnDriveOutcome::Interrupted));
            assert_eq!(presented, 0);
            assert!(model.captured_plans().is_empty());
            assert_eq!(model.remaining_scenarios(), 1);
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn post_admission_interrupt_cancels_model_and_joins_all_owned_work() -> TestResult {
        let model = Arc::new(test_model::scripted_model(
            descriptor()?,
            [plakous_test_support::ModelScenario::new(
                plakous_test_support::ExpectedRequest::Any,
                plakous_test_support::ScenarioStart::AwaitCancellation,
                Vec::new(),
            )],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let signal_model = Arc::clone(&model);
            let signal = async move {
                while signal_model.captured_plans().is_empty() {
                    tokio::task::yield_now().await;
                }
                Ok(())
            };
            let outcome = run_once_until_signal(agent, "interrupt me", signal, |_| Ok(())).await?;

            assert!(matches!(outcome, TurnDriveOutcome::Interrupted));
            assert_eq!(model.captured_plans().len(), 1, "model was not reached");
            assert_eq!(model.remaining_scenarios(), 0);
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    #[test]
    fn signal_error_joins_before_returning_without_admission() -> TestResult {
        let model = Arc::new(test_model::scripted_model(
            descriptor()?,
            [plakous_test_support::ModelScenario::new(
                plakous_test_support::ExpectedRequest::Any,
                plakous_test_support::ScenarioStart::AwaitCancellation,
                Vec::new(),
            )],
            timestamp()?,
        )?);
        let executor = ApplicationExecutor::new()?;
        executor.block_on(async {
            let agent = agent(Arc::clone(&model))?;
            let weak = agent.downgrade();
            let result = run_once_until_signal(
                agent,
                "signal failure",
                std::future::ready(Err(io::Error::other("injected signal failure"))),
                |_| Ok(()),
            )
            .await;

            assert!(matches!(result, Err(TurnDriveError::Signal(_))));
            assert!(model.captured_plans().is_empty());
            assert_eq!(model.remaining_scenarios(), 1);
            assert!(weak.upgrade().is_none(), "shutdown left the agent owned");
            Ok::<(), Box<dyn Error>>(())
        })??;
        Ok(())
    }

    /// The unoptimized turn loop needs more than the 2 MiB `std::thread`
    /// default on every thread that may poll it (a debug host
    /// aborted with `tokio-rt-worker has overflowed its stack` while a child
    /// ran `sleep 40`). Workers and blocking threads both get
    /// `RUNTIME_EXECUTOR_STACK_BYTES`; a frame that would overflow the default
    /// proves the reservation is real rather than reading the builder back.
    #[test]
    fn executor_threads_carry_the_runtime_stack_reservation() -> TestResult {
        // 8 KiB per frame, 384 frames deep: 3 MiB, past the 2 MiB default.
        const FRAME_BYTES: usize = 8 << 10;
        const FRAMES_PAST_THE_STD_DEFAULT: u32 = 384;
        #[inline(never)]
        fn touch_deep_frame(depth: u32) -> u8 {
            let mut frame = [0_u8; FRAME_BYTES];
            frame[FRAME_BYTES - 1] = 1;
            let here = std::hint::black_box(&frame)[FRAME_BYTES - 1];
            if depth == 0 {
                here
            } else {
                touch_deep_frame(depth - 1) & here
            }
        }
        let executor = ApplicationExecutor::new()?;
        let (worker, blocking) = executor.block_on(async {
            let worker =
                tokio::spawn(async { touch_deep_frame(FRAMES_PAST_THE_STD_DEFAULT) }).await?;
            let blocking =
                tokio::task::spawn_blocking(|| touch_deep_frame(FRAMES_PAST_THE_STD_DEFAULT))
                    .await?;
            Ok::<_, tokio::task::JoinError>((worker, blocking))
        })??;
        assert_eq!((worker, blocking), (1, 1));
        Ok(())
    }
}
