//! The version-7 checkpoint importer, kept for one document bump.

use super::document::{CarryCheckpoint, ImportError};
use plakous::model::{
    Actor, ContentBlock, ConversationItem, EntryId, HistorySegment, HistorySegmentKind, Provenance,
    TextBlock,
};
use plakous::sessions::PortableSessionHistory;

pub(super) fn import_checkpoint(
    checkpoint: Option<CarryCheckpoint>,
) -> Result<PortableSessionHistory, ImportError> {
    let checkpoint =
        checkpoint.ok_or_else(|| ImportError::Invalid("missing v7 checkpoint".to_owned()))?;
    let intent = checkpoint_item(&checkpoint.user_intent, Actor::User, Provenance::UserInput)?;
    let mut segments = vec![
        HistorySegment::new(HistorySegmentKind::RecentIntent, vec![intent])
            .map_err(|e| e.to_string())?,
    ];
    if !checkpoint.reference.is_empty() {
        segments.push(
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![checkpoint_item(
                    &checkpoint.reference,
                    Actor::Assistant,
                    Provenance::Imported,
                )?],
            )
            .map_err(|e| e.to_string())?,
        );
    }
    Ok(PortableSessionHistory::new(
        segments,
        checkpoint.replay_account,
        checkpoint.replay_model,
    ))
}

pub(super) fn checkpoint_carried_note() -> String {
    "resumed from a bounded checkpoint, with the readable archive retained only as untrusted reference data"
        .to_owned()
}

fn checkpoint_item(
    text: &str,
    actor: Actor,
    provenance: Provenance,
) -> Result<ConversationItem, String> {
    ConversationItem::new(
        EntryId::try_from(uuid::Uuid::new_v4())
            .map_err(|error| format!("could not identify the restart checkpoint: {error}"))?,
        actor,
        vec![ContentBlock::Text(
            TextBlock::new(text.to_owned(), None)
                .map_err(|error| format!("the restart checkpoint is invalid: {error}"))?,
        )],
        provenance,
    )
    .map_err(|error| format!("the restart checkpoint is invalid: {error}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "unit tests")]
mod tests;
