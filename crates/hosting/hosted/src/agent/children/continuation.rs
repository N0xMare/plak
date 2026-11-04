//! The continuation service's loss leg for children (spec 03 section 14.1):
//! consent is recorded durably before anything is dispatched, then one new
//! host-instruction turn runs under the decision's own command id. It never
//! replays the lost turn, and a recorded decision whose turn was not
//! admitted is resubmitted under the same id, so a continuation runs once.

use std::sync::Arc;

use plakous::agent::CommandId;

use crate::agent_tree::LossContinuationState;
use crate::driver::ledger::CommandKind;
use crate::topology_runtime::ProductTopologyRuntime;

use super::turns::ChildCommand;
use super::{
    ChildReopen, ChildReopenMode, ChildStartFailure, SpawnParent, StartAdmission,
    reopen_spawned_child,
};

/// What one continuation request did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LossResume {
    /// The continuation turn started (or a recorded one rejoined).
    Started,
    /// The child has no continuation held or pending after a loss.
    NotHeld,
    /// Another owner holds the child now; nothing was recorded.
    Busy,
    /// Consent is recorded, or could not be; the turn did not start. A
    /// recorded continuation is resubmitted by the next run.
    Deferred(String),
}

/// Resumes `agent_id`'s continuation after a loss: records consent (one
/// durable decision for the attempt and the stopped turn) and starts its
/// turn. A pending decision is not recorded again; its command is
/// resubmitted.
async fn resume_after_loss(
    parent: &SpawnParent,
    runtime: &Arc<ProductTopologyRuntime>,
    agent_id: &str,
) -> LossResume {
    let Ok(_claim) = crate::agent::recovery::claim_child(runtime, agent_id).await else {
        return LossResume::Busy;
    };
    let decided_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let id = agent_id.to_owned();
    // A refused publication rolls the decision back: nothing is dispatched
    // on consent that is not durable.
    let recorded = runtime
        .mutate_then_emit(false, move |tree| {
            let held = tree
                .loss_continuation(&id)
                .is_some_and(|continuation| continuation.state != LossContinuationState::Running);
            if !held {
                return Ok(None);
            }
            let command_id = tree
                .record_loss_continuation(&id, decided_at_ms)
                .map_err(|error| error.message)?;
            let node = tree.node(&id).map_err(|error| error.message)?;
            let Some(locator) = node.session_locator.clone() else {
                return Err("the child has no session to continue".to_owned());
            };
            Ok(Some((command_id, locator, node.path.display().to_string())))
        })
        .await;
    let (command_id, locator, path) = match recorded {
        Ok(Some(recorded)) => recorded,
        Ok(None) => {
            withdraw_ended_continuation(runtime, agent_id).await;
            return LossResume::NotHeld;
        }
        Err(error) => return LossResume::Deferred(error),
    };
    let Ok(command) = CommandId::new(command_id.clone()) else {
        return LossResume::Deferred("the continuation command id is invalid".to_owned());
    };
    let reopened = match reopen_spawned_child(
        parent,
        agent_id,
        &path,
        &locator,
        "loss-resume",
        ChildReopenMode::REJOIN,
    )
    .await
    {
        Ok(reopened) => reopened,
        Err(error) => return LossResume::Deferred(error.message),
    };
    match reopened {
        // The continuation (or a Start the lost process journaled) was
        // admitted before its turn bound: that turn rejoins.
        ChildReopen::Attached {
            session,
            recovered: Some(recovered),
        } => match super::reopen::rejoin_recovered_turn(
            runtime,
            agent_id,
            *session,
            recovered.turn_id(),
            recovered.control(),
            recovered.result(),
        )
        .await
        {
            Ok(_) => LossResume::Started,
            Err(failure) => LossResume::Deferred(delivery_failure_text(failure)),
        },
        ChildReopen::Attached { session, .. } => {
            #[cfg(test)]
            crate::agent::park::hooks::before_loss_continuation_reserve(agent_id).await;
            match super::turns::start_child_turn_as(
                runtime,
                agent_id,
                *session,
                command,
                ChildCommand::fresh(
                    crate::child_prompts::PLAK_CHILD_LOSS_RESUME_TEMPLATE.to_owned(),
                    CommandKind::Resume,
                ),
                StartAdmission::LossContinuation(command_id),
            )
            .await
            {
                Ok(_) => LossResume::Started,
                Err(ChildStartFailure::Refused(reason)) if super::turns::hold_ended(&reason) => {
                    withdraw_ended_continuation(runtime, agent_id).await;
                    LossResume::NotHeld
                }
                Err(failure) => LossResume::Deferred(String::from(failure)),
            }
        }
        ChildReopen::RecoveryRequired(_) => LossResume::Deferred(
            "the child's session is recovery-required; its continuation waits".to_owned(),
        ),
    }
}

/// A rejoin refusal as operator text.
fn delivery_failure_text(failure: crate::agent_tree_tools::MailDeliveryFailure) -> String {
    use crate::agent_tree_tools::MailDeliveryFailure;
    match failure {
        MailDeliveryFailure::Refused(reason) | MailDeliveryFailure::RecoveryRequired(reason) => {
            reason.to_string()
        }
        MailDeliveryFailure::TurnEnded => "the child's turn ended before it rejoined".to_owned(),
        MailDeliveryFailure::SlotBusy => {
            "the child's live slot was still starting or retiring".to_owned()
        }
    }
}

/// Withdraws a continuation command a hold that ended left prepared, so
/// its batch returns to unread and the child takes commands again.
async fn withdraw_ended_continuation(runtime: &ProductTopologyRuntime, agent_id: &str) {
    crate::agent::ledger::withdraw_ended_continuation(runtime.driver(), agent_id).await;
}

/// The continuation pass a recovery run ends with: each recorded but
/// unadmitted continuation is resubmitted, and when the attached root
/// continues after a loss (`resume_after_loss = "continue"`) each held one
/// is resumed, once.
pub(in crate::agent) async fn continue_after_loss(
    parent: &SpawnParent,
    runtime: &Arc<ProductTopologyRuntime>,
) {
    let continues = runtime.continues_after_loss();
    let Ok(continuations) = runtime.read(crate::agent_tree::ProductAgentTree::loss_continuations)
    else {
        return;
    };
    for (agent_id, continuation) in continuations {
        let due = match continuation.state {
            LossContinuationState::Pending => true,
            LossContinuationState::Held => continues,
            LossContinuationState::Running => false,
        };
        if due {
            let _ = resume_after_loss(parent, runtime, &agent_id).await;
        }
    }
}

/// The nodes the operator's Resume with no agent tries: every held or
/// pending continuation after a loss.
fn resumable_after_loss(tree: &crate::agent_tree::ProductAgentTree) -> Vec<String> {
    tree.loss_continuations()
        .into_iter()
        .filter(|(_, continuation)| continuation.state != LossContinuationState::Running)
        .map(|(agent_id, _)| agent_id)
        .collect()
}

/// Whether the tree holds any child's continuation after a loss: what the
/// root's one Resume has to resume in its tree.
pub(crate) fn holds_after_loss(runtime: &ProductTopologyRuntime) -> bool {
    runtime
        .read(|tree| !resumable_after_loss(tree).is_empty())
        .unwrap_or(false)
}

/// The operator's Resume: one node's held continuation, or every held one
/// when `agent_id` is `None`. Returns each node tried and what it did.
pub(crate) async fn operator_resume_after_loss(
    runtime: &Arc<ProductTopologyRuntime>,
    agent_id: Option<&str>,
) -> Result<Vec<(String, LossResume)>, String> {
    let parent = crate::agent::build::rejoin_parent(runtime)
        .ok_or_else(|| "no parent session is attached".to_owned())?;
    let targets = match agent_id {
        Some(agent_id) => vec![agent_id.to_owned()],
        None => runtime
            .read(resumable_after_loss)
            .map_err(|error| error.message)?,
    };
    let mut resumed = Vec::new();
    for agent_id in targets {
        let outcome = resume_after_loss(&parent, runtime, &agent_id).await;
        resumed.push((agent_id, outcome));
    }
    Ok(resumed)
}
