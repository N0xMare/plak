//! Stop durable work without opening a driver or admitting a continuation.

use crate::agent::ProductStateRoots;
use crate::agent_tree::{InterruptCause, ProductAgentTree, acquire_custody, release_custody_hold};
use crate::error::{CliFailure, CliResult};
use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    RootInterrupt, RootInterruptCause, RootLossIntent, RootStopAnchor,
};
use plakous::sessions::SessionOperationId;

/// Stops a dormant session under its tree custody and a lease-only root
/// head. No model, driver, attachment or continuation is constructed.
///
/// # Errors
/// Another owner holds the session, a durable write fails, or recorded
/// runner effects have not settled. A refusal never claims termination.
pub async fn stop_dormant_session(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &mut SessionRecord,
) -> CliResult<()> {
    let tree_store = roots.tree_store()?;
    let custody =
        acquire_custody(tree_store.custody_port(), &session.node.tree_id).map_err(|error| {
            match error {
                crate::agent_tree::CustodyError::HostedElsewhere(refusal) => {
                    CliFailure::hosted_elsewhere(refusal.to_string())
                }
                error => CliFailure::new(error.to_string()),
            }
        })?;
    let result = stop(roots, store, session).await;
    let released = release_custody_hold(tree_store.custody_port(), custody)
        .map_err(|error| CliFailure::new(error.to_string()));
    result.and(released)
}

async fn stop(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &mut SessionRecord,
) -> CliResult<()> {
    let sessions = roots.runtime_sessions()?;
    let head = if let Some(locator) = session.plakous_locator {
        let line = sessions
            .line_head_of(locator)
            .await
            .map_err(|error| CliFailure::new(error.to_string()))?;
        if line.session_id() != locator.session_id() || line.branch_id() != locator.branch_id() {
            return Err(CliFailure::new(
                "the session has a successor; stop its current session instead",
            ));
        }
        Some(
            sessions
                .open_lease_only(
                    &SessionOperationId::new(format!("plak:stop:{}", session.id))
                        .map_err(|error| CliFailure::new(error.to_string()))?,
                    line.line(),
                )
                .await
                .map_err(|error| CliFailure::new(error.to_string()))?,
        )
    } else {
        None
    };
    if let Some(head) = head {
        // A lease-only owner has no agent heartbeat. Keep the operation
        // strictly inside its lease; an unfinished recorded stop is retried
        // under a fresh fence, never answered as terminated.
        let budget = std::time::Duration::from_secs(sessions.lease_period_seconds()) / 2;
        let stopped = tokio::time::timeout(budget, async {
            let intent = record(roots, store, session).await?;
            roots.finish_dormant_root_stop(&head, &intent).await
        })
        .await
        .unwrap_or_else(|_| {
            Err(CliFailure::new(
                "the stop remains pending; runner effects did not settle within the lease budget",
            ))
        });
        let released = head
            .release()
            .await
            .map_err(|error| CliFailure::new(error.to_string()));
        stopped?;
        released?;
    } else {
        record(roots, store, session).await?;
    }
    Ok(())
}

async fn record(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &mut SessionRecord,
) -> CliResult<String> {
    if !session.child_wake_suppressed {
        store
            .write_host_controls(&session.id, true, None)
            .map_err(CliFailure::from)?;
    }
    let tree_store = roots.tree_store()?;
    if let Some(persisted) = tree_store
        .load(&session.node.tree_id)
        .map_err(|error| CliFailure::new(error.to_string()))?
    {
        let mut tree = ProductAgentTree::reopen(persisted);
        let interrupted = tree
            .interrupt_running_descendants_for(InterruptCause::Stopped)
            .map_err(|error| CliFailure::new(error.message))?;
        if !interrupted.is_empty() {
            let revision = tree_store
                .save_in_custody(&tree)
                .map_err(|error| CliFailure::new(error.to_string()))?;
            tree.acknowledge_revision(revision);
        }
    }
    roots
        .finish_child_process_stops(&session.node.tree_id)
        .await?;
    *session = store.load(&session.id).map_err(CliFailure::new)?;
    if !session.pending_queue.is_empty()
        && session.queue_drain_state != crate::sessions::QueueDrainState::RestoredPaused
    {
        session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
        store.save(session).map_err(CliFailure::from)?;
    }
    super::root_loss::stop_root_continuation(store, session, RootInterruptCause::Stopped)
        .map_err(CliFailure::new)?;
    root_stop_intent(roots, store, session).await
}

async fn root_stop_intent(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> CliResult<String> {
    let intents = store.load_intents(&session.id).map_err(CliFailure::new)?;
    let views = store.load_views(&session.id).map_err(CliFailure::new)?;
    let stopped = intents
        .root_interrupts
        .iter()
        .max_by_key(|intent| (intent.anchor, intent.ordinal))
        .filter(|intent| intent.value.cause == RootInterruptCause::Stopped)
        .filter(|stop| {
            !intents.submissions.iter().any(|submission| {
                intents
                    .submission_anchors
                    .get(&submission.command_id)
                    .is_some_and(|anchor| *anchor >= stop.anchor)
                    && views.as_ref().is_some_and(|views| {
                        matches!(
                            views.state.deliveries.get(&submission.command_id),
                            Some(crate::sessions::projector::Delivery::Accepted)
                        )
                    })
            })
        });
    let ordinal = if let Some(stopped) = stopped {
        stopped.ordinal
    } else {
        let cursor = store
            .load_views(&session.id)
            .map_err(CliFailure::new)?
            .map_or(0, |views| views.cursor.get());
        let sessions = roots.runtime_sessions()?;
        let turn = if let Some(scan) =
            super::root_loss::loss_scan(store, session).map_err(CliFailure::new)?
        {
            match super::root_loss::handed_off_root_turn(&sessions, scan)
                .await
                .map_err(CliFailure::new)?
            {
                Some(turn) => Some(turn),
                None => super::root_loss::lost_root_turn(&sessions, scan)
                    .await
                    .map_err(CliFailure::new)?,
            }
        } else {
            None
        };
        let anchor = turn.map_or_else(
            || RootStopAnchor::Command {
                command_id: format!("plak:stop:{}:{cursor}", session.id),
            },
            |turn| RootStopAnchor::Turn {
                turn_id: turn.turn_id,
            },
        );
        store
            .write_root_loss(
                &session.id,
                RootLossIntent::Interrupt(RootInterrupt {
                    anchor,
                    cause: RootInterruptCause::Stopped,
                }),
            )
            .map_err(CliFailure::from)?;
        store
            .load_intents(&session.id)
            .map_err(CliFailure::new)?
            .root_interrupts
            .last()
            .ok_or_else(|| CliFailure::new("the stop record was not readable"))?
            .ordinal
    };
    Ok(format!("plak:stop:{}:{ordinal}", session.id))
}
