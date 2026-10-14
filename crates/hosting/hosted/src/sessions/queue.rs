//! Bounded queue edits and delivery locks belonging to a session record.

use super::{MAX_PENDING_PROMPTS, MAX_PROMPT_BYTES, SessionRecord, new_uuid_v4_string};
use QueueItem as SessionQueueItem;

/// Process-local join of immutable submissions and projected command facts.
/// This is rebuilt on load; it is never persisted with the queue intents.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct QueueCommands(
    Vec<(
        super::projector::Submission,
        Option<super::projector::Delivery>,
    )>,
);

impl QueueCommands {
    #[must_use]
    pub fn pending_for(&self, item: &QueueItem) -> Option<&super::projector::Submission> {
        self.0.iter().find_map(|(submission, delivery)| {
            (submission.queue_item_id == item.id
                && submission.queue_item_revision == item.revision
                && delivery.is_none())
            .then_some(submission)
        })
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        self.0.iter().any(|(_, delivery)| delivery.is_none())
    }

    #[must_use]
    pub fn from_join(
        intents: &super::projector::ProductIntents,
        views: Option<&super::projector::SessionViews>,
    ) -> Self {
        Self(
            intents
                .submissions
                .iter()
                .map(|submission| {
                    let delivery = views.and_then(|views| {
                        if intents
                            .submission_consumed(&submission.command_id, views.state.retained_from)
                        {
                            Some(super::projector::Delivery::Accepted)
                        } else {
                            views.state.deliveries.get(&submission.command_id).cloned()
                        }
                    });
                    (submission.clone(), delivery)
                })
                .collect(),
        )
    }

    #[must_use]
    pub fn is_rejected(&self, item: &QueueItem) -> bool {
        self.0.iter().any(|(submission, delivery)| {
            submission.queue_item_id == item.id
                && submission.queue_item_revision == item.revision
                && matches!(delivery, Some(super::projector::Delivery::Rejected { .. }))
        })
    }

    #[must_use]
    pub fn is_locked(&self, id: &str) -> bool {
        self.0
            .iter()
            .any(|(submission, delivery)| submission.queue_item_id == id && delivery.is_none())
    }

    #[must_use]
    pub fn is_consumed(&self, item: &QueueItem) -> bool {
        self.0.iter().any(|(submission, delivery)| {
            submission.queue_item_id == item.id
                && submission.queue_item_revision == item.revision
                && matches!(
                    delivery,
                    Some(
                        super::projector::Delivery::Accepted | super::projector::Delivery::Consumed
                    )
                )
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueItem {
    pub id: String,
    pub revision: u64,
    pub text: String,
    /// When the operator submitted the prompt to the host, Unix
    /// milliseconds. Edits and moves keep it; the prompt's transcript row
    /// shows it as its send time.
    pub submitted_at_ms: u64,
}

impl QueueItem {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: new_uuid_v4_string(),
            revision: 1,
            text: text.into(),
            submitted_at_ms: plakous::model::now_unix_millis(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueDrainState {
    LiveAutoDrain,
    RestoredPaused,
}

impl QueueDrainState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveAutoDrain => "live_auto_drain",
            Self::RestoredPaused => "restored_paused",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "live_auto_drain" => Some(Self::LiveAutoDrain),
            "restored_paused" => Some(Self::RestoredPaused),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Refusal {
    Empty,
    TooLarge,
    Full,
    Stale,
}

/// Append a follow-up / queued prompt.
pub(crate) fn enqueue(
    pending: &mut Vec<SessionQueueItem>,
    text: String,
) -> Result<SessionQueueItem, Refusal> {
    let t = text.trim();
    if t.is_empty() {
        return Err(Refusal::Empty);
    }
    if t.len() > MAX_PROMPT_BYTES {
        return Err(Refusal::TooLarge);
    }
    if pending.len() >= MAX_PENDING_PROMPTS {
        return Err(Refusal::Full);
    }
    let item = SessionQueueItem::new(t);
    pending.push(item.clone());
    Ok(item)
}

/// Drop the last queued item (LIFO delete — "delete queued").
pub(crate) fn drop_last(pending: &mut Vec<SessionQueueItem>) -> Option<SessionQueueItem> {
    pending.pop()
}

/// Remove exactly the item and revision shown in the last host snapshot.
pub(crate) fn remove(
    pending: &mut Vec<SessionQueueItem>,
    id: &str,
    revision: u64,
) -> Option<SessionQueueItem> {
    let idx = pending
        .iter()
        .position(|q| q.id == id && q.revision == revision)?;
    Some(pending.remove(idx))
}

pub(crate) fn get(
    pending: &[SessionQueueItem],
    id: &str,
    revision: u64,
) -> Option<SessionQueueItem> {
    pending
        .iter()
        .find(|q| q.id == id && q.revision == revision)
        .cloned()
}

pub(crate) fn edit(
    pending: &mut [SessionQueueItem],
    id: &str,
    revision: u64,
    text: &str,
) -> Result<(), Refusal> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Refusal::Empty);
    }
    if text.len() > MAX_PROMPT_BYTES {
        return Err(Refusal::TooLarge);
    }
    let Some(item) = pending
        .iter_mut()
        .find(|q| q.id == id && q.revision == revision)
    else {
        return Err(Refusal::Stale);
    };
    let Some(next_revision) = item.revision.checked_add(1) else {
        return Err(Refusal::Stale);
    };
    text.clone_into(&mut item.text);
    item.revision = next_revision;
    Ok(())
}

pub(crate) fn move_to(
    pending: &mut Vec<SessionQueueItem>,
    id: &str,
    revision: u64,
    new_index: usize,
) -> bool {
    let Some(index) = pending
        .iter()
        .position(|q| q.id == id && q.revision == revision)
    else {
        return false;
    };
    let Some(next_revision) = pending[index].revision.checked_add(1) else {
        return false;
    };
    let mut item = pending.remove(index);
    item.revision = next_revision;
    let target = new_index.min(pending.len());
    pending.insert(target, item);
    true
}

/// Take the next FIFO item to deliver as a prompt.
#[cfg(test)]
pub(crate) fn take_next(pending: &mut Vec<SessionQueueItem>) -> Option<SessionQueueItem> {
    if pending.is_empty() {
        None
    } else {
        Some(pending.remove(0))
    }
}

/// Clone the next FIFO item whose current revision was not rejected.
/// Delivery removes it only after runtime admission is acknowledged.
pub(crate) fn next_drainable(session: &SessionRecord) -> Option<SessionQueueItem> {
    if session.queue_projection_debt {
        return None;
    }
    session
        .pending_queue
        .iter()
        .find(|item| {
            !session.queue_commands.is_rejected(item) && !session.queue_commands.is_consumed(item)
        })
        .cloned()
}

pub(crate) fn queue_item_is_delivery_locked(session: &SessionRecord, id: &str) -> bool {
    session.queue_commands.is_locked(id)
}

pub(crate) fn drop_last_mutable(session: &mut SessionRecord) -> bool {
    let Some(id) = session.pending_queue.last().map(|item| item.id.clone()) else {
        return false;
    };
    !queue_item_is_delivery_locked(session, &id) && drop_last(&mut session.pending_queue).is_some()
}

pub(crate) fn remove_mutable(session: &mut SessionRecord, id: &str, revision: u64) -> bool {
    !queue_item_is_delivery_locked(session, id)
        && remove(&mut session.pending_queue, id, revision).is_some()
}

pub(crate) fn edit_mutable(
    session: &mut SessionRecord,
    id: &str,
    revision: u64,
    text: &str,
) -> Result<(), Refusal> {
    if queue_item_is_delivery_locked(session, id) {
        return Err(Refusal::Stale);
    }
    edit(&mut session.pending_queue, id, revision, text)
}

pub(crate) fn move_mutable(
    session: &mut SessionRecord,
    id: &str,
    revision: u64,
    new_index: usize,
) -> bool {
    !queue_item_is_delivery_locked(session, id)
        && move_to(&mut session.pending_queue, id, revision, new_index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sessions::projector::{
        Delivery, ProductIntents, SessionViews, Submission, SubmissionKind,
    };

    #[test]
    fn queue_decisions_join_submissions_and_command_facts_by_exact_revision() {
        let mut session = SessionRecord::new("/work".into());
        let first = QueueItem::new("first");
        let second = QueueItem::new("second");
        session.pending_queue = vec![first.clone(), second.clone()];
        let intents = ProductIntents {
            submissions: vec![Submission {
                command_id: "queue-first".into(),
                kind: SubmissionKind::RootStart,
                queue_item_id: first.id.clone(),
                queue_item_revision: first.revision,
                target_turn_id: None,
                canonical_command: first.text.clone(),
                submitted_at_ms: first.submitted_at_ms,
            }],
            ..ProductIntents::default()
        };
        let mut views = SessionViews::empty();
        session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
        assert!(queue_item_is_delivery_locked(&session, &first.id));
        assert_eq!(next_drainable(&session), Some(first.clone()));
        session.queue_projection_debt = true;
        assert_eq!(next_drainable(&session), None);
        session.queue_projection_debt = false;
        assert!(!remove_mutable(&mut session, &first.id, first.revision));

        views.state.deliveries.insert(
            "queue-first".into(),
            Delivery::Rejected {
                error: "busy".into(),
            },
        );
        session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
        assert!(!queue_item_is_delivery_locked(&session, &first.id));
        assert_eq!(next_drainable(&session), Some(second.clone()));
        edit_mutable(&mut session, &first.id, first.revision, "edited").unwrap();
        assert_eq!(
            next_drainable(&session),
            Some(session.pending_queue[0].clone())
        );

        session.pending_queue[0] = first.clone();
        views
            .state
            .deliveries
            .insert("queue-first".into(), Delivery::Accepted);
        session.queue_commands = QueueCommands::from_join(&intents, Some(&views));
        assert_eq!(next_drainable(&session), Some(second));
        assert!(session.queue_commands.is_consumed(&first));
        assert!(!session.queue_commands.is_consumed(&QueueItem {
            revision: first.revision + 1,
            ..first.clone()
        }));
        assert_eq!(
            session.queue_commands,
            QueueCommands::from_join(&intents, Some(&views))
        );
    }

    #[test]
    fn edits_and_moves_keep_the_operator_submit_time() {
        let mut pending = Vec::new();
        let first = enqueue(&mut pending, "first".into()).unwrap();
        let second = enqueue(&mut pending, "second".into()).unwrap();
        assert!(first.submitted_at_ms > 0);
        pending[0].submitted_at_ms = 11;
        pending[1].submitted_at_ms = 22;
        edit(&mut pending, &first.id, first.revision, "first, edited").unwrap();
        assert!(move_to(&mut pending, &second.id, second.revision, 0));
        assert_eq!(
            pending
                .iter()
                .map(|item| (item.text.as_str(), item.submitted_at_ms))
                .collect::<Vec<_>>(),
            vec![("second", 22), ("first, edited", 11)]
        );
    }
}
