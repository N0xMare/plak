//! Finish source projection from real feed facts without opening a driver.
use crate::agent_tree::{ProductAgentTree, host_move::DormantTree};
use plakous::model::SessionId;
use plakous::sessions::AgentSessions;

/// Projects closed source rows through the actual feed tail before freezing a cut.
/// This host-layer seam grants no execution or effect authority and persists no views.
///
/// # Errors
/// Refuses unavailable/conflicting facts or any effect still owed after projection.
#[doc(hidden)]
pub async fn project_move_tree(
    sessions: &AgentSessions,
    mut tree: ProductAgentTree,
    bindings: &[(String, SessionId)],
) -> Result<DormantTree, String> {
    for (agent, session) in bindings {
        let feed = crate::agent::read_tree_feed(
            sessions,
            *session,
            tree.feed_cursor(&session.to_string()),
        )
        .await
        .map_err(|error| error.to_string())?;
        let (projection, _) =
            crate::agent::feed_projection::retry_tree_feed(sessions, feed, |feed| {
                std::future::ready(feed.apply_checked(&mut tree, agent))
            })
            .await
            .map_err(|error| error.message)?;
        if !projection.conflicts.is_empty() {
            return Err("source tree feed has conflicting command facts".into());
        }
    }
    DormantTree::capture(&tree)
}
