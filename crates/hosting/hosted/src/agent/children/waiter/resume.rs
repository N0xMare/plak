//! A failed child turn's transient resume: its gate and its instruction turn.

use super::{
    Arc, ChildFeedSource, ChildTurnIdentity, CommandKind, FailedTurnWaits, InstructionStartError,
    ResumeGate, ResumeOwner, TreeDriver, TurnResult, TurnResultHandle, TurnResultOutcome,
    end_attempt_work, failed_worktree, finalize_child_failure, mutate_child, persist_or_fault,
    project_turn_end, settle_or_fault, start_instruction_turn, transient,
};

pub(super) fn resume_allowed_in(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    identity.still_owns(tree, agent_id)
        && tree
            .node(agent_id)
            .is_ok_and(|node| node.status == crate::agent_tree_protocol::NodeStatus::Running)
        && tree
            .attempt(agent_id)
            .is_some_and(|attempt| !attempt.status.is_stopped() && attempt.output_json.is_none())
        && !tree.attempt_turns_spent(agent_id)
}

pub(in crate::agent) fn resume_allowed(
    driver: &TreeDriver,
    agent_id: &str,
    identity: &ChildTurnIdentity,
) -> bool {
    driver
        .read(|tree| resume_allowed_in(tree, agent_id, identity))
        .unwrap_or(false)
}

pub(in crate::agent::children) async fn wait_transient_gate(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    gate: ResumeGate,
) -> bool {
    let probe = live
        .lock()
        .await
        .get(agent_id)
        .and_then(crate::topology_runtime::LiveChildHandle::reachability_probe);
    let cancellation = plakous::model::CancellationToken::new();
    transient::wait_gate(
        gate,
        probe,
        &cancellation,
        Some(std::time::Duration::from_secs(1)),
        || {
            let notify = driver
                .read(crate::agent_tree::ProductAgentTree::waiters)
                .ok()?;
            let mut notified = Box::pin(notify.notified_owned());
            notified.as_mut().enable();
            Some(notified)
        },
        || async move {
            resume_allowed(driver, agent_id, identity)
                && live
                    .lock()
                    .await
                    .get(agent_id)
                    .is_some_and(|child| child.turn_id() == identity.turn)
        },
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "Transient child resume revalidates ownership, pending instructions, and canonical turn state before reconnecting the same attempt."
)]
pub(super) async fn try_transient_resume(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    feed: Option<&ChildFeedSource>,
    result: &TurnResult,
    observer: Option<&Arc<dyn crate::agent_tree::TopologyObserver>>,
) -> Option<Result<TurnResultHandle, ()>> {
    let TurnResultOutcome::Failed(error) = result.outcome() else {
        return None;
    };
    let gate = transient::failure_gate(result)?;
    if !resume_allowed(driver, agent_id, identity) {
        return None;
    }
    let plan = driver
        .read(|tree| {
            let spent = tree
                .transient_resume_count(agent_id, &identity.attempt)
                .ok()?;
            transient::plan(
                gate,
                ResumeOwner::Child {
                    agent: agent_id,
                    attempt: &identity.attempt,
                },
                error.message(),
                spent,
                transient_resume_bound(tree, agent_id)?,
            )
        })
        .ok()
        .flatten()?;
    match crate::agent::placement::wait_ready(driver, live, agent_id, identity).await {
        crate::agent::placement::Admission::Ready => {}
        crate::agent::placement::Admission::Stopped => {
            super::finish_interrupted_turn(driver, live, agent_id, identity, observer).await;
            return Some(Err(()));
        }
        crate::agent::placement::Admission::Drained => return Some(Err(())),
    }
    let notice = failed_worktree::snapshot_failure(driver, identity, agent_id)
        .await
        .unwrap_or_default();
    let (saved, effects) = driver
        .mutate({
            let (driver, agent_id, identity) =
                (driver.clone(), agent_id.to_owned(), identity.clone());
            move |tree| {
                let (driver, agent_id, identity) = (&driver, agent_id.as_str(), &identity);
                if !identity.still_owns(tree, agent_id) {
                    return Err("attempt changed".to_owned());
                }
                tree.record_child_finalization(
                    agent_id,
                    &identity.turn.to_string(),
                    crate::agent_tree::ChildFinalizationDecision::Resume { round: plan.round },
                    false,
                    None,
                )
                .map_err(|error| error.message)?;
                // The notice is the projector's join barrier: never release it before
                // the immutable continuation decision is durably saved.
                tree.record_failed_turn_notice(
                    agent_id,
                    &identity.attempt,
                    &identity.turn.to_string(),
                    notice,
                )
                .map_err(|error| error.message)?;
                if !persist_or_fault(driver, tree, agent_id) {
                    return Err("continuation decision could not be saved".to_owned());
                }
                Ok(())
            }
        })
        .await
        .ok()?;
    settle_or_fault(driver, effects, agent_id).await;
    if saved.is_err() {
        return Some(Err(()));
    }
    project_turn_end(driver, feed, agent_id, identity.turn).await;
    mutate_child(driver, agent_id, {
        let (agent_id, identity) = (agent_id.to_owned(), identity.clone());
        move |tree| {
            let (agent_id, identity) = (agent_id.as_str(), &identity);
            if resume_allowed_in(tree, agent_id, identity) {
                let _ = tree.park_child(agent_id, Vec::new());
                if gate == ResumeGate::Network {
                    tree.mark_transient_wait(agent_id, true);
                }
            }
        }
    })
    .await;
    if !wait_transient_gate(driver, live, agent_id, identity, gate).await {
        mutate_child(driver, agent_id, {
            let agent_id = agent_id.to_owned();
            move |tree| {
                let agent_id = agent_id.as_str();
                tree.unpark_child(agent_id);
            }
        })
        .await;
        return Some(Err(()));
    }
    let started = start_instruction_turn(
        driver,
        live,
        agent_id,
        identity,
        plan.command_id,
        plan.text,
        CommandKind::Resume,
    )
    .await;
    match started {
        Ok(next) => return Some(Ok(next)),
        Err(InstructionStartError::Drained) => return Some(Err(())),
        Err(InstructionStartError::Stopped) => {
            super::finish_interrupted_turn(driver, live, agent_id, identity, observer).await;
            return Some(Err(()));
        }
        Err(InstructionStartError::Failed(_)) => {}
    }
    if resume_allowed(driver, agent_id, identity) {
        let _ = end_attempt_work(driver, live, agent_id, identity).await;
        finalize_child_failure(
            driver,
            identity,
            agent_id,
            crate::agent_tree::turn_outcome::FailedTurnDisposition::Failed {
                kind: "turn_failed",
            },
            error.message().to_owned(),
            FailedTurnWaits::Settle(result),
            observer,
        )
        .await;
    }
    Some(Err(()))
}

/// The transient-resume bound of `agent_id`'s own epoch: a child keeps the
/// bound of the profile it was spawned under.
pub(in crate::agent) fn transient_resume_bound(
    tree: &crate::agent_tree::ProductAgentTree,
    agent_id: &str,
) -> Option<u8> {
    Some(tree.epoch_of(agent_id)?.transient_resumes)
}
