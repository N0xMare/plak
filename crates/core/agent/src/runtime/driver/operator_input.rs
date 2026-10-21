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
mod tests {
    use super::last_operator_input_unix_ms;
    use crate::runtime::driver::turn::{OPERATION_BUDGET_WRAP_UP_NOTE, output_limit_retry_note};
    use plakous_model::{
        Actor, CanonicalHistory, ContentBlock, ConversationItem, EntryId, HistorySegment,
        HistorySegmentKind, IdGenerator, Provenance, SystemIdGenerator, TextBlock, Timestamp,
    };
    use std::num::NonZeroU32;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const T0: u64 = 1_790_000_000_000;

    fn v7_at(unix_ms: u64) -> Result<EntryId, Box<dyn std::error::Error>> {
        let at = Timestamp::new(unix_ms / 1_000, u32::try_from(unix_ms % 1_000)? * 1_000_000)?;
        Ok(EntryId::try_from(SystemIdGenerator.next_uuid(at)?)?)
    }

    fn item(
        id: EntryId,
        actor: Actor,
        provenance: Provenance,
        text: &str,
    ) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        Ok(ConversationItem::new(
            id,
            actor,
            vec![ContentBlock::Text(TextBlock::new(text, None)?)],
            provenance,
        )?)
    }

    fn history(
        segments: Vec<(HistorySegmentKind, ConversationItem)>,
    ) -> Result<CanonicalHistory, Box<dyn std::error::Error>> {
        Ok(CanonicalHistory::new(
            segments
                .into_iter()
                .map(|(kind, item)| HistorySegment::new(kind, vec![item]))
                .collect::<Result<Vec<_>, _>>()?,
        )?)
    }

    fn prompt(unix_ms: u64, text: &str) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        item(v7_at(unix_ms)?, Actor::User, Provenance::UserInput, text)
    }

    fn reply(unix_ms: u64) -> Result<ConversationItem, Box<dyn std::error::Error>> {
        item(
            v7_at(unix_ms)?,
            Actor::Assistant,
            Provenance::ModelOutput,
            "done",
        )
    }

    #[test]
    fn the_newest_operator_prompt_or_steer_gives_the_time() -> TestResult {
        let prompt_then_steer = history(vec![
            (HistorySegmentKind::RecentIntent, prompt(T0, "first")?),
            (HistorySegmentKind::Conversation, reply(T0 + 1_000)?),
            (
                HistorySegmentKind::RecentIntent,
                prompt(T0 + 60_000, "second")?,
            ),
            (HistorySegmentKind::Steering, prompt(T0 + 90_123, "steer")?),
            (HistorySegmentKind::Conversation, reply(T0 + 120_000)?),
        ])?;
        assert_eq!(
            last_operator_input_unix_ms(&prompt_then_steer),
            Some(T0 + 90_123)
        );
        assert_eq!(
            last_operator_input_unix_ms(&CanonicalHistory::new(Vec::new())?),
            None
        );
        Ok(())
    }

    #[test]
    fn wakes_mail_and_harness_notes_are_not_operator_input() -> TestResult {
        let wake = item(
            v7_at(T0 + 60_000)?,
            Actor::User,
            Provenance::Instruction,
            "a child finished",
        )?;
        let wrap_up = item(
            v7_at(T0 + 70_000)?,
            Actor::User,
            Provenance::UserInput,
            OPERATION_BUDGET_WRAP_UP_NOTE,
        )?;
        let retry = |unix_ms, bound| {
            item(
                v7_at(unix_ms)?,
                Actor::User,
                Provenance::UserInput,
                &output_limit_retry_note(bound),
            )
        };
        let imported = item(
            v7_at(T0 + 90_000)?,
            Actor::User,
            Provenance::Imported,
            "imported",
        )?;
        let history = history(vec![
            (HistorySegmentKind::RecentIntent, prompt(T0, "operator")?),
            (HistorySegmentKind::RecentIntent, wake),
            (HistorySegmentKind::RecentIntent, wrap_up),
            (HistorySegmentKind::RecentIntent, retry(T0 + 80_000, None)?),
            (
                HistorySegmentKind::RecentIntent,
                retry(T0 + 85_000, NonZeroU32::new(32_000))?,
            ),
            (HistorySegmentKind::Conversation, imported),
        ])?;
        assert_eq!(last_operator_input_unix_ms(&history), Some(T0));
        Ok(())
    }

    #[test]
    fn an_entry_without_a_v7_identity_carries_no_time() -> TestResult {
        // Compaction and turn notes mint deterministic version-8 identities.
        let mut bytes = *v7_at(T0 + 60_000)?.as_uuid().as_bytes();
        bytes[6] = (bytes[6] & 0x0f) | 0x80;
        let derived = item(
            EntryId::try_from(uuid::Uuid::from_bytes(bytes))?,
            Actor::User,
            Provenance::UserInput,
            "derived",
        )?;
        let sequenced = item(
            EntryId::try_from(uuid::Uuid::from_u128(7))?,
            Actor::User,
            Provenance::UserInput,
            "sequenced",
        )?;
        let history = history(vec![
            (HistorySegmentKind::RecentIntent, prompt(T0, "operator")?),
            (HistorySegmentKind::RecentIntent, derived),
            (HistorySegmentKind::RecentIntent, sequenced),
        ])?;
        assert_eq!(last_operator_input_unix_ms(&history), Some(T0));
        Ok(())
    }

    #[test]
    fn harness_note_texts_are_pinned_and_operator_lookalikes_are_input() -> TestResult {
        assert_eq!(
            output_limit_retry_note(NonZeroU32::new(8_192)),
            "[harness] Your previous response reached its output limit of 8192 tokens before \
             completion, so the response was discarded and no tool call ran. Retry with a \
             shorter answer or less output per call: split a large file or patch into several \
             smaller writes or edits, or reason more briefly."
        );
        assert_eq!(
            output_limit_retry_note(None),
            "[harness] Your previous response reached the provider's output limit before \
             completion, so the response was discarded and no tool call ran. Retry with a \
             shorter answer or less output per call: split a large file or patch into several \
             smaller writes or edits, or reason more briefly."
        );
        let lookalike = output_limit_retry_note(None).replace("provider's", "model's");
        let history = history(vec![
            (HistorySegmentKind::RecentIntent, prompt(T0, "operator")?),
            (
                HistorySegmentKind::RecentIntent,
                prompt(T0 + 5_000, &lookalike)?,
            ),
        ])?;
        assert_eq!(last_operator_input_unix_ms(&history), Some(T0 + 5_000));
        Ok(())
    }
}
