//! The paid continuation a node is owed after its host was lost (spec 03
//! section 14.1): held for consent, pending dispatch, or running.
//!
//! Nothing here is stored beyond the existing continuation decision intent.
//! A held continuation is derived from the rows: the node is interrupted by
//! an owner-lost (or handoff) interrupt of its current attempt, no decision
//! names the turn the loss stopped, and no turn ran since. Any later turn,
//! a close, or another interrupt ends it.

use super::{
    AttemptId, AttemptStatus, CommandFact, ContinuationAnchor, ContinuationCause,
    ContinuationDecision, ContinuationIntent, InterruptCause, NodeStatus, ProductAgentTree,
    TopologyError, TopologyErrorCode,
};

/// Where a node's continuation after a loss stands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossContinuationState {
    /// No decision yet: the node waits for the operator's Resume.
    Held,
    /// Consent is recorded and the continuation turn was not admitted yet.
    Pending,
    /// The continuation turn is running.
    Running,
}

/// One node's continuation after a loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LossContinuation {
    /// The interrupted attempt.
    pub attempt_id: AttemptId,
    /// Why the node stopped: `owner_lost` or `handoff`.
    pub cause: ContinuationCause,
    /// What the loss stopped: the lost turn, or the attempt's last accepted
    /// turn for a node marked while parked between turns. With no accepted
    /// command on record, the loss's own interrupt intent.
    pub anchor: ContinuationAnchor,
    /// Its state.
    pub state: LossContinuationState,
    /// The continuation's runtime command id once consent is recorded.
    pub command_id: Option<String>,
}

impl ProductAgentTree {
    /// Prefix of every owner-lost or handoff continuation command identity.
    pub const LOSS_RESUME_COMMAND_PREFIX: &str = "plak:resume-loss:";

    /// The continuation command identity for one attempt and what the loss
    /// stopped: the same every time, so a resubmission is exact.
    #[must_use]
    pub fn loss_resume_command_id(
        agent_id: &str,
        attempt: &AttemptId,
        anchor: &ContinuationAnchor,
    ) -> String {
        let (kind, id) = match anchor {
            ContinuationAnchor::Turn { turn_id } => ("turn", turn_id),
            ContinuationAnchor::Command { command_id } => ("command", command_id),
        };
        // A runtime command id is at most 128 portable characters, so the
        // attempt and anchor enter it as a digest.
        let digest = super::digest_text(&format!("{attempt}\n{kind}\n{id}"));
        format!(
            "{}{agent_id}:{}",
            Self::LOSS_RESUME_COMMAND_PREFIX,
            &digest[..32]
        )
    }

    /// Every child with a continuation after a loss, in node order.
    #[must_use]
    pub fn loss_continuations(&self) -> Vec<(String, LossContinuation)> {
        self.nodes
            .keys()
            .filter(|agent_id| **agent_id != self.root_agent_id)
            .filter_map(|agent_id| {
                self.loss_continuation(agent_id)
                    .map(|continuation| (agent_id.clone(), continuation))
            })
            .collect()
    }

    /// `agent_id`'s continuation after a loss, if it has one.
    #[must_use]
    pub fn loss_continuation(&self, agent_id: &str) -> Option<LossContinuation> {
        if agent_id == self.root_agent_id {
            return None;
        }
        let node = self.nodes.get(agent_id)?;
        let attempt = self.current_attempt(agent_id).filter(|attempt| {
            attempt.status == AttemptStatus::Running && attempt.output_json.is_none()
        })?;
        let decisions = self.loss_decisions(agent_id, &attempt.attempt_id);
        if node.status == NodeStatus::Running
            && let Some(turn) = attempt.turn_id.as_deref()
            && let Some((decision, command_id)) = decisions.iter().find(|(_, command_id)| {
                self.commands.get(command_id).is_some_and(|command| {
                    command.fact == Some(CommandFact::Accepted)
                        && command.turn_id.as_deref() == Some(turn)
                })
            })
        {
            return Some(LossContinuation {
                attempt_id: attempt.attempt_id.clone(),
                cause: decision.cause,
                anchor: decision.anchor.clone(),
                state: LossContinuationState::Running,
                command_id: Some(command_id.clone()),
            });
        }
        // A spent attempt takes no more turns, a continuation included.
        if node.status != NodeStatus::Interrupted || self.attempt_turns_spent(agent_id) {
            return None;
        }
        let (interrupt, cause) = self.latest_interrupt(agent_id, &attempt.attempt_id)?;
        let cause = match cause? {
            InterruptCause::OwnerLost => ContinuationCause::OwnerLost,
            // A handoff covers the turn it interrupted: a turn admitted after
            // it and lost again is an owner loss, held for consent, so a
            // handoff continues by itself once.
            InterruptCause::Handoff
                if self.turn_admitted_after(agent_id, &attempt.attempt_id, interrupt) =>
            {
                ContinuationCause::OwnerLost
            }
            InterruptCause::Handoff => ContinuationCause::Handoff,
            _ => return None,
        };
        let anchor = self
            .last_accepted_turn(agent_id, &attempt.attempt_id)
            .map_or_else(
                || ContinuationAnchor::Command {
                    command_id: interrupt.to_owned(),
                },
                |turn_id| ContinuationAnchor::Turn {
                    turn_id: turn_id.to_owned(),
                },
            );
        let decided = decisions
            .iter()
            .find(|(decision, _)| decision.anchor == anchor);
        let (state, command_id) = match decided {
            None => (LossContinuationState::Held, None),
            Some((_, command_id))
                if self
                    .commands
                    .get(command_id)
                    .is_none_or(|command| command.fact.is_none()) =>
            {
                (LossContinuationState::Pending, Some(command_id.clone()))
            }
            Some(_) => return None,
        };
        Some(LossContinuation {
            attempt_id: attempt.attempt_id.clone(),
            cause,
            anchor,
            state,
            command_id,
        })
    }

    /// Records consent to `agent_id`'s held continuation: the one durable
    /// decision for this attempt, cause and stopped turn. A pending
    /// decision is returned as it is, so a retry resubmits its command.
    /// Returns the continuation's command id.
    ///
    /// # Errors
    ///
    /// Refuses a node with no held or pending continuation.
    pub fn record_loss_continuation(
        &mut self,
        agent_id: &str,
        decided_at_ms: u64,
    ) -> Result<String, TopologyError> {
        let continuation = self
            .loss_continuation(agent_id)
            .filter(|continuation| continuation.state != LossContinuationState::Running)
            .ok_or_else(|| {
                TopologyError::new(
                    TopologyErrorCode::GrantDenied,
                    "the agent has no continuation held after a loss",
                )
            })?;
        if let Some(command_id) = continuation.command_id {
            return Ok(command_id);
        }
        let command_id =
            Self::loss_resume_command_id(agent_id, &continuation.attempt_id, &continuation.anchor);
        let intent = ContinuationIntent {
            attempt_id: Some(continuation.attempt_id),
            anchor: continuation.anchor,
            host_generation: self.host_generation.max(1),
            cause: continuation.cause,
            round: 1,
            decided_at_ms,
            decision: ContinuationDecision::NewTurn {
                command_id: command_id.clone(),
                withdrawn: Vec::new(),
            },
        };
        let operation = intent.to_operation(agent_id, None)?;
        self.operations
            .insert(operation.operation_id.clone(), operation);
        self.bump();
        Ok(command_id)
    }

    /// Whether `command_id` is `agent_id`'s pending continuation: what a
    /// continuation Start re-checks when it reserves the child, so a close,
    /// an interrupt or another turn that ended it refuses the Start.
    #[must_use]
    pub fn loss_continuation_admissible(&self, agent_id: &str, command_id: &str) -> bool {
        self.loss_continuation(agent_id)
            .is_some_and(|continuation| {
                continuation.state == LossContinuationState::Pending
                    && continuation.command_id.as_deref() == Some(command_id)
            })
    }

    /// Whether `command_id` is the command of `agent_id`'s still-pending
    /// continuation: an owner-lost mark leaves only that command for the
    /// continuation to resubmit. A continuation that ended (an interrupt, a
    /// close, another turn, a spent attempt) keeps nothing, so its prepared
    /// command is withdrawn like any other.
    #[must_use]
    pub fn is_loss_continuation_command(&self, agent_id: &str, command_id: &str) -> bool {
        self.loss_continuation_admissible(agent_id, command_id)
    }

    /// The owner-lost and handoff decisions of one attempt, with the
    /// command each continues under.
    fn loss_decisions(
        &self,
        agent_id: &str,
        attempt_id: &AttemptId,
    ) -> Vec<(ContinuationIntent, String)> {
        self.operations
            .values()
            .filter(|operation| {
                operation.tool_name == ContinuationIntent::KIND
                    && operation.caller_agent_id == agent_id
            })
            .filter_map(|operation| ContinuationIntent::from_operation(operation).ok().flatten())
            .filter(|intent| {
                intent.attempt_id.as_ref() == Some(attempt_id)
                    && matches!(
                        intent.cause,
                        ContinuationCause::OwnerLost | ContinuationCause::Handoff
                    )
            })
            .filter_map(|intent| match &intent.decision {
                ContinuationDecision::NewTurn { command_id, .. } => {
                    let command_id = command_id.clone();
                    Some((intent, command_id))
                }
                _ => None,
            })
            .collect()
    }

    /// The turn of the attempt's latest accepted command.
    /// Whether a command of the attempt was accepted after the interrupt
    /// intent `interrupt` was recorded, by feed position.
    fn turn_admitted_after(&self, agent_id: &str, attempt_id: &AttemptId, interrupt: &str) -> bool {
        let Some(recorded) = self
            .operations
            .get(interrupt)
            .and_then(|operation| operation.fold)
        else {
            return false;
        };
        self.commands.values().any(|command| {
            command.recipient == agent_id
                && command.attempt_id.as_ref() == Some(attempt_id)
                && command.fact == Some(CommandFact::Accepted)
                && command.accepted_seq.is_some_and(|seq| seq > recorded.seq)
        })
    }

    fn last_accepted_turn(&self, agent_id: &str, attempt_id: &AttemptId) -> Option<&str> {
        self.commands
            .values()
            .filter(|command| {
                command.recipient == agent_id
                    && command.attempt_id.as_ref() == Some(attempt_id)
                    && command.fact == Some(CommandFact::Accepted)
            })
            .filter_map(|command| Some((command.accepted_seq?, command.turn_id.as_deref()?)))
            .max_by_key(|(seq, _)| *seq)
            .map(|(_, turn)| turn)
    }
}

#[cfg(test)]
mod tests;
