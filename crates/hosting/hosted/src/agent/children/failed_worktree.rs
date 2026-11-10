//! Read-only rescue snapshots, separate from adoptable candidate authority.

use crate::agent_tree_protocol::{WorkspaceMode, WorkspaceStatus};
use plakous_workspace::occupancy_vcs::{candidate_ref, digest_text, stage};

use super::{ChildTurnIdentity, TreeDriver};

pub(super) async fn snapshot_failure(
    driver: &TreeDriver,
    identity: &ChildTurnIdentity,
    agent_id: &str,
) -> Option<String> {
    if !crate::agent::placement::ready(driver, agent_id) {
        return None;
    }
    let (ticket, effects) = driver
        .mutate({
            let (driver, identity, agent_id) =
                (driver.clone(), identity.clone(), agent_id.to_owned());
            move |tree| {
                let (driver, identity, agent_id) = (&driver, &identity, agent_id.as_str());
                if !identity.still_owns(tree, agent_id) {
                    return Ok(None);
                }
                let node = tree.node(agent_id)?;
                let Some(workspace) = tree.workspace(&node.workspace_id) else {
                    return Ok(None);
                };
                if workspace.mode == WorkspaceMode::Inherit
                    || workspace.status != WorkspaceStatus::Active
                {
                    return Ok(None);
                }
                let Some(base) = workspace.base_commit.clone() else {
                    return Ok(None);
                };
                let ticket = (
                    tree.vcs_port(),
                    node.path.clone(),
                    digest_text(&tree.tree_id),
                    format!("failed-{}-{}", identity.attempt, identity.turn),
                    base,
                );
                driver.fold_and_commit(tree, |tree| {
                    tree.record_rescue_pin(
                        agent_id,
                        crate::agent_tree_protocol::CandidateId::from_persisted(ticket.3.clone()),
                    )?;
                    Ok(Some(ticket))
                })
            }
        })
        .await
        .ok()?;
    let ticket = ticket.ok()??;
    driver.settle(effects).await.ok()?;
    let (vcs, path, namespace, id, base) = ticket;
    let reference = candidate_ref(&namespace, &id);
    // The seal primitive leaves HEAD, the branch and the real index untouched,
    // and excludes secrets. Its durable rescue owner is not adoptable authority.
    let result = tokio::spawn(stage::unstaged(
        "rescue seal of a failed worktree, owned by its rescue pin",
        async move { vcs.seal(&path, &namespace, &id, &base).await },
    ))
    .await;
    Some(match result {
        Ok(Ok(snapshot)) => format!(
            "; failed worktree snapshot: {reference} (commit {})",
            snapshot.commit
        ),
        Ok(Err(error)) => format!(
            "; failed worktree snapshot unavailable: {} (ref {reference})",
            error.detail
        ),
        Err(_) => "; failed worktree snapshot unavailable: snapshot worker failed".to_owned(),
    })
}

#[cfg(test)]
mod tests;
