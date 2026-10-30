//! When the operator last wrote to a session, read back from its committed
//! canonical history.

use plakous_model::{Actor, CanonicalHistory, ContentBlock, ConversationItem, Provenance};

use super::turn::is_turn_harness_note;

/// Unix milliseconds of the newest operator prompt or steer in `history`.
///
/// The time is the one the driver stamped into the entry's `UUIDv7`
/// identity when it appended the item; an entry with any other identity
/// version (compaction, harness notes, imported or test identities) carries
/// no time and is skipped. Host wakes, mail and harness notes carry
/// `Instruction` provenance and are skipped; the notes a driver appends to a
/// running operator turn inherit `UserInput` and are skipped by their exact
/// text. Reads newest first and stops at the first match.
pub(crate) fn last_operator_input_unix_ms(history: &CanonicalHistory) -> Option<u64> {
    let segments = history.segments();
    (0..segments.len())
        .rev()
        .filter_map(|index| segments.get(index))
        .find_map(|segment| {
            segment
                .items()
                .iter()
                .rev()
                .filter(|item| is_operator_input(item))
                .find_map(entry_unix_ms)
        })
}

fn is_operator_input(item: &ConversationItem) -> bool {
    if item.actor() != Actor::User || item.provenance() != Provenance::UserInput {
        return false;
    }
    match item.blocks() {
        [ContentBlock::Text(text)] => !is_turn_harness_note(text.text()),
        blocks => blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Text(_))),
    }
}

fn entry_unix_ms(item: &ConversationItem) -> Option<u64> {
    let id = item.id().as_uuid();
    if id.get_version() != Some(uuid::Version::SortRand) {
        return None;
    }
    let (seconds, nanoseconds) = id.get_timestamp()?.to_unix();
    seconds
        .checked_mul(1_000)?
        .checked_add(u64::from(nanoseconds / 1_000_000))
}

#[cfg(test)]
mod tests;
