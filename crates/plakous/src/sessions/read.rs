use super::types::{MAX_SESSION_HISTORY_PAGE_BYTES, MAX_SESSION_HISTORY_PAGE_ITEMS};
use super::{
    AgentSessionsError, AgentSessionsErrorKind, SESSION_HISTORY_VISIBILITY_REVISION,
    SessionHistoryCompaction, SessionHistoryCompactionPass, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryItem, SessionHistoryPageOutcome,
    SessionHistoryPageRequest, SessionHistoryResnapshot, SessionHistoryToolBlock,
    SessionHistoryVisibility,
};
use plakous_agent::internal::{CompactionProvenance, RuntimeCheckpoint, RuntimeCompactionStrategy};
use plakous_model::{
    CanonicalHistory, ContentBlock, ConversationItem, Digest, HistorySegment, HistorySegmentKind,
};

/// Provenance facts the projector may attach to the one segment they name.
///
/// Carries the summary-segment digest, the selected-segment count and the
/// pass. Not the summary or a replay envelope.
#[derive(Clone, Copy)]
pub(super) struct CommittedCompactionFacts {
    summary_segment: Digest,
    replaced_segments: u32,
    pass: SessionHistoryCompactionPass,
}

pub(super) fn committed_compaction_facts(
    checkpoint: &RuntimeCheckpoint,
) -> Option<CommittedCompactionFacts> {
    checkpoint
        .manifest()
        .canonical()
        .context()
        .compaction_provenance()
        .map(facts_from_provenance)
}

fn facts_from_provenance(provenance: &CompactionProvenance) -> CommittedCompactionFacts {
    let pass = match provenance.strategy() {
        RuntimeCompactionStrategy::SemanticSummaryPrefixV1 => {
            SessionHistoryCompactionPass::SemanticSummary
        }
        RuntimeCompactionStrategy::ToolResultPlaceholdersV1 => {
            SessionHistoryCompactionPass::Placeholder
        }
        RuntimeCompactionStrategy::NativeResponsesTriggerV1 => SessionHistoryCompactionPass::Native,
        RuntimeCompactionStrategy::ReplayOmissionV1 => SessionHistoryCompactionPass::ReplayOmission,
    };
    CommittedCompactionFacts {
        summary_segment: provenance.summary_segment_identity(),
        replaced_segments: provenance.selected_range().2,
        pass,
    }
}

fn compaction_notice(
    segment: &HistorySegment,
    facts: Option<CommittedCompactionFacts>,
) -> Option<SessionHistoryCompaction> {
    if segment.kind() != HistorySegmentKind::Compaction {
        return None;
    }
    Some(match facts {
        Some(facts) if facts.summary_segment == segment.identity() => {
            SessionHistoryCompaction::Recorded {
                replaced_segments: facts.replaced_segments,
                pass: facts.pass,
            }
        }
        _ => SessionHistoryCompaction::Unrecorded,
    })
}

fn compaction_display(notice: SessionHistoryCompaction) -> String {
    match notice {
        SessionHistoryCompaction::Recorded {
            replaced_segments,
            pass,
        } => {
            let pass = pass.label();
            format!("compacted {replaced_segments} earlier segments ({pass})")
        }
        SessionHistoryCompaction::Unrecorded => "compacted earlier segments".to_owned(),
    }
}

pub(super) fn flatten_history_items(
    history: &CanonicalHistory,
    visibility: SessionHistoryVisibility,
    compaction: Option<CommittedCompactionFacts>,
) -> Vec<SessionHistoryItem> {
    let mut items = Vec::new();
    for segment in history.segments() {
        let notice = compaction_notice(segment, compaction);
        for item in segment.items() {
            items.push(project_history_item(item, visibility, notice));
        }
    }
    items
}

fn project_history_item(
    item: &ConversationItem,
    visibility: SessionHistoryVisibility,
    compaction: Option<SessionHistoryCompaction>,
) -> SessionHistoryItem {
    let mut text = Vec::new();
    let mut tool_calls = 0_usize;
    let mut tool_results = 0_usize;
    let mut tool_blocks = Vec::new();
    let mut has_non_display_content = false;
    // A compaction segment's own text is the summary. It is not display text.
    let suppress_text = compaction.is_some();
    for block in item.blocks() {
        match block {
            ContentBlock::Text(block) => {
                if suppress_text {
                    has_non_display_content = true;
                } else {
                    text.push(block.text());
                }
            }
            ContentBlock::Reasoning(block) => {
                if suppress_text {
                    has_non_display_content = true;
                } else if let Some(value) = block.text() {
                    text.push(value);
                } else {
                    has_non_display_content = true;
                }
                has_non_display_content |= block.replay().is_some();
            }
            ContentBlock::Refusal(block) => {
                if suppress_text {
                    has_non_display_content = true;
                } else {
                    text.push(block.text());
                }
            }
            ContentBlock::ToolCall(block) => {
                tool_calls = tool_calls.saturating_add(1);
                tool_blocks.push(SessionHistoryToolBlock {
                    call_id: block.call_id(),
                    tool: Some(block.exposed_name().clone()),
                });
                has_non_display_content = true;
            }
            ContentBlock::ToolResult(block) => {
                tool_results = tool_results.saturating_add(1);
                tool_blocks.push(SessionHistoryToolBlock {
                    call_id: block.call_id(),
                    tool: None,
                });
                has_non_display_content = true;
            }
            _ => has_non_display_content = true,
        }
    }
    let display_text = if let Some(notice) = compaction {
        match visibility {
            SessionHistoryVisibility::Content => Some(compaction_display(notice)),
            SessionHistoryVisibility::Metadata => None,
        }
    } else if matches!(visibility, SessionHistoryVisibility::Content) && !text.is_empty() {
        Some(text.join("\n"))
    } else {
        None
    };
    SessionHistoryItem {
        entry_id: item.id(),
        actor: item.actor(),
        provenance: item.provenance(),
        block_count: item.blocks().len(),
        display_text,
        tool_blocks,
        tool_calls,
        tool_results,
        has_non_display_content,
        compaction,
    }
}

pub(super) fn page_history_items_forward(
    projected: &[SessionHistoryItem],
    start: usize,
    max_items: usize,
    max_bytes: usize,
) -> (Vec<SessionHistoryItem>, usize) {
    let mut items = Vec::new();
    let mut retained = 0_usize;
    let mut index = start.min(projected.len());
    while index < projected.len() && items.len() < max_items {
        let item = &projected[index];
        let item_bytes = item.retained_bytes();
        if !items.is_empty() && retained.saturating_add(item_bytes) > max_bytes {
            break;
        }
        retained = retained.saturating_add(item_bytes);
        items.push(item.clone());
        index = index.saturating_add(1);
    }
    (items, index)
}

pub(super) fn page_history_items_backward(
    projected: &[SessionHistoryItem],
    end: usize,
    max_items: usize,
    max_bytes: usize,
) -> (Vec<SessionHistoryItem>, usize) {
    let end = end.min(projected.len());
    let mut items = Vec::new();
    let mut retained = 0_usize;
    let mut index = end;
    while index > 0 && items.len() < max_items {
        let item = &projected[index - 1];
        let item_bytes = item.retained_bytes();
        if !items.is_empty() && retained.saturating_add(item_bytes) > max_bytes {
            break;
        }
        retained = retained.saturating_add(item_bytes);
        items.push(item.clone());
        // Step back only after the item is taken, so a byte-bounded stop leaves
        // the refused item at the start of the next backward page.
        index -= 1;
    }
    items.reverse();
    (items, index)
}

pub(super) fn history_page_preflight(
    request: &SessionHistoryPageRequest,
) -> Result<Option<SessionHistoryPageOutcome>, AgentSessionsError> {
    if request.checkpoint_id != request.locator.checkpoint_id {
        return Ok(Some(SessionHistoryPageOutcome::Resnapshot {
            locator: request.locator,
            reason: SessionHistoryResnapshot::Checkpoint,
        }));
    }
    if request.max_items == 0
        || request.max_bytes == 0
        || request.max_items > MAX_SESSION_HISTORY_PAGE_ITEMS
        || request.max_bytes > MAX_SESSION_HISTORY_PAGE_BYTES
    {
        return Err(AgentSessionsError::new(
            AgentSessionsErrorKind::InvalidInput,
            "history page limits are outside the admitted bound",
        ));
    }
    if request.visibility_revision != SESSION_HISTORY_VISIBILITY_REVISION {
        return Ok(Some(SessionHistoryPageOutcome::Resnapshot {
            locator: request.locator,
            reason: SessionHistoryResnapshot::VisibilityRevision,
        }));
    }
    Ok(None)
}

pub(super) fn project_history_page(
    history: &CanonicalHistory,
    request: &SessionHistoryPageRequest,
    compaction: Option<CommittedCompactionFacts>,
) -> SessionHistoryPageOutcome {
    let history_identity = history.identity();
    if let Some(cursor) = request.cursor {
        if cursor.locator != request.locator {
            return SessionHistoryPageOutcome::Resnapshot {
                locator: request.locator,
                reason: SessionHistoryResnapshot::Checkpoint,
            };
        }
        if cursor.history_identity != history_identity {
            return SessionHistoryPageOutcome::Resnapshot {
                locator: request.locator,
                reason: SessionHistoryResnapshot::HistoryIdentity,
            };
        }
    }
    let projected = flatten_history_items(history, request.visibility, compaction);
    let (items, next_index) = match request.direction {
        SessionHistoryDirection::Forward => {
            let start = request.cursor.map_or(0, |cursor| {
                usize::try_from(cursor.next_index).unwrap_or(usize::MAX)
            });
            page_history_items_forward(
                &projected,
                start,
                usize::try_from(request.max_items).unwrap_or(usize::MAX),
                usize::try_from(request.max_bytes).unwrap_or(usize::MAX),
            )
        }
        SessionHistoryDirection::Backward => {
            let end = request.cursor.map_or(projected.len(), |cursor| {
                usize::try_from(cursor.next_index).unwrap_or(usize::MAX)
            });
            page_history_items_backward(
                &projected,
                end,
                usize::try_from(request.max_items).unwrap_or(usize::MAX),
                usize::try_from(request.max_bytes).unwrap_or(usize::MAX),
            )
        }
    };
    let has_next = match request.direction {
        SessionHistoryDirection::Forward => next_index < projected.len(),
        SessionHistoryDirection::Backward => next_index > 0,
    };
    let next = has_next.then_some(SessionHistoryCursor {
        locator: request.locator,
        history_identity,
        next_index: u64::try_from(next_index).unwrap_or(u64::MAX),
    });
    SessionHistoryPageOutcome::Page {
        locator: request.locator,
        visibility: request.visibility,
        history_identity,
        items,
        next,
    }
}

#[cfg(test)]
mod tests {
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
}
