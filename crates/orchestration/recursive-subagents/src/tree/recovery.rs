//! Settling a `recovery_required` child once its session outcome is known.
//!
//! The tree cannot open a session, so it cannot learn on its own what an
//! unresolved turn did. The host reopens the child's session (which abandons
//! exactly what a lost or fenced owner left) and hands the answer back here.
//! The tree projector's host-recovery and feed joins are the only recovery clearers
//! of sticky session failures. Explicit deletion may instead close an obligation
//! as abandoned; close, seal completion, adoption and replayed binding never do.

use serde::{Deserialize, Serialize};

use super::{
    AttemptFailure, AttemptId, AttemptStatus, NodeStatus, ProductAgentTree, TURN_ABANDONED,
    TopologyError, TopologyErrorCode, bounded_text, now_unix_millis,
};
use crate::protocol::AttemptScopedRequest;

/// Failure kind of an attempt whose recovery reopen found the session's head
/// quiescent: nothing was abandoned because nothing was left running, but the
/// attempt never reported a result either. Never a claim of completion.
pub const RECOVERED_WITHOUT_TURN: &str = "recovered_without_turn";

pub(super) const RECOVERY_ABANDONED: &str = "recovery_abandoned";

/// Failure kind of a spawn whose child session was never created: the create
/// was refused before it wrote anything, at the spawn or when recovery
/// replayed it.
pub(super) const SPAWN_REFUSED: &str = "spawn_refused";

/// What stands between a node and an ordinary failed-idle state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryNeed {
    /// The node is not waiting on recovery.
    NotRequired,
    /// The attempt's session outcome is unresolved, or its fold has not started
    /// because its views were deleted. An exact reopen of the
    /// child session (or, with no locator, replay of its deterministic create)
    /// settles it without replaying a model call or a tool.
    Session {
        /// The attempt the resolution must still be current for.
        attempt_id: AttemptId,
        /// The node's bound session, when the startup published one.
        locator: Option<String>,
        /// The checkout the child runs in.
        path: String,
    },
    /// A workspace condition that cannot be settled automatically: an
    /// interrupted seal, adopt, or copy removal has an unknown VCS outcome;
    /// an observed seal failure carries its explicit refusal instead.
    Workspace {
        /// The refusal for the recorded workspace condition. An observed
        /// seal failure is not an interrupted VCS outcome.
        code: TopologyErrorCode,
        /// Why, in the host's words.
        reason: String,
    },
}

/// How a recovery reopen settled the attempt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RecoveryResolution {
    /// The reopen abandoned the turn the lost owner left.
    TurnAbandoned {
        /// The abandonment's operator line.
        message: String,
    },
    /// The reopen found nothing running and nothing to abandon.
    RecoveredWithoutTurn {
        /// What the host observed.
        message: String,
    },
    /// The startup published no session locator, no session exists for the
    /// spawn's deterministic create, and replaying that create was refused:
    /// the spawn never produced a session, so nothing ran and nothing is
    /// left to reopen.
    NeverCreated {
        /// Why the create was refused.
        message: String,
    },
}

/// The `recover_agent` receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoverAgentOutput {
    /// The node recovered.
    pub agent_id: String,
    /// Its current attempt.
    pub attempt_id: Option<AttemptId>,
    /// Whether this call settled the attempt; `false` when it was not
    /// waiting on recovery (a repeat is idempotent).
    pub recovered: bool,
    /// The node's status after the call.
    pub status: NodeStatus,
    /// The attempt's status after the call.
    pub attempt: Option<AttemptStatus>,
    /// The attempt's failure kind after the call.
    pub failure_kind: Option<String>,
    /// Set when the session part is settled but an interrupted seal, adopt,
    /// or copy removal still holds the node `recovery_required`; names why,
    /// and `close_agent` ends the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_recovery: Option<String>,
}

impl ProductAgentTree {
    /// Closes an unresolved child's obligation for explicit session deletion.
    /// The host must fence its runtime session before publishing this change.
    /// This records an operator abandonment, not a successful recovery, and
    /// performs no session, seal, or workspace operation.
    ///
    /// # Errors
    ///
    /// Refuses the root, a live child, or a child without its current attempt.
    pub fn abandon_recovery_for_delete(&mut self, agent_id: &str) -> Result<bool, TopologyError> {
        let node = self.node(agent_id)?;
        if node.parent_agent_id.is_none() {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "session deletion may abandon child obligations only",
            ));
        }
        let attempt = self.current_attempt(agent_id).ok_or_else(|| {
            TopologyError::new(
                TopologyErrorCode::AttemptMismatch,
                "the child has no current attempt to abandon",
            )
        })?;
        if attempt.recovery_was_abandoned() {
            return Ok(false);
        }
        if node.status != NodeStatus::RecoveryRequired {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "only a recovery-required child may be abandoned for deletion",
            ));
        }
        let message = bounded_text(
            &format!(
                "operator abandoned {agent_id}/{} via session delete --abandon-children; \
             recovery will not run{}",
                attempt.attempt_id,
                attempt
                    .failure
                    .as_ref()
                    .map_or_else(String::new, |failure| format!(
                        "; prior {}: {}",
                        failure.kind, failure.message
                    ))
            ),
            512,
        );
        let attempt_id = attempt.attempt_id.clone();
        self.record_operator_intent(
            agent_id,
            Some(attempt_id),
            super::operator_intents::Decision::RecoveryAbandoned {
                abandoned_at_ms: now_unix_millis(),
                failure: AttemptFailure {
                    kind: RECOVERY_ABANDONED.to_owned(),
                    message,
                },
            },
        )?;
        Ok(true)
    }

    /// What `agent_id` needs before it can be re-tasked or closed.
    ///
    /// An unresolved session outcome comes first even when a seal or adopt
    /// was interrupted too: settling the session never touches the copy, and
    /// what the VCS step did is answered separately (`Workspace`) once the
    /// session part is settled.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent.
    pub fn recovery_need(&self, agent_id: &str) -> Result<RecoveryNeed, TopologyError> {
        let node = self.node(agent_id)?;
        if node.agent_id == self.root_agent_id {
            return Ok(RecoveryNeed::NotRequired);
        }
        if self
            .current_attempt(agent_id)
            .is_some_and(super::AttemptRecord::recovery_was_abandoned)
        {
            return Ok(RecoveryNeed::NotRequired);
        }
        if let Some(attempt) = self
            .current_attempt(agent_id)
            .filter(|attempt| node.fold_order.is_none() || attempt.session_outcome_unresolved())
        {
            return Ok(RecoveryNeed::Session {
                attempt_id: attempt.attempt_id.clone(),
                locator: if node.fold_order.is_none() {
                    self.operations
                        .get(&format!(
                            "{}:{agent_id}",
                            super::projector::CHILD_SESSION_KIND
                        ))
                        .map(|binding| binding.canonical_request.clone())
                } else {
                    node.session_locator.clone()
                },
                path: node.path.display().to_string(),
            });
        }
        if node.status != NodeStatus::RecoveryRequired {
            return Ok(RecoveryNeed::NotRequired);
        }
        let blocker = self.workspace_blocker(agent_id);
        if let Some(failure) = self
            .current_attempt(agent_id)
            .and_then(|attempt| attempt.failure.as_ref())
            .filter(|failure| failure.kind == Self::SEAL_FAILED_KIND && blocker.is_none())
        {
            let remedy = "; inspect the copy, then close_agent ends the node";
            let detail = self.fault_reason(agent_id).unwrap_or(&failure.message);
            return Ok(RecoveryNeed::Workspace {
                code: TopologyErrorCode::MintUnavailable,
                reason: format!("{}{remedy}", bounded_text(detail, 512 - remedy.len())),
            });
        }
        Ok(RecoveryNeed::Workspace {
            code: TopologyErrorCode::RecoveryVcsAmbiguous,
            reason: blocker.unwrap_or_else(|| {
                self.fault_reason(agent_id).map_or_else(
                    || {
                        "a seal was in flight or failed and whether its snapshot landed is not \
                         recorded; inspect the copy, then close_agent ends the node"
                            .to_owned()
                    },
                    |fault| format!("{fault}; inspect the copy, then close_agent ends the node"),
                )
            }),
        })
    }

    /// The VCS step still standing between `agent_id` and an ordinary state,
    /// if any: an in-flight seal, adopt, or copy removal.
    fn workspace_blocker(&self, agent_id: &str) -> Option<String> {
        let node = self.nodes.get(agent_id)?;
        if let Some(row) = self
            .workspaces
            .get(&node.workspace_id)
            .filter(|row| row.status.is_in_flight())
        {
            return Some(format!(
                "workspace {} is {}: the VCS step may or may not have landed, and the tree \
                 records nothing to compare it against; inspect the copy, then close_agent ends \
                 the node",
                row.workspace_id,
                row.status.as_str()
            ));
        }
        if self.seal_pending(agent_id) || node.status == NodeStatus::Sealing {
            return Some(
                "the node's seal is still running in this process; your agent await sees it \
                 finish, then close_agent ends the node"
                    .to_owned(),
            );
        }
        None
    }

    /// Records a recovery refusal that no retry can get past for `agent_id`'s
    /// attempt `attempt_id`: `close_agent` then ends the node, settling the
    /// attempt `failed` with `kind` (the refusal's code). Ignored when the
    /// attempt is no longer current.
    pub fn record_terminal_recovery_refusal(
        &mut self,
        agent_id: &str,
        attempt_id: &AttemptId,
        kind: &str,
        message: &str,
    ) {
        if self
            .nodes
            .get(agent_id)
            .and_then(|node| node.attempt_id.as_ref())
            != Some(attempt_id)
        {
            return;
        }
        self.terminal_recovery.insert(
            agent_id.to_owned(),
            (
                attempt_id.clone(),
                AttemptFailure {
                    kind: kind.to_owned(),
                    message: bounded_text(message, 512),
                },
            ),
        );
    }

    /// The terminal recovery refusal recorded for `agent_id`'s current
    /// attempt, if any.
    #[must_use]
    pub fn terminal_recovery_refusal(&self, agent_id: &str) -> Option<&AttemptFailure> {
        let current = self.nodes.get(agent_id)?.attempt_id.as_ref()?;
        self.terminal_recovery
            .get(agent_id)
            .filter(|(attempt_id, _)| attempt_id == current)
            .map(|(_, failure)| failure)
    }

    /// Non-root nodes whose session outcome an exact reopen can settle, in
    /// post-order, so deleted descendant folds rebuild before parent cascades.
    #[must_use]
    pub fn session_recovery_targets(&self) -> Vec<String> {
        let mut ordered = Vec::new();
        self.preorder(self.root_agent_id.as_str(), &mut ordered);
        ordered
            .into_iter()
            .rev()
            .filter(|id| {
                matches!(self.recovery_need(id), Ok(RecoveryNeed::Session { .. }))
                    && self.terminal_recovery_refusal(id).is_none()
            })
            .collect()
    }

    /// Authorizes `recover_agent` exactly as `close_agent` is authorized: the
    /// caller must manage the node, and an `expected_attempt_id` must name its
    /// current attempt.
    ///
    /// # Errors
    ///
    /// Refuses a stopped-for-good tree, a paused tree, a caller that does not
    /// manage the target, and a stale attempt id.
    pub fn authorize_recover(
        &self,
        caller_id: &str,
        request: &AttemptScopedRequest,
    ) -> Result<(), TopologyError> {
        self.require_live_or_terminal_cleanup()?;
        let grant = self.grant(caller_id)?;
        let allowed = grant.managed_ids.iter().any(|id| id == &request.agent_id)
            || grant.close_ids.iter().any(|id| id == &request.agent_id);
        if !allowed {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "caller may not recover that node",
            ));
        }
        self.require_expected_attempt(&request.agent_id, request.expected_attempt_id.as_ref())
    }

    /// The receipt for a node as it stands now.
    ///
    /// # Errors
    ///
    /// Refuses an unknown agent or an attempt that is no longer current.
    pub fn recover_receipt(
        &self,
        agent_id: &str,
        expected_attempt_id: Option<&AttemptId>,
        recovered: bool,
    ) -> Result<RecoverAgentOutput, TopologyError> {
        self.require_expected_attempt(agent_id, expected_attempt_id)?;
        let node = self.node(agent_id)?;
        let attempt = self.current_attempt(agent_id);
        Ok(RecoverAgentOutput {
            agent_id: agent_id.to_owned(),
            attempt_id: node.attempt_id.clone(),
            recovered,
            status: node.status,
            attempt: attempt.map(|attempt| attempt.status),
            failure_kind: attempt
                .and_then(|attempt| attempt.failure.as_ref())
                .map(|failure| failure.kind.clone()),
            workspace_recovery: self.workspace_blocker(agent_id),
        })
    }

    /// Replaces an unresolved session failure with the settled one a recovery
    /// reopen established. The attempt ends `failed`, except a quiescent
    /// retirement-only recovery preserves a submitted result as `completed`.
    /// When nothing else is pending the node is idle, so `followup` and `close_agent`
    /// work again, and partial work in the copy is sealed like any failed
    /// attempt's (a seal that failed earlier for the attempt is tried once
    /// more; a second failure records `seal_failed`). When an interrupted
    /// seal, adopt, or copy removal remains, the node stays
    /// `recovery_required` for that reason alone and its copy, seal state, and
    /// fault are untouched: `close_agent` ends it.
    ///
    /// This derived writer is called only by the projector, from either
    /// committed feed evidence or an immutable host-recovery intent. The
    /// caller preserves the external-effect fence on a view rebuild.
    pub(super) fn apply_host_recovery(
        &mut self,
        agent_id: &str,
        decision: &super::projector::host_recovery::HostRecovery,
        reserve_effects: bool,
    ) -> Result<RecoverAgentOutput, TopologyError> {
        let expected_attempt_id = &decision.attempt_id;
        self.require_expected_attempt(agent_id, Some(expected_attempt_id))?;
        let closed = decision.closed;
        let submitted_retirement = decision.submitted_retirement;
        let never_created = matches!(decision.resolution, RecoveryResolution::NeverCreated { .. });
        self.apply_host_recovery_attempt(decision);
        self.terminal_recovery.remove(agent_id);
        let workspace_blocked = self.has_faulted_vcs_obligation(agent_id);
        let seal_pending = self.seal_effect_pending(agent_id);
        if let Some(node) = self.nodes.get_mut(agent_id) {
            node.active_turn = false;
            if !workspace_blocked {
                node.status = if seal_pending {
                    NodeStatus::Sealing
                } else if closed {
                    NodeStatus::Closed
                } else if submitted_retirement {
                    NodeStatus::Completed
                } else {
                    NodeStatus::Failed
                };
            }
        }
        if !workspace_blocked {
            self.fault_reasons.remove(agent_id);
            if reserve_effects {
                if never_created {
                    self.unmint_unstarted_copy(agent_id);
                } else {
                    self.begin_seal(agent_id);
                }
                self.push_result_unless_sealing(agent_id);
            }
        }
        self.bump();
        self.recover_receipt(agent_id, Some(expected_attempt_id), true)
    }

    pub(super) fn apply_host_recovery_attempt(
        &mut self,
        decision: &super::projector::host_recovery::HostRecovery,
    ) {
        let (kind, message) = match decision.resolution.clone() {
            RecoveryResolution::TurnAbandoned { message } => (TURN_ABANDONED, message),
            RecoveryResolution::RecoveredWithoutTurn { message } => {
                (RECOVERED_WITHOUT_TURN, message)
            }
            RecoveryResolution::NeverCreated { message } => {
                (SPAWN_REFUSED, bounded_text(&message, 512))
            }
        };
        if let Some(attempt) = self.attempts.get_mut(&decision.attempt_id) {
            attempt.turn_id = None;
            attempt.status = if decision.closed {
                AttemptStatus::Closed
            } else if decision.submitted_retirement {
                AttemptStatus::Completed
            } else {
                AttemptStatus::Failed
            };
            let ended_at = decision.observed_at_ms;
            attempt.ended_at = Some(ended_at);
            attempt.usage.elapsed_ms = ended_at
                .saturating_sub(attempt.started_at)
                .max(attempt.usage.elapsed_ms);
            attempt.failure = (!decision.submitted_retirement).then_some(AttemptFailure {
                kind: kind.to_owned(),
                message,
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
