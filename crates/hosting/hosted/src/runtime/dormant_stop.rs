//! Stop durable work without opening a driver or admitting a continuation.

use crate::agent::ProductStateRoots;
use crate::agent_tree::{InterruptCause, ProductAgentTree, acquire_custody, release_custody_hold};
use crate::error::{CliFailure, CliResult};
use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    RootInterrupt, RootInterruptCause, RootLossIntent, RootStopAnchor,
};
use plakous::sessions::{AgentSessionsErrorKind, LeaseOnlyHead, SessionOperationId};

/// Stops new dormant work under tree custody and a lease-only root head.
/// An already-durable pending cancellation needs no new head lease.
/// No model, driver, attachment or continuation is constructed.
///
/// # Errors
/// New cancellation cannot acquire ownership or a durable write fails. Success records
/// cancellation; `ProductStateRoots::runner_pending` distinguishes runner debt
/// from proven settlement. No continuation is admitted to finish a stop.
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

enum StopHead {
    Absent,
    Pending,
    Held(LeaseOnlyHead),
}

async fn stop_head(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> CliResult<StopHead> {
    let sessions = roots.runtime_sessions()?;
    if let Some(locator) = session.plakous_locator {
        let line = sessions
            .line_head_of(locator)
            .await
            .map_err(|error| CliFailure::new(error.to_string()))?;
        if line.session_id() != locator.session_id() || line.branch_id() != locator.branch_id() {
            return Err(CliFailure::new(
                "the session has a successor; stop its current session instead",
            ));
        }
        let reconciling = roots.runner_pending();
        match sessions
            .open_lease_only(
                &SessionOperationId::new(format!("plak:stop:{}", session.id))
                    .map_err(|error| CliFailure::new(error.to_string()))?,
                line.line(),
            )
            .await
        {
            Ok(head) => Ok(StopHead::Held(head)),
            Err(error) => {
                // Repeating an already-durable cancellation must not compete
                // with its recovery lease. A new stop cannot use this path.
                if (reconciling || roots.runner_pending())
                    && matches!(
                        error.kind(),
                        AgentSessionsErrorKind::Ownership | AgentSessionsErrorKind::Busy
                    )
                    && let Some(ordinal) = recorded_stop_ordinal(store, session)?
                    && roots.retain_runner_stop_debt(
                        line.line(),
                        &format!("plak:stop:{}:{ordinal}", session.id),
                    )
                {
                    return Ok(StopHead::Pending);
                }
                Err(CliFailure::new(error.to_string()))
            }
        }
    } else {
        Ok(StopHead::Absent)
    }
}

async fn stop(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &mut SessionRecord,
) -> CliResult<()> {
    let head = match stop_head(roots, store, session).await? {
        StopHead::Pending => return Ok(()),
        StopHead::Absent => {
            record(roots, store, session).await?;
            return Ok(());
        }
        StopHead::Held(head) => head,
    };
    // A lease-only owner has no agent heartbeat. Keep the operation
    // strictly inside its lease; an unfinished recorded stop is retried
    // under a fresh fence, never answered as terminated.
    let budget =
        std::time::Duration::from_secs(roots.runtime_sessions()?.lease_period_seconds()) / 2;
    let intent = match record(roots, store, session).await {
        Ok(intent) => intent,
        Err(error) => {
            let _ = head.release().await;
            return Err(error);
        }
    };
    let stopped = if !roots.runner_pending() {
        tokio::time::timeout(budget, async {
            roots
                .finish_child_process_stops(&session.node.tree_id)
                .await?;
            roots.finish_dormant_root_stop(&head, &intent).await
        })
        .await
        .unwrap_or_else(|_| {
            Err(CliFailure::new(
                "runner stop settlement exceeded its lease budget",
            ))
        })
    } else {
        Err(CliFailure::new("runner stop settlement remains pending"))
    };
    let line = head.line();
    let released = head
        .release()
        .await
        .map_err(|error| CliFailure::new(error.to_string()));
    released?;
    if let Err(error) = stopped {
        if !roots.retain_runner_stop_debt(line, &intent) {
            return Err(error);
        }
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
    *session = store.load(&session.id).map_err(CliFailure::new)?;
    if !session.pending_queue.is_empty()
        && session.queue_drain_state != crate::sessions::QueueDrainState::RestoredPaused
    {
        session.queue_drain_state = crate::sessions::QueueDrainState::RestoredPaused;
        store.save(session).map_err(CliFailure::from)?;
    }
    super::root_loss::stop_root_continuation(store, session, RootInterruptCause::Stopped)
        .map_err(CliFailure::new)?;
    let intent = root_stop_intent(roots, store, session).await?;
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
    Ok(intent)
}

fn recorded_stop_ordinal(
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> CliResult<Option<u32>> {
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
    Ok(stopped.map(|stop| stop.ordinal))
}

async fn root_stop_intent(
    roots: &ProductStateRoots,
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> CliResult<String> {
    let ordinal = if let Some(ordinal) = recorded_stop_ordinal(store, session)? {
        ordinal
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
