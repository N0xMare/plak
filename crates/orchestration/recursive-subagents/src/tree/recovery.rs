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
mod tests {
    use super::RECOVERY_ABANDONED;
    use crate::protocol::{
        AttemptScopedRequest, AttemptStatus, MessagePurpose, NodeStatus, SendAgentMessageRequest,
        Tier, TopologyErrorCode, WorkspaceStatus,
    };
    use crate::tree::test_support::*;
    use crate::tree::turn_outcome::FailedTurnDisposition;
    use crate::tree::*;

    fn stuck_child(tree: &mut ProductAgentTree) -> String {
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&id, "locator-1").unwrap();
        tree.set_active_turn(&id, true).unwrap();
        tree.record_turn_failure(
            &id,
            FailedTurnDisposition::RecoveryRequired,
            "child turn outcome is unknown and its session could not be reopened".into(),
        )
        .unwrap();
        tree.settle_for_test();
        id
    }

    /// A spawn recovery settles never-created ran nothing in its copy: the
    /// copy is unminted like a spawn-time refusal's, not sealed into a
    /// candidate.
    #[test]
    fn a_never_created_spawn_unmints_its_copy_instead_of_sealing_it() {
        let (_root, vcs, mut tree) = fake_tree();
        let mut binding = RosterBinding::leaf("test-account", "selected-tuple");
        binding.max_tier = Tier::Mutate;
        tree.pin_template("implement", binding);
        let id = tree
            .spawn_agent("root", &mint_req(), None)
            .unwrap()
            .agent_id;
        tree.settle_for_test();
        let workspace_id = tree.node(&id).unwrap().workspace_id.clone();
        tree.record_host_retirement(
            &id,
            "startup Creating: child session could not be created (Revalidation)".into(),
        )
        .and_then(|value| tree.fold_due(&id).map(|_| value))
        .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
        let attempt_id = tree.node(&id).unwrap().attempt_id.clone().unwrap();

        tree.recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::NeverCreated {
                message: "construction refused".into(),
            },
        )
        .unwrap();
        assert_eq!(
            tree.workspaces.get(&workspace_id).unwrap().status,
            WorkspaceStatus::Discarding
        );
        tree.settle_for_test();
        assert!(vcs.calls_of(FakeOp::Seal).is_empty(), "the copy was sealed");
        assert_eq!(vcs.calls_of(FakeOp::Unmint).len(), 1);
        assert!(!tree.seal_pending(&id));
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
        assert_eq!(
            tree.attempt(&id).unwrap().failure.as_ref().unwrap().kind,
            super::SPAWN_REFUSED
        );
    }

    #[test]
    fn interrupted_tool_note_distinguishes_conversation_from_workspace_changes() {
        let note = AbandonedChildTurn {
            operations: vec![1],
            claim: plakous::agent::AbandonedTurnClaim::RunningTool,
        }
        .note("c1");
        assert!(note.contains("conversation"), "{note}");
        assert!(note.contains("review"), "{note}");
        assert!(!note.contains("nothing from it was kept"), "{note}");
    }

    #[test]
    fn closed_submission_stays_closed_after_late_retirement_recovery() {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.set_active_turn(&id, true).unwrap();
        let token = tree.attempt(&id).unwrap().turn_token;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: token,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.close_agent("root", &close_req(&id), None).unwrap();
        tree.current_attempt_mut(&id).unwrap().ended_at = Some(1);
        let ended_at = tree.attempt(&id).unwrap().ended_at;
        tree.record_host_retirement(&id, "release deadline".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap();
        let mut tree = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
        let listed = tree
            .list_agents(
                "root",
                &ListAgentsRequest {
                    include_completed: Some(false),
                    include_self: None,
                    include_read: None,
                },
            )
            .unwrap();
        assert!(listed.agents.iter().any(|agent| agent["agent_id"] == id));
        assert!(tree.pending_result_mail("root").is_empty());
        let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.recover_host_decision(
            &id,
            &attempt,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiescent".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Closed);
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
        assert_eq!(tree.attempt(&id).unwrap().ended_at, ended_at);
        assert_eq!(
            tree.attempt(&id).unwrap().output_json.as_deref(),
            Some(r#"{"ok":true}"#)
        );
        assert!(tree.pending_result_mail("root").is_empty());
    }

    #[test]
    fn delete_abandonment_survives_reopen_and_cannot_be_recovered_or_overwritten() {
        let (_tmp, mut tree) = tree();
        let child = stuck_child(&mut tree);
        let attempt_id = tree.attempt(&child).unwrap().attempt_id.clone();
        assert!(tree.take_effects().is_empty());
        assert!(tree.abandon_recovery_for_delete(&child).unwrap());
        assert!(!tree.abandon_recovery_for_delete(&child).unwrap());
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        assert_eq!(
            tree.attempt(&child).unwrap().failure.as_ref().unwrap().kind,
            RECOVERY_ABANDONED
        );
        assert!(
            tree.take_effects().is_empty(),
            "abandon never runs a seal or VCS operation"
        );
        tree.record_turn_failure(
            &child,
            FailedTurnDisposition::RecoveryRequired,
            "late waiter".into(),
        )
        .unwrap();
        assert_eq!(tree.node(&child).unwrap().status, NodeStatus::Closed);
        let store = MemoryTreeStore::default();
        tree.acknowledge_revision(store.save(&tree).unwrap());
        let mut reopened = ProductAgentTree::reopen(store.load(&tree.tree_id).unwrap().unwrap());
        assert_eq!(
            reopened.recovery_need(&child).unwrap(),
            RecoveryNeed::NotRequired
        );
        assert!(reopened.session_recovery_targets().is_empty());
        let recovered = reopened
            .recover_host_decision(
                &child,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "must not replace abandonment".into(),
                },
            )
            .unwrap();
        assert!(!recovered.recovered);
        assert_eq!(recovered.failure_kind.as_deref(), Some(RECOVERY_ABANDONED));
        assert!(reopened.take_effects().is_empty());
        assert_eq!(reopened.node(&child).unwrap().status, NodeStatus::Closed);
    }

    #[test]
    fn submitted_result_survives_quiescent_retirement_recovery() {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let token = tree.attempt(&id).unwrap().turn_token;
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: token,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        tree.record_host_retirement(&id, "release deadline".into())
            .and_then(|value| tree.fold_due(&id).map(|_| value))
            .unwrap();
        let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.recover_host_decision(
            &id,
            &attempt,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiescent after late release".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Completed);
        assert_eq!(
            tree.attempt(&id).unwrap().output_json.as_deref(),
            Some(r#"{"ok":true}"#)
        );
        assert!(tree.attempt(&id).unwrap().failure.is_none());
    }

    #[test]
    fn scoped_retirement_fault_skips_settled_and_replaced_attempts() {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        let expected = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.submit_result(
            &id,
            &SubmitResultRequest {
                turn_token: 1,
                output_json: r#"{"ok":true}"#.into(),
            },
            None,
        )
        .unwrap();
        assert!(
            tree.record_host_retirement_for_attempt(&id, Some(&expected), "owner deadline".into())
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap()
        );
        tree.recover_host_decision(
            &id,
            &expected,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiescent".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        let settled = (
            tree.revision,
            tree.node(&id).unwrap().clone(),
            tree.attempt(&id).unwrap().clone(),
        );
        assert!(
            !tree
                .record_host_retirement_for_attempt(
                    &id,
                    Some(&expected),
                    "late owner failure".into()
                )
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap()
        );
        assert_eq!(
            (
                tree.revision,
                tree.node(&id).unwrap().clone(),
                tree.attempt(&id).unwrap().clone()
            ),
            settled
        );
        assert!(tree.take_effects().is_empty());
        let node = tree.node(&id).unwrap();
        let schema = node.output_schema_json.clone();
        let digest = node.schema_digest.clone();
        tree.open_followup_attempt(&id, "m-followup", schema, digest)
            .unwrap();
        let successor = tree.attempt(&id).unwrap().clone();
        let revision = tree.revision;
        assert!(
            !tree
                .record_host_retirement_for_attempt(
                    &id,
                    Some(&expected),
                    "old owner failure".into()
                )
                .and_then(|value| tree.fold_due(&id).map(|_| value))
                .unwrap()
        );
        assert_eq!(tree.attempt(&id).unwrap(), &successor);
        assert_eq!(tree.revision, revision);
    }

    #[test]
    fn closed_turn_failures_keep_recovery_visible_without_result_mail() {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.set_active_turn(&id, true).unwrap();
        tree.close_agent("root", &close_req(&id), None).unwrap();
        let expected = tree.attempt(&id).unwrap().attempt_id.clone();
        let ended_at = tree.attempt(&id).unwrap().ended_at;
        for message in ["unknown terminal", "late terminal waiter"] {
            tree.record_turn_failure(&id, FailedTurnDisposition::RecoveryRequired, message.into())
                .unwrap();
            assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
            assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Closed);
            assert!(tree.pending_result_mail("root").is_empty());
        }
        tree.recover_host_decision(
            &id,
            &expected,
            RecoveryResolution::TurnAbandoned {
                message: "recovered".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Closed);
        assert_eq!(tree.attempt(&id).unwrap().ended_at, ended_at);
        assert!(tree.pending_result_mail("root").is_empty());
    }

    #[test]
    fn session_bound_after_failure_clears_only_its_terminal_timestamp() {
        let (_tmp, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.record_host_materialization(&id).unwrap();
        tree.fold_due(&id).unwrap();
        assert!(tree.attempt(&id).unwrap().ended_at.is_some());
        tree.bind_session_locator(&id, "bound-child-session")
            .unwrap();
        tree.record_session_bound_after_failure(&id).unwrap();
        tree.fold_due(&id).unwrap();
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Running);
        assert_eq!(tree.attempt(&id).unwrap().ended_at, None);
        tree.record_turn_failure(
            &id,
            FailedTurnDisposition::Failed { kind: "terminal" },
            "new terminal".into(),
        )
        .unwrap();
        assert!(tree.attempt(&id).unwrap().ended_at.unwrap() > 1);
    }

    #[test]
    fn submitted_failed_terminal_is_not_completed_by_retirement_recovery() {
        for terminal_first in [true, false] {
            let (_tmp, mut tree) = tree();
            let id = tree
                .spawn_agent("root", &spawn_req(), None)
                .unwrap()
                .agent_id;
            let token = tree.attempt(&id).unwrap().turn_token;
            tree.submit_result(
                &id,
                &SubmitResultRequest {
                    turn_token: token,
                    output_json: r#"{"ok":true}"#.into(),
                },
                None,
            )
            .unwrap();
            if !terminal_first {
                tree.record_host_retirement(&id, "release deadline".into())
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .unwrap();
            }
            tree.record_turn_failure(
                &id,
                FailedTurnDisposition::Failed {
                    kind: "committed_failure",
                },
                "authoritative terminal failure".into(),
            )
            .unwrap();
            if terminal_first {
                tree.record_host_retirement(&id, "release deadline".into())
                    .and_then(|value| tree.fold_due(&id).map(|_| value))
                    .unwrap();
            }
            let attempt = tree.attempt(&id).unwrap().attempt_id.clone();
            tree.recover_host_decision(
                &id,
                &attempt,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiescent".into(),
                },
            )
            .unwrap();
            tree.settle_for_test();
            assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Failed);
        }
    }

    #[test]
    fn resolving_recovery_does_not_repush_a_pruned_result() {
        let (_tmp, mut tree) = tree();
        let child = stuck_child(&mut tree);
        let attempt_id = tree.attempt(&child).unwrap().attempt_id.clone();
        let result = tree.pending_result_mail("root")[0].message_id.clone();
        tree.acknowledge_mail(&result).unwrap();
        let sender = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        for _ in 0..70 {
            let sent = tree
                .send_agent_message(
                    &sender.agent_id,
                    &SendAgentMessageRequest {
                        agent_id: "root".into(),
                        message: "later".into(),
                        purpose: None,
                        priority: None,
                        in_reply_to: None,
                        output_schema_json: None,
                    },
                    None,
                )
                .unwrap();
            tree.acknowledge_mail(&sent.message_id).unwrap();
        }
        assert_eq!(tree.inbox_for("root", true).len(), 64);
        let mut reopened = ProductAgentTree::reopen(tree.to_persisted());
        reopened
            .recover_host_decision(
                &child,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiet".into(),
                },
            )
            .unwrap();
        reopened.settle_for_test();
        assert!(reopened.pending_result_mail("root").is_empty());
    }

    fn scoped(agent_id: &str) -> AttemptScopedRequest {
        AttemptScopedRequest {
            agent_id: agent_id.to_owned(),
            expected_attempt_id: None,
        }
    }

    fn followup(agent_id: &str) -> SendAgentMessageRequest {
        SendAgentMessageRequest {
            agent_id: agent_id.to_owned(),
            message: "again".into(),
            purpose: Some(MessagePurpose::Followup),
            priority: None,
            in_reply_to: None,
            output_schema_json: None,
        }
    }

    /// A turn ended recovery-required and the one reopen that could have
    /// abandoned it failed. Close and followup refuse it until a recovery settles it; then
    /// both work, the attempt is refunded, and closing frees the node slot.
    #[test]
    fn a_settled_recovery_unsticks_close_and_followup() {
        let (_tmp, mut tree) = tree();
        let id = stuck_child(&mut tree);
        let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
        assert!(matches!(
            tree.recovery_need(&id).unwrap(),
            RecoveryNeed::Session {
                locator: Some(_),
                ..
            }
        ));
        assert_eq!(tree.session_recovery_targets(), vec![id.clone()]);
        let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
        assert_eq!(closed, vec![(id.clone(), NodeStatus::RecoveryRequired)]);
        let refused = tree
            .send_agent_message("root", &followup(&id), None)
            .unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::FollowupRecoveryRequired);

        let receipt = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::TurnAbandoned {
                    message: "abandoned".into(),
                },
            )
            .unwrap();
        tree.settle_for_test();
        assert!(receipt.recovered);
        assert_eq!(receipt.status, NodeStatus::Failed);
        assert_eq!(receipt.failure_kind.as_deref(), Some(TURN_ABANDONED));
        assert!(tree.attempt(&id).unwrap().turn_was_abandoned());
        assert_eq!(tree.recovery_need(&id).unwrap(), RecoveryNeed::NotRequired);
        assert!(tree.session_recovery_targets().is_empty());

        let again = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "second".into(),
                },
            )
            .unwrap();
        assert!(!again.recovered, "a repeat changes nothing");
        assert_eq!(again.failure_kind.as_deref(), Some(TURN_ABANDONED));

        tree.send_agent_message("root", &followup(&id), None)
            .unwrap();
        assert_eq!(tree.attempt(&id).unwrap().status, AttemptStatus::Running);
        tree.record_turn_completed(&id).unwrap();
        tree.settle_for_test();
        let before = tree
            .nodes()
            .values()
            .filter(|node| node.status != NodeStatus::Closed)
            .count();
        let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
        assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
        let after = tree
            .nodes()
            .values()
            .filter(|node| node.status != NodeStatus::Closed)
            .count();
        assert_eq!(after + 1, before);
    }

    /// A quiescent head settles as its own kind, never as completion, and a
    /// resolution for an attempt that is no longer current is refused.
    #[test]
    fn a_quiescent_recovery_is_its_own_kind_and_is_fenced_to_the_attempt() {
        let (_tmp, mut tree) = tree();
        let id = stuck_child(&mut tree);
        let stale = AttemptId::new(&id, 9);
        let error = tree
            .recover_host_decision(
                &id,
                &stale,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiet".into(),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::RecoveryRequired);
        let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
        let receipt = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiet".into(),
                },
            )
            .unwrap();
        assert_eq!(
            receipt.failure_kind.as_deref(),
            Some(RECOVERED_WITHOUT_TURN)
        );
        assert_eq!(receipt.attempt, Some(AttemptStatus::Failed));
        assert_ne!(receipt.attempt, Some(AttemptStatus::Completed));
        tree.send_agent_message("root", &followup(&id), None)
            .unwrap();
        let error = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "late".into(),
                },
            )
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
        let error = tree
            .recover_receipt(&id, Some(&attempt_id), true)
            .unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::AttemptMismatch);
    }

    /// An interrupted adopt behind a session failure: the session part is
    /// settled first without touching the copy or its adopt, the node stays
    /// `recovery_required` for the VCS reason alone, and `close_agent` ends it
    /// and frees its node slot.
    #[test]
    fn an_interrupted_adopt_settles_its_session_then_closes_to_a_free_slot() {
        let (_tmp, mut tree) = tree();
        let id = stuck_child(&mut tree);
        tree.grants.get_mut("root").unwrap().max_nodes = 2;
        let workspace_id = tree.node(&id).unwrap().workspace_id.clone();
        tree.workspaces.get_mut(&workspace_id).unwrap().status = WorkspaceStatus::Adopting;
        let mut tree = ProductAgentTree::reopen(tree.to_persisted());
        assert!(matches!(
            tree.recovery_need(&id).unwrap(),
            RecoveryNeed::Session { .. }
        ));
        assert_eq!(tree.session_recovery_targets(), vec![id.clone()]);
        let full = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(
            full.code,
            TopologyErrorCode::SpawnBudget,
            "it holds its slot"
        );

        let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
        let receipt = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::TurnAbandoned {
                    message: "abandoned".into(),
                },
            )
            .unwrap();
        assert!(receipt.recovered);
        assert_eq!(receipt.status, NodeStatus::RecoveryRequired);
        assert_eq!(receipt.failure_kind.as_deref(), Some(TURN_ABANDONED));
        assert!(
            receipt
                .workspace_recovery
                .as_deref()
                .is_some_and(|reason| reason.contains("close_agent"))
        );
        assert_eq!(
            tree.workspaces.get(&workspace_id).unwrap().status,
            WorkspaceStatus::Adopting,
            "settling the session leaves the adopt alone"
        );
        assert!(matches!(
            tree.recovery_need(&id).unwrap(),
            RecoveryNeed::Workspace { .. }
        ));
        assert!(tree.session_recovery_targets().is_empty());
        let again = tree
            .recover_host_decision(
                &id,
                &attempt_id,
                RecoveryResolution::RecoveredWithoutTurn {
                    message: "quiet".into(),
                },
            )
            .unwrap_err();
        assert_eq!(again.code, TopologyErrorCode::RecoveryVcsAmbiguous);

        let live = |tree: &ProductAgentTree| {
            tree.nodes()
                .values()
                .filter(|node| node.status != NodeStatus::Closed)
                .count()
        };
        let before = live(&tree);
        let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
        assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
        tree.settle_for_test();
        assert_eq!(live(&tree) + 1, before, "close freed the node slot");
    }

    /// A settled abandonment is final for its attempt: a failure report that
    /// arrives after it (a turn waiter finishing late) changes nothing.
    #[test]
    fn the_first_settlement_of_an_attempt_wins() {
        let (_tmp, mut tree) = tree();
        let id = stuck_child(&mut tree);
        let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.recover_host_decision(
            &id,
            &attempt_id,
            RecoveryResolution::TurnAbandoned {
                message: "abandoned".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        tree.record_turn_failure(
            &id,
            FailedTurnDisposition::RecoveryRequired,
            "late waiter".into(),
        )
        .unwrap();
        let attempt = tree.attempt(&id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert_eq!(
            attempt
                .failure
                .as_ref()
                .map(|failure| failure.kind.as_str()),
            Some(TURN_ABANDONED)
        );
        assert_eq!(tree.node(&id).unwrap().status, NodeStatus::Failed);
    }

    /// Recovery authority is close authority; a sibling cannot recover.
    #[test]
    fn recover_authority_follows_close_authority() {
        let (_tmp, mut tree) = tree();
        let id = stuck_child(&mut tree);
        tree.authorize_recover("root", &scoped(&id)).unwrap();
        let error = tree.authorize_recover(&id, &scoped("root")).unwrap_err();
        assert_eq!(error.code, TopologyErrorCode::GrantDenied);
    }

    /// A stuck node keeps its tree-wide node slot until it is resolved and
    /// closed.
    #[test]
    fn a_recovery_required_node_holds_its_tree_node_slot_until_closed() {
        let (_tmp, mut tree) = tree();
        let stuck = stuck_child(&mut tree);
        tree.grants.get_mut("root").unwrap().max_nodes = 2;
        let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
        let attempt_id = tree.attempt(&stuck).unwrap().attempt_id.clone();
        tree.recover_host_decision(
            &stuck,
            &attempt_id,
            RecoveryResolution::RecoveredWithoutTurn {
                message: "quiet".into(),
            },
        )
        .unwrap();
        tree.settle_for_test();
        let refused = tree.spawn_agent("root", &spawn_req(), None).unwrap_err();
        assert_eq!(refused.code, TopologyErrorCode::SpawnBudget);
        tree.close_agent("root", &scoped(&stuck), None).unwrap();
        tree.settle_for_test();
        tree.spawn_agent("root", &spawn_req(), None).unwrap();
    }

    /// A failed copy removal is a VCS question, not a session one: no
    /// session reopen is attempted for it, and `close_agent` is its remedy.
    #[test]
    fn a_failed_copy_removal_is_not_a_session_recovery() {
        let (_root, mut tree) = tree();
        let id = tree
            .spawn_agent("root", &spawn_req(), None)
            .unwrap()
            .agent_id;
        tree.bind_session_locator(&id, "locator-1").unwrap();
        if let Some(node) = tree.nodes.get_mut(&id) {
            node.status = NodeStatus::RecoveryRequired;
        }
        let attempt = tree.current_attempt_mut(&id).unwrap();
        attempt.status = AttemptStatus::RecoveryRequired;
        attempt.failure = Some(AttemptFailure {
            kind: WORKSPACE_CLEANUP_KIND.into(),
            message: "the copy could not be removed".into(),
        });
        assert!(matches!(
            tree.recovery_need(&id).unwrap(),
            RecoveryNeed::Workspace { .. }
        ));
        assert!(tree.session_recovery_targets().is_empty());
    }

    /// A terminal recovery refusal recorded for the current attempt lets
    /// `close_agent` settle and end the node; one for a stale attempt is
    /// ignored.
    #[test]
    fn close_agent_settles_a_terminal_recovery_refusal() {
        let (_root, mut tree) = tree();
        let id = stuck_child(&mut tree);
        let stale = AttemptId::new(&id, 99);
        tree.record_terminal_recovery_refusal(&id, &stale, "recovery_session_unavailable", "x");
        assert!(tree.terminal_recovery_refusal(&id).is_none());
        let attempt_id = tree.attempt(&id).unwrap().attempt_id.clone();
        tree.record_terminal_recovery_refusal(
            &id,
            &attempt_id,
            "recovery_session_unavailable",
            "the child session was created by an incompatible build",
        );
        assert!(tree.session_recovery_targets().is_empty());
        let closed = tree.close_agent("root", &scoped(&id), None).unwrap();
        assert_eq!(closed, vec![(id.clone(), NodeStatus::Closed)]);
        let attempt = tree.attempt(&id).unwrap();
        assert_eq!(attempt.status, AttemptStatus::Failed);
        assert_eq!(
            attempt.failure.as_ref().unwrap().kind,
            "recovery_session_unavailable"
        );
    }
}
