//! Parent consent to settle one exact child attempt before a followup.

use super::{
    AttemptId, ProductAgentTree, SendAgentMessageRequest, SendMessageOutput, TopologyError,
    TopologyErrorCode,
};

impl ProductAgentTree {
    pub(super) fn acknowledged_hold_is_settled(
        &self,
        agent: &str,
        acknowledged: Option<&AttemptId>,
    ) -> bool {
        acknowledged.is_some_and(|expected| {
            !self.recovery_owed(agent)
                && !self.has_current_turn_owner(agent)
                && matches!(
                    self.recovery_need(agent),
                    Ok(super::RecoveryNeed::NotRequired)
                )
                && self.loss_for_fresh_attempt(agent).is_some_and(|held| {
                    held.attempt_id == *expected && held.state == super::LossContinuationState::Held
                })
        })
    }

    pub(crate) fn parent_followup_recovery(
        &self,
        caller: &str,
        request: &SendAgentMessageRequest,
        operation: Option<&str>,
    ) -> Result<Option<AttemptId>, TopologyError> {
        let Some(expected) = request.acknowledge_recovery.as_ref() else {
            return Ok(None);
        };
        if operation
            .and_then(|id| self.operations.get(id))
            .is_some_and(|record| record.caller_agent_id != caller)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "recovery followup receipt belongs to a different caller",
            ));
        }
        if self
            .replay_typed::<_, SendMessageOutput>(operation, "send_agent_message", request)?
            .is_some()
        {
            return Ok(None);
        }
        self.require_live()?;
        request
            .validate(caller)
            .map_err(|code| TopologyError::new(code, "invalid recovery followup request"))?;
        let node = self.node(&request.agent_id)?;
        if node.parent_agent_id.as_deref() != Some(caller)
            || !self
                .grant(caller)?
                .message_peers
                .contains(&request.agent_id)
        {
            return Err(TopologyError::new(
                TopologyErrorCode::GrantDenied,
                "only the recipient's parent may acknowledge recovery for a followup",
            ));
        }
        if node.attempt_id.as_ref() != Some(expected) {
            return Err(TopologyError::new(
                TopologyErrorCode::AttemptMismatch,
                "acknowledge_recovery must name the recipient's current attempt_id",
            ));
        }
        Ok(Some(expected.clone()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "parent recovery admission fixtures")]
mod tests {
    use super::*;
    use crate::protocol::MessagePurpose;
    use crate::tree::test_support::{fake_tree, mail, spawn_req};

    #[test]
    fn only_a_settled_held_continuation_can_be_superseded_with_consent() {
        for blocker in ["none", "spent", "owner", "recovery", "pending"] {
            let (_directory, _vcs, mut tree) = fake_tree();
            if blocker == "spent" {
                let mut binding =
                    crate::epoch::RosterBinding::leaf("test-account", "selected-tuple");
                binding.max_turns = Some(1);
                tree.pin_template("review", binding);
            }
            let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
            tree.bind_session_locator(&child.agent_id, "locator")
                .unwrap();
            tree.bind_attempt_turn(&child.agent_id, "00000000-0000-0000-0000-000000000001")
                .unwrap();
            tree.record_turn_failure(
                &child.agent_id,
                crate::tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                "unknown outcome".into(),
            )
            .unwrap();
            tree.fold_due(&child.agent_id).unwrap();
            tree.lose_owner(&child.agent_id, Some(child.attempt_id.clone()));
            tree.set_active_turn(&child.agent_id, false).unwrap();
            if blocker == "spent" {
                assert!(tree.attempt_turns_spent(&child.agent_id));
                assert!(tree.loss_continuation(&child.agent_id).is_none());
            }
            assert!(tree.acknowledged_hold_is_settled(&child.agent_id, Some(&child.attempt_id)));
            let mut request = mail(&child.agent_id, "fresh work after inspecting uncertainty");
            request.purpose = Some(MessagePurpose::Followup);
            let refused = tree.send_agent_message("root", &request, None).unwrap_err();
            assert_eq!(refused.code, TopologyErrorCode::FollowupNotIdle);
            assert!(refused.message.contains("acknowledge_recovery"));
            assert!(refused.message.contains(child.attempt_id.as_str()));
            request.acknowledge_recovery = Some(child.attempt_id.clone());
            match blocker {
                "owner" => {
                    tree.reserve_turn_owner(&child.agent_id);
                    assert!(!tree.node(&child.agent_id).unwrap().active_turn);
                    assert!(
                        !tree
                            .acknowledged_hold_is_settled(&child.agent_id, Some(&child.attempt_id))
                    );
                }
                "recovery" => {
                    tree.recovery_owed.insert(child.agent_id.clone());
                }
                "pending" => {
                    tree.record_loss_continuation(&child.agent_id, 7).unwrap();
                }
                _ => {}
            }
            let sent = tree.send_agent_message("root", &request, None);
            if matches!(blocker, "none" | "spent") {
                assert!(sent.is_ok(), "{sent:?}");
                assert_ne!(
                    tree.node(&child.agent_id).unwrap().attempt_id.as_ref(),
                    Some(&child.attempt_id)
                );
                assert_eq!(
                    tree.attempts()[&child.attempt_id].status,
                    crate::protocol::AttemptStatus::Cancelled
                );
            } else {
                assert_eq!(
                    sent.unwrap_err().code,
                    TopologyErrorCode::FollowupNotIdle,
                    "{blocker}"
                );
                assert_eq!(
                    tree.node(&child.agent_id).unwrap().attempt_id.as_ref(),
                    Some(&child.attempt_id)
                );
            }
        }
    }

    #[test]
    fn recovery_consent_is_parent_only_and_bound_to_the_current_attempt() {
        let (_directory, _vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let peer = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let mut request = mail(&child.agent_id, "inspect and continue");
        request.purpose = Some(MessagePurpose::Followup);
        request.acknowledge_recovery = Some(child.attempt_id.clone());
        assert_eq!(
            tree.parent_followup_recovery("root", &request, None)
                .unwrap(),
            Some(child.attempt_id.clone())
        );
        assert_eq!(
            tree.parent_followup_recovery(&peer.agent_id, &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        request.acknowledge_recovery = Some(peer.attempt_id);
        assert_eq!(
            tree.parent_followup_recovery("root", &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::AttemptMismatch
        );
        assert_eq!(
            tree.node(&child.agent_id).unwrap().attempt_id,
            Some(child.attempt_id)
        );
    }

    #[test]
    fn a_grandchild_cannot_acknowledge_recovery_for_its_parent_despite_message_permission() {
        let (_directory, _vcs, mut tree) = fake_tree();
        tree.grants_mut_for_test("root").max_depth = 3;
        crate::tree::test_support::pin(
            &mut tree,
            "orchestrate",
            "test-account",
            "selected-tuple",
            None,
        );
        let parent = tree
            .spawn_agent(
                "root",
                &crate::protocol::SpawnAgentRequest::test_orchestrate(),
                None,
            )
            .unwrap();
        let child = tree
            .spawn_agent(&parent.agent_id, &spawn_req(), None)
            .unwrap();
        assert!(
            tree.grant(&child.agent_id)
                .unwrap()
                .message_peers
                .contains(&parent.agent_id)
        );
        let mut request = mail(&parent.agent_id, "inspect and continue");
        request.purpose = Some(MessagePurpose::Followup);
        request.acknowledge_recovery = Some(parent.attempt_id.clone());
        assert_eq!(
            tree.parent_followup_recovery(&child.agent_id, &request, None)
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        assert_eq!(
            tree.node(&parent.agent_id).unwrap().attempt_id,
            Some(parent.attempt_id)
        );
    }

    #[test]
    fn acknowledged_followup_replay_does_not_recover_or_start_the_replacement_again() {
        let (_directory, _vcs, mut tree) = fake_tree();
        let child = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        let peer = tree.spawn_agent("root", &spawn_req(), None).unwrap();
        tree.record_turn_completed(&child.agent_id).unwrap();
        tree.settle_for_test();
        let mut request = mail(&child.agent_id, "fresh work");
        request.purpose = Some(MessagePurpose::Followup);
        request.acknowledge_recovery = Some(child.attempt_id.clone());
        let output = tree
            .send_agent_message("root", &request, Some("acknowledged-followup"))
            .unwrap();
        let replacement = tree.node(&child.agent_id).unwrap().attempt_id.clone();
        assert_ne!(replacement.as_ref(), Some(&child.attempt_id));
        assert!(output.recovery_warning.is_some());
        let mut tree = ProductAgentTree::reopen(tree.to_persisted());
        assert_eq!(
            tree.parent_followup_recovery("root", &request, Some("acknowledged-followup"))
                .unwrap(),
            None
        );
        assert_eq!(
            tree.send_agent_message("root", &request, Some("acknowledged-followup"))
                .unwrap(),
            output
        );
        assert_eq!(
            tree.send_agent_message(&peer.agent_id, &request, Some("acknowledged-followup"))
                .unwrap_err()
                .code,
            TopologyErrorCode::GrantDenied
        );
        let mut changed = request.clone();
        changed.message = "different task".into();
        assert_eq!(
            tree.send_agent_message("root", &changed, Some("acknowledged-followup"))
                .unwrap_err()
                .code,
            TopologyErrorCode::OperationRecoveryRequired
        );
        assert_eq!(
            tree.send_agent_message("root", &request, Some("stale-consent"))
                .unwrap_err()
                .code,
            TopologyErrorCode::AttemptMismatch
        );
        assert_eq!(tree.node(&child.agent_id).unwrap().attempt_id, replacement);
    }
}
