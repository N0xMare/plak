//! Internal projection refusals stay typed until the one bounded reread.

use crate::agent_tree::{TopologyError, TreeFeed, TreeProjection, TreeProjectorError};
use crate::agent_tree_protocol::TopologyErrorCode;
use crate::agent_tree_tools::TreeDriver;
use crate::topology_runtime::ProductTopologyRuntime;

pub(crate) async fn retry_tree_feed<R>(
    sessions: &plakous::sessions::AgentSessions,
    mut feed: TreeFeed,
    mut apply: impl FnMut(&TreeFeed) -> Result<R, TreeProjectorError>,
) -> Result<(R, TreeFeed), TopologyError> {
    match apply(&feed) {
        Ok(result) => return Ok((result, feed)),
        Err(TreeProjectorError::RefoldRequired | TreeProjectorError::Gap { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    let session = feed.session_id.parse().map_err(|_| {
        TopologyError::new(
            TopologyErrorCode::OperationRecoveryRequired,
            "feed session id is invalid",
        )
    })?;
    let mut reread = super::read_tree_feed(sessions, session, 0)
        .await
        .map_err(|message| {
            TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, message)
        })?;
    reread.reads.extend_waits_from(&feed.reads);
    feed = reread;
    apply(&feed)
        .map(|result| (result, feed))
        .map_err(TopologyError::from)
}

pub(crate) fn apply_on_driver(
    driver: &TreeDriver,
    feed: &TreeFeed,
    agent: &str,
) -> Result<TreeProjection, TreeProjectorError> {
    let mut refusal = None;
    let (result, effects) = driver.mutate(|tree| {
        if tree.revision != tree.stored_revision {
            driver.save(tree)?;
        }
        driver.fold_and_commit(tree, |tree| {
            feed.apply_checked(tree, agent)
                .inspect_err(|error| refusal = Some(error.clone()))
                .map_err(TopologyError::from)
        })
    })?;
    let result = result.map_err(|error| refusal.unwrap_or(TreeProjectorError::Tree(error)));
    driver.settle(effects)?;
    result
}

pub(crate) fn apply_on_runtime(
    runtime: &ProductTopologyRuntime,
    feed: &TreeFeed,
    agent: &str,
) -> Result<TreeProjection, TreeProjectorError> {
    let mut refusal = None;
    runtime
        .mutate_then_emit(false, |tree| {
            feed.apply_checked(tree, agent)
                .inspect_err(|error| refusal = Some(error.clone()))
                .map_err(|error| error.to_string())
        })
        .map_err(|message| {
            refusal.unwrap_or_else(|| {
                TreeProjectorError::Tree(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    message,
                ))
            })
        })
}
