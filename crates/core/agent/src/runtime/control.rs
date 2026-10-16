//! Canonical command identities and durable acknowledgement codecs.

use crate::runtime::config::AgentConfig;
use crate::runtime::{
    ArtifactTransition, DurableActorCommandEffect, DurableControlRejection, OperationId,
    PreparedPromptCommand, PromptCommand,
};
use crate::tool_runtime::EffectDispatchIntent;
use crate::{
    ActorCommandEffect, AgentError, AgentErrorKind, CancellationStatus, ControlError,
    ControlErrorKind, EffectDecisionChoice, EffectDecisionStatus, SessionStoreError,
    SteeringStatus, SubscriptionLimits, TurnCommand, UsageAggregationError, ViewError,
};
use plakous_model::{Digest, DigestDomain, ModelResponse, RunId, TextBlock, ValidatedRequestPlan};
use plakous_tools::{
    EffectAuthorizationGrantSource, ExecutionEnvironment, ExecutionEnvironmentConfig,
    ExecutionEnvironmentScope, WorkspacePolicy,
};
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "durable recovery witness explicitly binds each independent policy input"
)]
pub(super) fn run_environment(
    workspace: &WorkspacePolicy,
    run_id: RunId,
    host_process_options: plakous_tools::HostProcessOptions,
    job_config: plakous_tools::JobConfig,
) -> Result<ExecutionEnvironment, AgentError> {
    ExecutionEnvironment::host_with_policy_and_process_options(
        ExecutionEnvironmentScope::Run(run_id),
        workspace.clone(),
        ExecutionEnvironmentConfig::new(
            job_config
                .limits(plakous_tools::ExecutionEnvironmentLimits::default())
                .map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::InvalidInput,
                        "job construction bounds are invalid",
                    )
                })?,
            [],
            [],
        )
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "job environment could not be initialized",
            )
        })?,
        host_process_options,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "execution environment could not be initialized",
        )
    })
}

pub(super) fn subscription_limits(config: AgentConfig) -> Result<SubscriptionLimits, AgentError> {
    SubscriptionLimits::new(config.event_capacity, config.event_byte_capacity)
        .map_err(|error| AgentError::new(AgentErrorKind::InvalidInput, error.to_string()))
}

pub(super) fn event_error(error: ViewError) -> AgentError {
    let kind = match error {
        ViewError::SubscriberLimit => AgentErrorKind::Backpressure,
        ViewError::Identity => AgentErrorKind::Identity,
        ViewError::Closed => AgentErrorKind::Shutdown,
        ViewError::FenceSaturated | ViewError::FenceStateUnavailable => AgentErrorKind::Session,
        ViewError::IncompatibleCursor | ViewError::CursorAhead | ViewError::InvalidEvent => {
            AgentErrorKind::Internal
        }
    };
    AgentError::new(kind, error.to_string())
}

pub(super) fn usage_error(error: UsageAggregationError) -> AgentError {
    AgentError::new(AgentErrorKind::Internal, error.to_string())
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "owned ControlError is consumed directly by Result::map_err call sites"
)]
pub(super) fn control_agent_error(error: ControlError) -> AgentError {
    if let Some(source) = error.agent_source() {
        return source.clone();
    }
    if let Some(capacity) = error.capacity() {
        return AgentError::from_store(SessionStoreError::Capacity(capacity));
    }
    let kind = match error.kind() {
        ControlErrorKind::Backpressure | ControlErrorKind::QueueFull => {
            AgentErrorKind::Backpressure
        }
        ControlErrorKind::Shutdown | ControlErrorKind::Cancelled => AgentErrorKind::Shutdown,
        ControlErrorKind::InvalidInput | ControlErrorKind::IdentityConflict => {
            AgentErrorKind::InvalidInput
        }
        ControlErrorKind::ForeignTurn
        | ControlErrorKind::Stale
        | ControlErrorKind::WrongEffectDecision
        | ControlErrorKind::Expired
        | ControlErrorKind::WrongPhase => AgentErrorKind::InvalidInput,
        ControlErrorKind::Internal => AgentErrorKind::Internal,
        ControlErrorKind::OutcomeUnknown => AgentErrorKind::OutcomeUnknown,
        ControlErrorKind::StoragePressure
        | ControlErrorKind::LedgerFull
        | ControlErrorKind::Reclaimed
        | ControlErrorKind::AdmissionRejected => AgentErrorKind::Session,
    };
    AgentError::new(kind, error.message())
}

pub(super) fn validate_command_text(text: &str, label: &str) -> Result<(), ControlError> {
    TextBlock::new(text.to_owned(), None)
        .map(|_| ())
        .map_err(|_| {
            ControlError::new(
                ControlErrorKind::InvalidInput,
                format!("{label} text must be valid canonical text"),
            )
        })
}

pub(super) fn command_binding(command: &TurnCommand) -> Result<Digest, ControlError> {
    let canonical = command_canonical_bytes(command);
    Digest::sha256(DigestDomain::Blob, 1, &[&canonical]).map_err(|_| {
        ControlError::new(
            ControlErrorKind::Internal,
            "command binding could not be constructed",
        )
    })
}

pub(super) fn command_canonical_bytes(command: &TurnCommand) -> Vec<u8> {
    fn frame(bytes: &mut Vec<u8>, value: &[u8]) {
        bytes.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
        bytes.extend_from_slice(value);
    }

    let mut canonical = Vec::with_capacity(256);
    match command {
        TurnCommand::StartInstruction { text, .. } => {
            frame(&mut canonical, b"start-instruction");
            frame(&mut canonical, text.as_bytes());
        }
        TurnCommand::Start { input, .. } => {
            frame(&mut canonical, b"start");
            input.frame_into(&mut |value| frame(&mut canonical, value));
        }
        TurnCommand::FollowUp {
            target_turn_id,
            input,
            ..
        } => {
            frame(&mut canonical, b"follow-up");
            frame(&mut canonical, target_turn_id.as_uuid().as_bytes());
            input.frame_into(&mut |value| frame(&mut canonical, value));
        }
        TurnCommand::Steer { turn_id, input, .. } => {
            frame(&mut canonical, b"steer");
            frame(&mut canonical, turn_id.as_uuid().as_bytes());
            input.frame_into(&mut |value| frame(&mut canonical, value));
        }
        TurnCommand::ResolveEffectDecision {
            reference,
            decision,
            ..
        } => {
            frame(&mut canonical, b"resolve-decision");
            frame(&mut canonical, reference.event_id().as_uuid().as_bytes());
            frame(&mut canonical, reference.turn_id().as_uuid().as_bytes());
            frame(&mut canonical, reference.attempt_id().as_uuid().as_bytes());
            frame(&mut canonical, reference.call_id().as_uuid().as_bytes());
            frame(&mut canonical, reference.binding_digest().bytes());
            match decision {
                EffectDecisionChoice::Allow(decision_audit_id) => {
                    frame(&mut canonical, b"allow");
                    frame(&mut canonical, decision_audit_id.as_str().as_bytes());
                }
                EffectDecisionChoice::Deny(denial) => {
                    frame(&mut canonical, b"deny");
                    frame(&mut canonical, denial.reason().as_str().as_bytes());
                }
            }
        }
        TurnCommand::Cancel { turn_id, .. } => {
            frame(&mut canonical, b"cancel");
            frame(&mut canonical, turn_id.as_uuid().as_bytes());
        }
        TurnCommand::Shutdown { .. } => frame(&mut canonical, b"shutdown"),
    }
    canonical
}

pub(super) fn durable_command_effect(
    effect: ActorCommandEffect,
) -> Result<DurableActorCommandEffect, AgentError> {
    match effect {
        ActorCommandEffect::Root => Ok(DurableActorCommandEffect::Root),
        ActorCommandEffect::Cancel(CancellationStatus::Requested) => {
            Ok(DurableActorCommandEffect::CancelRequested)
        }
        ActorCommandEffect::Cancel(CancellationStatus::AlreadyRequested) => {
            Ok(DurableActorCommandEffect::CancelAlreadyRequested)
        }
        ActorCommandEffect::Cancel(CancellationStatus::AlreadyTerminal) => {
            Ok(DurableActorCommandEffect::CancelAlreadyTerminal)
        }
        ActorCommandEffect::EffectDecision(EffectDecisionStatus::Allowed) => {
            Ok(DurableActorCommandEffect::EffectDecisionAllowed)
        }
        ActorCommandEffect::EffectDecision(EffectDecisionStatus::Denied) => {
            Ok(DurableActorCommandEffect::EffectDecisionDenied)
        }
        ActorCommandEffect::Steer(SteeringStatus::Queued { position }) => {
            Ok(DurableActorCommandEffect::SteerQueued {
                position: u32::try_from(position).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Internal,
                        "steering receipt position exceeded its durable bound",
                    )
                })?,
            })
        }
        ActorCommandEffect::Shutdown => Ok(DurableActorCommandEffect::Shutdown),
        ActorCommandEffect::Rejected => Ok(DurableActorCommandEffect::Rejected),
    }
}

pub(super) fn actor_command_effect(
    effect: &DurableActorCommandEffect,
) -> Result<ActorCommandEffect, AgentError> {
    Ok(match effect {
        DurableActorCommandEffect::Root => ActorCommandEffect::Root,
        DurableActorCommandEffect::CancelRequested => {
            ActorCommandEffect::Cancel(CancellationStatus::Requested)
        }
        DurableActorCommandEffect::CancelAlreadyRequested => {
            ActorCommandEffect::Cancel(CancellationStatus::AlreadyRequested)
        }
        DurableActorCommandEffect::CancelAlreadyTerminal => {
            ActorCommandEffect::Cancel(CancellationStatus::AlreadyTerminal)
        }
        DurableActorCommandEffect::EffectDecisionAllowed => {
            ActorCommandEffect::EffectDecision(EffectDecisionStatus::Allowed)
        }
        DurableActorCommandEffect::EffectDecisionDenied => {
            ActorCommandEffect::EffectDecision(EffectDecisionStatus::Denied)
        }
        DurableActorCommandEffect::SteerQueued { position } => {
            ActorCommandEffect::Steer(SteeringStatus::Queued {
                position: usize::try_from(*position).map_err(|_| {
                    AgentError::new(
                        AgentErrorKind::Session,
                        "durable steering receipt position is unavailable",
                    )
                })?,
            })
        }
        DurableActorCommandEffect::Shutdown => ActorCommandEffect::Shutdown,
        DurableActorCommandEffect::Rejected => ActorCommandEffect::Rejected,
    })
}

pub(super) fn durable_rejection(error: &ControlError) -> DurableControlRejection {
    DurableControlRejection {
        kind: durable_control_error_kind(error.kind()).to_owned(),
        message: error.message().to_owned(),
    }
}

pub(super) const fn durable_control_error_kind(kind: ControlErrorKind) -> &'static str {
    match kind {
        ControlErrorKind::Backpressure => "Backpressure",
        ControlErrorKind::LedgerFull => "LedgerFull",
        ControlErrorKind::Shutdown => "Shutdown",
        ControlErrorKind::ForeignTurn => "ForeignTurn",
        ControlErrorKind::Stale => "Stale",
        ControlErrorKind::WrongEffectDecision => "WrongEffectDecision",
        ControlErrorKind::Expired => "Expired",
        ControlErrorKind::Cancelled => "Cancelled",
        ControlErrorKind::WrongPhase => "WrongPhase",
        ControlErrorKind::QueueFull => "QueueFull",
        ControlErrorKind::InvalidInput => "InvalidInput",
        ControlErrorKind::IdentityConflict => "IdentityConflict",
        ControlErrorKind::Internal => "Internal",
        ControlErrorKind::StoragePressure => "StoragePressure",
        ControlErrorKind::AdmissionRejected => "AdmissionRejected",
        ControlErrorKind::OutcomeUnknown => "OutcomeUnknown",
        ControlErrorKind::Reclaimed => "Reclaimed",
    }
}

pub(super) fn control_error_from_durable(
    rejection: &DurableControlRejection,
) -> Result<ControlError, AgentError> {
    let kind = match rejection.kind.as_str() {
        "Backpressure" => ControlErrorKind::Backpressure,
        "Shutdown" => ControlErrorKind::Shutdown,
        "ForeignTurn" => ControlErrorKind::ForeignTurn,
        "Stale" => ControlErrorKind::Stale,
        "WrongEffectDecision" => ControlErrorKind::WrongEffectDecision,
        "Expired" => ControlErrorKind::Expired,
        "Cancelled" => ControlErrorKind::Cancelled,
        "WrongPhase" => ControlErrorKind::WrongPhase,
        "QueueFull" => ControlErrorKind::QueueFull,
        "InvalidInput" => ControlErrorKind::InvalidInput,
        "IdentityConflict" => ControlErrorKind::IdentityConflict,
        "Internal" => ControlErrorKind::Internal,
        "StoragePressure" => ControlErrorKind::StoragePressure,
        "Reclaimed" => ControlErrorKind::Reclaimed,
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::Session,
                "durable command rejection kind is unsupported",
            ));
        }
    };
    Ok(ControlError::new(kind, &rejection.message))
}

pub(super) fn effect_intent_bytes(intent: &EffectDispatchIntent) -> Result<Vec<u8>, AgentError> {
    fn frame(bytes: &mut Vec<u8>, value: &[u8]) -> Result<(), AgentError> {
        let len = u32::try_from(value.len()).map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect intent field exceeded its durable bound",
            )
        })?;
        bytes.extend_from_slice(&len.to_be_bytes());
        bytes.extend_from_slice(value);
        Ok(())
    }

    let mut bytes = Vec::with_capacity(1_024);
    frame(&mut bytes, b"plakous-agent/effect-intent-v1")?;
    frame(&mut bytes, intent.digest().bytes())?;
    frame(&mut bytes, intent.run_id().as_uuid().as_bytes())?;
    frame(&mut bytes, intent.turn_id().as_uuid().as_bytes())?;
    frame(&mut bytes, intent.attempt_id().as_uuid().as_bytes())?;
    frame(&mut bytes, intent.call_id().as_uuid().as_bytes())?;
    let identity = intent.identity();
    frame(&mut bytes, identity.tool_id().as_str().as_bytes())?;
    frame(&mut bytes, identity.name().as_str().as_bytes())?;
    frame(&mut bytes, identity.semantic_version().as_bytes())?;
    frame(&mut bytes, identity.definition_digest().bytes())?;
    frame(&mut bytes, identity.schema_digest().bytes())?;
    frame(&mut bytes, identity.argument_limits_digest().bytes())?;
    frame(&mut bytes, identity.executor_id().as_str().as_bytes())?;
    frame(&mut bytes, identity.effect_metadata_digest().bytes())?;
    if let Some(exposure) = intent.exposure() {
        frame(&mut bytes, b"exposed")?;
        frame(&mut bytes, exposure.exposed_name().as_str().as_bytes())?;
        frame(&mut bytes, exposure.map_digest().bytes())?;
    } else {
        frame(&mut bytes, b"canonical")?;
    }
    frame(&mut bytes, intent.argument_digest().bytes())?;
    frame(&mut bytes, intent.environment_digest().bytes())?;
    frame(&mut bytes, intent.policy_revision().policy_id().as_bytes())?;
    frame(
        &mut bytes,
        &intent.policy_revision().revision().to_be_bytes(),
    )?;
    match intent.grant_source() {
        EffectAuthorizationGrantSource::Policy => frame(&mut bytes, b"policy")?,
        EffectAuthorizationGrantSource::Operator(decision) => {
            frame(&mut bytes, b"operator")?;
            frame(&mut bytes, decision.as_str().as_bytes())?;
        }
        _ => {
            return Err(AgentError::new(
                AgentErrorKind::InvalidInput,
                "effect intent uses an unsupported grant source",
            ));
        }
    }
    frame(&mut bytes, intent.authorization_digest().bytes())?;
    frame(&mut bytes, b"manual-non-idempotent")?;
    frame(&mut bytes, b"no-key")?;
    Ok(bytes)
}

impl PreparedPromptCommand {
    pub(super) fn admit(
        self,
        operation_id: OperationId,
        binding: Digest,
        max_model_rounds: usize,
    ) -> PromptCommand {
        PromptCommand {
            max_model_rounds,
            instruction: self.instruction,
            command_id: self.command_id,
            turn_id: self.turn_id,
            input: self.input,
            follow_up: self.follow_up,
            operation_id,
            binding,
            admitted_at: self.admitted_at,
            cancellation: self.cancellation,
            shared: self.shared,
        }
    }
}

pub(super) fn provider_artifact_transition(
    plan: &ValidatedRequestPlan,
    response: &ModelResponse,
) -> ArtifactTransition {
    response
        .family_artifact_candidate()
        .map_or(ArtifactTransition::Clear, |candidate| {
            ArtifactTransition::Replace {
                candidate: candidate.clone(),
                plan: Box::new(plan.clone()),
                response_output_items: response.output().len(),
            }
        })
}
