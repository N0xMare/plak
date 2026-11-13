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
    /// The continuation turn started.
    Started,
    /// The child has no continuation held or pending after a loss.
    NotHeld,
    /// Another owner holds the child now; nothing was recorded.
    Busy,
    /// Consent is recorded, or could not be; the turn did not start. A
    /// recorded continuation is resubmitted by the next run.
    Deferred(String),
    /// The runtime admitted the continuation's Start, but a close or an
    /// interrupt ended the hold before its turn bound: nothing is kept.
    Ended(String),
}

impl LossResume {
    /// A continuation Start that failed: one whose hold ended after its
    /// admission is not kept; any other is deferred.
    fn not_started(failure: ChildStartFailure) -> Self {
        match failure {
            ChildStartFailure::HoldEndedAfterAdmission(reason) => Self::Ended(reason),
            failure => Self::Deferred(String::from(failure)),
        }
    }
}

/// The instruction of a continuation turn, by what stopped the node.
fn continuation_instruction(cause: crate::agent_tree::ContinuationCause) -> String {
    if cause == crate::agent_tree::ContinuationCause::Handoff {
        crate::child_prompts::PLAK_CHILD_HANDOFF_RESUME_TEMPLATE
    } else {
        crate::child_prompts::PLAK_CHILD_LOSS_RESUME_TEMPLATE
    }
    .to_owned()
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
    // A turn the open restored is settled as lost and the decision taken
    // again on it, once, in this same pass.
    for _ in 0..2 {
        match resume_claimed(parent, runtime, agent_id).await {
            Resumed::Settled(resume) => return resume,
            Resumed::Restored => {}
        }
    }
    LossResume::Deferred(
        "the child's lost turn was settled twice; its continuation waits".to_owned(),
    )
}

/// One try of [`resume_after_loss`] for a child its caller holds.
enum Resumed {
    Settled(LossResume),
    /// The open restored a turn the lost owner left; it is settled as lost,
    /// so the continuation is decided again.
    Restored,
}

/// The durable consent of one continuation: the command id, the child's
/// session locator and path, and the cause. `Ok(None)` when the child holds
/// no continuation. A refused publication rolls the decision back: nothing
/// is dispatched on consent that is not durable.
async fn record_consent(
    runtime: &ProductTopologyRuntime,
    agent_id: &str,
) -> Result<Option<(String, String, String, crate::agent_tree::ContinuationCause)>, String> {
    let decided_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    let id = agent_id.to_owned();
    runtime
        .mutate_then_emit(false, move |tree| {
            let Some(cause) = tree
                .loss_continuation(&id)
                .filter(|continuation| continuation.state != LossContinuationState::Running)
                .map(|continuation| continuation.cause)
            else {
                return Ok(None);
            };
            let command_id = tree
                .record_loss_continuation(&id, decided_at_ms)
                .map_err(|error| error.message)?;
            let node = tree.node(&id).map_err(|error| error.message)?;
            let Some(locator) = node.session_locator.clone() else {
                return Err("the child has no session to continue".to_owned());
            };
            let path = node.path.display().to_string();
            Ok(Some((command_id, locator, path, cause)))
        })
        .await
}

async fn resume_claimed(
    parent: &SpawnParent,
    runtime: &Arc<ProductTopologyRuntime>,
    agent_id: &str,
) -> Resumed {
    let (command_id, locator, path, cause) = match record_consent(runtime, agent_id).await {
        Ok(Some(recorded)) => recorded,
        Ok(None) => {
            withdraw_ended_continuation(runtime, agent_id).await;
            return Resumed::Settled(LossResume::NotHeld);
        }
        Err(error) => return Resumed::Settled(LossResume::Deferred(error)),
    };
    if !crate::agent::placement::ready(runtime.driver(), agent_id) {
        return Resumed::Settled(LossResume::Deferred(
            "child placement is unavailable".into(),
        ));
    }
    let Ok(command) = CommandId::new(command_id.clone()) else {
        return Resumed::Settled(LossResume::Deferred(
            "the continuation command id is invalid".to_owned(),
        ));
    };
    // The open projects nothing before it is known whether it restored a
    // turn: a restored turn's acceptance must fold after its re-mark.
    let reopened = match reopen_spawned_child(
        parent,
        agent_id,
        &path,
        &locator,
        "loss-resume",
        ChildReopenMode::TAKE,
    )
    .await
    {
        Ok(reopened) => reopened,
        Err(error) => return Resumed::Settled(LossResume::Deferred(error.message)),
    };
    match reopened {
        // A turn the open restored (this continuation, or a Start the lost
        // process admitted before binding it) is lost again, never rejoined:
        // it is cancelled and the child is marked owner-lost on it, so its
        // continuation is decided anew on that turn.
        ChildReopen::Attached {
            session,
            recovered: Some(recovered),
        } => {
            super::reopen::mark_restored_lost(runtime, agent_id, *session, recovered).await;
            Resumed::Restored
        }
        reopened @ ChildReopen::Attached { .. } => {
            let reconciled = super::reopen::reconcile_reopened_child_results(
                runtime,
                agent_id,
                reopened,
                ChildReopenMode::OWNER,
            )
            .await;
            let session = match reconciled {
                Ok(ChildReopen::Attached { session, .. }) => session,
                Ok(ChildReopen::RecoveryRequired(_)) => {
                    return Resumed::Settled(LossResume::Deferred(
                        "the child's session is recovery-required; its continuation waits"
                            .to_owned(),
                    ));
                }
                Err(error) => return Resumed::Settled(LossResume::Deferred(error)),
            };
            #[cfg(test)]
            crate::agent::park::hooks::before_loss_continuation_reserve(agent_id).await;
            Resumed::Settled(
                match super::turns::start_child_turn_as(
                    runtime,
                    agent_id,
                    *session,
                    command,
                    ChildCommand::fresh(continuation_instruction(cause), CommandKind::Resume),
                    StartAdmission::LossContinuation(command_id),
                )
                .await
                {
                    Ok(_) => LossResume::Started,
                    Err(ChildStartFailure::Refused(reason))
                        if super::turns::hold_ended(&reason) =>
                    {
                        withdraw_ended_continuation(runtime, agent_id).await;
                        LossResume::NotHeld
                    }
                    Err(failure) => LossResume::not_started(failure),
                },
            )
        }
        ChildReopen::RecoveryRequired(_) => Resumed::Settled(LossResume::Deferred(
            "the child's session is recovery-required; its continuation waits".to_owned(),
        )),
    }
}

/// Withdraws a continuation command a hold that ended left prepared, so
/// its batch returns to unread and the child takes commands again.
async fn withdraw_ended_continuation(runtime: &ProductTopologyRuntime, agent_id: &str) {
    crate::agent::ledger::withdraw_ended_continuation(runtime.driver(), agent_id).await;
}

/// The continuation pass a recovery run ends with: each recorded but
/// unadmitted continuation is resubmitted, each one a planned handoff left
/// is resumed by itself, and when the attached root continues after a loss
/// (`resume_after_loss = "continue"`) each held one is resumed, once.
pub(in crate::agent) async fn continue_after_loss(
    parent: &SpawnParent,
    runtime: &Arc<ProductTopologyRuntime>,
) {
    if runtime
        .wait_step_release(tokio::time::Instant::now() + super::SPAWN_MATERIALIZE_TIMEOUT)
        .await
        .is_err()
    {
        runtime.defer_step_recovery();
        return;
    }
    if runtime.is_closed() {
        return;
    }
    let continues = runtime.continues_after_loss();
    let Ok(continuations) = runtime.read(crate::agent_tree::ProductAgentTree::loss_continuations)
    else {
        return;
    };
    for (agent_id, continuation) in continuations {
        let due = match continuation.state {
            LossContinuationState::Pending => true,
            LossContinuationState::Held => {
                continues || continuation.cause == crate::agent_tree::ContinuationCause::Handoff
            }
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
