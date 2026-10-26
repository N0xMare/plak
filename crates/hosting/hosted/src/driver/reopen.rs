//! Exact reopen facts and bounded retry state shared by hosted roles.

#[cfg(test)]
mod tests;

use std::time::Duration;

use plakous::agent::RecoveredTurnHandle;
use plakous::sessions::{
    AgentSessionsError, AgentSessionsErrorKind, OpenSessionOutcome, SessionLocator,
};

pub(crate) fn classify_opened(outcome: &OpenSessionOutcome) -> Option<RecoveredTurnHandle> {
    match outcome {
        OpenSessionOutcome::RestoredWorking { turn, .. } => Some(turn.clone()),
        _ => None,
    }
}

pub(crate) fn stale_head(error: &AgentSessionsError) -> Option<SessionLocator> {
    (error.kind() == AgentSessionsErrorKind::StaleLocator)
        .then(|| error.current_locator())
        .flatten()
}

#[derive(Default)]
pub(crate) struct ExactRetry {
    pub(crate) adoptions: bool,
    pub(crate) lease_waits: u32,
    until: Option<tokio::time::Instant>,
}

impl ExactRetry {
    pub(crate) fn with_lease_wait(wait: Duration) -> Self {
        Self {
            until: Some(tokio::time::Instant::now() + wait),
            ..Self::default()
        }
    }

    pub(crate) fn adopt(&mut self) -> bool {
        if self.adoptions {
            return false;
        }
        self.adoptions = true;
        true
    }

    pub(crate) fn lease_delay(&mut self, poll: Duration) -> Option<Duration> {
        let remaining = self
            .until?
            .checked_duration_since(tokio::time::Instant::now())?;
        if remaining.is_zero() {
            return None;
        }
        self.lease_waits = self.lease_waits.saturating_add(1);
        Some(poll.min(remaining))
    }
}

pub(crate) async fn terminal_published_between(
    sessions: &plakous::sessions::AgentSessions,
    locator: SessionLocator,
    from: plakous::sessions::feed::FeedSeq,
    to: plakous::sessions::feed::FeedSeq,
) -> Result<bool, AgentSessionsError> {
    use plakous::sessions::feed::{FeedCursor, FeedFact, FeedPage, FeedPageRequest};
    let mut cursor = from;
    while cursor < to {
        let page = sessions
            .feed_page(FeedPageRequest::after(FeedCursor::new(
                locator.session_id(),
                cursor,
            )))
            .await?;
        let FeedPage::Records { records, .. } = page else {
            return Ok(false);
        };
        if records.is_empty() {
            return Ok(false);
        }
        for record in records {
            if record.header().seq > to {
                return Ok(false);
            }
            cursor = record.header().seq;
            if record.header().branch_id == locator.branch_id()
                && record
                    .facts()
                    .iter()
                    .any(|fact| matches!(fact, FeedFact::TurnTerminal { .. }))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Pairs await receipts with committed canonical calls. A nested wait's id is
/// never in model history, so it is never committed on its own.
pub(crate) async fn wait_evidence(
    sessions: &plakous::sessions::AgentSessions,
    locator: SessionLocator,
    ids: Vec<String>,
) -> Result<Vec<(String, bool)>, String> {
    let ids: Vec<_> = ids
        .into_iter()
        .map(|id| {
            let call = uuid::Uuid::parse_str(&id)
                .ok()
                .and_then(|uuid| plakous::model::CallId::try_from(uuid).ok());
            (id, call)
        })
        .collect();
    let evidence: std::collections::BTreeMap<_, _> = sessions
        .committed_calls(locator, ids.iter().filter_map(|(_, call)| *call).collect())
        .await
        .map_err(|error| format!("wait settlement could not read history: {error}"))?
        .into_iter()
        .collect();
    Ok(ids
        .into_iter()
        .map(|(id, call)| {
            let committed = call.is_some_and(|call| evidence.get(&call) == Some(&true));
            (id, committed)
        })
        .collect())
}
