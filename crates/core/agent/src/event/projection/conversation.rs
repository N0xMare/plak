//! Bounded persistent conversation values shared by snapshots and publication.
use plakous_model::{Actor, ContentBlock, ConversationItem, EntryId, Provenance};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};
/// Presentation-safe summary of one committed canonical conversation item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationItemView {
    entry_id: EntryId,
    actor: Actor,
    provenance: Provenance,
    block_count: usize,
    display_text: Option<Arc<str>>,
    tool_calls: usize,
    tool_results: usize,
    has_non_display_content: bool,
}

impl ConversationItemView {
    pub(in crate::event) fn from_item(item: &ConversationItem) -> Self {
        let mut text = Vec::new();
        let mut tool_calls = 0_usize;
        let mut tool_results = 0_usize;
        let mut has_non_display_content = false;
        for block in item.blocks() {
            match block {
                ContentBlock::Text(block) => text.push(block.text()),
                ContentBlock::Reasoning(block) => {
                    if let Some(value) = block.text() {
                        text.push(value);
                    } else {
                        has_non_display_content = true;
                    }
                    has_non_display_content |= block.replay().is_some();
                }
                ContentBlock::Refusal(block) => text.push(block.text()),
                ContentBlock::ToolCall(_) => {
                    tool_calls = tool_calls.saturating_add(1);
                    has_non_display_content = true;
                }
                ContentBlock::ToolResult(_) => {
                    tool_results = tool_results.saturating_add(1);
                    has_non_display_content = true;
                }
                _ => has_non_display_content = true,
            }
        }
        Self {
            entry_id: item.id(),
            actor: item.actor(),
            provenance: item.provenance(),
            block_count: item.blocks().len(),
            display_text: (!text.is_empty()).then(|| Arc::from(text.join("\n"))),
            tool_calls,
            tool_results,
            has_non_display_content,
        }
    }

    /// Returns the canonical conversation-entry identity.
    #[must_use]
    pub const fn entry_id(&self) -> EntryId {
        self.entry_id
    }

    /// Returns the responsible actor.
    #[must_use]
    pub const fn actor(&self) -> Actor {
        self.actor
    }

    /// Returns the canonical provenance.
    #[must_use]
    pub const fn provenance(&self) -> Provenance {
        self.provenance
    }

    /// Returns the canonical block count.
    #[must_use]
    pub const fn block_count(&self) -> usize {
        self.block_count
    }

    /// Returns displayable text with replay/raw/opaque material excluded.
    #[must_use]
    pub fn display_text(&self) -> Option<&str> {
        self.display_text.as_deref()
    }

    /// Returns the number of tool-call blocks without exposing arguments.
    #[must_use]
    pub const fn tool_calls(&self) -> usize {
        self.tool_calls
    }

    /// Returns the number of tool-result blocks without exposing result content.
    #[must_use]
    pub const fn tool_results(&self) -> usize {
        self.tool_results
    }

    /// Reports whether non-display content was intentionally omitted.
    #[must_use]
    pub const fn has_non_display_content(&self) -> bool {
        self.has_non_display_content
    }

    fn retained_bytes(&self) -> usize {
        128_usize.saturating_add(self.display_text.as_deref().map_or(0, str::len))
    }
}

#[derive(Clone)]
pub(in crate::event) struct ConversationProjection {
    inner: Arc<ConversationProjectionInner>,
}

struct ConversationProjectionInner {
    roots: Arc<[Arc<ConversationProjectionTree>]>,
    item_count: usize,
    retained_bytes: usize,
    flattened: OnceLock<Arc<[ConversationItemView]>>,
}

enum ConversationProjectionTree {
    Leaf(Arc<[ConversationItemView]>),
    Branch {
        left: Arc<Self>,
        right: Arc<Self>,
        item_count: usize,
        retained_bytes: usize,
        height: u32,
    },
}

impl ConversationProjectionTree {
    fn branch(left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        Arc::new(Self::Branch {
            item_count: left.item_count().saturating_add(right.item_count()),
            retained_bytes: left.retained_bytes().saturating_add(right.retained_bytes()),
            height: left.height().saturating_add(1),
            left,
            right,
        })
    }

    fn item_count(&self) -> usize {
        match self {
            Self::Leaf(items) => items.len(),
            Self::Branch { item_count, .. } => *item_count,
        }
    }

    fn retained_bytes(&self) -> usize {
        match self {
            Self::Leaf(items) => items
                .iter()
                .fold(0, |total, item| total.saturating_add(item.retained_bytes())),
            Self::Branch { retained_bytes, .. } => *retained_bytes,
        }
    }

    const fn height(&self) -> u32 {
        match self {
            Self::Leaf(_) => 0,
            Self::Branch { height, .. } => *height,
        }
    }
}

impl ConversationProjection {
    pub(in crate::event) fn from_flat_bounded(
        items: Arc<[ConversationItemView]>,
        max_items: usize,
        max_bytes: usize,
    ) -> Self {
        let item_count = items.len();
        let retained_bytes = items.iter().fold(0_usize, |total, item| {
            total.saturating_add(item.retained_bytes())
        });
        let flattened = OnceLock::from(Arc::clone(&items));
        let roots = if items.is_empty() {
            Arc::from(Vec::new())
        } else {
            Arc::from(vec![Arc::new(ConversationProjectionTree::Leaf(items))])
        };
        Self {
            inner: Arc::new(ConversationProjectionInner {
                roots,
                item_count,
                retained_bytes,
                flattened,
            }),
        }
        .bounded_tail(max_items, max_bytes)
    }

    pub(super) fn appended_bounded(
        &self,
        items: Arc<[ConversationItemView]>,
        max_items: usize,
        max_bytes: usize,
    ) -> Self {
        if items.is_empty() {
            return self.clone();
        }
        let item_count = self.inner.item_count.saturating_add(items.len());
        let mut roots = self.inner.roots.to_vec();
        let mut carry = Arc::new(ConversationProjectionTree::Leaf(items));
        while roots
            .last()
            .is_some_and(|root| root.height() == carry.height())
        {
            let Some(left) = roots.pop() else {
                break;
            };
            carry = ConversationProjectionTree::branch(left, carry);
        }
        roots.push(carry);
        let retained_bytes = roots.iter().fold(0_usize, |total, root| {
            total.saturating_add(root.retained_bytes())
        });
        Self {
            inner: Arc::new(ConversationProjectionInner {
                roots: Arc::from(roots),
                item_count,
                retained_bytes,
                flattened: OnceLock::new(),
            }),
        }
        .bounded_tail(max_items, max_bytes)
    }

    pub(super) fn bounded_tail(&self, max_items: usize, max_bytes: usize) -> Self {
        if self.inner.item_count <= max_items && self.inner.retained_bytes <= max_bytes {
            return self.clone();
        }
        let mut remaining_items = max_items;
        let mut remaining_bytes = max_bytes;
        let mut newest_first = Vec::new();
        for root in self.inner.roots.iter().rev() {
            if !collect_bounded_tail(
                root,
                &mut remaining_items,
                &mut remaining_bytes,
                &mut newest_first,
            ) {
                break;
            }
        }
        newest_first.reverse();
        let item_count = newest_first.iter().fold(0_usize, |total, node| {
            total.saturating_add(node.item_count())
        });
        let retained_bytes = newest_first.iter().fold(0_usize, |total, node| {
            total.saturating_add(node.retained_bytes())
        });
        Self {
            inner: Arc::new(ConversationProjectionInner {
                roots: Arc::from(newest_first),
                item_count,
                retained_bytes,
                flattened: OnceLock::new(),
            }),
        }
    }

    pub(in crate::event) fn items(&self) -> &[ConversationItemView] {
        self.inner
            .flattened
            .get_or_init(|| {
                let mut flattened = Vec::with_capacity(self.inner.item_count);
                let mut stack = self
                    .inner
                    .roots
                    .iter()
                    .rev()
                    .map(Arc::as_ref)
                    .collect::<Vec<_>>();
                while let Some(node) = stack.pop() {
                    match node {
                        ConversationProjectionTree::Leaf(items) => {
                            flattened.extend_from_slice(items);
                        }
                        ConversationProjectionTree::Branch { left, right, .. } => {
                            stack.push(right);
                            stack.push(left);
                        }
                    }
                }
                Arc::from(flattened)
            })
            .as_ref()
    }
}

fn collect_bounded_tail(
    node: &Arc<ConversationProjectionTree>,
    remaining_items: &mut usize,
    remaining_bytes: &mut usize,
    newest_first: &mut Vec<Arc<ConversationProjectionTree>>,
) -> bool {
    if node.item_count() <= *remaining_items && node.retained_bytes() <= *remaining_bytes {
        *remaining_items -= node.item_count();
        *remaining_bytes -= node.retained_bytes();
        newest_first.push(Arc::clone(node));
        return true;
    }
    match node.as_ref() {
        ConversationProjectionTree::Branch { left, right, .. } => {
            collect_bounded_tail(right, remaining_items, remaining_bytes, newest_first)
                && collect_bounded_tail(left, remaining_items, remaining_bytes, newest_first)
        }
        ConversationProjectionTree::Leaf(items) => {
            let mut start = items.len();
            while start != 0 && *remaining_items != 0 {
                let next = &items[start - 1];
                let bytes = next.retained_bytes();
                if bytes > *remaining_bytes {
                    break;
                }
                start -= 1;
                *remaining_items -= 1;
                *remaining_bytes -= bytes;
            }
            if start != items.len() {
                newest_first.push(Arc::new(ConversationProjectionTree::Leaf(Arc::from(
                    items[start..].to_vec(),
                ))));
            }
            start == 0
        }
    }
}

impl PartialEq for ConversationProjection {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
            || (self.inner.item_count == other.inner.item_count && self.items() == other.items())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        clippy::unwrap_used,
        reason = "unit tests may assert directly"
    )]
    use super::*;
    use crate::event::test_support::*;
    use crate::event::{AgentEventKind, EventContext, ViewRequest};
    #[test]
    fn conversation_commit_reuses_bounded_chunk_and_shared_prefix() -> TestResult {
        let conversation = long_conversation(1_024)?;
        let fixture = fixture_with_run_and_conversation(4, &conversation, 2_048, 4 * 1024 * 1024)?;
        fixture.hub.publish(
            EventContext::turn(fixture.turn_id),
            AgentEventKind::TurnStarted,
        )?;
        let prefix = fixture
            .hub
            .attach(ViewRequest::content())?
            .into_parts()
            .0
            .conversation
            .ok_or("missing content projection")?;
        let committed: Arc<[ConversationItem]> =
            Arc::from(vec![conversation_item(20_000, "appended item")?]);
        let views: Arc<[ConversationItemView]> = Arc::from(
            committed
                .iter()
                .map(ConversationItemView::from_item)
                .collect::<Vec<_>>(),
        );

        fixture.hub.publish(
            EventContext::turn(fixture.turn_id),
            AgentEventKind::ConversationCommitted {
                item_count: views.len(),
                items: Some(Arc::clone(&views)),
            },
        )?;

        let projection = fixture
            .hub
            .attach(ViewRequest::content())?
            .into_parts()
            .0
            .conversation
            .ok_or("missing content projection")?;
        let root = projection
            .inner
            .roots
            .first()
            .ok_or("conversation append omitted its tree")?;
        let ConversationProjectionTree::Branch { left, right, .. } = root.as_ref() else {
            return Err("conversation append did not create a shared branch".into());
        };
        assert!(Arc::ptr_eq(left, &prefix.inner.roots[0]));
        let ConversationProjectionTree::Leaf(appended) = right.as_ref() else {
            return Err("conversation append did not retain its bounded leaf".into());
        };
        assert!(Arc::ptr_eq(appended, &views));
        let snapshot = fixture.hub.attach(ViewRequest::content())?;
        let items = snapshot
            .snapshot()
            .conversation()
            .ok_or("content snapshot omitted appended conversation")?;
        assert_eq!(items.len(), 1_025);
        assert_eq!(
            items.last().and_then(ConversationItemView::display_text),
            Some("appended item")
        );
        Ok(())
    }
    #[test]
    fn conversation_append_metadata_stays_logarithmic_and_lazy() -> TestResult {
        let item = ConversationItemView::from_item(&conversation_item(30_000, "bounded leaf")?);
        let mut projection =
            ConversationProjection::from_flat_bounded(Arc::from(Vec::new()), 64, 1_024);
        for _ in 0..8_191 {
            projection = projection.appended_bounded(Arc::from(vec![item.clone()]), 64, 1_024);
        }

        assert_eq!(projection.inner.item_count, 7);
        assert_eq!(projection.inner.retained_bytes, 980);
        assert!(projection.inner.roots.len() <= u64::BITS as usize);
        assert!(projection.inner.flattened.get().is_none());
        assert_eq!(projection.items().len(), 7);
        assert!(projection.inner.flattened.get().is_some());
        Ok(())
    }
}
