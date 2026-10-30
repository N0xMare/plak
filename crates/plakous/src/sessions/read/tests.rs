use super::{page_history_items_backward, page_history_items_forward};
use crate::sessions::SessionHistoryItem;
use plakous_model::{Actor, EntryId, Provenance};
use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

fn items(count: u128) -> TestResult<Vec<SessionHistoryItem>> {
    (1..=count)
        .map(|value| {
            Ok(SessionHistoryItem {
                entry_id: EntryId::try_from(Uuid::from_u128(value))?,
                actor: Actor::User,
                provenance: Provenance::UserInput,
                block_count: 0,
                display_text: None,
                tool_blocks: Vec::new(),
                tool_calls: 0,
                tool_results: 0,
                has_non_display_content: false,
                compaction: None,
            })
        })
        .collect()
}

fn ids(items: &[SessionHistoryItem]) -> Vec<EntryId> {
    items.iter().map(SessionHistoryItem::entry_id).collect()
}

#[test]
fn byte_bounded_backward_pages_return_every_item_exactly_once() -> TestResult {
    let projected = items(5)?;
    // Each metadata-only item retains the same bytes, so this bound fits two.
    let max_bytes = projected
        .iter()
        .take(2)
        .map(SessionHistoryItem::retained_bytes)
        .sum();
    let mut seen = Vec::new();
    let mut end = projected.len();
    while end > 0 {
        let (page, next) = page_history_items_backward(&projected, end, 10, max_bytes);
        assert!(
            !page.is_empty() && next < end,
            "a backward page must make progress"
        );
        let mut page_ids = ids(&page);
        page_ids.extend(seen);
        seen = page_ids;
        end = next;
    }
    assert_eq!(seen, ids(&projected));
    Ok(())
}

#[test]
fn byte_bounded_forward_pages_return_every_item_exactly_once() -> TestResult {
    let projected = items(5)?;
    let max_bytes = projected
        .iter()
        .take(2)
        .map(SessionHistoryItem::retained_bytes)
        .sum();
    let mut seen = Vec::new();
    let mut start = 0;
    while start < projected.len() {
        let (page, next) = page_history_items_forward(&projected, start, 10, max_bytes);
        assert!(next > start, "a forward page must make progress");
        seen.extend(ids(&page));
        start = next;
    }
    assert_eq!(seen, ids(&projected));
    Ok(())
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one history fixture covers every pass, an unnamed segment, and metadata omission"
)]
fn compaction_segment_projects_its_own_kind_without_the_summary() -> TestResult {
    use super::{CommittedCompactionFacts, flatten_history_items};
    use crate::sessions::{
        SessionHistoryCompaction, SessionHistoryCompactionPass, SessionHistoryVisibility,
    };
    use plakous_model::{
        CanonicalHistory, CompactionBlock, ContentBlock, ConversationItem, HistorySegment,
        HistorySegmentKind, TextBlock,
    };

    let secret = "SECRET SUMMARY ciphertext";
    let replay = "provider-replay-envelope";
    let summary = HistorySegment::new(
        HistorySegmentKind::Compaction,
        vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(1))?,
            Actor::Assistant,
            vec![ContentBlock::Compaction(
                CompactionBlock::new(secret)?.with_native_item(replay.to_owned()),
            )],
            Provenance::ModelOutput,
        )?],
    )?;
    let user = HistorySegment::new(
        HistorySegmentKind::Conversation,
        vec![ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(2))?,
            Actor::User,
            vec![ContentBlock::Text(TextBlock::new("kept prompt", None)?)],
            Provenance::UserInput,
        )?],
    )?;
    let history = CanonicalHistory::new(vec![summary, user])?;
    let summary_identity = history.segments()[0].identity();
    let passes = [
        (
            SessionHistoryCompactionPass::SemanticSummary,
            "semantic summary",
        ),
        (
            SessionHistoryCompactionPass::Placeholder,
            "placeholder pass",
        ),
        (SessionHistoryCompactionPass::Native, "native compaction"),
        (
            SessionHistoryCompactionPass::ReplayOmission,
            "replay omission",
        ),
    ];
    for (pass, label) in passes {
        let projected = flatten_history_items(
            &history,
            SessionHistoryVisibility::Content,
            Some(CommittedCompactionFacts {
                summary_segment: summary_identity,
                replaced_segments: 4,
                pass,
            }),
        );
        assert_eq!(projected.len(), 2);
        let compaction = &projected[0];
        let phrase = format!("compacted 4 earlier segments ({label})");
        assert_eq!(compaction.projected_kind(), "compaction");
        assert_eq!(compaction.display_text(), Some(phrase.as_str()));
        assert_eq!(
            compaction.compaction(),
            Some(SessionHistoryCompaction::Recorded {
                replaced_segments: 4,
                pass,
            })
        );
        assert!(compaction.has_non_display_content());
        assert_eq!(compaction.tool_calls(), 0);
        assert_eq!(compaction.tool_results(), 0);
        let rendered = format!("{compaction:?}{}", compaction.display_text().unwrap_or(""));
        assert!(!rendered.contains(secret), "{rendered}");
        assert!(!rendered.contains(replay), "{rendered}");
        assert_eq!(projected[1].projected_kind(), "user");
        assert_eq!(projected[1].display_text(), Some("kept prompt"));
        assert!(projected[1].compaction().is_none());
    }
    let unnamed = flatten_history_items(
        &history,
        SessionHistoryVisibility::Content,
        Some(CommittedCompactionFacts {
            summary_segment: history.segments()[1].identity(),
            replaced_segments: 9,
            pass: SessionHistoryCompactionPass::Placeholder,
        }),
    );
    assert_eq!(unnamed[0].projected_kind(), "compaction");
    assert_eq!(
        unnamed[0].display_text(),
        Some("compacted earlier segments")
    );
    assert_eq!(
        unnamed[0].compaction(),
        Some(SessionHistoryCompaction::Unrecorded)
    );
    assert!(!format!("{:?}", unnamed[0]).contains("placeholder"));
    let hidden = flatten_history_items(
        &history,
        SessionHistoryVisibility::Metadata,
        Some(CommittedCompactionFacts {
            summary_segment: summary_identity,
            replaced_segments: 4,
            pass: SessionHistoryCompactionPass::SemanticSummary,
        }),
    );
    assert_eq!(hidden[0].projected_kind(), "compaction");
    assert!(hidden[0].display_text().is_none());
    let facts = CommittedCompactionFacts {
        summary_segment: summary_identity,
        replaced_segments: 4,
        pass: SessionHistoryCompactionPass::SemanticSummary,
    };
    assert_eq!(
        flatten_history_items(&history, SessionHistoryVisibility::Content, Some(facts)),
        flatten_history_items(&history, SessionHistoryVisibility::Content, Some(facts)),
    );
    Ok(())
}
