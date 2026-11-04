//! The root's continuation after its host was lost (spec 03 §14.1).
//!
//! A root turn a lost host left running is closed by the next exact open,
//! whose terminal is an abandonment with cause `process_lost`. The root
//! records that loss as a product-store root interrupt with cause
//! `owner_lost`, anchored on the lost turn. Its continuation is held until
//! the operator's Resume (or `resume_after_loss = "continue"`) records one
//! decision naming that turn and runs one new host-instruction turn under
//! the decision's command id. Nothing records the held state: a root turn
//! admitted after the loss ends it, and a decision whose command was
//! admitted is no longer pending.
//!
//! The loss is found in the runtime feed, not in a view a projection may
//! already have moved past: every open scans the feed from the root's last
//! accepted submission, so a crash anywhere between the closure and the
//! interrupt's write leaves the next open the same evidence.

use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{
    FeedCancelCause, FeedCommandKind, FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq,
    FeedTurnOutcome,
};

use crate::session_store::SessionRecordStore;
use crate::sessions::SessionRecord;
use crate::sessions::projector::{
    Delivery, ProductIntents, RootContinuation, RootContinuationCause, RootContinuationDecision,
    RootInterrupt, RootInterruptCause, RootLossIntent, RootStopAnchor, SessionViews,
};

/// Prefix of the root's continuation command identity after a loss.
pub(crate) const ROOT_LOSS_RESUME_PREFIX: &str = "plak:resume-loss:root:";

/// The host instruction a root continuation after a loss runs.
pub(crate) const ROOT_LOSS_RESUME_TEXT: &str = "Your previous turn was interrupted because the host running it was lost. Continue the current task from the last completed step: check what already happened before you act, and do not repeat completed actions.";

/// Where the root's continuation after a loss stands.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RootLossState {
    /// No decision yet: the root waits for the operator's Resume.
    Held,
    /// Consent is recorded under this command; its turn was not admitted.
    Pending(String),
    /// Its continuation turn is running.
    Running,
}

/// The root's latest loss.
struct Loss {
    /// What the decision's command id is derived from.
    identity: String,
    /// The applied cursor when the loss was recorded: a root submission
    /// written at or after it is a later turn.
    cursor: u64,
    cause: RootContinuationCause,
    decided: Option<String>,
    /// The stop anchor its decision names.
    anchor: RootStopAnchor,
}

/// Where an open's scan for a lost root turn starts: the runtime session
/// the views follow, and the earlier of their cursor and the anchor of the
/// root's latest accepted submission. Read before the open projects
/// anything; a projection, here or in another process, never moves either
/// past a lost turn's terminal, since the lost turn's submission is older.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LossScan {
    session: plakous::model::SessionId,
    after: FeedSeq,
}

impl LossScan {
    /// The runtime session the scan reads.
    pub(crate) const fn session(self) -> plakous::model::SessionId {
        self.session
    }
}

/// A root turn the feed shows closed by a new owner after its process was
/// lost, with no root turn admitted after it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LostRootTurn {
    pub(crate) turn_id: String,
    /// The sequence of the record that carried its terminal.
    pub(crate) terminal_seq: u64,
}

/// Whether `command_id` is a root continuation after a loss.
pub(crate) fn is_root_loss_command(command_id: &str) -> bool {
    command_id.starts_with(ROOT_LOSS_RESUME_PREFIX)
}

fn command_id(identity: &str) -> String {
    let digest = plakous_workspace::occupancy_vcs::digest_text(identity);
    format!("{ROOT_LOSS_RESUME_PREFIX}{}", &digest[..32])
}

fn latest_loss(intents: &ProductIntents) -> Option<Loss> {
    let latest = intents
        .root_interrupts
        .iter()
        .max_by_key(|intent| (intent.anchor, intent.ordinal))?;
    let cause = match latest.value.cause {
        RootInterruptCause::OwnerLost => RootContinuationCause::OwnerLost,
        RootInterruptCause::Handoff => RootContinuationCause::Handoff,
        RootInterruptCause::Operator | RootInterruptCause::Stopped => return None,
    };
    let decided = intents
        .root_continuations
        .iter()
        .filter(|decision| decision.value.anchor == latest.value.anchor)
        .find_map(|decision| match &decision.value.decision {
            RootContinuationDecision::NewTurn { command_id, .. } => Some(command_id.clone()),
            _ => None,
        });
    Some(Loss {
        identity: format!("{}:{}", latest.anchor, latest.ordinal),
        cursor: latest.anchor,
        cause,
        decided,
        anchor: latest.value.anchor.clone(),
    })
}

/// The root's continuation after its latest loss, if it has one.
///
/// # Errors
/// A product store read refused.
pub(crate) fn root_loss_state(
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> Result<Option<RootLossState>, String> {
    let intents = store.load_intents(&session.id)?;
    let Some(loss) = latest_loss(&intents) else {
        return Ok(None);
    };
    let views = store
        .load_views(&session.id)?
        .unwrap_or_else(SessionViews::empty);
    Ok(state(&loss, &intents, &views))
}

fn state(loss: &Loss, intents: &ProductIntents, views: &SessionViews) -> Option<RootLossState> {
    if let Some(command) = &loss.decided {
        return match views.state.deliveries.get(command) {
            None => Some(RootLossState::Pending(command.clone())),
            Some(Delivery::Accepted)
                if views
                    .state
                    .open_turns
                    .values()
                    .any(|turn| &turn.command_id == command) =>
            {
                Some(RootLossState::Running)
            }
            Some(_) => None,
        };
    }
    // Any root turn admitted since the loss ends the hold: a prompt, a
    // follow-up or a host wake. The loss is recorded at a cursor past the
    // lost turn's terminal, so the lost turn's own submission is older.
    let later = intents.submissions.iter().any(|submission| {
        intents
            .submission_anchors
            .get(&submission.command_id)
            .is_some_and(|anchor| *anchor >= loss.cursor)
            && matches!(
                views.state.deliveries.get(&submission.command_id),
                Some(Delivery::Accepted)
            )
    });
    (!later).then_some(RootLossState::Held)
}

/// Where this open's scan for a lost root turn starts; `None` for a
/// session with no projected runtime yet.
///
/// # Errors
/// A product store read refused.
pub(crate) fn loss_scan(
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> Result<Option<LossScan>, String> {
    let Some(views) = store.load_views(&session.id)? else {
        return Ok(None);
    };
    Ok(scan_start(&views, &store.load_intents(&session.id)?))
}

fn scan_start(views: &SessionViews, intents: &ProductIntents) -> Option<LossScan> {
    let runtime_session = views.state.runtime_session_id?;
    let accepted = intents
        .submissions
        .iter()
        .filter(|submission| {
            matches!(
                views.state.deliveries.get(&submission.command_id),
                Some(Delivery::Accepted)
            )
        })
        .filter_map(|submission| intents.submission_anchors.get(&submission.command_id))
        .max()
        .copied();
    let cursor = views.cursor.get();
    Some(LossScan {
        session: runtime_session,
        after: FeedSeq::new(accepted.map_or(cursor, |anchor| anchor.min(cursor))),
    })
}

/// The root turn a lost host left, if the feed after `scan` ends with one:
/// its terminal is an abandonment with cause `process_lost` and no root
/// turn was admitted after it. A fenced turn a live owner ended
/// recovery-required is not a loss of its owner. A scan start below the
/// feed's retention restarts at the retained start, which is sound because
/// only the last root terminal matters; a lost turn whose terminal is itself
/// no longer retained is not found.
///
/// # Errors
/// A feed read refused.
pub(crate) async fn lost_root_turn(
    sessions: &AgentSessions,
    scan: LossScan,
) -> Result<Option<LostRootTurn>, String> {
    let mut cursor = scan.after;
    let mut restarted = false;
    let mut lost = None;
    loop {
        let page = sessions
            .feed_page(FeedPageRequest::after(FeedCursor::new(
                scan.session,
                cursor,
            )))
            .await
            .map_err(|error| error.to_string())?;
        let records = match page {
            FeedPage::Records { records, .. } => records,
            FeedPage::BehindRetention { retained_from, .. } if !restarted => {
                restarted = true;
                lost = None;
                cursor = FeedSeq::new(retained_from.get().saturating_sub(1));
                continue;
            }
            _ => return Ok(None),
        };
        if records.is_empty() {
            return Ok(lost);
        }
        for record in records {
            cursor = record.header().seq;
            for fact in record.facts() {
                fold_lost_turn(&mut lost, cursor.get(), fact);
            }
        }
    }
}

/// One feed fact's effect on the scan's candidate: a root admission clears
/// it, and a root terminal replaces it, with the turn when the terminal is
/// an abandonment with cause `process_lost`.
fn fold_lost_turn(lost: &mut Option<LostRootTurn>, seq: u64, fact: &FeedFact) {
    match fact {
        FeedFact::CommandAccepted {
            kind: FeedCommandKind::Root { .. },
            ..
        } => *lost = None,
        FeedFact::TurnTerminal {
            turn_id, outcome, ..
        } => {
            *lost = matches!(
                outcome,
                FeedTurnOutcome::Cancelled {
                    cause: FeedCancelCause::Abandoned {
                        cause: plakous::agent::AbandonmentCause::ProcessLost,
                        ..
                    },
                }
            )
            .then(|| LostRootTurn {
                turn_id: turn_id.to_string(),
                terminal_seq: seq,
            });
        }
        _ => {}
    }
}

/// Records the root's owner-lost interrupt for `lost`, once per lost turn,
/// at the applied cursor, and says whether it wrote one. It waits for the
/// views to have applied the lost turn's terminal, so that the turn's own
/// submission is older than the loss; until then the next open records it.
///
/// # Errors
/// A product store read or write refused.
pub(crate) fn record_root_owner_lost(
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
    lost: &LostRootTurn,
) -> Result<bool, String> {
    let anchor = RootStopAnchor::Turn {
        turn_id: lost.turn_id.clone(),
    };
    let intents = store.load_intents(&session.id)?;
    // A reopen of a loss already recorded records nothing again.
    if intents
        .root_interrupts
        .iter()
        .any(|intent| intent.value.anchor == anchor)
    {
        return Ok(false);
    }
    let applied = store
        .load_views(&session.id)?
        .map_or(0, |views| views.cursor.get());
    if applied < lost.terminal_seq {
        return Ok(false);
    }
    store
        .write_root_loss(
            &session.id,
            RootLossIntent::Interrupt(RootInterrupt {
                anchor,
                cause: RootInterruptCause::OwnerLost,
            }),
        )
        .map_err(|error| error.to_string())?;
    Ok(true)
}

/// Records consent to the root's held continuation, before anything is
/// dispatched, and returns its command id. A pending decision is returned
/// as it is. `None` when nothing is held.
///
/// The decision records host generation 1: the root's decision is keyed by
/// the loss it names, and a loss names the one root turn a lost host left,
/// so one decision per loss is already one per host generation. The product
/// store keeps no host generation of its own to record instead.
///
/// # Errors
/// A product store read or write refused.
pub(crate) fn record_root_consent(
    store: &dyn SessionRecordStore,
    session: &SessionRecord,
) -> Result<Option<String>, String> {
    let intents = store.load_intents(&session.id)?;
    let Some(loss) = latest_loss(&intents) else {
        return Ok(None);
    };
    let views = store
        .load_views(&session.id)?
        .unwrap_or_else(SessionViews::empty);
    match state(&loss, &intents, &views) {
        Some(RootLossState::Pending(command)) => return Ok(Some(command)),
        Some(RootLossState::Held) => {}
        Some(RootLossState::Running) | None => return Ok(None),
    }
    let command = command_id(&loss.identity);
    let decided_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        });
    store
        .write_root_loss(
            &session.id,
            RootLossIntent::Continuation(RootContinuation {
                anchor: loss.anchor,
                host_generation: 1,
                cause: loss.cause,
                round: 1,
                decided_at_ms,
                decision: RootContinuationDecision::NewTurn {
                    command_id: command.clone(),
                    withdrawn: Vec::new(),
                },
            }),
        )
        .map_err(|error| error.to_string())?;
    Ok(Some(command))
}

#[cfg(test)]
mod tests;
