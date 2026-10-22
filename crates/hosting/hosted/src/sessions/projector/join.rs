//! The one product-to-tree join that survives the feed (spec 02 §2.6,
//! chapter 03 §14). Defined here; the host wiring and the tree implement it.

use super::intents::ProductIntents;
use super::views::SessionViews;

/// The tree side of the product's pending root commands.
///
/// After open has applied the feed to its tail, a root command submission
/// without a fact was not recorded. The product names those command ids; the
/// tree keeps the result-mail batches bound to them (they are resubmitted
/// under the same identity) and withdraws every other unconsumed batch of the
/// session with its mail unread.
pub trait ProductTreeJoin: Send + Sync {
    /// Joins the pending root command ids of product session `session_id`.
    ///
    /// # Errors
    ///
    /// Returns the tree store's refusal.
    fn join_pending_root_commands(
        &self,
        session_id: &str,
        pending_command_ids: &[String],
    ) -> Result<(), String>;
}

/// The command ids of the submissions the views hold no fact for, in
/// submission order: what the product names to the tree after open.
#[must_use]
pub fn pending_command_ids(views: &SessionViews, intents: &ProductIntents) -> Vec<String> {
    intents
        .submissions
        .iter()
        .filter(|submission| {
            !intents.submission_consumed(&submission.command_id, views.state.retained_from)
                && !views.state.deliveries.contains_key(&submission.command_id)
        })
        .map(|submission| submission.command_id.clone())
        .collect()
}

/// The operation ids of the workflow requests the views hold no fact for, in
/// request order: a request that still names its source checkpoint.
#[must_use]
pub fn pending_workflow_operation_ids(
    views: &SessionViews,
    intents: &ProductIntents,
) -> Vec<String> {
    intents
        .workflow_requests
        .iter()
        .filter(|request| {
            !intents.workflow_consumed(&request.operation_id, views.state.retained_from)
                && !views.state.workflows.contains_key(&request.operation_id)
        })
        .map(|request| request.operation_id.clone())
        .collect()
}

/// The notice intents written atomically when open withdraws unadmitted
/// workflow requests. Admitted or refused requests are left to the feed.
#[must_use]
pub fn workflow_withdrawals(
    views: &SessionViews,
    intents: &ProductIntents,
) -> Vec<(String, super::OperatorNotice)> {
    let anchor = views.cursor.get();
    let mut ordinal = intents
        .notices
        .iter()
        .filter(|notice| notice.anchor == anchor)
        .map(|notice| notice.ordinal.saturating_add(1))
        .max()
        .unwrap_or(0);
    pending_workflow_operation_ids(views, intents)
        .into_iter()
        .filter_map(|id| {
            let request = intents.workflow_request(&id)?;
            let notice = super::OperatorNotice {
                notice_id: format!("withdrawn-workflow:{id}"),
                anchor,
                ordinal,
                row: crate::sessions::TranscriptRow::info(
                    crate::sessions::NoticeScope::Recovery,
                    request.withdrawal_note(),
                ),
            };
            ordinal = ordinal.saturating_add(1);
            Some((id, notice))
        })
        .collect()
}

/// Joins a successor's creation fact to its source's immutable clear request.
/// The store applies this consequence in the target projector transaction.
#[must_use]
pub fn clear_completion(
    request: &super::WorkflowRequest,
    target: &crate::sessions::SessionRecord,
    views: &super::SessionViewState,
) -> Option<super::WorkflowCompletion> {
    if request.kind != crate::sessions::SessionWorkflowKind::Clear
        || target.id != request.successor_product_id()
        || target.source_workflow_operation_id.as_deref() != Some(&request.operation_id)
        || views.created_operation_id.as_deref() != Some(&target.create_operation_id)
    {
        return None;
    }
    let create = crate::sessions::clear_create_operation_id(&request.operation_id).ok()?;
    if target.create_operation_id != create {
        return None;
    }
    let operation = plakous::sessions::SessionOperationId::new(create).ok()?;
    let genesis = plakous::sessions::AgentSessions::genesis_locator_for(&operation).ok()?;
    if views.runtime_session_id != Some(genesis.session_id()) {
        return None;
    }
    Some(super::WorkflowCompletion {
        admitted_at: Some(1),
        settled: Some(super::WorkflowSettlement::Published {
            target: plakous::sessions::feed::FeedLocator {
                session_id: genesis.session_id(),
                branch_id: genesis.branch_id(),
                checkpoint_id: genesis.checkpoint_id(),
            },
        }),
    })
}
