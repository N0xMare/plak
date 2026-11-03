//! Internal projection refusals stay typed until the one bounded reread.

use std::sync::{Arc, Mutex, PoisonError};

use crate::agent_tree::{TopologyError, TreeFeed, TreeProjection, TreeProjectorError};
use crate::agent_tree_protocol::TopologyErrorCode;
use crate::agent_tree_tools::TreeDriver;
use crate::topology_runtime::ProductTopologyRuntime;

/// Applies `feed`, and once more from the session's first retained record
/// when the tree needs a refold or the feed has a gap. The feed is shared
/// with the tree writer that applies it.
pub(crate) async fn retry_tree_feed<R, A, F>(
    sessions: &plakous::sessions::AgentSessions,
    feed: TreeFeed,
    mut apply: A,
) -> Result<(R, TreeFeed), TopologyError>
where
    A: FnMut(Arc<TreeFeed>) -> F,
    F: Future<Output = Result<R, TreeProjectorError>>,
{
    let feed = Arc::new(feed);
    match apply(Arc::clone(&feed)).await {
        Ok(result) => return Ok((result, Arc::unwrap_or_clone(feed))),
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
        .map_err(|failure| {
            TopologyError::new(TopologyErrorCode::OperationRecoveryRequired, failure)
        })?;
    reread.reads.extend_waits_from(&feed.reads);
    let feed = Arc::new(reread);
    apply(Arc::clone(&feed))
        .await
        .map(|result| (result, Arc::unwrap_or_clone(feed)))
        .map_err(TopologyError::from)
}

/// Where a tree writer leaves the typed projector refusal its closure saw.
#[derive(Clone, Default)]
pub(crate) struct RefusalSlot(Arc<Mutex<Option<TreeProjectorError>>>);

impl RefusalSlot {
    pub(crate) fn record(&self, error: &TreeProjectorError) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.clone());
    }

    pub(crate) fn take(&self) -> Option<TreeProjectorError> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

pub(crate) async fn apply_on_driver(
    driver: &TreeDriver,
    feed: Arc<TreeFeed>,
    agent: &str,
) -> Result<TreeProjection, TreeProjectorError> {
    let driver = &driver.for_agent(agent);
    let writer = driver.clone();
    let agent_id = agent.to_owned();
    let refusal = RefusalSlot::default();
    let seen = refusal.clone();
    let (result, effects) = driver
        .mutate(move |tree| {
            if tree.revision != tree.stored_revision {
                writer.save(tree)?;
            }
            writer.fold_and_commit(tree, |tree| {
                feed.apply_checked(tree, &agent_id)
                    .inspect_err(|error| seen.record(error))
                    .map_err(TopologyError::from)
            })
        })
        .await?;
    let result = result.map_err(|error| refusal.take().unwrap_or(TreeProjectorError::Tree(error)));
    driver.settle(effects).await?;
    result
}

pub(crate) async fn apply_on_runtime(
    runtime: &ProductTopologyRuntime,
    feed: Arc<TreeFeed>,
    agent: &str,
) -> Result<TreeProjection, TreeProjectorError> {
    let agent_id = agent.to_owned();
    let refusal = RefusalSlot::default();
    let seen = refusal.clone();
    runtime
        .mutate_then_emit(false, move |tree| {
            feed.apply_checked(tree, &agent_id)
                .inspect_err(|error| seen.record(error))
                .map_err(|error| error.to_string())
        })
        .await
        .map_err(|message| {
            refusal.take().unwrap_or_else(|| {
                TreeProjectorError::Tree(TopologyError::new(
                    TopologyErrorCode::OperationRecoveryRequired,
                    message,
                ))
            })
        })
}
