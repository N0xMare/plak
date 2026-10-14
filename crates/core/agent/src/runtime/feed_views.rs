//! Typed views of runtime-private record encodings for the change feed's
//! fact function (`crate::feed::facts`). Nothing here decides a fact; it only
//! reads what a record already says.

use std::sync::Arc;

use plakous_model::{
    CallId, CheckpointId, Digest, FinishReason, ToolCallBlock, ToolCallerLineage, ToolName, TurnId,
};

use super::compaction::{
    DurableCompactionIdentity, DurableCompactionPublicationEvidence,
    DurableCompactionRequestRecord, DurablePlaceholderEvidence, DurablePlaceholderRequestRecord,
};
use super::composition::record::decode_local_composition_evaluation_request;
use super::control::control_error_from_durable;
use super::durable::DurableSessionAddress;
use super::durable::effect_recovery::decode_effect_call;
use super::durable::turn_recovery::controls::steering_not_applied;
use super::observation::{arguments_presentation_subject, history_tool_call_observation};
use super::terminal::{DurableTerminalOutcome, read_terminal_result_record, terminal_turn_result};
use super::{DurableActorCommandEffect, DurableCommandEnvelope};
use crate::feed::{
    FeedCallerLineage, FeedCancelCause, FeedCommandKind, FeedCompactPass, FeedContextLimit,
    FeedTurnOutcome,
};
use crate::internal::JobRow;
use crate::{
    AgentError, AgentErrorKind, CommandId, ContextOverflowRemedy, ControlErrorKind,
    DurableCommandDisposition, DurableCommandRecord, DurableOperationRecord, DurableOperationState,
    RuntimeCheckpoint, TurnUsage,
};

/// What one command record says about itself.
pub(crate) struct CommandView {
    pub(crate) kind: FeedCommandKind,
    pub(crate) rejection: Option<ControlErrorKind>,
}

pub(crate) fn command_view(record: &DurableCommandRecord) -> Result<CommandView, AgentError> {
    let envelope = command_envelope(record)?;
    let kind = command_kind(&envelope.canonical_input)?;
    let rejection = envelope
        .rejection
        .as_ref()
        .map(|rejection| control_error_from_durable(rejection).map(|error| error.kind()))
        .transpose()?;
    Ok(CommandView { kind, rejection })
}

/// The first frame of a canonical command names its class.
fn command_kind(canonical: &[u8]) -> Result<FeedCommandKind, AgentError> {
    let tag = canonical
        .get(..8)
        .and_then(|length| <[u8; 8]>::try_from(length).ok())
        .map(u64::from_be_bytes)
        .and_then(|length| usize::try_from(length).ok())
        .and_then(|length| canonical.get(8..8_usize.checked_add(length)?))
        .ok_or_else(|| integrity("durable command canonical input is malformed"))?;
    Ok(match tag {
        b"start-instruction" => FeedCommandKind::Root {
            origin: plakous_model::TurnOrigin::HostWake,
        },
        b"start" | b"follow-up" => FeedCommandKind::Root {
            origin: plakous_model::TurnOrigin::UserPrompt,
        },
        b"steer" => FeedCommandKind::Steer,
        b"resolve-decision" => FeedCommandKind::EffectDecision,
        b"cancel" => FeedCommandKind::Cancel,
        b"shutdown" => FeedCommandKind::Shutdown,
        _ => return Err(integrity("durable command class is unsupported")),
    })
}

fn command_envelope(record: &DurableCommandRecord) -> Result<DurableCommandEnvelope, AgentError> {
    serde_json::from_slice(record.command_component().bytes())
        .map_err(|_| integrity("durable command receipt bytes could not be reconstructed"))
}

/// What an effect's committed call block says about the call.
pub(crate) struct EffectView {
    pub(crate) exposed_name: ToolName,
    pub(crate) lineage: FeedCallerLineage,
    pub(crate) subject: Option<String>,
}

/// The recursion depth every local Code Cell nested call carries.
pub(crate) const LOCAL_COMPOSITION_DEPTH: u16 = 1;

/// The exposed tool name, caller lineage and presentation subject of a direct
/// effect, from its committed call block.
pub(crate) fn effect_view(record: &DurableOperationRecord) -> Result<EffectView, AgentError> {
    Ok(call_view(decode_effect_call(record)?.call()))
}

/// A Code Cell's outer call, from its evaluation record.
pub(crate) fn code_cell_view(record: &DurableOperationRecord) -> Result<EffectView, AgentError> {
    let request = decode_local_composition_evaluation_request(record.request())
        .ok_or_else(|| integrity("Code Cell evaluation request could not be reconstructed"))?;
    Ok(call_view(&request.call))
}

fn call_view(call: &ToolCallBlock) -> EffectView {
    let lineage = match call.correlation().caller_lineage() {
        ToolCallerLineage::Direct => FeedCallerLineage::Direct,
        ToolCallerLineage::ProviderProgrammatic { kind, execution_id } => {
            FeedCallerLineage::ProviderProgrammatic {
                kind: kind.clone(),
                execution_id: execution_id.clone(),
            }
        }
        &ToolCallerLineage::LocalProgrammatic {
            evaluation_operation_id,
            ref cell_id,
            request_sequence,
            depth,
            ..
        } => FeedCallerLineage::LocalProgrammatic {
            evaluation_operation_id,
            parent_call_id: None,
            cell_id: cell_id.clone(),
            request_sequence,
            depth,
        },
    };
    EffectView {
        exposed_name: call.exposed_name().clone(),
        lineage,
        subject: history_tool_call_observation(call)
            .subject()
            .map(str::to_owned),
    }
}

/// The tool name and presentation subject of a Code Cell's nested call. Its
/// lineage is the record's own; the caller fills it in.
pub(crate) fn nested_effect_view(
    record: &DurableOperationRecord,
) -> Result<EffectView, AgentError> {
    let (exposed_name, arguments) = nested_request(record)?;
    // The nested call's raw arguments are the compact JSON of its input, so
    // the subject takes the same history variant a live `ToolStarted` does.
    let subject = arguments.as_ref().and_then(|arguments| {
        arguments_presentation_subject(&serde_json::from_str(arguments).ok()?, arguments.len())
    });
    Ok(EffectView {
        exposed_name,
        lineage: FeedCallerLineage::Direct,
        subject,
    })
}

/// The command operation the ledger recorded for an accepted command.
pub(crate) fn command_operation_id(
    successor: &RuntimeCheckpoint,
    command: &DurableCommandRecord,
) -> Option<u64> {
    successor
        .manifest()
        .ledger()
        .operations()
        .into_iter()
        // The command's operation was just appended: search newest first.
        .rev()
        .find(|record| {
            matches!(record.kind(), crate::DurableOperationKind::Command { .. })
                && record.request().bytes() == command.command_component().bytes()
        })
        .map(DurableOperationRecord::operation_id)
}

/// What a compaction operation's request and evidence record about it.
pub(crate) struct CompactionView {
    pub(crate) turn_fence: bool,
    pub(crate) operation_id: String,
    pub(crate) pass: FeedCompactPass,
    pub(crate) tokens_before: u64,
    /// Present once a successor was published.
    pub(crate) tokens_after: Option<u64>,
    /// The checkpoint only a lost owner's closure publishes.
    pub(crate) abandonment_checkpoint: CheckpointId,
}

pub(crate) fn compaction_view(
    record: &DurableOperationRecord,
) -> Result<CompactionView, AgentError> {
    let invalid = || integrity("compaction operation request could not be reconstructed");
    let bytes = record.request().bytes();
    let (operation_id, source, pass, tokens_before, turn_fence) =
        if let Ok(offline) = serde_json::from_slice::<DurablePlaceholderRequestRecord>(bytes) {
            let pass = match offline.version {
                1 => FeedCompactPass::Placeholders,
                2 => FeedCompactPass::ReplayOmission,
                _ => return Err(invalid()),
            };
            let source = DurableSessionAddress::new(
                offline.source_session,
                offline.source_branch,
                offline.source_checkpoint,
            );
            (
                offline.operation_id,
                source,
                pass,
                offline.before_input_tokens,
                offline.turn_fence,
            )
        } else {
            let paid: DurableCompactionRequestRecord =
                serde_json::from_slice(bytes).map_err(|_| invalid())?;
            let pass = match paid.version {
                2 => FeedCompactPass::Summary,
                3 => FeedCompactPass::Native,
                _ => return Err(invalid()),
            };
            let source = paid.source();
            (
                paid.operation_id,
                source,
                pass,
                paid.before_input_tokens,
                paid.turn_fence,
            )
        };
    let identity = DurableCompactionIdentity::new(&operation_id, source)?;
    let tokens_after = match record.state() {
        DurableOperationState::Finalized(evidence) => {
            if let Some(offline) = DurablePlaceholderEvidence::decode_publication(
                evidence.bytes(),
                identity.workflow_id,
            ) {
                Some(offline.after_input_tokens)
            } else {
                serde_json::from_slice::<DurableCompactionPublicationEvidence>(evidence.bytes())
                    .ok()
                    .map(|publication| publication.after_input_tokens())
            }
        }
        _ => None,
    };
    Ok(CompactionView {
        turn_fence,
        operation_id,
        pass,
        tokens_before,
        tokens_after,
        abandonment_checkpoint: identity.abandonment_evidence_checkpoint,
    })
}

/// The failure a compaction finalized without a successor recorded.
pub(crate) fn compaction_failure(
    record: &DurableOperationRecord,
) -> Result<AgentError, AgentError> {
    let DurableOperationState::Finalized(evidence) = record.state() else {
        return Err(integrity("compaction failure is not finalized"));
    };
    super::durable::compaction_recovery::decode_compaction_failure_evidence(evidence)
}

/// Whether the closure record names this operation among those it abandoned.
pub(crate) fn operation_abandoned_at(
    successor: &RuntimeCheckpoint,
    operation_id: u64,
) -> Result<bool, AgentError> {
    for record in successor.manifest().ledger().operations() {
        let crate::DurableOperationKind::Command {
            turn_id: Some(turn_id),
        } = record.kind()
        else {
            continue;
        };
        let DurableOperationState::Finalized(evidence) = record.state() else {
            continue;
        };
        let Ok(terminal) = read_terminal_result_record(evidence.bytes()) else {
            continue;
        };
        if terminal.turn_id != *turn_id {
            return Err(integrity("root terminal record names a foreign turn"));
        }
        if terminal
            .abandonment
            .is_some_and(|report| report.operations().contains(&operation_id))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A root turn's terminal as its consequence commit recorded it.
pub(crate) struct RootTerminal {
    pub(crate) turn_id: TurnId,
    pub(crate) command_id: CommandId,
    pub(crate) outcome: FeedTurnOutcome,
    pub(crate) usage: TurnUsage,
}

/// What a command operation finalized at `successor` closed.
pub(crate) enum CommandOperation {
    RootTerminal(RootTerminal),
    /// A steer this commit settled with not-applied evidence.
    SteerNotApplied,
    Other,
}

pub(crate) fn command_operation(
    _parent: &RuntimeCheckpoint,
    successor: &Arc<RuntimeCheckpoint>,
    record: &DurableOperationRecord,
) -> Result<CommandOperation, AgentError> {
    let crate::DurableOperationKind::Command {
        turn_id: Some(turn_id),
    } = record.kind()
    else {
        return Ok(CommandOperation::Other);
    };
    let commands = successor.manifest().ledger().commands();
    let Some(command) = commands.into_iter().find(|command| {
        command.turn_id() == Some(*turn_id)
            && command.disposition() == DurableCommandDisposition::Accepted
            && command.command_component().bytes() == record.request().bytes()
    }) else {
        return Ok(CommandOperation::Other);
    };
    match command_envelope(command)?.effect {
        DurableActorCommandEffect::Root => {}
        DurableActorCommandEffect::SteerQueued { .. } => {
            let checkpoint_id = successor.id();
            let not_applied = record.transitions().iter().any(|transition| {
                transition.checkpoint_id() == checkpoint_id
                    && match transition.state() {
                        DurableOperationState::ProvenNotDispatched(evidence)
                        | DurableOperationState::Completed(evidence)
                        | DurableOperationState::Finalized(evidence) => {
                            steering_not_applied(evidence.bytes(), record)
                        }
                        _ => false,
                    }
            });
            return Ok(if not_applied {
                CommandOperation::SteerNotApplied
            } else {
                CommandOperation::Other
            });
        }
        _ => return Ok(CommandOperation::Other),
    }
    let DurableOperationState::Finalized(evidence) = record.state() else {
        return Ok(CommandOperation::Other);
    };
    let terminal = read_terminal_result_record(evidence.bytes())
        .map_err(|_| integrity("root terminal record could not be read"))?;
    let command_id = CommandId::new(terminal.command_id.as_str())
        .map_err(|_| integrity("root terminal command identity is invalid"))?;
    if terminal.turn_id != *turn_id || terminal.command_id != command.command_id() {
        return Err(integrity("root terminal record does not match its command"));
    }
    let outcome = match &terminal.outcome {
        DurableTerminalOutcome::Completed(response) => match response.finish().reason() {
            FinishReason::Refusal => FeedTurnOutcome::Refused,
            finish => FeedTurnOutcome::Completed { finish },
        },
        DurableTerminalOutcome::Failed(error) => {
            let result = terminal_turn_result(&terminal, Arc::clone(successor))?;
            let context = result
                .context_overflow_remedy()
                .map(|remedy| FeedContextLimit {
                    withheld: remedy == ContextOverflowRemedy::ResendShorter,
                    remedy,
                });
            let error = error.clone().into_error()?;
            FeedTurnOutcome::Failed {
                kind: error.kind(),
                model_kind: error.model_kind(),
                context,
            }
        }
        DurableTerminalOutcome::Cancelled(_) => {
            let report = terminal.abandonment.clone();
            FeedTurnOutcome::Cancelled {
                cause: report.map_or(FeedCancelCause::Requested, |report| {
                    FeedCancelCause::Abandoned {
                        claim: report.claim(),
                        cause: report.cause(),
                        model_round_completed: report.model_round_completed(),
                        tool_round_completed: report.tool_round_completed(),
                    }
                }),
            }
        }
        DurableTerminalOutcome::RecoveryRequired(_) => FeedTurnOutcome::RecoveryRequired,
    };
    Ok(CommandOperation::RootTerminal(RootTerminal {
        turn_id: terminal.turn_id,
        command_id,
        outcome,
        usage: terminal.usage,
    }))
}

fn integrity(message: &'static str) -> AgentError {
    AgentError::new(AgentErrorKind::Session, message)
}

/// What one operation names for a change-feed call read.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum FeedOperationCall {
    /// A call whose block and result canonical history holds.
    Call(CallId),
    /// A direct effect: its block and result once history holds them, else
    /// the block its operation record's request holds (from preparation
    /// until the result commits).
    Effect {
        /// The call identity.
        call_id: CallId,
        /// The block the request holds.
        requested: Box<ToolCallBlock>,
    },
    /// A Code Cell's nested call: its name and arguments as its operation
    /// record holds them.
    Nested {
        /// The nested call identity.
        call_id: CallId,
        /// The nested tool name.
        name: ToolName,
        /// The nested call's arguments, as JSON text.
        arguments: Option<String>,
    },
    /// A process session and its job row, while the ledger retains it.
    Job(Option<JobRow>),
    /// A steer command's text.
    Steer(String),
}

/// Resolves one operation of `checkpoint` to the call it names, or `None`
/// when the ledger holds no such operation or it names no call.
///
/// # Errors
///
/// Returns an integrity failure for a record this build cannot read.
pub fn checkpoint_operation_call(
    checkpoint: &RuntimeCheckpoint,
    operation_id: u64,
) -> Result<Option<FeedOperationCall>, AgentError> {
    let ledger = checkpoint.manifest().ledger();
    let Some(record) = ledger.operation(operation_id) else {
        return Ok(None);
    };
    Ok(match record.kind() {
        crate::DurableOperationKind::Effect { call_id, .. } => match decode_effect_call(record) {
            Ok(requested) => Some(FeedOperationCall::Effect {
                call_id: *call_id,
                requested: Box::new(requested.into_call()),
            }),
            Err(_) => Some(FeedOperationCall::Call(*call_id)),
        },
        crate::DurableOperationKind::EffectDecision { call_id, .. }
        | crate::DurableOperationKind::CompositionEvaluation { call_id, .. } => {
            Some(FeedOperationCall::Call(*call_id))
        }
        crate::DurableOperationKind::CompositionChild { call_id, .. } => {
            let (name, arguments) = nested_request(record)?;
            Some(FeedOperationCall::Nested {
                call_id: *call_id,
                name,
                arguments,
            })
        }
        crate::DurableOperationKind::ProcessSession { session_id, .. } => {
            Some(FeedOperationCall::Job(
                checkpoint
                    .manifest()
                    .jobs()
                    .rows()
                    .iter()
                    .find(|row| row.session_id == *session_id)
                    .cloned(),
            ))
        }
        crate::DurableOperationKind::Command { turn_id: Some(_) } => ledger
            .commands()
            .into_iter()
            .find(|command| command.command_component().bytes() == record.request().bytes())
            .map(command_envelope)
            .transpose()?
            .filter(|envelope| {
                matches!(
                    envelope.effect,
                    DurableActorCommandEffect::SteerQueued { .. }
                )
            })
            .map(|envelope| steer_text(&envelope.canonical_input))
            .transpose()?
            .map(FeedOperationCall::Steer),
        _ => None,
    })
}

fn nested_request(
    record: &DurableOperationRecord,
) -> Result<(ToolName, Option<String>), AgentError> {
    type ChildEdge = (String, u64, u64, String, u32, Digest, Vec<u8>);
    type ChildRequest = (String, String, String, ToolName, Option<serde_json::Value>);
    let invalid = || integrity("nested tool operation request could not be reconstructed");
    let edge: ChildEdge =
        serde_json::from_slice(record.request().bytes()).map_err(|_| invalid())?;
    let request: ChildRequest = serde_json::from_slice(&edge.6).map_err(|_| invalid())?;
    Ok((
        request.3,
        request.4.as_ref().map(serde_json::Value::to_string),
    ))
}

/// The text frame of a canonical steer command (tag, turn, text).
fn steer_text(canonical: &[u8]) -> Result<String, AgentError> {
    let mut rest = canonical;
    let mut frames = Vec::with_capacity(3);
    while !rest.is_empty() {
        let (length, tail) = rest
            .split_first_chunk::<8>()
            .ok_or_else(|| integrity("durable steer command is malformed"))?;
        let length = usize::try_from(u64::from_be_bytes(*length))
            .map_err(|_| integrity("durable steer command is malformed"))?;
        let (frame, tail) = tail
            .split_at_checked(length)
            .ok_or_else(|| integrity("durable steer command is malformed"))?;
        frames.push(frame);
        rest = tail;
    }
    match frames.as_slice() {
        [b"steer", _, text] => String::from_utf8(text.to_vec())
            .map_err(|_| integrity("durable steer command text is not UTF-8")),
        _ => Err(integrity("durable steer command is malformed")),
    }
}

/// The committed terminal of `turn_id`'s root command at `checkpoint`, with
/// the abandonment report its closure certified when `parent` is the
/// checkpoint the closure ran over. `None` when the checkpoint holds no
/// committed terminal for that turn.
///
/// # Errors
///
/// Returns an integrity failure for a record this build cannot read.
#[allow(
    clippy::type_complexity,
    reason = "the pure read carries only the terminal and its interruption report"
)]
pub fn checkpoint_turn_terminal(
    checkpoint: &Arc<RuntimeCheckpoint>,
    turn_id: TurnId,
) -> Result<Option<(crate::TurnResult, Option<crate::AbandonedTurnReport>)>, AgentError> {
    read_checkpoint_turn_terminal(checkpoint, turn_id, false)
}

/// A pruned exact request may look up its retained terminal at the head.
///
/// # Errors
///
/// Returns an integrity error when the retained terminal cannot be decoded.
#[allow(
    clippy::type_complexity,
    reason = "the pure read carries the terminal and interruption report"
)]
pub fn checkpoint_retained_turn_terminal(
    checkpoint: &Arc<RuntimeCheckpoint>,
    turn_id: TurnId,
) -> Result<Option<(crate::TurnResult, Option<crate::AbandonedTurnReport>)>, AgentError> {
    read_checkpoint_turn_terminal(checkpoint, turn_id, true)
}

/// The authenticated publication checkpoint of a retained completed root.
/// Later history is not the completed turn's conversation authority.
///
/// # Errors
/// Returns an integrity error when the admitted command cannot be decoded.
pub fn checkpoint_completed_terminal_source(
    checkpoint: &RuntimeCheckpoint,
    turn_id: TurnId,
) -> Result<Option<CheckpointId>, AgentError> {
    for command in checkpoint.manifest().ledger().commands() {
        if command.turn_id() != Some(turn_id)
            || command.disposition() != DurableCommandDisposition::Accepted
            || !matches!(
                command_envelope(command)?.effect,
                DurableActorCommandEffect::Root
            )
        {
            continue;
        }
        let id = CommandId::new(command.command_id())
            .map_err(|_| integrity("root command identity is invalid"))?;
        let Ok(terminal) = super::continuation::terminal_record_at_checkpoint(checkpoint, &id)
        else {
            return Ok(None);
        };
        return Ok(
            matches!(terminal.outcome, DurableTerminalOutcome::Completed(_))
                .then_some(terminal.checkpoint_id),
        );
    }
    Ok(None)
}

#[allow(
    clippy::type_complexity,
    reason = "the pure read carries the terminal and interruption report"
)]
fn read_checkpoint_turn_terminal(
    checkpoint: &Arc<RuntimeCheckpoint>,
    turn_id: TurnId,
    retained_lookup: bool,
) -> Result<Option<(crate::TurnResult, Option<crate::AbandonedTurnReport>)>, AgentError> {
    let mut root = None;
    for command in checkpoint.manifest().ledger().commands() {
        if command.turn_id() == Some(turn_id)
            && command.disposition() == DurableCommandDisposition::Accepted
            && matches!(
                command_envelope(command)?.effect,
                DurableActorCommandEffect::Root
            )
        {
            root = Some(
                CommandId::new(command.command_id())
                    .map_err(|_| integrity("root command identity is invalid"))?,
            );
            break;
        }
    }
    let Some(command_id) = root else {
        return Ok(None);
    };
    if !matches!(
        checkpoint.manifest().lifecycle(),
        crate::RuntimeLifecycle::Quiescent
    ) {
        let Ok(terminal) =
            super::continuation::terminal_record_at_checkpoint(checkpoint, &command_id)
        else {
            return Ok(None);
        };
        let report = terminal.abandonment.clone();
        return terminal_turn_result(&terminal, Arc::clone(checkpoint))
            .map(|result| Some((result, report)));
    }
    let Ok(terminal) = super::continuation::terminal_record_at_checkpoint(checkpoint, &command_id)
    else {
        return Ok(None);
    };
    if terminal.checkpoint_id != checkpoint.id() && !retained_lookup {
        let enriched_here = terminal.abandonment.is_some()
            && checkpoint
                .manifest()
                .ledger()
                .operations()
                .into_iter()
                .any(|record| {
                    record.operation_id() == terminal.operation_id.0
                        && record
                            .transitions()
                            .last()
                            .is_some_and(|transition| transition.checkpoint_id() == checkpoint.id())
                });
        if !enriched_here {
            return Ok(None);
        }
    }
    if matches!(terminal.outcome, DurableTerminalOutcome::Completed(_))
        && terminal.conversation_identity != checkpoint.manifest().canonical().history().identity()
    {
        return Err(integrity("terminal conversation identity is inconsistent"));
    }
    let report = terminal.abandonment.clone();
    terminal_turn_result(&terminal, Arc::clone(checkpoint)).map(|result| Some((result, report)))
}
