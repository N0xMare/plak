//! Immutable segmented canonical history and bounded model-visible projections.

use std::collections::BTreeSet;
use std::fmt;
use std::mem::size_of;
use std::ops::Index;
use std::slice;
use std::sync::Arc;

use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

use crate::semantic_json::{canonicalize_normalized_prevalidated, canonicalize_prevalidated};
use crate::{Actor, ContentBlock, ConversationItem, Digest, DigestDomain, DigestError, EntryId};

mod planning;
mod view;

pub use planning::{
    ContextHistoryPlan, ContextHistoryStrategy, ContextPlannerDisposition,
    ContextPlannerDispositionKind, ContextRetainedTail, ContextSelectedIntentGroup,
    ContextSelectedSegment, ContextSummaryRetainedTail, HistoryPlanningError,
    RetainedHistorySegmentIter, SemanticCompactionCut, SemanticCompactionCutError,
};
pub use view::{
    CanonicalItemPrefixIdentity, ConversationItemIter, ConversationItems,
    ConversationPrefixIdentity, ConversationView, WorkingProjection, WorkingProjectionLimits,
};

const MAX_SEGMENT_ITEMS: usize = 4_096;
const MAX_SEGMENT_BYTES: usize = 64 * 1024 * 1024;
const MAX_HISTORY_SEGMENTS: usize = 65_536;
const HISTORY_ITER_STACK_CAPACITY: usize = 32;
/// Digest revision for ordered canonical-item artifact coverage.
pub const CANONICAL_ITEM_PREFIX_DIGEST_REVISION: u32 = 2;
/// Closed driver-owned context-protection policy revision.
pub const CONTEXT_PLANNER_POLICY_REVISION: u32 = 2;
/// Closed whole-group selection strategy revision.
pub const CONTEXT_PLANNER_STRATEGY_REVISION: u32 = 2;
/// Semantic revision for one sealed model-generated compaction cut.
pub const SEMANTIC_COMPACTION_CUT_REVISION: u32 = 2;
/// Maximum committed plus working groups accepted by one context plan.
pub const MAX_CONTEXT_PLANNER_SEGMENTS: usize = 65_536;

/// An error constructing immutable canonical history or a working projection.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum HistoryError {
    /// A semantic segment had no conversation items.
    #[error("history segment cannot be empty")]
    EmptySegment,
    /// A structural count or retained-byte limit was exceeded.
    #[error("{field} exceeds the limit of {maximum}")]
    LimitExceeded {
        /// The bounded field.
        field: &'static str,
        /// The accepted maximum.
        maximum: usize,
    },
    /// A caller supplied an unusable zero projection bound.
    #[error("working projection limits must be nonzero")]
    InvalidLimits,
    /// A segment kind did not match its canonical content.
    #[error("history segment kind does not match its semantic content")]
    KindMismatch,
    /// A protected tool-call/result group was incomplete or ambiguous.
    #[error("tool exchange must contain exactly one call and result for every call identity")]
    IncompleteToolExchange,
    /// Canonical history repeated a stable entry identity.
    #[error("canonical history contains a duplicate entry identity")]
    DuplicateEntry,
    /// Canonical JSON construction failed.
    #[error("canonical history serialization failed: {0}")]
    Serialization(String),
    /// A content identity could not be computed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// Serialized content disagreed with its declared identity.
    #[error("serialized {field} identity does not match canonical content")]
    IntegrityMismatch {
        /// The semantic value whose identity was invalid.
        field: &'static str,
    },
    /// A requested projection prefix exceeded the complete item projection.
    #[error("conversation prefix exceeds the model-visible projection")]
    PrefixOutsideProjection,
}

/// The provider-neutral semantic role of one atomic history segment.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum HistorySegmentKind {
    /// Ordinary conversation content without protected blocks.
    Conversation,
    /// The recent user intent protected by context planning.
    RecentIntent,
    /// User steering accepted at a safe boundary.
    Steering,
    /// A complete tool-call/result exchange protected as one unit.
    ToolExchange,
    /// Reasoning and opaque replay material protected as one unit.
    ReasoningReplay,
    /// One committed compaction output protected as one unit.
    Compaction,
}

impl HistorySegmentKind {
    const fn tag(self) -> u8 {
        match self {
            Self::Conversation => 0,
            Self::RecentIntent => 1,
            Self::Steering => 2,
            Self::ToolExchange => 3,
            Self::ReasoningReplay => 4,
            Self::Compaction => 5,
        }
    }
}

#[derive(Debug, PartialEq)]
struct HistorySegmentInner {
    kind: HistorySegmentKind,
    items: Arc<[ConversationItem]>,
    identity: Digest,
    canonical_bytes: Arc<[u8]>,
    item_identities: Arc<[Digest]>,
    model_visible_complete: bool,
}

/// One immutable, independently shared semantic history group.
#[derive(Clone, Debug, PartialEq)]
pub struct HistorySegment {
    inner: Arc<HistorySegmentInner>,
}

#[derive(Serialize)]
struct HistorySegmentContent<'a> {
    kind: HistorySegmentKind,
    items: &'a [ConversationItem],
}

#[derive(Deserialize, Serialize)]
struct HistorySegmentWire {
    kind: HistorySegmentKind,
    items: Vec<ConversationItem>,
    identity: Digest,
}

#[derive(Serialize)]
struct HistorySegmentWireRef<'a> {
    kind: HistorySegmentKind,
    items: &'a [ConversationItem],
    identity: Digest,
}

impl HistorySegment {
    /// Creates and validates one complete semantic group.
    ///
    /// # Errors
    ///
    /// Returns an error for empty or oversized content, a mismatched kind, or
    /// an incomplete tool-call/result group.
    pub fn new(
        kind: HistorySegmentKind,
        items: Vec<ConversationItem>,
    ) -> Result<Self, HistoryError> {
        validate_segment(kind, &items)?;
        Self::freeze(kind, items, true)
    }

    /// Decodes exact canonical segment content, proving canonicality once.
    ///
    /// # Errors
    /// Returns an error for invalid semantic content or noncanonical bytes.
    /// A malformed canonical envelope is an integrity mismatch; malformed item
    /// JSON and invalid model content retain their serialization/semantic errors.
    pub fn from_canonical(bytes: &[u8]) -> Result<Self, HistoryError> {
        if bytes.len() > MAX_SEGMENT_BYTES {
            return Err(HistoryError::LimitExceeded {
                field: "history segment bytes",
                maximum: MAX_SEGMENT_BYTES,
            });
        }
        let noncanonical = || HistoryError::IntegrityMismatch {
            field: "history segment canonical bytes",
        };
        // JCS orders `items` before `kind`. Decode items directly from that
        // envelope and retain their consumed slices, rather than parsing a
        // Value tree and then deserializing it a second time.
        let mut remaining = bytes
            .strip_prefix(b"{\"items\":[")
            .ok_or_else(noncanonical)?;
        let mut items = Vec::new();
        let mut item_bytes = Vec::new();
        if !remaining.starts_with(b"]") {
            loop {
                if items.len() == MAX_SEGMENT_ITEMS {
                    return Err(HistoryError::LimitExceeded {
                        field: "history segment items",
                        maximum: MAX_SEGMENT_ITEMS,
                    });
                }
                let mut stream =
                    serde_json::Deserializer::from_slice(remaining).into_iter::<ConversationItem>();
                let item = stream
                    .next()
                    .ok_or_else(noncanonical)?
                    .map_err(|error| HistoryError::Serialization(error.to_string()))?;
                let consumed = stream.byte_offset();
                item_bytes.push(&remaining[..consumed]);
                items.push(item);
                remaining = &remaining[consumed..];
                if let Some(rest) = remaining.strip_prefix(b",") {
                    remaining = rest;
                } else {
                    break;
                }
            }
        }
        let kind_bytes = remaining
            .strip_prefix(b"],\"kind\":")
            .and_then(|rest| rest.strip_suffix(b"}"))
            .ok_or_else(noncanonical)?;
        let kind: HistorySegmentKind = serde_json::from_slice(kind_bytes)
            .map_err(|error| HistoryError::Serialization(error.to_string()))?;
        validate_segment(kind, &items)?;
        let value = serde_json::to_value(HistorySegmentContent {
            kind,
            items: &items,
        })
        .map_err(|error| HistoryError::Serialization(error.to_string()))?;
        let canonical = canonicalize_prevalidated(&value, MAX_SEGMENT_BYTES)
            .map_err(|error| HistoryError::Serialization(error.to_string()))?;
        if canonical != bytes {
            return Err(noncanonical());
        }
        // Each consumed item slice inherits the one whole-content proof.
        let item_identities = item_bytes
            .into_iter()
            .map(|bytes| {
                Digest::sha256(
                    DigestDomain::HistoryProjection,
                    4,
                    &[b"plakous-model/semantic-projection-item", bytes],
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        let identity = Digest::sha256(
            DigestDomain::CanonicalHistorySegment,
            2,
            &[&[kind.tag()], bytes],
        )?;
        Ok(Self {
            inner: Arc::new(HistorySegmentInner {
                kind,
                items: items.into(),
                identity,
                canonical_bytes: bytes.into(),
                item_identities: item_identities.into(),
                model_visible_complete: true,
            }),
        })
    }

    fn inferred(items: Vec<ConversationItem>) -> Result<Self, HistoryError> {
        if items.is_empty() {
            return Err(HistoryError::EmptySegment);
        }
        let shape = inspect_items(&items);
        let kind = if shape.has_tool {
            HistorySegmentKind::ToolExchange
        } else if shape.has_reasoning_replay {
            HistorySegmentKind::ReasoningReplay
        } else if shape.has_compaction {
            HistorySegmentKind::Compaction
        } else if items.last().is_some_and(|item| item.actor() == Actor::User) {
            HistorySegmentKind::RecentIntent
        } else {
            HistorySegmentKind::Conversation
        };
        validate_segment_bounds(&items)?;
        Self::freeze(kind, items, !shape.has_tool || shape.tool_exchange_complete)
    }

    fn freeze(
        kind: HistorySegmentKind,
        items: Vec<ConversationItem>,
        model_visible_complete: bool,
    ) -> Result<Self, HistoryError> {
        let value = serde_json::to_value(HistorySegmentContent {
            kind,
            items: &items,
        })
        .map_err(|error| HistoryError::Serialization(error.to_string()))?;
        let canonical_bytes = canonicalize_prevalidated(&value, MAX_SEGMENT_BYTES)
            .map_err(|error| HistoryError::Serialization(error.to_string()))?;
        if canonical_bytes.len() > MAX_SEGMENT_BYTES {
            return Err(HistoryError::LimitExceeded {
                field: "history segment bytes",
                maximum: MAX_SEGMENT_BYTES,
            });
        }
        let identity = Digest::sha256(
            DigestDomain::CanonicalHistorySegment,
            2,
            &[&[kind.tag()], &canonical_bytes],
        )?;
        let item_identities = items
            .iter()
            .map(projection_item_identity)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            inner: Arc::new(HistorySegmentInner {
                kind,
                items: items.into(),
                identity,
                canonical_bytes: canonical_bytes.into(),
                item_identities: item_identities.into(),
                model_visible_complete,
            }),
        })
    }

    /// Returns the semantic role of this atomic group.
    #[must_use]
    pub fn kind(&self) -> HistorySegmentKind {
        self.inner.kind
    }

    /// Returns the ordered canonical items in this group.
    #[must_use]
    pub fn items(&self) -> &[ConversationItem] {
        &self.inner.items
    }

    /// Returns the versioned identity of the canonical segment bytes.
    #[must_use]
    pub fn identity(&self) -> Digest {
        self.inner.identity
    }

    /// Returns the exact deterministic bytes bound by [`Self::identity`].
    #[must_use]
    pub fn canonical_bytes(&self) -> &[u8] {
        &self.inner.canonical_bytes
    }

    fn item_identities(&self) -> &[Digest] {
        &self.inner.item_identities
    }

    /// Returns a conservative retained-memory bound for this segment.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.inner
            .canonical_bytes
            .len()
            .saturating_mul(2)
            .saturating_add(size_of::<HistorySegmentInner>())
            .saturating_add(
                self.inner
                    .item_identities
                    .len()
                    .saturating_mul(size_of::<Digest>()),
            )
            .saturating_add(size_of::<Arc<[Digest]>>())
            .saturating_add(size_of::<usize>().saturating_mul(2))
    }

    /// Reports whether two handles share the exact immutable segment allocation.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    fn model_visible_complete(&self) -> bool {
        self.inner.model_visible_complete
    }
}

impl Serialize for HistorySegment {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        HistorySegmentWireRef {
            kind: self.kind(),
            items: self.items(),
            identity: self.identity(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HistorySegment {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = HistorySegmentWire::deserialize(deserializer)?;
        let segment = Self::new(wire.kind, wire.items).map_err(serde::de::Error::custom)?;
        if segment.identity() != wire.identity {
            return Err(serde::de::Error::custom(HistoryError::IntegrityMismatch {
                field: "history segment",
            }));
        }
        Ok(segment)
    }
}

#[derive(Debug)]
enum HistoryTreeKind {
    Leaf(HistorySegment),
    Branch {
        left: Arc<HistoryTree>,
        right: Arc<HistoryTree>,
    },
}

#[derive(Debug)]
struct HistoryTree {
    segment_count: usize,
    kind: HistoryTreeKind,
}

impl HistoryTree {
    fn leaf(segment: HistorySegment) -> Arc<Self> {
        Arc::new(Self {
            segment_count: 1,
            kind: HistoryTreeKind::Leaf(segment),
        })
    }

    fn branch(left: Arc<Self>, right: Arc<Self>) -> Arc<Self> {
        debug_assert_eq!(left.segment_count, right.segment_count);
        Arc::new(Self {
            segment_count: left.segment_count.saturating_add(right.segment_count),
            kind: HistoryTreeKind::Branch { left, right },
        })
    }

    fn get(&self, index: usize) -> Option<&HistorySegment> {
        match &self.kind {
            HistoryTreeKind::Leaf(segment) => (index == 0).then_some(segment),
            HistoryTreeKind::Branch { left, right } => {
                if index < left.segment_count {
                    left.get(index)
                } else {
                    right.get(index.saturating_sub(left.segment_count))
                }
            }
        }
    }
}

#[derive(Debug)]
struct EntryNode {
    key: EntryId,
    height: u8,
    left: Option<Arc<EntryNode>>,
    right: Option<Arc<EntryNode>>,
}

impl EntryNode {
    fn height(node: Option<&Self>) -> u8 {
        node.map_or(0, |node| node.height)
    }

    fn make(
        key: EntryId,
        left: Option<Arc<Self>>,
        right: Option<Arc<Self>>,
        nodes_created: &mut usize,
    ) -> Arc<Self> {
        *nodes_created = nodes_created.saturating_add(1);
        Arc::new(Self {
            key,
            height: Self::height(left.as_deref())
                .max(Self::height(right.as_deref()))
                .saturating_add(1),
            left,
            right,
        })
    }

    fn insert(
        node: Option<&Self>,
        key: EntryId,
        nodes_created: &mut usize,
    ) -> Result<Arc<Self>, HistoryError> {
        let Some(node) = node else {
            return Ok(Self::make(key, None, None, nodes_created));
        };
        match key.cmp(&node.key) {
            std::cmp::Ordering::Less => {
                let left = Some(Self::insert(node.left.as_deref(), key, nodes_created)?);
                Ok(Self::balance(
                    node.key,
                    left,
                    node.right.clone(),
                    nodes_created,
                ))
            }
            std::cmp::Ordering::Greater => {
                let right = Some(Self::insert(node.right.as_deref(), key, nodes_created)?);
                Ok(Self::balance(
                    node.key,
                    node.left.clone(),
                    right,
                    nodes_created,
                ))
            }
            std::cmp::Ordering::Equal => Err(HistoryError::DuplicateEntry),
        }
    }

    fn contains(node: Option<&Self>, key: EntryId) -> bool {
        let Some(node) = node else {
            return false;
        };
        match key.cmp(&node.key) {
            std::cmp::Ordering::Less => Self::contains(node.left.as_deref(), key),
            std::cmp::Ordering::Greater => Self::contains(node.right.as_deref(), key),
            std::cmp::Ordering::Equal => true,
        }
    }

    fn balance(
        key: EntryId,
        left: Option<Arc<Self>>,
        right: Option<Arc<Self>>,
        nodes_created: &mut usize,
    ) -> Arc<Self> {
        let skew =
            i16::from(Self::height(left.as_deref())) - i16::from(Self::height(right.as_deref()));
        if skew > 1 {
            return Self::rotate_right(key, left, right, nodes_created);
        }
        if skew < -1 {
            return Self::rotate_left(key, left, right, nodes_created);
        }
        Self::make(key, left, right, nodes_created)
    }

    #[allow(
        clippy::expect_used,
        reason = "an AVL skew greater than one proves the required child exists"
    )]
    fn rotate_right(
        key: EntryId,
        left: Option<Arc<Self>>,
        right: Option<Arc<Self>>,
        nodes_created: &mut usize,
    ) -> Arc<Self> {
        let pivot = left.expect("left-heavy AVL node has a left child");
        if Self::height(pivot.left.as_deref()) >= Self::height(pivot.right.as_deref()) {
            let new_right = Self::make(key, pivot.right.clone(), right, nodes_created);
            return Self::make(
                pivot.key,
                pivot.left.clone(),
                Some(new_right),
                nodes_created,
            );
        }
        let middle = pivot
            .right
            .as_ref()
            .expect("left-right AVL rotation has a middle child");
        let new_left = Self::make(
            pivot.key,
            pivot.left.clone(),
            middle.left.clone(),
            nodes_created,
        );
        let new_right = Self::make(key, middle.right.clone(), right, nodes_created);
        Self::make(middle.key, Some(new_left), Some(new_right), nodes_created)
    }

    #[allow(
        clippy::expect_used,
        reason = "an AVL skew less than minus one proves the required child exists"
    )]
    fn rotate_left(
        key: EntryId,
        left: Option<Arc<Self>>,
        right: Option<Arc<Self>>,
        nodes_created: &mut usize,
    ) -> Arc<Self> {
        let pivot = right.expect("right-heavy AVL node has a right child");
        if Self::height(pivot.right.as_deref()) >= Self::height(pivot.left.as_deref()) {
            let new_left = Self::make(key, left, pivot.left.clone(), nodes_created);
            return Self::make(
                pivot.key,
                Some(new_left),
                pivot.right.clone(),
                nodes_created,
            );
        }
        let middle = pivot
            .left
            .as_ref()
            .expect("right-left AVL rotation has a middle child");
        let new_left = Self::make(key, left, middle.left.clone(), nodes_created);
        let new_right = Self::make(
            pivot.key,
            middle.right.clone(),
            pivot.right.clone(),
            nodes_created,
        );
        Self::make(middle.key, Some(new_left), Some(new_right), nodes_created)
    }
}

#[derive(Clone, Debug)]
struct CanonicalHistoryInner {
    roots: Arc<[Arc<HistoryTree>]>,
    entries: Option<Arc<EntryNode>>,
    parent: Option<Arc<CanonicalHistoryInner>>,
    appended_segment: Option<HistorySegment>,
    identity: Digest,
    segment_count: usize,
    item_count: usize,
    segment_retained_bytes: usize,
    tree_node_count: usize,
}

impl Drop for CanonicalHistoryInner {
    fn drop(&mut self) {
        let mut parent = self.parent.take();
        while let Some(link) = parent {
            let Ok(mut node) = Arc::try_unwrap(link) else {
                break;
            };
            parent = node.parent.take();
        }
    }
}

/// Immutable provider-neutral canonical history composed of shared groups.
#[derive(Clone, Debug)]
pub struct CanonicalHistory {
    inner: Arc<CanonicalHistoryInner>,
}

#[derive(Deserialize)]
struct CanonicalHistoryWire {
    segments: Vec<HistorySegment>,
    identity: Digest,
}

#[derive(Serialize)]
struct CanonicalHistoryWireRef<'a> {
    segments: CanonicalSegments<'a>,
    identity: Digest,
}

#[derive(Default)]
#[allow(
    dead_code,
    reason = "test-only observations measure the production append path without instrumentation globals"
)]
struct AppendWork {
    root_handles_copied: usize,
    tree_nodes_created: usize,
    entry_nodes_created: usize,
}

impl CanonicalHistory {
    /// Creates canonical history from ordered, independently validated groups.
    ///
    /// # Errors
    ///
    /// Returns an error for excessive segments, duplicate entry identities, or
    /// digest construction failure.
    pub fn new(segments: Vec<HistorySegment>) -> Result<Self, HistoryError> {
        if segments.len() > MAX_HISTORY_SEGMENTS {
            return Err(HistoryError::LimitExceeded {
                field: "canonical history segments",
                maximum: MAX_HISTORY_SEGMENTS,
            });
        }
        let mut history = Self::empty()?;
        for segment in segments {
            history = history.append(segment)?;
        }
        Ok(history)
    }

    /// Appends one group while sharing every prior segment payload.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::new`].
    pub fn append(&self, segment: HistorySegment) -> Result<Self, HistoryError> {
        self.append_with_work(segment)
            .map(|(history, _work)| history)
    }

    /// Returns ordered shared semantic groups.
    #[must_use]
    pub const fn segments(&self) -> CanonicalSegments<'_> {
        CanonicalSegments { history: self }
    }

    /// Returns the total conversation item count.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.inner.item_count
    }

    /// Reports whether canonical history is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.segment_count == 0
    }

    /// Returns the history identity derived from ordered segment identities.
    #[must_use]
    pub fn identity(&self) -> Digest {
        self.inner.identity
    }

    /// Recomputes the exact append-chain identity for a segment prefix.
    ///
    /// This is a cold reconstructive check for compact provenance and does not
    /// allocate or copy segment content.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested prefix exceeds this history.
    pub fn segment_prefix_identity(&self, segment_count: usize) -> Result<Digest, HistoryError> {
        if segment_count > self.inner.segment_count {
            return Err(HistoryError::PrefixOutsideProjection);
        }
        let mut identity = Digest::sha256(DigestDomain::CanonicalHistory, 3, &[b"empty"])?;
        for segment in self.segments().iter().take(segment_count) {
            identity = Digest::sha256(
                DigestDomain::CanonicalHistory,
                3,
                &[b"append", identity.bytes(), segment.identity().bytes()],
            )?;
        }
        Ok(identity)
    }

    /// Returns a conservative retained-memory bound without flattening history.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.inner
            .segment_retained_bytes
            .saturating_add(
                self.inner
                    .tree_node_count
                    .saturating_mul(size_of::<HistoryTree>()),
            )
            .saturating_add(self.inner.item_count.saturating_mul(size_of::<EntryNode>()))
            .saturating_add(
                self.inner
                    .roots
                    .len()
                    .saturating_mul(size_of::<Arc<HistoryTree>>()),
            )
            .saturating_add(size_of::<CanonicalHistoryInner>())
    }

    /// Reports whether another handle shares the exact history allocation.
    #[must_use]
    pub fn shares_storage_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Returns the append-only suffix shared after an exact ancestor allocation.
    ///
    /// This is a cold persistence optimization proof, not reconstruction data:
    /// callers still retain every returned segment's exact canonical bytes.
    #[doc(hidden)]
    #[must_use]
    pub fn append_delta_since(&self, ancestor: &Self) -> Option<Vec<HistorySegment>> {
        let mut current = self.inner.clone();
        let mut reverse = Vec::new();
        while current.segment_count > ancestor.inner.segment_count {
            reverse.push(current.appended_segment.clone()?);
            current = current.parent.clone()?;
        }
        if !Arc::ptr_eq(&current, &ancestor.inner) {
            return None;
        }
        reverse.reverse();
        Some(reverse)
    }

    fn empty() -> Result<Self, HistoryError> {
        let identity = Digest::sha256(DigestDomain::CanonicalHistory, 3, &[b"empty"])?;
        Ok(Self {
            inner: Arc::new(CanonicalHistoryInner {
                roots: Arc::new([]),
                entries: None,
                parent: None,
                appended_segment: None,
                identity,
                segment_count: 0,
                item_count: 0,
                segment_retained_bytes: 0,
                tree_node_count: 0,
            }),
        })
    }

    fn append_with_work(
        &self,
        segment: HistorySegment,
    ) -> Result<(Self, AppendWork), HistoryError> {
        if self.inner.segment_count >= MAX_HISTORY_SEGMENTS {
            return Err(HistoryError::LimitExceeded {
                field: "canonical history segments",
                maximum: MAX_HISTORY_SEGMENTS,
            });
        }

        let mut work = AppendWork::default();
        let mut entries = self.inner.entries.clone();
        for item in segment.items() {
            entries = Some(EntryNode::insert(
                entries.as_deref(),
                item.id(),
                &mut work.entry_nodes_created,
            )?);
        }
        let segment_identity = segment.identity();
        let appended_segment = segment.clone();
        let segment_item_count = segment.items().len();
        let segment_retained_bytes = segment.retained_bytes_upper_bound();

        work.root_handles_copied = self.inner.roots.len();
        let mut roots = Vec::with_capacity(self.inner.roots.len().saturating_add(1));
        roots.extend(self.inner.roots.iter().cloned());
        let mut carry = HistoryTree::leaf(segment);
        work.tree_nodes_created = 1;
        while roots
            .last()
            .is_some_and(|root| root.segment_count == carry.segment_count)
        {
            let left = roots.pop().ok_or(HistoryError::IntegrityMismatch {
                field: "canonical history forest",
            })?;
            carry = HistoryTree::branch(left, carry);
            work.tree_nodes_created = work.tree_nodes_created.saturating_add(1);
        }
        roots.push(carry);

        let identity = Digest::sha256(
            DigestDomain::CanonicalHistory,
            3,
            &[
                b"append",
                self.inner.identity.bytes(),
                segment_identity.bytes(),
            ],
        )?;
        let history = Self {
            inner: Arc::new(CanonicalHistoryInner {
                roots: roots.into(),
                entries,
                parent: Some(self.inner.clone()),
                appended_segment: Some(appended_segment),
                identity,
                segment_count: self.inner.segment_count.saturating_add(1),
                item_count: self.inner.item_count.saturating_add(segment_item_count),
                segment_retained_bytes: self
                    .inner
                    .segment_retained_bytes
                    .saturating_add(segment_retained_bytes),
                tree_node_count: self
                    .inner
                    .tree_node_count
                    .saturating_add(work.tree_nodes_created),
            }),
        };
        Ok((history, work))
    }
}

impl PartialEq for CanonicalHistory {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
            && self.inner.segment_count == other.inner.segment_count
            && self.segments().iter().eq(other.segments())
    }
}

/// A zero-copy ordered collection view over persistent canonical segments.
#[derive(Clone, Copy)]
pub struct CanonicalSegments<'a> {
    history: &'a CanonicalHistory,
}

impl fmt::Debug for CanonicalSegments<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> CanonicalSegments<'a> {
    /// Returns the segment count without walking the persistent forest.
    #[must_use]
    pub fn len(self) -> usize {
        self.history.inner.segment_count
    }

    /// Reports whether the history contains no segments.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates over shared segments in canonical order.
    #[must_use]
    pub fn iter(self) -> CanonicalSegmentIter<'a> {
        CanonicalSegmentIter {
            roots: self.history.inner.roots.iter(),
            stack: [None; HISTORY_ITER_STACK_CAPACITY],
            stack_len: 0,
            remaining: self.len(),
        }
    }

    /// Returns a shared segment by canonical position.
    #[must_use]
    pub fn get(self, mut index: usize) -> Option<&'a HistorySegment> {
        for root in self.history.inner.roots.iter() {
            if index < root.segment_count {
                return root.get(index);
            }
            index = index.saturating_sub(root.segment_count);
        }
        None
    }

    /// Explicitly materializes cloned segment handles for cold-path consumers.
    #[must_use]
    pub fn to_vec(self) -> Vec<HistorySegment> {
        self.iter().cloned().collect()
    }
}

impl Serialize for CanonicalSegments<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut sequence = serializer.serialize_seq(Some(self.len()))?;
        for segment in *self {
            sequence.serialize_element(segment)?;
        }
        sequence.end()
    }
}

impl<'a> IntoIterator for CanonicalSegments<'a> {
    type Item = &'a HistorySegment;
    type IntoIter = CanonicalSegmentIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Index<usize> for CanonicalSegments<'_> {
    type Output = HistorySegment;

    #[allow(
        clippy::expect_used,
        reason = "Index follows the standard collection panic contract for an out-of-bounds index"
    )]
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .expect("canonical history segment index out of bounds")
    }
}

/// Exact-size iterator over persistent canonical history segments.
#[derive(Clone)]
pub struct CanonicalSegmentIter<'a> {
    roots: slice::Iter<'a, Arc<HistoryTree>>,
    stack: [Option<&'a HistoryTree>; HISTORY_ITER_STACK_CAPACITY],
    stack_len: usize,
    remaining: usize,
}

impl<'a> CanonicalSegmentIter<'a> {
    fn push(&mut self, tree: &'a HistoryTree) {
        debug_assert!(self.stack_len < self.stack.len());
        if let Some(slot) = self.stack.get_mut(self.stack_len) {
            *slot = Some(tree);
            self.stack_len = self.stack_len.saturating_add(1);
        }
    }

    fn pop(&mut self) -> Option<&'a HistoryTree> {
        self.stack_len = self.stack_len.checked_sub(1)?;
        self.stack.get_mut(self.stack_len)?.take()
    }
}

impl<'a> Iterator for CanonicalSegmentIter<'a> {
    type Item = &'a HistorySegment;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(tree) = self.pop() {
                match &tree.kind {
                    HistoryTreeKind::Leaf(segment) => {
                        self.remaining = self.remaining.saturating_sub(1);
                        return Some(segment);
                    }
                    HistoryTreeKind::Branch { left, right } => {
                        self.push(right);
                        self.push(left);
                    }
                }
            } else {
                let root = self.roots.next()?;
                self.push(root);
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for CanonicalSegmentIter<'_> {}

impl Default for CanonicalHistory {
    #[allow(
        clippy::expect_used,
        reason = "an empty history cannot violate any constructor invariant"
    )]
    fn default() -> Self {
        Self::new(Vec::new()).expect("the empty canonical history is valid")
    }
}

impl Serialize for CanonicalHistory {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        CanonicalHistoryWireRef {
            segments: self.segments(),
            identity: self.identity(),
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CanonicalHistory {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CanonicalHistoryWire::deserialize(deserializer)?;
        let history = Self::new(wire.segments).map_err(serde::de::Error::custom)?;
        if history.identity() != wire.identity {
            return Err(serde::de::Error::custom(HistoryError::IntegrityMismatch {
                field: "canonical history",
            }));
        }
        Ok(history)
    }
}

#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "these independent semantic-content observations validate combinations rather than encode a state machine"
)]
struct SegmentShape {
    has_tool: bool,
    has_reasoning_replay: bool,
    has_compaction: bool,
    tool_exchange_complete: bool,
}

fn inspect_items(items: &[ConversationItem]) -> SegmentShape {
    let mut calls = BTreeSet::new();
    let mut results = BTreeSet::new();
    let mut duplicate_call = false;
    let mut duplicate_result = false;
    let mut shape = SegmentShape::default();
    for block in items.iter().flat_map(ConversationItem::blocks) {
        match block {
            ContentBlock::ToolCall(call) => {
                shape.has_tool = true;
                duplicate_call |= !calls.insert(call.call_id());
            }
            ContentBlock::ToolResult(result) => {
                shape.has_tool = true;
                duplicate_result |= !results.insert(result.call_id());
            }
            ContentBlock::ToolResultUpdate(_) => shape.has_tool = true,
            ContentBlock::Reasoning(_) | ContentBlock::Opaque(_) => {
                shape.has_reasoning_replay = true;
            }
            ContentBlock::Compaction(_) => shape.has_compaction = true,
            _ => {}
        }
    }
    shape.tool_exchange_complete = shape.has_tool
        && !duplicate_call
        && !duplicate_result
        && !calls.is_empty()
        && calls == results;
    shape
}

fn validate_segment(
    kind: HistorySegmentKind,
    items: &[ConversationItem],
) -> Result<(), HistoryError> {
    validate_segment_bounds(items)?;
    let shape = inspect_items(items);
    if shape.has_tool {
        if kind != HistorySegmentKind::ToolExchange {
            return Err(HistoryError::KindMismatch);
        }
        if !shape.tool_exchange_complete {
            return Err(HistoryError::IncompleteToolExchange);
        }
    } else if kind == HistorySegmentKind::ToolExchange {
        return Err(HistoryError::KindMismatch);
    }
    if shape.has_reasoning_replay
        && !matches!(
            kind,
            HistorySegmentKind::ReasoningReplay | HistorySegmentKind::ToolExchange
        )
    {
        return Err(HistoryError::KindMismatch);
    }
    if kind == HistorySegmentKind::ReasoningReplay && !shape.has_reasoning_replay {
        return Err(HistoryError::KindMismatch);
    }
    if shape.has_compaction && kind != HistorySegmentKind::Compaction {
        return Err(HistoryError::KindMismatch);
    }
    if kind == HistorySegmentKind::Compaction && !shape.has_compaction {
        return Err(HistoryError::KindMismatch);
    }
    if matches!(
        kind,
        HistorySegmentKind::RecentIntent | HistorySegmentKind::Steering
    ) && (shape.has_tool
        || shape.has_reasoning_replay
        || shape.has_compaction
        || !items.iter().all(|item| item.actor() == Actor::User))
    {
        return Err(HistoryError::KindMismatch);
    }
    Ok(())
}

fn validate_segment_bounds(items: &[ConversationItem]) -> Result<(), HistoryError> {
    if items.is_empty() {
        return Err(HistoryError::EmptySegment);
    }
    if items.len() > MAX_SEGMENT_ITEMS {
        return Err(HistoryError::LimitExceeded {
            field: "history segment items",
            maximum: MAX_SEGMENT_ITEMS,
        });
    }
    Ok(())
}

fn projection_item_identity(item: &ConversationItem) -> Result<Digest, HistoryError> {
    let value = serde_json::to_value(item)
        .map_err(|error| HistoryError::Serialization(error.to_string()))?;
    let bytes = canonicalize_normalized_prevalidated(value, MAX_SEGMENT_BYTES)
        .map_err(|error| HistoryError::Serialization(error.to_string()))?;
    Ok(Digest::sha256(
        DigestDomain::HistoryProjection,
        4,
        &[b"plakous-model/semantic-projection-item", &bytes],
    )?)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for fixture clarity"
)]
mod tests;
