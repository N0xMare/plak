//! Corrective and transient child instruction admission and binding.

use super::{
    Arc, ChildTurnIdentity, CommandId, FailedTurnWaits, TreeDriver, TurnResultHandle,
    finalize_child_failure, persist_or_fault, resume_allowed, settle_or_fault,
};
use crate::driver::ledger::{BatchRule, CommandIntent, CommandKind, CommandLedger as _};

#[allow(
    clippy::too_many_lines,
    reason = "Instruction-turn start preserves pending delivery identity, admission acknowledgement, and runtime release when the attempt cannot bind."
)]
pub(in crate::agent) async fn start_instruction_turn(
    driver: &TreeDriver,
    live: &Arc<tokio::sync::Mutex<crate::topology_runtime::LiveChildTable>>,
    agent_id: &str,
    identity: &ChildTurnIdentity,
    command_id: CommandId,
    prompt: String,
    kind: CommandKind,
) -> Result<TurnResultHandle, String> {
    loop {
        if !crate::agent::placement::wait_ready(driver, live, agent_id, identity).await {
            return Err("child instruction was cancelled before placement admission".into());
        }
        let command_text = command_id.as_str().to_owned();
        let transient = kind == CommandKind::Resume;
        let result_context = crate::agent::ledger::ChildLedger::new(driver, agent_id)
            .prepare(CommandIntent {
                command_id: &command_text,
                kind,
                base_text: &prompt,
                batch: if transient {
                    BatchRule::WakeAll
                } else {
                    BatchRule::Results
                },
            })
            .await
            .map_err(String::from)?;
        let (mut live_map, delivery) =
            crate::agent::live::lock_for_transition(live, agent_id, identity.turn).await?;
        let admissible = if transient {
            resume_allowed(driver, agent_id, identity)
        } else {
            driver
                .read(|tree| {
                    identity.still_owns(tree, agent_id)
                        && tree.node(agent_id).is_ok_and(|node| {
                            node.status == crate::agent_tree_protocol::NodeStatus::Running
                        })
                        && tree.attempt(agent_id).is_some_and(|attempt| {
                            !attempt.status.is_stopped() && attempt.output_json.is_none()
                        })
                })
                .unwrap_or(false)
        };
        if !admissible {
            crate::agent::ledger::ChildLedger::settlement_only(driver)
                .settle(
                    &command_text,
                    crate::driver::ledger::Settlement::Refused { pause: None },
                )
                .await
                .map_err(String::from)?;
            return Err(if transient {
                "transient continuation was cancelled"
            } else {
                "child instruction was cancelled"
            }
            .to_owned());
        }
        let child = live_map
            .get_mut(agent_id)
            .ok_or_else(|| "child live session is missing".to_owned())?;
        if child.turn_id() != identity.turn {
            return Err("child turn has already been replaced".into());
        }
        if !crate::agent::placement::ready(driver, agent_id) {
            drop(live_map);
            continue;
        }
        let result = match child
            .restart(
                driver.clone(),
                command_id.clone(),
                result_context.prompt.clone(),
                delivery,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                drop(live_map);
                if error == crate::agent::live::RESTART_PLACEMENT_UNAVAILABLE {
                    continue;
                }
                if error == crate::agent::live::RESTART_RECONCILIATION_REQUIRED {
                    finalize_child_failure(
                        driver,
                        identity,
                        agent_id,
                        crate::agent_tree::turn_outcome::FailedTurnDisposition::RecoveryRequired,
                        error.clone(),
                        FailedTurnWaits::Keep,
                        None,
                    )
                    .await;
                }
                return Err(error);
            }
        };
        let turn_id = child.turn_id();
        let cursor = crate::agent::live::live_feed_cursor(driver, agent_id)?;
        let read = child.feed(cursor);
        drop(live_map);
        let feed = read.await;
        let (published, effects) = driver
            .mutate({
                let (driver, agent_id, identity) =
                    (driver.clone(), agent_id.to_owned(), identity.clone());
                move |tree| {
                    let (driver, agent_id, identity) = (&driver, agent_id.as_str(), &identity);
                    if !identity.still_owns(tree, agent_id) {
                        return Err("child attempt has already been replaced".into());
                    }
                    if transient {
                        tree.unpark_child(agent_id);
                    }
                    tree.bind_turn_owner(agent_id, turn_id.to_string());
                    // Through the restarted turn's acceptance only: the turn
                    // runs on, and its turn end folds the rest.
                    if let Some(feed) = feed? {
                        crate::agent::feed_projection::through_acceptance(Arc::new(feed), turn_id)
                            .apply(tree, agent_id)
                            .map_err(|error| error.message)?;
                    }
                    // Once Start was admitted the result must retain a waiter even
                    // when persistence refuses this publication. The fault remains
                    // visible and the acknowledged revision is not advanced.
                    persist_or_fault(driver, tree, agent_id);
                    Ok::<_, String>(())
                }
            })
            .await
            .unwrap_or_else(|error| (Err(error.message), crate::agent_tree::Effects::default()));
        if let Err(error) = published {
            settle_or_fault(driver, effects, agent_id).await;
            if let Err(retirement_error) =
                crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true)
                    .await
            {
                return Err(format!("{error}; {retirement_error}"));
            }
            return Err(error);
        }
        if let Err(error) = crate::agent::ledger::ChildLedger::settlement_only(driver)
            .settle(&command_text, crate::driver::ledger::Settlement::Recorded)
            .await
            .map_err(String::from)
        {
            settle_or_fault(driver, effects, agent_id).await;
            if let Err(retirement_error) =
                crate::topology_runtime::retire_live(driver, live, agent_id, Some(turn_id), true)
                    .await
            {
                return Err(format!("{error}; {retirement_error}"));
            }
            return Err(error);
        }
        settle_or_fault(driver, effects, agent_id).await;
        return Ok(result);
    }
}
