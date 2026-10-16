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
mod tests {
    use super::*;
    use crate::sessions::projector::{
        Delivery, ProductIntents, SessionViews, Submission, SubmissionKind,
    };

    fn png(extra: usize) -> DraftImage {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&4_u32.to_be_bytes());
        bytes.extend_from_slice(&3_u32.to_be_bytes());
        bytes.resize(bytes.len() + extra, 0);
        DraftImage {
            name: "shot.png".into(),
            media_type: "image/png".into(),
            bytes,
        }
    }

    #[test]
    fn enqueue_bounds_images_and_an_edit_keeps_them() {
        let mut pending = Vec::new();
        let item = enqueue(
            &mut pending,
            Draft::new("look".into(), vec![png(0), png(1)]),
        )
        .unwrap();
        assert_eq!(item.images.len(), 2);
        assert_eq!(
            item.user_input("look").parts().len(),
            3,
            "text then both images"
        );
        edit(&mut pending, &item.id, item.revision, "look again").unwrap();
        assert_eq!(pending[0].text, "look again");
        assert_eq!(pending[0].images, item.images);
        assert_eq!(
            pending[0].protocol(false).images,
            ["[image: shot.png 4x3 1 KB]", "[image: shot.png 4x3 1 KB]"]
        );

        let refused = |draft: Draft, pending: &mut Vec<SessionQueueItem>| {
            enqueue(pending, draft).unwrap_err()
        };
        let nine = (0..=plakous::agent::MAX_USER_IMAGES)
            .map(|_| png(0))
            .collect();
        assert_eq!(
            refused(Draft::new("x".into(), nine), &mut pending),
            Refusal::TooManyImages
        );
        assert_eq!(
            refused(
                Draft::new("x".into(), vec![png(plakous::agent::MAX_USER_IMAGE_BYTES)]),
                &mut pending
            ),
            Refusal::ImageTooLarge
        );
        let mut mislabeled = png(0);
        mislabeled.media_type = "image/jpeg".into();
        assert_eq!(
            refused(Draft::new("x".into(), vec![mislabeled]), &mut pending),
            Refusal::NotAnImage
        );
        assert_eq!(
            refused(Draft::new(" ".into(), Vec::new()), &mut pending),
            Refusal::Empty
        );

        // The session's queued images share one aggregate bound.
        let large = || png(plakous::agent::MAX_USER_IMAGE_BYTES - 64);
        let mut full = Vec::new();
        while enqueue(&mut full, Draft::new("x".into(), vec![large(); 8])).is_ok() {}
        assert_eq!(
            refused(Draft::new("x".into(), vec![large(); 8]), &mut full),
            Refusal::ImagesFull
        );
        assert!(
            full.iter()
                .map(|item| image_bytes(&item.images))
                .sum::<usize>()
                <= MAX_PENDING_QUEUE_IMAGE_BYTES
        );
        assert_eq!(pending.len(), 1);

        // Images alone are a message: no empty text part reaches the model,
        // and an edit may clear the text of an item that keeps images.
        let only = enqueue(&mut pending, Draft::new(" ".into(), vec![png(2)])).unwrap();
        assert_eq!(only.text, "");
        assert!(matches!(
            only.user_input("").parts(),
            [plakous::agent::UserPart::Image { .. }]
        ));
        let revision = pending[0].revision;
        edit(&mut pending, &item.id, revision, " ").unwrap();
        assert_eq!(pending[0].text, "");
    }

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
        let first = enqueue(&mut pending, "first").unwrap();
        let second = enqueue(&mut pending, "second").unwrap();
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
