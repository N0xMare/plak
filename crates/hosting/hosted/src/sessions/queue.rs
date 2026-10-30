//! Bounded queue edits and delivery locks belonging to a session record.

use std::sync::Arc;

use plakous_host_protocol::{Draft, DraftImage};

use super::{
    MAX_PENDING_PROMPTS, MAX_PENDING_QUEUE_IMAGE_BYTES, MAX_PROMPT_BYTES, SessionRecord,
    new_uuid_v4_string,
};
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
    /// Images attached at submission. Immutable for the item's life: an
    /// edit replaces only the text, and a delete drops them with the item.
    pub images: Arc<[DraftImage]>,
}

impl QueueItem {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            id: new_uuid_v4_string(),
            revision: 1,
            text: text.into(),
            submitted_at_ms: plakous::model::now_unix_millis(),
            images: Arc::default(),
        }
    }

    /// The surface view of this item.
    #[must_use]
    pub fn protocol(&self, rejected: bool) -> plakous_host_protocol::QueueItem {
        plakous_host_protocol::QueueItem {
            rejected,
            id: self.id.clone(),
            revision: self.revision,
            text: self.text.clone(),
            images: self.images.iter().map(super::named_image_chip).collect(),
        }
    }

    /// The model input this item delivers with `text` as its (possibly
    /// mail-extended) canonical command text: the text, then the images.
    #[must_use]
    pub fn user_input(&self, text: &str) -> plakous::agent::UserInput {
        user_input(text, &self.images)
    }
}

/// One user message: the text part when there is text, then the images in
/// attachment order.
pub(crate) fn user_input(text: &str, images: &[DraftImage]) -> plakous::agent::UserInput {
    let mut parts = Vec::with_capacity(1 + images.len());
    if !text.is_empty() {
        parts.push(plakous::agent::UserPart::Text(text.to_owned()));
    }
    parts.extend(images.iter().map(|image| plakous::agent::UserPart::Image {
        media_type: image.media_type.clone(),
        bytes: image.bytes.clone(),
    }));
    plakous::agent::UserInput::new(parts)
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
    TooManyImages,
    ImageTooLarge,
    NotAnImage,
    MessageTooLarge(crate::attachments::MessageBudgetExceeded),
    ImagesFull,
}

/// Append a follow-up / queued prompt with its attached images.
pub(crate) fn enqueue(
    pending: &mut Vec<SessionQueueItem>,
    draft: impl Into<Draft>,
) -> Result<SessionQueueItem, Refusal> {
    let draft = draft.into();
    let t = draft.text.trim();
    if t.is_empty() && draft.images.is_empty() {
        return Err(Refusal::Empty);
    }
    if t.len() > MAX_PROMPT_BYTES {
        return Err(Refusal::TooLarge);
    }
    if pending.len() >= MAX_PENDING_PROMPTS {
        return Err(Refusal::Full);
    }
    validate_images(&draft.images)?;
    crate::attachments::check_message_budget(t, &draft.images).map_err(Refusal::MessageTooLarge)?;
    let queued: usize = pending.iter().map(|item| image_bytes(&item.images)).sum();
    if queued.saturating_add(image_bytes(&draft.images)) > MAX_PENDING_QUEUE_IMAGE_BYTES {
        return Err(Refusal::ImagesFull);
    }
    let mut item = SessionQueueItem::new(t);
    item.images = draft.images.into();
    pending.push(item.clone());
    Ok(item)
}

pub(crate) fn image_bytes(images: &[DraftImage]) -> usize {
    images.iter().map(|image| image.bytes.len()).sum()
}

/// The provider-neutral admission bounds, checked early so a refused image
/// never reaches the queue: count, raw size, and a declared type equal to
/// the sniffed one.
pub(crate) fn validate_images(images: &[DraftImage]) -> Result<(), Refusal> {
    if images.len() > plakous::agent::MAX_USER_IMAGES {
        return Err(Refusal::TooManyImages);
    }
    for image in images {
        if image.bytes.len() > plakous::agent::MAX_USER_IMAGE_BYTES {
            return Err(Refusal::ImageTooLarge);
        }
        if plakous::tools::sniff_image(&image.bytes)
            .map_or(true, |header| header.media_type() != image.media_type)
        {
            return Err(Refusal::NotAnImage);
        }
    }
    Ok(())
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
    if text.len() > MAX_PROMPT_BYTES {
        return Err(Refusal::TooLarge);
    }
    let Some(item) = pending
        .iter_mut()
        .find(|q| q.id == id && q.revision == revision)
    else {
        return Err(Refusal::Stale);
    };
    if text.is_empty() && item.images.is_empty() {
        return Err(Refusal::Empty);
    }
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
mod tests;
