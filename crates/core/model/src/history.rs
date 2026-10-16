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

/// Explicit bounds for uncommitted model-visible working content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(
    clippy::struct_field_names,
    reason = "the max prefix makes each serialized resource bound unambiguous"
)]
pub struct WorkingProjectionLimits {
    max_segments: usize,
    max_items: usize,
    max_retained_bytes: usize,
}

impl WorkingProjectionLimits {
    /// Creates nonzero working projection bounds.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::InvalidLimits`] when any bound is zero.
    pub const fn new(
        max_segments: usize,
        max_items: usize,
        max_retained_bytes: usize,
    ) -> Result<Self, HistoryError> {
        if max_segments == 0 || max_items == 0 || max_retained_bytes == 0 {
            return Err(HistoryError::InvalidLimits);
        }
        Ok(Self {
            max_segments,
            max_items,
            max_retained_bytes,
        })
    }

    /// Returns the maximum semantic group count.
    #[must_use]
    pub const fn max_segments(self) -> usize {
        self.max_segments
    }

    /// Returns the maximum conversation item count.
    #[must_use]
    pub const fn max_items(self) -> usize {
        self.max_items
    }

    /// Returns the maximum conservative retained-byte count.
    #[must_use]
    pub const fn max_retained_bytes(self) -> usize {
        self.max_retained_bytes
    }
}

impl Default for WorkingProjectionLimits {
    fn default() -> Self {
        Self {
            max_segments: 64,
            max_items: 4_096,
            max_retained_bytes: 16 * 1024 * 1024,
        }
    }
}

/// A bounded immutable projection of uncommitted model-visible groups.
#[derive(Clone, Debug, PartialEq)]
pub struct WorkingProjection {
    segments: Arc<[HistorySegment]>,
    limits: WorkingProjectionLimits,
    item_count: usize,
    retained_bytes: usize,
}

#[derive(Deserialize, Serialize)]
struct WorkingProjectionWire {
    segments: Vec<HistorySegment>,
    limits: WorkingProjectionLimits,
    item_count: usize,
    retained_bytes: usize,
}

impl WorkingProjection {
    /// Creates a bounded working projection from complete semantic groups.
    ///
    /// # Errors
    ///
    /// Returns an error when a configured count or retained-byte bound is exceeded.
    pub fn new(
        segments: Vec<HistorySegment>,
        limits: WorkingProjectionLimits,
    ) -> Result<Self, HistoryError> {
        let item_count = segments.iter().fold(0_usize, |total, segment| {
            total.saturating_add(segment.items().len())
        });
        let retained_bytes = segments.iter().fold(0_usize, |total, segment| {
            total.saturating_add(segment.retained_bytes_upper_bound())
        });
        for (field, actual, maximum) in [
            (
                "working projection segments",
                segments.len(),
                limits.max_segments,
            ),
            ("working projection items", item_count, limits.max_items),
            (
                "working projection retained bytes",
                retained_bytes,
                limits.max_retained_bytes,
            ),
        ] {
            if actual > maximum {
                return Err(HistoryError::LimitExceeded { field, maximum });
            }
        }
        Ok(Self {
            segments: segments.into(),
            limits,
            item_count,
            retained_bytes,
        })
    }

    /// Returns ordered working groups.
    #[must_use]
    pub fn segments(&self) -> &[HistorySegment] {
        &self.segments
    }

    /// Returns the bounded working item count.
    #[must_use]
    pub const fn item_count(&self) -> usize {
        self.item_count
    }

    /// Returns the exact limits applied to this projection.
    #[must_use]
    pub const fn limits(&self) -> WorkingProjectionLimits {
        self.limits
    }

    /// Returns the conservative retained-byte count checked at construction.
    #[must_use]
    pub const fn retained_bytes_upper_bound(&self) -> usize {
        self.retained_bytes
    }
}

impl Default for WorkingProjection {
    #[allow(
        clippy::expect_used,
        reason = "an empty projection cannot exceed nonzero default limits"
    )]
    fn default() -> Self {
        Self::new(Vec::new(), WorkingProjectionLimits::default())
            .expect("the empty working projection is valid")
    }
}

impl Serialize for WorkingProjection {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        WorkingProjectionWire {
            segments: self.segments().to_vec(),
            limits: self.limits,
            item_count: self.item_count,
            retained_bytes: self.retained_bytes,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WorkingProjection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkingProjectionWire::deserialize(deserializer)?;
        let projection = Self::new(wire.segments, wire.limits).map_err(serde::de::Error::custom)?;
        if projection.item_count != wire.item_count
            || projection.retained_bytes != wire.retained_bytes
        {
            return Err(serde::de::Error::custom(HistoryError::IntegrityMismatch {
                field: "working projection",
            }));
        }
        Ok(projection)
    }
}

/// An immutable segmented model-visible projection.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConversationView {
    history: CanonicalHistory,
    working: WorkingProjection,
    identity: Digest,
}

/// Compact identity of one segment-boundary-aware semantic prefix.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConversationPrefixIdentity {
    item_count: usize,
    identity: Digest,
}

/// Compact identity of ordered canonical items only.
///
/// This identity is reserved for family-artifact source and coverage facts.
/// Unlike [`ConversationPrefixIdentity`], completing or regrouping semantic
/// segments cannot change it. It is therefore not suitable for token,
/// protected-group, or complete [`ConversationView`] identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanonicalItemPrefixIdentity {
    item_count: usize,
    identity: Digest,
}

impl CanonicalItemPrefixIdentity {
    /// Returns the ordered canonical item count covered by this prefix.
    #[must_use]
    pub const fn item_count(self) -> usize {
        self.item_count
    }

    /// Returns the revisioned ordered-item prefix identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

impl ConversationPrefixIdentity {
    /// Returns the complete canonical item count covered by this prefix.
    #[must_use]
    pub const fn item_count(self) -> usize {
        self.item_count
    }

    /// Returns the folded identity of the covered complete items and segment boundaries.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

#[derive(Deserialize)]
struct ConversationViewWire {
    history: CanonicalHistory,
    working: WorkingProjection,
    identity: Digest,
}

impl ConversationView {
    /// Creates a one-segment projection from ordered items.
    ///
    /// Canonical owners use [`Self::from_shared`]. The inferred segment is
    /// atomic and protected according to its content; request construction
    /// rejects an incomplete inferred tool exchange.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`] when the items exceed the segment bounds,
    /// repeat an entry identity, or cannot be serialized canonically.
    pub fn from_items(items: Vec<ConversationItem>) -> Result<Self, HistoryError> {
        if items.is_empty() {
            return Ok(Self::default());
        }
        let segment = HistorySegment::inferred(items)?;
        let history = CanonicalHistory::new(vec![segment])?;
        Ok(Self::from_shared(history, WorkingProjection::default()))
    }

    /// Projects shared canonical and bounded working state without cloning item payloads.
    #[must_use]
    #[allow(
        clippy::expect_used,
        clippy::missing_panics_doc,
        reason = "the fixed nonzero digest revision makes identity construction infallible"
    )]
    pub fn from_shared(history: CanonicalHistory, working: WorkingProjection) -> Self {
        let identity =
            fold_projection_segments(history.segments().iter().chain(working.segments()))
                .expect("history projection uses a nonzero digest version");
        Self {
            history,
            working,
            identity,
        }
    }

    /// Returns a zero-copy ordered view over committed and working items.
    #[must_use]
    pub fn items(&self) -> ConversationItems<'_> {
        ConversationItems { view: self }
    }

    /// Returns committed canonical history.
    #[must_use]
    pub const fn history(&self) -> &CanonicalHistory {
        &self.history
    }

    /// Returns the bounded working projection.
    #[must_use]
    pub const fn working(&self) -> &WorkingProjection {
        &self.working
    }

    /// Returns the identity of ordered committed and working semantic content.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Identifies an exact model-visible prefix without flattening or serializing items.
    ///
    /// Prefixes may end after any complete [`ConversationItem`], including
    /// between provider output and its later result inside an atomic tool
    /// exchange. Segment boundaries and kinds remain part of the identity;
    /// accepting an item prefix does not make that prefix a mutable checkpoint.
    ///
    /// # Errors
    ///
    /// Returns a typed error when `item_count` exceeds this view.
    pub fn semantic_prefix_identity(
        &self,
        item_count: usize,
    ) -> Result<ConversationPrefixIdentity, HistoryError> {
        let total = self.items().len();
        if item_count > total {
            return Err(HistoryError::PrefixOutsideProjection);
        }
        if item_count == total {
            return Ok(ConversationPrefixIdentity {
                item_count,
                identity: self.identity,
            });
        }

        let mut covered = 0_usize;
        let mut identity = empty_projection_identity()?;
        if item_count == 0 {
            return Ok(ConversationPrefixIdentity {
                item_count,
                identity,
            });
        }
        for segment in self.segments() {
            identity = fold_projection_segment_start(identity, segment.kind())?;
            for (index, item_identity) in segment.item_identities().iter().enumerate() {
                identity = fold_projection_item(identity, *item_identity)?;
                covered = covered.saturating_add(1);
                if item_count == covered {
                    if index.saturating_add(1) == segment.items().len() {
                        identity = fold_projection_segment_end(identity)?;
                    }
                    return Ok(ConversationPrefixIdentity {
                        item_count,
                        identity,
                    });
                }
            }
            identity = fold_projection_segment_end(identity)?;
        }
        Err(HistoryError::PrefixOutsideProjection)
    }

    /// Identifies an ordered canonical-item prefix without segment boundaries.
    ///
    /// This narrow identity exists for family-artifact coverage. Request,
    /// token, and protected-group identities must continue to use the complete
    /// segment-boundary-aware projection.
    ///
    /// # Errors
    ///
    /// Returns a typed error when `item_count` exceeds this view.
    pub fn canonical_item_prefix_identity(
        &self,
        item_count: usize,
    ) -> Result<CanonicalItemPrefixIdentity, HistoryError> {
        if item_count > self.items().len() {
            return Err(HistoryError::PrefixOutsideProjection);
        }
        let mut identity = empty_canonical_item_prefix_identity()?;
        for item_identity in self
            .segments()
            .flat_map(HistorySegment::item_identities)
            .take(item_count)
        {
            identity = fold_canonical_item_prefix(identity, *item_identity)?;
        }
        Ok(CanonicalItemPrefixIdentity {
            item_count,
            identity,
        })
    }

    /// Stages ordered appended items for family-artifact coverage validation.
    ///
    /// The returned identity is exactly the identity the same items have after
    /// a later complete semantic-group commit. No incomplete history segment is
    /// created or admitted by this operation.
    ///
    /// # Errors
    ///
    /// Returns an error when an appended item repeats an existing or staged
    /// entry identity, or when canonical item identity construction fails.
    pub fn staged_canonical_item_prefix_identity(
        &self,
        appended: &[ConversationItem],
    ) -> Result<CanonicalItemPrefixIdentity, HistoryError> {
        let mut entries = self
            .items()
            .iter()
            .map(ConversationItem::id)
            .collect::<BTreeSet<_>>();
        let mut prefix = self.canonical_item_prefix_identity(self.items().len())?;
        for item in appended {
            if !entries.insert(item.id()) {
                return Err(HistoryError::DuplicateEntry);
            }
            prefix.identity =
                fold_canonical_item_prefix(prefix.identity, projection_item_identity(item)?)?;
            prefix.item_count = prefix.item_count.saturating_add(1);
        }
        Ok(prefix)
    }

    /// Reports whether the projection contains no model-visible items.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items().is_empty()
    }

    /// Returns a conservative retained-memory bound without flattening items.
    #[must_use]
    pub fn retained_bytes_upper_bound(&self) -> usize {
        self.history
            .retained_bytes_upper_bound()
            .saturating_add(self.working.retained_bytes_upper_bound())
    }

    /// Proves that this projection shares the exact canonical-history allocation.
    #[must_use]
    pub fn shares_history_with(&self, history: &CanonicalHistory) -> bool {
        self.history.shares_storage_with(history)
    }

    pub(crate) fn validate_model_visible(&self) -> Result<(), HistoryError> {
        if self
            .history
            .segments()
            .iter()
            .chain(self.working.segments())
            .all(HistorySegment::model_visible_complete)
        {
            Ok(())
        } else {
            Err(HistoryError::IncompleteToolExchange)
        }
    }

    fn segments(&self) -> impl Iterator<Item = &HistorySegment> {
        self.history
            .segments()
            .iter()
            .chain(self.working.segments())
    }
}

fn empty_projection_identity() -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::HistoryProjection,
        4,
        &[b"plakous-model/semantic-projection-empty"],
    )
}

fn empty_canonical_item_prefix_identity() -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::CanonicalItemPrefix,
        CANONICAL_ITEM_PREFIX_DIGEST_REVISION,
        &[b"plakous-model/canonical-item-prefix-empty-v2"],
    )
}

fn fold_canonical_item_prefix(prefix: Digest, item: Digest) -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::CanonicalItemPrefix,
        CANONICAL_ITEM_PREFIX_DIGEST_REVISION,
        &[
            b"plakous-model/canonical-item-prefix-append-v2",
            prefix.bytes(),
            item.identity_bytes().as_slice(),
        ],
    )
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

fn fold_projection_segment_start(
    prefix: Digest,
    kind: HistorySegmentKind,
) -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::HistoryProjection,
        4,
        &[
            b"plakous-model/semantic-projection-segment-start",
            prefix.bytes(),
            &[kind.tag()],
        ],
    )
}

fn fold_projection_item(prefix: Digest, item: Digest) -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::HistoryProjection,
        4,
        &[
            b"plakous-model/semantic-projection-item-append",
            prefix.bytes(),
            item.bytes(),
        ],
    )
}

fn fold_projection_segment_end(prefix: Digest) -> Result<Digest, DigestError> {
    Digest::sha256(
        DigestDomain::HistoryProjection,
        4,
        &[
            b"plakous-model/semantic-projection-segment-end",
            prefix.bytes(),
        ],
    )
}

fn fold_projection_segments<'a>(
    segments: impl IntoIterator<Item = &'a HistorySegment>,
) -> Result<Digest, DigestError> {
    let mut identity = empty_projection_identity()?;
    for segment in segments {
        identity = fold_projection_segment_start(identity, segment.kind())?;
        for item in segment.item_identities() {
            identity = fold_projection_item(identity, *item)?;
        }
        identity = fold_projection_segment_end(identity)?;
    }
    Ok(identity)
}

impl Default for ConversationView {
    fn default() -> Self {
        Self::from_shared(CanonicalHistory::default(), WorkingProjection::default())
    }
}

impl<'de> Deserialize<'de> for ConversationView {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ConversationViewWire::deserialize(deserializer)?;
        let projection = Self::from_shared(wire.history, wire.working);
        if projection.identity != wire.identity {
            return Err(serde::de::Error::custom(HistoryError::IntegrityMismatch {
                field: "history projection",
            }));
        }
        Ok(projection)
    }
}

/// A zero-copy collection view over segmented conversation items.
#[derive(Clone, Copy)]
pub struct ConversationItems<'a> {
    view: &'a ConversationView,
}

impl fmt::Debug for ConversationItems<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_list().entries(self.iter()).finish()
    }
}

impl<'a> ConversationItems<'a> {
    /// Returns the total item count without walking item payloads.
    #[must_use]
    pub fn len(self) -> usize {
        self.view
            .history
            .item_count()
            .saturating_add(self.view.working.item_count())
    }

    /// Reports whether no items are visible.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Iterates in canonical segment and item order.
    #[must_use]
    pub fn iter(self) -> ConversationItemIter<'a> {
        ConversationItemIter {
            segments: self
                .view
                .history
                .segments()
                .iter()
                .chain(self.view.working.segments()),
            items: None,
            remaining: self.len(),
        }
    }

    /// Returns one item by its flattened canonical position.
    #[must_use]
    pub fn get(self, mut index: usize) -> Option<&'a ConversationItem> {
        for segment in self.view.segments() {
            if index < segment.items().len() {
                return segment.items().get(index);
            }
            index = index.saturating_sub(segment.items().len());
        }
        None
    }

    /// Explicitly materializes a deep owned copy for non-hot-path consumers.
    #[must_use]
    pub fn to_vec(self) -> Vec<ConversationItem> {
        self.iter().cloned().collect()
    }
}

impl<'a> IntoIterator for ConversationItems<'a> {
    type Item = &'a ConversationItem;
    type IntoIter = ConversationItemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Index<usize> for ConversationItems<'_> {
    type Output = ConversationItem;

    #[allow(
        clippy::expect_used,
        reason = "Index follows the standard collection panic contract for an out-of-bounds index"
    )]
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .expect("conversation item index out of bounds")
    }
}

/// Exact-size iterator over a segmented conversation projection.
pub struct ConversationItemIter<'a> {
    segments: std::iter::Chain<CanonicalSegmentIter<'a>, slice::Iter<'a, HistorySegment>>,
    items: Option<slice::Iter<'a, ConversationItem>>,
    remaining: usize,
}

impl<'a> Iterator for ConversationItemIter<'a> {
    type Item = &'a ConversationItem;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.items.as_mut().and_then(Iterator::next) {
                self.remaining = self.remaining.saturating_sub(1);
                return Some(item);
            }
            self.items = Some(self.segments.next()?.items().iter());
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for ConversationItemIter<'_> {}

/// The sole whole-group history-selection strategy admitted by the driver policy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ContextHistoryStrategy {
    /// Select a contiguous prefix of complete older intent groups for summary replacement.
    SemanticSummaryPrefixV1,
}

impl ContextHistoryStrategy {
    const fn tag(self) -> u8 {
        match self {
            Self::SemanticSummaryPrefixV1 => 1,
        }
    }
}

/// Exact evidence that one complete older intent and its causal tail were selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSelectedIntentGroup {
    start_position: u32,
    end_position: u32,
    identity: Digest,
}

impl ContextSelectedIntentGroup {
    /// Returns the inclusive user-intent segment position.
    #[must_use]
    pub const fn start_position(self) -> u32 {
        self.start_position
    }

    /// Returns the exclusive next-intent boundary.
    #[must_use]
    pub const fn end_position(self) -> u32 {
        self.end_position
    }

    /// Returns the versioned identity of every ordered segment in the group.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Exact evidence for one selected complete canonical segment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextSelectedSegment {
    position: u32,
    identity: Digest,
}

impl ContextSelectedSegment {
    /// Returns the segment's canonical zero-based position.
    #[must_use]
    pub const fn position(self) -> u32 {
        self.position
    }

    /// Returns the exact versioned canonical segment identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Exact evidence for the latest user intent (or steering-only anchor) and tail.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct ContextRetainedTail {
    start_position: u32,
    segment_count: u32,
    identity: Digest,
}

impl ContextRetainedTail {
    /// Returns the zero-based position across committed then working groups.
    #[must_use]
    pub const fn start_position(self) -> u32 {
        self.start_position
    }

    /// Returns the complete retained causal group count.
    #[must_use]
    pub const fn segment_count(self) -> u32 {
        self.segment_count
    }

    /// Returns the versioned identity of the ordered causal tail.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }
}

/// Closed overflow classification minted only from a complete history plan.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub enum ContextPlannerDispositionKind {
    /// At least one complete committed group is eligible for transformation.
    EligibleWholeGroups,
    /// The selection reaches inside the active turn: complete older groups
    /// plus sealed committed segments between the latest intent and the
    /// protected current-round tail.
    EligibleActiveTurnPrefix,
    /// No complete committed group may be removed under the closed policy.
    ProtectedOnly,
}

/// Bounded sealed planner evidence consumed by context-overflow status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContextPlannerDisposition {
    plan: Digest,
    source_history: Digest,
    source_projection: Digest,
    policy_revision: u32,
    strategy: ContextHistoryStrategy,
    strategy_revision: u32,
    kind: ContextPlannerDispositionKind,
    selected_group_count: u32,
    selected_segment_count: u32,
    retained_segment_count: u32,
    identity: Digest,
}

impl ContextPlannerDisposition {
    fn for_plan(plan: &ContextHistoryPlan) -> Result<Self, HistoryPlanningError> {
        let selected_group_count = u32::try_from(plan.selected_groups.len())
            .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let selected_segment_count =
            u32::try_from(plan.selected.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let retained_segment_count = plan.retained_segment_count;
        let kind = if selected_group_count == 0 {
            ContextPlannerDispositionKind::ProtectedOnly
        } else if plan.latest_user_intent.is_some_and(|latest| {
            plan.selected
                .iter()
                .any(|selected| selected.position > latest)
        }) {
            ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        } else {
            ContextPlannerDispositionKind::EligibleWholeGroups
        };
        let identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-planner-disposition-v1",
                &plan.identity.identity_bytes(),
                &plan.source.history().identity().identity_bytes(),
                &plan.source.identity().identity_bytes(),
                &CONTEXT_PLANNER_POLICY_REVISION.to_be_bytes(),
                &[plan.strategy.tag()],
                &CONTEXT_PLANNER_STRATEGY_REVISION.to_be_bytes(),
                &[match kind {
                    ContextPlannerDispositionKind::EligibleWholeGroups => 1,
                    ContextPlannerDispositionKind::ProtectedOnly => 2,
                    ContextPlannerDispositionKind::EligibleActiveTurnPrefix => 3,
                }],
                &selected_group_count.to_be_bytes(),
                &selected_segment_count.to_be_bytes(),
                &retained_segment_count.to_be_bytes(),
            ],
        )?;
        Ok(Self {
            plan: plan.identity,
            source_history: plan.source.history().identity(),
            source_projection: plan.source.identity(),
            policy_revision: CONTEXT_PLANNER_POLICY_REVISION,
            strategy: plan.strategy,
            strategy_revision: CONTEXT_PLANNER_STRATEGY_REVISION,
            kind,
            selected_group_count,
            selected_segment_count,
            retained_segment_count,
            identity,
        })
    }

    /// Returns whether any complete whole group is eligible.
    #[must_use]
    pub const fn kind(&self) -> ContextPlannerDispositionKind {
        self.kind
    }

    /// Returns the complete history-plan identity.
    #[must_use]
    pub const fn plan_identity(&self) -> Digest {
        self.plan
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub const fn source_history_identity(&self) -> Digest {
        self.source_history
    }

    /// Returns the complete source projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source_projection
    }

    /// Returns the closed protection-policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        self.policy_revision
    }

    /// Returns the closed selection strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.strategy
    }

    /// Returns the closed strategy revision.
    #[must_use]
    pub const fn strategy_revision(&self) -> u32 {
        self.strategy_revision
    }

    /// Returns the eligible complete intent-group count.
    #[must_use]
    pub const fn selected_group_count(&self) -> u32 {
        self.selected_group_count
    }

    /// Returns the complete selected segment count across eligible intent groups.
    #[must_use]
    pub const fn selected_segment_count(&self) -> u32 {
        self.selected_segment_count
    }

    /// Returns the retained complete-group count.
    #[must_use]
    pub const fn retained_segment_count(&self) -> u32 {
        self.retained_segment_count
    }

    /// Returns the sealed disposition identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

/// A fail-closed pure context-planning error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum HistoryPlanningError {
    /// Committed plus working semantic groups exceeded the fixed planner bound.
    #[error("context history exceeds the closed planner segment bound")]
    LimitExceeded,
    /// A protected semantic group was not complete at the named position.
    #[error("history group at position {position} is incomplete")]
    IncompleteGroup {
        /// Zero-based position across committed then working groups.
        position: u32,
    },
    /// A current model-visible projection retained more than one compaction
    /// output.
    #[error("current history has a noncanonical compaction layout")]
    NoncanonicalCompaction,
    /// A versioned plan identity could not be constructed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// A pure, bounded whole-group plan over one immutable conversation projection.
///
/// The stable request prefix is outside history and is always protected by
/// this policy. The plan contains identities and shared handles only; it never
/// estimates tokens, predicts savings, or claims that a summary successor fits.
#[derive(Clone, Debug, PartialEq)]
pub struct ContextHistoryPlan {
    source: ConversationView,
    strategy: ContextHistoryStrategy,
    latest_user_intent: Option<u32>,
    selected_groups: Arc<[ContextSelectedIntentGroup]>,
    selected: Arc<[ContextSelectedSegment]>,
    retained_tail: Option<ContextRetainedTail>,
    retained_segment_count: u32,
    summary_tail_identity: Option<Digest>,
    disposition: ContextPlannerDisposition,
    identity: Digest,
}

struct ContextHistoryAnalysis {
    total: usize,
    canonical_count: u32,
    intent_positions: Vec<u32>,
    latest_user_intent: Option<u32>,
    latest_steering: Option<u32>,
    compaction_positions: Vec<u32>,
}

struct ContextSummarySelection {
    groups: Vec<ContextSelectedIntentGroup>,
    segments: Vec<ContextSelectedSegment>,
}

impl ContextHistoryPlan {
    /// Applies the sole driver-owned policy to committed and working groups.
    ///
    /// Working groups are always retained. Tool exchanges, reasoning/replay,
    /// and steering are invariant-bearing atomic segments: they may move only
    /// with their complete older intent group. A committed compaction output is
    /// a retained barrier. The latest semantic user intent and every later
    /// group form the retained causal tail.
    ///
    /// # Errors
    ///
    /// Returns a typed error for an incomplete group, excessive group count,
    /// or identity construction failure.
    pub fn new(source: &ConversationView) -> Result<Self, HistoryPlanningError> {
        let analysis = analyze_context_history(source)?;
        let strategy = ContextHistoryStrategy::SemanticSummaryPrefixV1;
        let selection = select_context_summary_prefix(source, &analysis)?;
        let retained_tail = analysis
            .latest_user_intent
            .or(analysis.latest_steering)
            .map(|_| context_retained_tail(source, &selection.segments, analysis.total))
            .transpose()?;
        let retained_segment_count =
            u32::try_from(analysis.total.saturating_sub(selection.segments.len()))
                .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let selected_groups: Arc<[ContextSelectedIntentGroup]> = selection.groups.into();
        let selected: Arc<[ContextSelectedSegment]> = selection.segments.into();
        let identity = context_history_plan_identity(
            source,
            strategy,
            &selected_groups,
            &selected,
            retained_tail.as_ref(),
        )?;
        let summary_tail_identity = (!selected.is_empty())
            .then(|| {
                Digest::sha256(
                    DigestDomain::ContextPlan,
                    1,
                    &[
                        b"plakous-model/context-summary-retained-tail-v1",
                        &identity.identity_bytes(),
                        &retained_segment_count.to_be_bytes(),
                        b"insert-summary-then-fresh-prepare-and-measurement-required",
                    ],
                )
            })
            .transpose()?;
        let mut plan = Self {
            source: source.clone(),
            strategy,
            latest_user_intent: analysis.latest_user_intent,
            selected_groups,
            selected,
            retained_tail,
            retained_segment_count,
            summary_tail_identity,
            disposition: ContextPlannerDisposition {
                plan: identity,
                source_history: source.history().identity(),
                source_projection: source.identity(),
                policy_revision: CONTEXT_PLANNER_POLICY_REVISION,
                strategy,
                strategy_revision: CONTEXT_PLANNER_STRATEGY_REVISION,
                kind: ContextPlannerDispositionKind::ProtectedOnly,
                selected_group_count: 0,
                selected_segment_count: 0,
                retained_segment_count,
                identity,
            },
            identity,
        };
        plan.disposition = ContextPlannerDisposition::for_plan(&plan)?;
        Ok(plan)
    }

    /// Returns the closed policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        CONTEXT_PLANNER_POLICY_REVISION
    }

    /// Returns the sole closed strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.strategy
    }

    /// Returns the latest semantic user-intent position, if one exists.
    #[must_use]
    pub const fn latest_user_intent_position(&self) -> Option<u32> {
        self.latest_user_intent
    }

    /// Returns exact ordered evidence for complete selected intent groups.
    #[must_use]
    pub fn selected_intent_groups(&self) -> &[ContextSelectedIntentGroup] {
        &self.selected_groups
    }

    /// Returns exact ordered evidence for every selected complete group.
    #[must_use]
    pub fn selected_segments(&self) -> &[ContextSelectedSegment] {
        &self.selected
    }

    /// Returns exact latest-intent/steering causal-tail evidence, when present.
    #[must_use]
    pub const fn retained_tail(&self) -> Option<&ContextRetainedTail> {
        self.retained_tail.as_ref()
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub fn source_history_identity(&self) -> Digest {
        self.source.history().identity()
    }

    /// Returns the complete source projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source.identity()
    }

    /// Returns the sealed overflow disposition.
    #[must_use]
    pub const fn disposition(&self) -> &ContextPlannerDisposition {
        &self.disposition
    }

    /// Returns the complete plan identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Returns the shared retained tail only when semantic-summary input exists.
    ///
    /// The tail is not a naked-deletion successor: a later driver must
    /// insert and commit the semantic summary before rebuilding a request.
    #[must_use]
    pub fn summary_replacement_tail(&self) -> Option<ContextSummaryRetainedTail<'_>> {
        self.summary_tail_identity
            .map(|identity| ContextSummaryRetainedTail {
                plan: self,
                identity,
            })
    }

    /// Proves that the plan retained the exact source-history allocation.
    #[must_use]
    pub fn shares_source_history_with(&self, history: &CanonicalHistory) -> bool {
        self.source.shares_history_with(history)
    }
}

/// A fail-closed error sealing or applying one semantic compaction cut.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum SemanticCompactionCutError {
    /// Compaction was requested while model-visible working state existed.
    #[error("semantic compaction requires an empty working projection")]
    WorkingProjectionNotEmpty,
    /// The supplied source no longer matched the closed plan's exact source.
    #[error("semantic compaction source and history plan have drifted")]
    SourcePlanDrift,
    /// The plan did not select a complete replaceable source prefix.
    #[error("semantic compaction has no eligible selected prefix")]
    NoEligibleSelectedPrefix,
    /// Selected handles or evidence did not form the plan's complete groups.
    #[error("semantic compaction selected groups are invalid")]
    InvalidSelectedSemantics,
    /// No complete latest-intent causal tail would survive the cut.
    #[error("semantic compaction retained semantics cannot be empty")]
    EmptyRetainedSemantics,
    /// Retained handles did not match the plan's exact causal tail.
    #[error("semantic compaction retained semantics are invalid")]
    InvalidRetainedSemantics,
    /// The supplied successor summary was not one canonical non-empty summary.
    #[error("semantic compaction successor requires exactly one canonical compaction summary")]
    InvalidSummary,
    /// Canonical history construction failed.
    #[error(transparent)]
    History(#[from] HistoryError),
    /// Stable proof identity construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
}

/// One owned exact cut from a quiescent committed source history.
///
/// This proof carries only whole shared segment handles. It is not a token
/// measurement, a fit claim, a provider request, or context-state authority.
/// The closed [`ContextHistoryPlan`] supplies every selected boundary; callers
/// cannot author a cut or count.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticCompactionCut {
    source: ConversationView,
    plan: ContextHistoryPlan,
    summary_input: ConversationView,
    selected: Arc<[HistorySegment]>,
    retained: Arc<[HistorySegment]>,
    prior_compaction: Option<HistorySegment>,
    retained_tail_identity: Digest,
    identity: Digest,
}

impl SemanticCompactionCut {
    /// Seals a closed plan against its exact quiescent committed source.
    ///
    /// A sole leading prior compaction is included in [`Self::summary_input`]
    /// so the new summary must fold its semantics, but it is excluded from the
    /// retained successor handles. All other selected and retained content is
    /// shared at complete semantic-group boundaries.
    ///
    /// # Errors
    ///
    /// Returns a typed error for working state, source or plan drift, an absent
    /// selected prefix, noncanonical selected/retained semantics, or identity
    /// construction failure.
    #[allow(
        clippy::too_many_lines,
        reason = "the single closed sealing sequence keeps every source, selection, retention, and identity fence auditable in order"
    )]
    pub fn seal(
        source: &ConversationView,
        plan: &ContextHistoryPlan,
    ) -> Result<Self, SemanticCompactionCutError> {
        if !source.working().segments().is_empty() {
            return Err(SemanticCompactionCutError::WorkingProjectionNotEmpty);
        }
        if source.history().identity() != plan.source_history_identity()
            || source.identity() != plan.source_projection_identity()
            || !plan.source.working().segments().is_empty()
        {
            return Err(SemanticCompactionCutError::SourcePlanDrift);
        }
        let evidence = plan.selected_segments();
        if !matches!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::EligibleWholeGroups
                | ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        ) || evidence.is_empty()
            || plan.selected_intent_groups().is_empty()
        {
            return Err(SemanticCompactionCutError::NoEligibleSelectedPrefix);
        }

        let source_segments = source.history().segments();
        let prior_compaction = source_segments
            .get(0)
            .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
            .cloned();
        if prior_compaction
            .as_ref()
            .is_some_and(|segment| !is_canonical_compaction_summary(segment))
        {
            return Err(SemanticCompactionCutError::InvalidSelectedSemantics);
        }
        // Re-run the closed selection over this exact source: the sealed
        // evidence must equal it — positions, identities and group runs alike
        // — so a plan cannot widen, reorder, or re-aim the cut past the closed
        // eligibility policy (older whole groups plus the sealed middle of
        // the active turn; never intent, steering, compaction, or the
        // protected current-round tail).
        let analysis = analyze_context_history(source)
            .map_err(|_| SemanticCompactionCutError::SourcePlanDrift)?;
        let recomputed = select_context_summary_prefix(source, &analysis)
            .map_err(|_| SemanticCompactionCutError::SourcePlanDrift)?;
        if recomputed.groups.as_slice() != plan.selected_intent_groups()
            || recomputed.segments.as_slice() != evidence
            || plan.disposition().selected_group_count()
                != u32::try_from(recomputed.groups.len()).unwrap_or(u32::MAX)
            || plan.disposition().selected_segment_count()
                != u32::try_from(evidence.len()).unwrap_or(u32::MAX)
        {
            return Err(SemanticCompactionCutError::InvalidSelectedSemantics);
        }
        let selected_start = evidence.first().map_or(0, |selected| selected.position());
        let selected_end = evidence
            .last()
            .and_then(|selected| selected.position().checked_add(1))
            .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?;
        if selected_start < u32::from(prior_compaction.is_some())
            || evidence
                .windows(2)
                .any(|pair| pair[1].position() <= pair[0].position())
        {
            return Err(SemanticCompactionCutError::NoEligibleSelectedPrefix);
        }
        let selected = evidence
            .iter()
            .map(|selected| {
                source_segments
                    .get(usize::try_from(selected.position()).unwrap_or(usize::MAX))
                    .filter(|segment| {
                        segment.identity() == selected.identity()
                            && segment.model_visible_complete()
                    })
                    .cloned()
                    .ok_or(SemanticCompactionCutError::NoEligibleSelectedPrefix)
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Retained is the exact complement: every committed segment the cut
        // does not fold, minus the position-zero prior compaction whose
        // semantics the new summary must carry forward.
        let mut selected_iter = evidence.iter().peekable();
        let mut retained = Vec::new();
        for (position, segment) in source_segments.iter().enumerate() {
            if position == 0 && prior_compaction.is_some() {
                continue;
            }
            if selected_iter.peek().is_some_and(|next| {
                usize::try_from(next.position()).unwrap_or(usize::MAX) == position
            }) {
                let _ = selected_iter.next();
                continue;
            }
            retained.push(segment.clone());
        }
        if retained.is_empty() {
            return Err(SemanticCompactionCutError::EmptyRetainedSemantics);
        }
        let retained_tail = plan
            .retained_tail()
            .ok_or(SemanticCompactionCutError::InvalidRetainedSemantics)?;
        let recomputed_tail = context_retained_tail(source, evidence, analysis.total)
            .map_err(|_| SemanticCompactionCutError::InvalidRetainedSemantics)?;
        let retained_count = u32::try_from(retained.len())
            .map_err(|_| SemanticCompactionCutError::InvalidRetainedSemantics)?;
        if retained_tail != &recomputed_tail
            || plan.disposition().retained_segment_count()
                != retained_count.saturating_add(u32::from(prior_compaction.is_some()))
            || retained.iter().any(|segment| {
                !segment.model_visible_complete()
                    || segment.kind() == HistorySegmentKind::Compaction
            })
            || selected_iter.next().is_some()
        {
            return Err(SemanticCompactionCutError::InvalidRetainedSemantics);
        }

        // The summarizer sees the folded prefix in source order. When the cut
        // reaches inside the active turn it also sees the latest intent: the
        // summary must preserve the user intent it is folding work for, and a
        // verbatim copy still leads the retained tail.
        let intra_turn = plan
            .latest_user_intent_position()
            .is_some_and(|latest| evidence.iter().any(|selected| selected.position() > latest));
        let mut input_handles: Vec<HistorySegment> = prior_compaction.iter().cloned().collect();
        if intra_turn {
            let latest = plan
                .latest_user_intent_position()
                .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?;
            let split = evidence
                .iter()
                .position(|selected| selected.position() > latest)
                .unwrap_or(evidence.len());
            input_handles.extend_from_slice(&selected[..split]);
            input_handles.push(
                source_segments
                    .get(usize::try_from(latest).unwrap_or(usize::MAX))
                    .cloned()
                    .ok_or(SemanticCompactionCutError::InvalidSelectedSemantics)?,
            );
            input_handles.extend_from_slice(&selected[split..]);
        } else {
            input_handles.extend_from_slice(&selected);
        }
        let summary_history = CanonicalHistory::new(input_handles)?;
        let summary_input =
            ConversationView::from_shared(summary_history, WorkingProjection::default());
        let retained_tail_identity = retained_tail.identity();
        let identity = Digest::sha256(
            DigestDomain::ContextPlan,
            SEMANTIC_COMPACTION_CUT_REVISION,
            &[
                b"plakous-model/semantic-compaction-cut-v1",
                &source.history().identity().identity_bytes(),
                &source.identity().identity_bytes(),
                &plan.identity().identity_bytes(),
                &plan.disposition().identity().identity_bytes(),
                &plan.policy_revision().to_be_bytes(),
                &[plan.strategy().tag()],
                &plan.disposition().strategy_revision().to_be_bytes(),
                &summary_input.history().identity().identity_bytes(),
                &summary_input.identity().identity_bytes(),
                &prior_compaction
                    .as_ref()
                    .map(HistorySegment::identity)
                    .map_or_else(Vec::new, |digest| digest.identity_bytes()),
                &selected_start.to_be_bytes(),
                &selected_end.to_be_bytes(),
                &retained_tail_identity.identity_bytes(),
                &retained_count.to_be_bytes(),
            ],
        )?;
        Ok(Self {
            source: source.clone(),
            plan: plan.clone(),
            summary_input,
            selected: selected.into(),
            retained: retained.into(),
            prior_compaction,
            retained_tail_identity,
            identity,
        })
    }

    /// Returns the exact committed source projection.
    #[must_use]
    pub const fn source(&self) -> &ConversationView {
        &self.source
    }

    /// Returns the exact source canonical-history identity.
    #[must_use]
    pub fn source_history_identity(&self) -> Digest {
        self.source.history().identity()
    }

    /// Returns the exact source model-visible projection identity.
    #[must_use]
    pub const fn source_projection_identity(&self) -> Digest {
        self.source.identity()
    }

    /// Returns the complete closed history-plan identity.
    #[must_use]
    pub const fn plan_identity(&self) -> Digest {
        self.plan.identity()
    }

    /// Returns the closed protection-policy revision.
    #[must_use]
    pub const fn policy_revision(&self) -> u32 {
        self.plan.policy_revision()
    }

    /// Returns the closed whole-group selection strategy.
    #[must_use]
    pub const fn strategy(&self) -> ContextHistoryStrategy {
        self.plan.strategy()
    }

    /// Returns the closed whole-group strategy revision.
    #[must_use]
    pub const fn strategy_revision(&self) -> u32 {
        self.plan.disposition().strategy_revision()
    }

    /// Returns the exact planner-selected intent-group evidence.
    #[must_use]
    pub fn selected_intent_groups(&self) -> &[ContextSelectedIntentGroup] {
        self.plan.selected_intent_groups()
    }

    /// Returns exact planner-selected segment evidence.
    #[must_use]
    pub fn selected_segment_evidence(&self) -> &[ContextSelectedSegment] {
        self.plan.selected_segments()
    }

    /// Returns the exact shared handles selected for replacement.
    ///
    /// A prior position-zero compaction is not in this slice; it is exposed by
    /// [`Self::prior_compaction`] and included in [`Self::summary_input`].
    #[must_use]
    pub fn selected_segments(&self) -> &[HistorySegment] {
        &self.selected
    }

    /// Returns the planner disposition identity binding the full selection.
    #[must_use]
    pub const fn selection_identity(&self) -> Digest {
        self.plan.disposition().identity()
    }

    /// Returns the exact owned summary input and no retained causal tail.
    #[must_use]
    pub const fn summary_input(&self) -> &ConversationView {
        &self.summary_input
    }

    /// Returns the prior compaction that the new summary must fold, if any.
    #[must_use]
    pub fn prior_compaction(&self) -> Option<&HistorySegment> {
        self.prior_compaction.as_ref()
    }

    /// Returns exact shared handles retained after the sole new summary.
    #[must_use]
    pub fn retained_segments(&self) -> &[HistorySegment] {
        &self.retained
    }

    /// Returns the complete retained group count.
    #[must_use]
    pub fn retained_segment_count(&self) -> usize {
        self.retained.len()
    }

    /// Returns the planner's exact retained causal-tail identity.
    #[must_use]
    pub const fn retained_tail_identity(&self) -> Digest {
        self.retained_tail_identity
    }

    /// Returns the stable semantic identity of this complete cut.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }

    /// Builds the sole canonical successor from one new semantic summary.
    ///
    /// The result contains the supplied summary at position zero followed by
    /// the sealed retained handles in source order. The old compaction, if
    /// any, cannot survive as a second compaction segment. Retained segment
    /// payloads are shared exactly; no item is flattened or copied.
    ///
    /// # Errors
    ///
    /// Returns a typed error unless `summary` is exactly one canonical
    /// non-empty compaction group with fresh entry identities, or if canonical
    /// successor construction fails.
    pub fn build_successor(
        &self,
        summary: &HistorySegment,
    ) -> Result<CanonicalHistory, SemanticCompactionCutError> {
        if !is_canonical_compaction_summary(summary)
            || summary.items().iter().any(|item| {
                EntryNode::contains(self.source.history().inner.entries.as_deref(), item.id())
            })
        {
            return Err(SemanticCompactionCutError::InvalidSummary);
        }
        let mut segments = Vec::with_capacity(self.retained.len().saturating_add(1));
        segments.push(summary.clone());
        segments.extend(self.retained.iter().cloned());
        let successor = CanonicalHistory::new(segments)?;
        if successor.segments().len() != self.retained.len().saturating_add(1)
            || successor
                .segments()
                .get(0)
                .is_none_or(|candidate| !candidate.shares_storage_with(summary))
            || successor
                .segments()
                .iter()
                .skip(1)
                .zip(self.retained.iter())
                .any(|(candidate, retained)| !candidate.shares_storage_with(retained))
            || successor
                .segments()
                .iter()
                .skip(1)
                .any(|segment| segment.kind() == HistorySegmentKind::Compaction)
        {
            return Err(SemanticCompactionCutError::InvalidRetainedSemantics);
        }
        Ok(successor)
    }
}

fn is_canonical_compaction_summary(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::Compaction
        && segment.model_visible_complete()
        && segment.items().len() == 1
        && segment.items()[0].actor() == Actor::Assistant
        && matches!(segment.items()[0].blocks(), [ContentBlock::Compaction(_)])
}

/// Zero-copy retained groups awaiting a committed semantic-summary insertion.
///
/// This value is neither a valid compaction successor nor an admission/fit
/// witness. A driver must first insert and commit the summary which replaces
/// the selected intent groups, then the descriptor context path must freshly
/// prepare and measure the complete successor request. An existing position-zero
/// compaction is retained here as required semantic input, but a canonical
/// successor folds it into the sole newly committed compaction output.
#[derive(Clone, Copy, Debug)]
pub struct ContextSummaryRetainedTail<'a> {
    plan: &'a ContextHistoryPlan,
    identity: Digest,
}

impl<'a> ContextSummaryRetainedTail<'a> {
    /// Returns the exact summary-retained-tail identity.
    #[must_use]
    pub const fn identity(self) -> Digest {
        self.identity
    }

    /// Returns the retained complete-group count.
    #[must_use]
    pub const fn segment_count(self) -> u32 {
        self.plan.retained_segment_count
    }

    /// Iterates retained group handles without flattening or copying payloads.
    #[must_use]
    pub fn segments(self) -> RetainedHistorySegmentIter<'a> {
        RetainedHistorySegmentIter {
            segments: self
                .plan
                .source
                .history()
                .segments()
                .iter()
                .chain(self.plan.source.working().segments()),
            selected: self.plan.selected.iter(),
            position: 0,
            remaining: usize::try_from(self.plan.retained_segment_count).unwrap_or(usize::MAX),
        }
    }
}

/// Exact-size zero-copy iterator over one semantic-summary retained tail.
pub struct RetainedHistorySegmentIter<'a> {
    segments: std::iter::Chain<CanonicalSegmentIter<'a>, slice::Iter<'a, HistorySegment>>,
    selected: slice::Iter<'a, ContextSelectedSegment>,
    position: u32,
    remaining: usize,
}

impl<'a> Iterator for RetainedHistorySegmentIter<'a> {
    type Item = &'a HistorySegment;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let segment = self.segments.next()?;
            let position = self.position;
            self.position = self.position.saturating_add(1);
            if self
                .selected
                .as_slice()
                .first()
                .is_some_and(|selected| selected.position == position)
            {
                let _ = self.selected.next();
                continue;
            }
            self.remaining = self.remaining.saturating_sub(1);
            return Some(segment);
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for RetainedHistorySegmentIter<'_> {}

fn is_semantic_user_intent(segment: &HistorySegment) -> bool {
    segment.kind() == HistorySegmentKind::RecentIntent
        || (segment.kind() == HistorySegmentKind::Conversation
            && segment
                .items()
                .iter()
                .any(|item| item.actor() == Actor::User))
}

fn analyze_context_history(
    source: &ConversationView,
) -> Result<ContextHistoryAnalysis, HistoryPlanningError> {
    let canonical_count = source.history().segments().len();
    let total = canonical_count
        .checked_add(source.working().segments().len())
        .ok_or(HistoryPlanningError::LimitExceeded)?;
    if total > MAX_CONTEXT_PLANNER_SEGMENTS {
        return Err(HistoryPlanningError::LimitExceeded);
    }
    let mut intent_positions = Vec::new();
    let mut latest_steering = None;
    let mut compaction_positions = Vec::new();
    for (position, segment) in source.segments().enumerate() {
        let position = u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        if !segment.model_visible_complete() {
            return Err(HistoryPlanningError::IncompleteGroup { position });
        }
        if is_semantic_user_intent(segment) {
            intent_positions.push(position);
        }
        if segment.kind() == HistorySegmentKind::Steering {
            latest_steering = Some(position);
        }
        if segment.kind() == HistorySegmentKind::Compaction {
            compaction_positions.push(position);
        }
    }
    match compaction_positions.as_slice() {
        [] | [_] => {}
        _ => return Err(HistoryPlanningError::NoncanonicalCompaction),
    }
    Ok(ContextHistoryAnalysis {
        total,
        canonical_count: u32::try_from(canonical_count)
            .map_err(|_| HistoryPlanningError::LimitExceeded)?,
        latest_user_intent: intent_positions.last().copied(),
        intent_positions,
        latest_steering,
        compaction_positions,
    })
}

fn select_context_summary_prefix(
    source: &ConversationView,
    analysis: &ContextHistoryAnalysis,
) -> Result<ContextSummarySelection, HistoryPlanningError> {
    let total = u32::try_from(analysis.total).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let barrier = analysis
        .latest_user_intent
        .and_then(|latest| {
            analysis
                .compaction_positions
                .iter()
                .rev()
                .copied()
                .find(|position| *position < latest)
        })
        .map_or(0, |position| position.saturating_add(1));
    let mut groups = Vec::new();
    if let Some(latest) = analysis.latest_user_intent {
        for (index, start) in analysis.intent_positions.iter().copied().enumerate() {
            if start < barrier || start >= latest {
                continue;
            }
            let end = analysis
                .intent_positions
                .get(index.saturating_add(1))
                .copied()
                .unwrap_or(total);
            if end > analysis.canonical_count {
                break;
            }
            groups.push(context_selected_intent_group(source, start, end)?);
        }
    }
    let mut segments = Vec::new();
    if let (Some(first), Some(last)) = (groups.first(), groups.last()) {
        for position in first.start_position..last.end_position {
            let index =
                usize::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
            let segment = source
                .history()
                .segments()
                .get(index)
                .ok_or(HistoryPlanningError::LimitExceeded)?;
            segments.push(ContextSelectedSegment {
                position,
                identity: segment.identity(),
            });
        }
    }
    // Rolling cut: the sealed middle of the active intent group is eligible
    // too. The intent and every steering segment keep their verbatim user
    // authority, and the protected tail begins at the newest complete tool
    // exchange — the batch of the current round, the same boundary the
    // placeholder rung refuses to touch.
    if let Some(latest) = analysis.latest_user_intent {
        let canonical = usize::try_from(analysis.canonical_count)
            .map_err(|_| HistoryPlanningError::LimitExceeded)?;
        let committed = source.history().segments();
        let tail_start = committed
            .iter()
            .take(canonical)
            .enumerate()
            .skip(
                usize::try_from(latest)
                    .unwrap_or(usize::MAX)
                    .saturating_add(1),
            )
            .filter(|(_, segment)| segment.kind() == HistorySegmentKind::ToolExchange)
            .map(|(position, _)| position)
            .last()
            .map(|position| {
                u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)
            })
            .transpose()?;
        if let Some(tail_start) = tail_start {
            let mut run_start = None;
            for position in latest.saturating_add(1)..tail_start {
                let segment = committed
                    .get(usize::try_from(position).unwrap_or(usize::MAX))
                    .ok_or(HistoryPlanningError::LimitExceeded)?;
                let selectable = !matches!(
                    segment.kind(),
                    HistorySegmentKind::RecentIntent
                        | HistorySegmentKind::Steering
                        | HistorySegmentKind::Compaction
                );
                match (run_start, selectable) {
                    (None, true) => run_start = Some(position),
                    (Some(start), false) => {
                        groups.push(context_selected_intent_group(source, start, position)?);
                        run_start = None;
                    }
                    _ => {}
                }
                if selectable {
                    segments.push(ContextSelectedSegment {
                        position,
                        identity: segment.identity(),
                    });
                }
            }
            if let Some(start) = run_start {
                groups.push(context_selected_intent_group(source, start, tail_start)?);
            }
        }
    }
    Ok(ContextSummarySelection { groups, segments })
}

/// The retained set is every segment the selection does not fold, minus a
/// position-zero prior compaction (its semantics fold into the new summary
/// rather than surviving as a second compaction). `start_position` is the
/// first retained position; the identity formula is unchanged, so a wholly
/// contiguous retained set produces the same digest as before.
fn context_retained_tail(
    source: &ConversationView,
    selected: &[ContextSelectedSegment],
    total: usize,
) -> Result<ContextRetainedTail, HistoryPlanningError> {
    let prior_compaction = source
        .history()
        .segments()
        .get(0)
        .is_some_and(|segment| segment.kind() == HistorySegmentKind::Compaction);
    let mut selected = selected.iter().peekable();
    let mut start = None;
    let mut retained = Vec::new();
    for (position, segment) in source.segments().enumerate() {
        let position = u32::try_from(position).map_err(|_| HistoryPlanningError::LimitExceeded)?;
        if prior_compaction && position == 0 {
            continue;
        }
        if selected
            .peek()
            .is_some_and(|next| next.position == position)
        {
            let _ = selected.next();
            continue;
        }
        if start.is_none() {
            start = Some(position);
        }
        retained.push(segment);
    }
    let Some(start) = start else {
        return Ok(ContextRetainedTail {
            start_position: u32::try_from(total).unwrap_or(u32::MAX),
            segment_count: 0,
            identity: Digest::sha256(
                DigestDomain::ContextPlan,
                1,
                &[
                    b"plakous-model/context-retained-tail-v1",
                    &source.identity().identity_bytes(),
                    &u32::try_from(total).unwrap_or(u32::MAX).to_be_bytes(),
                    &0_u32.to_be_bytes(),
                ],
            )?,
        });
    };
    let segment_count =
        u32::try_from(retained.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-retained-tail-v1",
            &source.identity().identity_bytes(),
            &start.to_be_bytes(),
            &segment_count.to_be_bytes(),
        ],
    )?;
    for segment in retained {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-retained-tail-append-v1",
                &identity.identity_bytes(),
                &segment.identity().identity_bytes(),
            ],
        )?;
    }
    Ok(ContextRetainedTail {
        start_position: start,
        segment_count,
        identity,
    })
}

fn context_history_plan_identity(
    source: &ConversationView,
    strategy: ContextHistoryStrategy,
    selected_groups: &[ContextSelectedIntentGroup],
    selected: &[ContextSelectedSegment],
    retained_tail: Option<&ContextRetainedTail>,
) -> Result<Digest, HistoryPlanningError> {
    let selected_count =
        u32::try_from(selected.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let selected_group_count =
        u32::try_from(selected_groups.len()).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-history-plan-v1",
            &source.history().identity().identity_bytes(),
            &source.identity().identity_bytes(),
            &CONTEXT_PLANNER_POLICY_REVISION.to_be_bytes(),
            &[strategy.tag()],
            &CONTEXT_PLANNER_STRATEGY_REVISION.to_be_bytes(),
            &selected_group_count.to_be_bytes(),
            &selected_count.to_be_bytes(),
            &retained_tail
                .map(|tail| tail.identity())
                .map_or_else(Vec::new, |digest| digest.identity_bytes()),
        ],
    )?;
    for group in selected_groups {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-history-plan-intent-group-v1",
                &identity.identity_bytes(),
                &group.start_position.to_be_bytes(),
                &group.end_position.to_be_bytes(),
                &group.identity.identity_bytes(),
            ],
        )?;
    }
    for selected in selected {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-history-plan-selection-v1",
                &identity.identity_bytes(),
                &selected.position.to_be_bytes(),
                &selected.identity.identity_bytes(),
            ],
        )?;
    }
    Ok(identity)
}

fn context_selected_intent_group(
    source: &ConversationView,
    start: u32,
    end: u32,
) -> Result<ContextSelectedIntentGroup, HistoryPlanningError> {
    let start_usize = usize::try_from(start).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let end_usize = usize::try_from(end).map_err(|_| HistoryPlanningError::LimitExceeded)?;
    let mut identity = Digest::sha256(
        DigestDomain::ContextPlan,
        1,
        &[
            b"plakous-model/context-selected-intent-group-v1",
            &source.history().identity().identity_bytes(),
            &source.identity().identity_bytes(),
            &start.to_be_bytes(),
            &end.to_be_bytes(),
        ],
    )?;
    for segment in source
        .history()
        .segments()
        .iter()
        .skip(start_usize)
        .take(end_usize.saturating_sub(start_usize))
    {
        identity = Digest::sha256(
            DigestDomain::ContextPlan,
            1,
            &[
                b"plakous-model/context-selected-intent-group-append-v1",
                &identity.identity_bytes(),
                &segment.identity().identity_bytes(),
            ],
        )?;
    }
    Ok(ContextSelectedIntentGroup {
        start_position: start,
        end_position: end,
        identity,
    })
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "unit tests use expect for fixture clarity"
)]
mod tests {
    use super::*;
    use crate::{
        CallId, CompactionBlock, ContentBlock, EntryId, Provenance, ReasoningBlock,
        ReasoningVisibility, TextBlock, ToolCallBlock, ToolCallState, ToolCallerLineage,
        ToolCorrelation, ToolExecutionOwner, ToolName, ToolResultBlock, ToolResultKind,
        ToolResultPart,
    };
    use uuid::Uuid;

    fn entry(value: u128, actor: Actor, block: ContentBlock) -> ConversationItem {
        ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(value)).expect("entry ID"),
            actor,
            vec![block],
            if actor == Actor::User {
                Provenance::UserInput
            } else {
                Provenance::ModelOutput
            },
        )
        .expect("item")
    }

    fn text(value: u128, actor: Actor, content: &str) -> ConversationItem {
        entry(
            value,
            actor,
            ContentBlock::Text(TextBlock::new(content, None).expect("text")),
        )
    }

    fn complete_exchange(value: u128) -> HistorySegment {
        let call_id = CallId::try_from(Uuid::from_u128(value)).expect("call ID");
        let correlation = ToolCorrelation::default();
        let call = entry(
            value.saturating_add(1),
            Actor::Assistant,
            ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    ToolName::new("read").expect("tool"),
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    "{}",
                )
                .expect("call"),
            ),
        );
        let result = entry(
            value.saturating_add(2),
            Actor::Tool,
            ContentBlock::ToolResult(
                ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new("ok", None).expect("text"),
                    )],
                    correlation,
                )
                .expect("result"),
            ),
        );
        HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call, result])
            .expect("complete exchange")
    }

    fn reasoning(value: u128) -> HistorySegment {
        HistorySegment::new(
            HistorySegmentKind::ReasoningReplay,
            vec![entry(
                value,
                Actor::Assistant,
                ContentBlock::Reasoning(
                    ReasoningBlock::new(
                        ReasoningVisibility::Summary,
                        Some("reasoning summary".to_owned()),
                        None,
                    )
                    .expect("reasoning"),
                ),
            )],
        )
        .expect("reasoning segment")
    }

    fn compaction(value: u128) -> HistorySegment {
        compaction_summary(value, "summary")
    }

    fn compaction_summary(value: u128, summary: &str) -> HistorySegment {
        HistorySegment::new(
            HistorySegmentKind::Compaction,
            vec![entry(
                value,
                Actor::Assistant,
                ContentBlock::Compaction(CompactionBlock::new(summary).expect("compaction")),
            )],
        )
        .expect("compaction segment")
    }

    fn assert_versioned_context_plan_identities(plan: &ContextHistoryPlan) {
        for identity in [plan.identity(), plan.disposition().identity()] {
            assert_eq!(identity.domain(), DigestDomain::ContextPlan);
            assert_eq!(identity.version(), 1);
        }
        for group in plan.selected_intent_groups() {
            assert_eq!(group.identity().domain(), DigestDomain::ContextPlan);
            assert_eq!(group.identity().version(), 1);
        }
    }

    #[test]
    fn a_photo_sized_user_image_appends_to_canonical_history() {
        let mut bytes = vec![0x5a; 280_000];
        bytes[..3].copy_from_slice(&[0xff, 0xd8, 0xff]);
        let image = crate::ImageBlock::new(
            "image/jpeg",
            crate::MediaSource::bytes(bytes).expect("bytes"),
        )
        .expect("image");
        let item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(1)).expect("entry ID"),
            Actor::User,
            vec![
                ContentBlock::Text(TextBlock::new("what animal?", None).expect("text")),
                ContentBlock::Image(image),
            ],
            Provenance::UserInput,
        )
        .expect("item");
        let segment = HistorySegment::new(HistorySegmentKind::RecentIntent, vec![item])
            .expect("image segment");
        CanonicalHistory::new(Vec::new())
            .expect("history")
            .append(segment)
            .expect("append");
    }

    #[test]
    fn canonical_history_append_shares_prior_segment_payload() {
        let first = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(1, Actor::Assistant, "one")],
        )
        .expect("first segment");
        let history = CanonicalHistory::new(vec![first.clone()]).expect("history");
        let next = history
            .append(
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(2, Actor::User, "two")],
                )
                .expect("second segment"),
            )
            .expect("append");

        assert!(history.segments()[0].shares_storage_with(&next.segments()[0]));
        assert_ne!(history.identity(), next.identity());
    }

    #[test]
    fn persistent_append_work_is_logarithmically_bounded() {
        let mut history = CanonicalHistory::default();
        let mut first = None;
        for value in 1..=1_024_u128 {
            let segment = HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(value, Actor::Assistant, "bounded")],
            )
            .expect("segment");
            if first.is_none() {
                first = Some(segment.clone());
            }
            let (next, work) = history.append_with_work(segment).expect("append");
            assert!(work.root_handles_copied <= HISTORY_ITER_STACK_CAPACITY);
            assert!(work.tree_nodes_created <= HISTORY_ITER_STACK_CAPACITY);
            assert!(
                work.entry_nodes_created <= HISTORY_ITER_STACK_CAPACITY.saturating_mul(3),
                "{} entry-index nodes exceeded the logarithmic append bound",
                work.entry_nodes_created
            );
            history = next;
        }

        assert_eq!(history.segments().len(), 1_024);
        assert_eq!(history.item_count(), 1_024);
        assert_eq!(history.inner.roots.len(), 1);
        assert_eq!(history.inner.tree_node_count, 2_047);
        assert!(
            first
                .as_ref()
                .is_some_and(|first| first.shares_storage_with(&history.segments()[0]))
        );
    }

    #[test]
    fn old_history_handle_remains_stable_across_later_commits() {
        let mut history = CanonicalHistory::default();
        for value in 1..=17_u128 {
            history = history
                .append(
                    HistorySegment::new(
                        HistorySegmentKind::Conversation,
                        vec![text(value, Actor::Assistant, "before")],
                    )
                    .expect("segment"),
                )
                .expect("append");
        }
        let old = history.clone();
        let old_identity = old.identity();
        let old_bytes = serde_json::to_vec(&old).expect("serialize old history");
        for value in 18..=257_u128 {
            history = history
                .append(
                    HistorySegment::new(
                        HistorySegmentKind::Conversation,
                        vec![text(value, Actor::Assistant, "after")],
                    )
                    .expect("segment"),
                )
                .expect("append");
        }

        assert_eq!(old.identity(), old_identity);
        assert_eq!(old.segments().len(), 17);
        assert_eq!(
            serde_json::to_vec(&old).expect("serialize old history"),
            old_bytes
        );
        assert!(old.segments()[0].shares_storage_with(&history.segments()[0]));
        assert!(!old.shares_storage_with(&history));
    }

    #[test]
    fn persistent_history_has_deterministic_wire_bytes_and_identity() {
        let segments = (1..=65_u128)
            .map(|value| {
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(value, Actor::User, "same")],
                )
                .expect("segment")
            })
            .collect::<Vec<_>>();
        let bulk = CanonicalHistory::new(segments.clone()).expect("bulk history");
        let mut appended = CanonicalHistory::default();
        for segment in segments {
            appended = appended.append(segment).expect("incremental append");
        }
        let bytes = serde_json::to_vec(&appended).expect("serialize history");
        let decoded: CanonicalHistory =
            serde_json::from_slice(&bytes).expect("deserialize history");

        assert_eq!(bulk.identity(), appended.identity());
        assert_eq!(serde_json::to_vec(&bulk).expect("serialize bulk"), bytes);
        assert_eq!(decoded, appended);
        assert!(
            decoded
                .segments()
                .iter()
                .all(|segment| segment.kind() == HistorySegmentKind::RecentIntent)
        );
    }

    #[test]
    fn persistent_entry_index_rejects_duplicates_without_mutating_history() {
        let original = CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(7, Actor::Assistant, "original")],
            )
            .expect("segment"),
        ])
        .expect("history");
        let identity = original.identity();
        let duplicate = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(7, Actor::User, "duplicate")],
        )
        .expect("segment");

        assert_eq!(
            original.append(duplicate),
            Err(HistoryError::DuplicateEntry)
        );
        assert_eq!(original.identity(), identity);
        assert_eq!(original.segments().len(), 1);
    }

    #[test]
    fn projection_clone_shares_history_and_has_stable_identity() {
        let history = CanonicalHistory::new(vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "intent")],
            )
            .expect("segment"),
        ])
        .expect("history");
        let projection =
            ConversationView::from_shared(history.clone(), WorkingProjection::default());
        let cloned = projection.clone();

        assert!(projection.shares_history_with(&history));
        assert!(cloned.shares_history_with(&history));
        assert_eq!(projection.identity(), cloned.identity());
        assert_eq!(projection.items().len(), 1);
    }

    #[test]
    fn context_plan_empty_and_no_user_history_are_protected_only() {
        let empty = ContextHistoryPlan::new(&ConversationView::default()).expect("empty plan");
        assert_eq!(empty.policy_revision(), CONTEXT_PLANNER_POLICY_REVISION);
        assert_eq!(
            empty.strategy(),
            ContextHistoryStrategy::SemanticSummaryPrefixV1
        );
        assert_eq!(empty.latest_user_intent_position(), None);
        assert_eq!(empty.selected_intent_groups(), []);
        assert_eq!(empty.selected_segments(), []);
        assert!(empty.retained_tail().is_none());
        assert!(empty.summary_replacement_tail().is_none());
        assert_eq!(
            empty.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );

        let no_user = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(10, Actor::Assistant, "orphan output")],
                )
                .expect("conversation"),
                reasoning(20),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&no_user).expect("no-user plan");
        assert_eq!(plan.selected_intent_groups(), []);
        assert_eq!(plan.selected_segments(), []);
        assert_eq!(plan.latest_user_intent_position(), None);
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );
    }

    #[test]
    fn semantic_summary_selects_contiguous_complete_older_intent_groups() {
        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "first")],
            )
            .expect("first intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(2, Actor::Assistant, "first answer")],
            )
            .expect("first answer"),
            complete_exchange(100),
            reasoning(200),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![text(300, Actor::User, "steer first")],
            )
            .expect("steering"),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(4, Actor::User, "second")],
            )
            .expect("second intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(5, Actor::Assistant, "second answer")],
            )
            .expect("second answer"),
            complete_exchange(400),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(6, Actor::User, "latest")],
            )
            .expect("latest intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(7, Actor::Assistant, "latest answer")],
            )
            .expect("latest answer"),
        ];
        let latest = segments[8].clone();
        let latest_answer = segments[9].clone();
        let history = CanonicalHistory::new(segments.clone()).expect("history");
        let source = ConversationView::from_shared(history.clone(), WorkingProjection::default());
        let plan = ContextHistoryPlan::new(&source).expect("plan");

        assert_eq!(plan.latest_user_intent_position(), Some(8));
        assert_eq!(
            plan.selected_intent_groups()
                .iter()
                .map(|group| (group.start_position(), group.end_position()))
                .collect::<Vec<_>>(),
            vec![(0, 5), (5, 8)]
        );
        assert_eq!(
            plan.selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            (0_u32..8).collect::<Vec<_>>()
        );
        for selected in plan.selected_segments() {
            assert_eq!(
                selected.identity(),
                segments[usize::try_from(selected.position()).expect("position")].identity()
            );
        }
        assert_eq!(
            plan.retained_tail()
                .map(|tail| (tail.start_position(), tail.segment_count())),
            Some((8, 2))
        );
        assert_eq!(plan.disposition().selected_group_count(), 2);
        assert_eq!(plan.disposition().selected_segment_count(), 8);
        assert_versioned_context_plan_identities(&plan);
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::EligibleWholeGroups
        );
        assert!(plan.shares_source_history_with(&history));

        let candidate = plan
            .summary_replacement_tail()
            .expect("summary replacement tail");
        let retained = candidate.segments().collect::<Vec<_>>();
        assert_eq!(retained.len(), 2);
        assert!(retained[0].shares_storage_with(&latest));
        assert!(retained[1].shares_storage_with(&latest_answer));
    }

    #[test]
    fn rolling_cut_selects_the_sealed_middle_of_the_active_turn() {
        // One long turn: the intent, the newest exchange (the current round's
        // batch) and steering stay verbatim; the sealed work between them is
        // the foldable prefix.
        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "build the feature")],
            )
            .expect("intent"),
            complete_exchange(100),
            reasoning(200),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(3, Actor::Assistant, "halfway note")],
            )
            .expect("mid-turn note"),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![text(4, Actor::User, "also cover the edge case")],
            )
            .expect("mid-turn steering"),
            complete_exchange(500),
            complete_exchange(600),
        ];
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments.clone()).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");

        assert_eq!(plan.latest_user_intent_position(), Some(0));
        // The newest exchange is the protected tail start (position 6); the
        // steering at 4 breaks the middle into two selected runs.
        assert_eq!(
            plan.selected_intent_groups()
                .iter()
                .map(|group| (group.start_position(), group.end_position()))
                .collect::<Vec<_>>(),
            vec![(1, 4), (5, 6)]
        );
        assert_eq!(
            plan.selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );
        assert_eq!(
            plan.retained_tail()
                .map(|tail| (tail.start_position(), tail.segment_count())),
            Some((0, 3))
        );
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        );
        assert_versioned_context_plan_identities(&plan);

        let cut = SemanticCompactionCut::seal(&source, &plan).expect("rolling cut");
        // The summarizer sees the intent it is folding work for.
        assert_eq!(
            cut.summary_input()
                .history()
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::ToolExchange,
                HistorySegmentKind::ReasoningReplay,
                HistorySegmentKind::Conversation,
                HistorySegmentKind::ToolExchange,
            ]
        );
        let summary = compaction_summary(1_000, "rolled middle");
        let successor = cut.build_successor(&summary).expect("successor");
        assert_eq!(
            successor
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Steering,
                HistorySegmentKind::ToolExchange,
            ]
        );
        assert!(successor.segments()[1].shares_storage_with(&segments[0]));
        assert!(successor.segments()[2].shares_storage_with(&segments[4]));
        assert!(successor.segments()[3].shares_storage_with(&segments[6]));
    }

    #[test]
    fn a_rolled_successor_keeps_folding_on_the_next_pass() {
        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "build the feature")],
            )
            .expect("intent"),
            complete_exchange(100),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![text(2, Actor::User, "also cover the edge case")],
            )
            .expect("mid-turn steering"),
            complete_exchange(300),
            complete_exchange(400),
        ];
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let successor = SemanticCompactionCut::seal(&source, &plan)
            .expect("rolling cut")
            .build_successor(&compaction_summary(1_000, "rolled"))
            .expect("successor");
        // [summary, intent, steering, ex]: append the next round and the
        // steering stays protected while the older exchange folds again.
        let replanned_source = ConversationView::from_shared(
            successor
                .append(complete_exchange(700))
                .expect("append next round"),
            WorkingProjection::default(),
        );
        let replanned = ContextHistoryPlan::new(&replanned_source).expect("replanned");
        assert_eq!(replanned.latest_user_intent_position(), Some(1));
        assert_eq!(
            replanned
                .selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            vec![3]
        );
        assert_eq!(
            replanned.disposition().kind(),
            ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        );
    }

    #[test]
    fn rolling_cut_stops_at_the_newest_exchange_and_empty_middles() {
        // Intent plus exactly one exchange: the newest exchange is the whole
        // tail, the middle is empty, and the disposition stays protected.
        let source = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(1, Actor::User, "one round")],
                )
                .expect("intent"),
                complete_exchange(100),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        assert_eq!(plan.selected_segments(), []);
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );

        // No exchange at all inside the turn: only intent and text, nothing
        // foldable even though the turn may be large.
        let text_only = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(2, Actor::User, "chat only")],
                )
                .expect("intent"),
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(3, Actor::Assistant, "a long answer")],
                )
                .expect("answer"),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&text_only).expect("plan");
        assert_eq!(plan.selected_segments(), []);
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );
    }

    #[test]
    fn rolling_cut_composes_with_older_intent_groups() {
        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "older ask")],
            )
            .expect("older intent"),
            complete_exchange(100),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(2, Actor::User, "current ask")],
            )
            .expect("latest intent"),
            complete_exchange(300),
            reasoning(400),
            complete_exchange(500),
        ];
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");

        // Older group folds whole; inside the active turn only the sealed
        // middle (positions 3..5 minus none-selectable kinds) joins the cut.
        assert_eq!(
            plan.selected_intent_groups()
                .iter()
                .map(|group| (group.start_position(), group.end_position()))
                .collect::<Vec<_>>(),
            vec![(0, 2), (3, 5)]
        );
        assert_eq!(
            plan.selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            vec![0, 1, 3, 4]
        );
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::EligibleActiveTurnPrefix
        );

        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
        // Whole-group input order is preserved; the latest intent is inserted
        // before the intra-turn middle so the summarizer keeps the task.
        assert_eq!(
            cut.summary_input()
                .history()
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::ToolExchange,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::ToolExchange,
                HistorySegmentKind::ReasoningReplay,
            ]
        );
        let successor = cut
            .build_successor(&compaction_summary(2_000, "folded"))
            .expect("successor");
        assert_eq!(
            successor
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::ToolExchange,
            ]
        );
    }

    #[test]
    fn committed_compaction_is_a_retained_selection_barrier() {
        let segments = vec![
            compaction(1),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(2, Actor::User, "after summary")],
            )
            .expect("middle intent"),
            complete_exchange(500),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(3, Actor::User, "latest")],
            )
            .expect("latest intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(4, Actor::Assistant, "latest answer")],
            )
            .expect("latest answer"),
        ];
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");

        assert_eq!(
            plan.selected_intent_groups()
                .iter()
                .map(|group| (group.start_position(), group.end_position()))
                .collect::<Vec<_>>(),
            vec![(1, 3)]
        );
        assert_eq!(
            plan.selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        let retained = plan
            .summary_replacement_tail()
            .expect("summary replacement tail")
            .segments()
            .map(HistorySegment::kind)
            .collect::<Vec<_>>();
        assert_eq!(
            retained,
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Conversation,
            ]
        );
    }

    #[test]
    fn semantic_compaction_cut_seals_whole_groups_and_builds_shared_successor() {
        let segments = vec![
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(1, Actor::User, "old intent")],
            )
            .expect("old intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(2, Actor::Assistant, "old answer")],
            )
            .expect("old answer"),
            complete_exchange(100),
            reasoning(200),
            HistorySegment::new(
                HistorySegmentKind::Steering,
                vec![text(300, Actor::User, "old steering")],
            )
            .expect("old steering"),
            HistorySegment::new(
                HistorySegmentKind::RecentIntent,
                vec![text(4, Actor::User, "latest intent")],
            )
            .expect("latest intent"),
            HistorySegment::new(
                HistorySegmentKind::Conversation,
                vec![text(5, Actor::Assistant, "latest answer")],
            )
            .expect("latest answer"),
        ];
        let history = CanonicalHistory::new(segments.clone()).expect("history");
        let source = ConversationView::from_shared(history, WorkingProjection::default());
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let left = SemanticCompactionCut::seal(&source, &plan).expect("left cut");
        let right = SemanticCompactionCut::seal(&source, &plan).expect("right cut");

        assert_eq!(left, right);
        assert_eq!(left.identity(), right.identity());
        assert_eq!(left.identity().domain(), DigestDomain::ContextPlan);
        assert_eq!(left.identity().version(), SEMANTIC_COMPACTION_CUT_REVISION);
        assert_eq!(left.source_history_identity(), source.history().identity());
        assert_eq!(left.source_projection_identity(), source.identity());
        assert_eq!(left.plan_identity(), plan.identity());
        assert_eq!(left.selection_identity(), plan.disposition().identity());
        assert_eq!(left.policy_revision(), CONTEXT_PLANNER_POLICY_REVISION);
        assert_eq!(left.strategy_revision(), CONTEXT_PLANNER_STRATEGY_REVISION);
        assert_eq!(left.selected_segments().len(), 5);
        assert_eq!(left.summary_input().history().segments().len(), 5);
        assert_eq!(left.summary_input().working().segments(), []);
        assert_eq!(left.retained_segment_count(), 2);
        assert_eq!(
            left.retained_tail_identity(),
            plan.retained_tail().expect("retained tail").identity()
        );
        for (selected, original) in left.selected_segments().iter().zip(&segments[..5]) {
            assert!(selected.shares_storage_with(original));
        }
        for (retained, original) in left.retained_segments().iter().zip(&segments[5..]) {
            assert!(retained.shares_storage_with(original));
        }
        assert_eq!(
            serde_json::to_vec(left.summary_input()).expect("left bytes"),
            serde_json::to_vec(right.summary_input()).expect("right bytes")
        );
        let decoded_source: ConversationView =
            serde_json::from_slice(&serde_json::to_vec(&source).expect("source bytes"))
                .expect("decoded source");
        let decoded_plan = ContextHistoryPlan::new(&decoded_source).expect("decoded plan");
        let decoded_cut =
            SemanticCompactionCut::seal(&decoded_source, &decoded_plan).expect("decoded cut");
        assert_eq!(decoded_cut.identity(), left.identity());
        assert_eq!(
            decoded_cut.retained_tail_identity(),
            left.retained_tail_identity()
        );

        let summary = compaction_summary(1_000, "folded old groups");
        let successor = left.build_successor(&summary).expect("successor");
        assert_eq!(successor.segments().len(), 3);
        assert!(successor.segments()[0].shares_storage_with(&summary));
        assert_eq!(
            successor
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::Conversation,
            ]
        );
        for (retained, original) in successor.segments().iter().skip(1).zip(&segments[5..]) {
            assert!(retained.shares_storage_with(original));
        }
    }

    #[test]
    fn semantic_compaction_cut_folds_prior_compaction_exactly_once() {
        let old_summary = compaction_summary(1, "prior summary");
        let old_intent = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, Actor::User, "after summary")],
        )
        .expect("old intent");
        let old_tool = complete_exchange(500);
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(3, Actor::User, "latest")],
        )
        .expect("latest intent");
        let latest_answer = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(4, Actor::Assistant, "latest answer")],
        )
        .expect("latest answer");
        let segments = vec![
            old_summary.clone(),
            old_intent.clone(),
            old_tool.clone(),
            latest.clone(),
            latest_answer.clone(),
        ];
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");

        assert!(
            cut.prior_compaction()
                .expect("prior compaction")
                .shares_storage_with(&old_summary)
        );
        assert_eq!(
            cut.summary_input()
                .history()
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::RecentIntent,
                HistorySegmentKind::ToolExchange,
            ]
        );
        assert_eq!(cut.selected_segments().len(), 2);
        assert!(cut.selected_segments()[0].shares_storage_with(&old_intent));
        assert!(cut.selected_segments()[1].shares_storage_with(&old_tool));
        assert_eq!(cut.retained_segments().len(), 2);
        assert!(cut.retained_segments()[0].shares_storage_with(&latest));

        let new_summary = compaction_summary(1_001, "prior plus selected groups");
        let successor = cut.build_successor(&new_summary).expect("successor");
        assert_eq!(
            successor
                .segments()
                .iter()
                .filter(|segment| segment.kind() == HistorySegmentKind::Compaction)
                .count(),
            1
        );
        assert!(successor.segments()[0].shares_storage_with(&new_summary));
        assert!(successor.segments()[1].shares_storage_with(&latest));
        assert!(successor.segments()[2].shares_storage_with(&latest_answer));
        assert!(
            successor
                .segments()
                .iter()
                .all(|segment| !segment.shares_storage_with(&old_summary))
        );
    }

    #[test]
    fn semantic_compaction_cut_rejects_working_protected_drift_and_invalid_summary() {
        let old = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "old")],
        )
        .expect("old intent");
        let old_answer = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(2, Actor::Assistant, "old answer")],
        )
        .expect("old answer");
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(3, Actor::User, "latest")],
        )
        .expect("latest intent");
        let committed = CanonicalHistory::new(vec![old.clone(), old_answer.clone()])
            .expect("committed history");
        let working =
            WorkingProjection::new(vec![latest.clone()], WorkingProjectionLimits::default())
                .expect("working");
        let working_source = ConversationView::from_shared(committed, working);
        let working_plan = ContextHistoryPlan::new(&working_source).expect("working plan");
        assert_eq!(
            SemanticCompactionCut::seal(&working_source, &working_plan),
            Err(SemanticCompactionCutError::WorkingProjectionNotEmpty)
        );

        let protected = ConversationView::from_shared(
            CanonicalHistory::new(vec![latest.clone()]).expect("protected history"),
            WorkingProjection::default(),
        );
        let protected_plan = ContextHistoryPlan::new(&protected).expect("protected plan");
        assert_eq!(
            SemanticCompactionCut::seal(&protected, &protected_plan),
            Err(SemanticCompactionCutError::NoEligibleSelectedPrefix)
        );

        let source = ConversationView::from_shared(
            CanonicalHistory::new(vec![old.clone(), old_answer, latest.clone()])
                .expect("source history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("source plan");
        assert_eq!(
            SemanticCompactionCut::seal(&protected, &plan),
            Err(SemanticCompactionCutError::SourcePlanDrift)
        );
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("cut");
        let ordinary = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(900, Actor::Assistant, "not a summary")],
        )
        .expect("ordinary segment");
        assert_eq!(
            cut.build_successor(&ordinary),
            Err(SemanticCompactionCutError::InvalidSummary)
        );
        assert_eq!(
            cut.build_successor(&compaction_summary(1, "duplicate source ID")),
            Err(SemanticCompactionCutError::InvalidSummary)
        );
    }

    #[test]
    fn semantic_compaction_cut_rejects_nonprefix_and_noncanonical_prior_summary() {
        let leading = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(1, Actor::Assistant, "orphan")],
        )
        .expect("leading orphan");
        let old = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, Actor::User, "old")],
        )
        .expect("old intent");
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(3, Actor::User, "latest")],
        )
        .expect("latest intent");
        // An orphan segment before the first intent is outside every intent
        // group, so the selection cannot fold it: it is retained verbatim and
        // only the complete older intent group is replaced.
        let nonprefix = ConversationView::from_shared(
            CanonicalHistory::new(vec![leading.clone(), old.clone(), latest.clone()])
                .expect("nonprefix history"),
            WorkingProjection::default(),
        );
        let nonprefix_plan = ContextHistoryPlan::new(&nonprefix).expect("nonprefix plan");
        let nonprefix_cut =
            SemanticCompactionCut::seal(&nonprefix, &nonprefix_plan).expect("nonprefix cut");
        assert_eq!(
            nonprefix_cut
                .selected_segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![HistorySegmentKind::RecentIntent]
        );
        let orphan_successor = nonprefix_cut
            .build_successor(&compaction_summary(20, "folded"))
            .expect("orphan successor");
        assert_eq!(
            orphan_successor
                .segments()
                .iter()
                .map(HistorySegment::kind)
                .collect::<Vec<_>>(),
            vec![
                HistorySegmentKind::Compaction,
                HistorySegmentKind::Conversation,
                HistorySegmentKind::RecentIntent,
            ]
        );
        assert!(orphan_successor.segments()[1].shares_storage_with(&leading));

        let invalid_item = ConversationItem::new(
            EntryId::try_from(Uuid::from_u128(10)).expect("entry ID"),
            Actor::Assistant,
            vec![
                ContentBlock::Compaction(CompactionBlock::new("prior").expect("compaction block")),
                ContentBlock::Text(TextBlock::new("extra", None).expect("text")),
            ],
            Provenance::ModelOutput,
        )
        .expect("invalid compaction item");
        let invalid_prior = HistorySegment::new(HistorySegmentKind::Compaction, vec![invalid_item])
            .expect("loosely valid prior compaction");
        let invalid_source = ConversationView::from_shared(
            CanonicalHistory::new(vec![invalid_prior, old, latest]).expect("invalid source"),
            WorkingProjection::default(),
        );
        let invalid_plan = ContextHistoryPlan::new(&invalid_source).expect("invalid plan");
        assert_eq!(
            SemanticCompactionCut::seal(&invalid_source, &invalid_plan),
            Err(SemanticCompactionCutError::InvalidSelectedSemantics)
        );
    }

    #[test]
    fn large_semantic_compaction_cut_shares_every_selected_and_retained_payload() {
        const OLDER_INTENTS: usize = 2_048;
        let mut segments = Vec::with_capacity(OLDER_INTENTS.saturating_mul(2).saturating_add(2));
        for intent in 0..OLDER_INTENTS {
            let base = u128::try_from(intent)
                .expect("bounded intent position")
                .saturating_mul(10)
                .saturating_add(1);
            segments.push(
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(base, Actor::User, "older intent")],
                )
                .expect("older intent"),
            );
            segments.push(
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(
                        base.saturating_add(1),
                        Actor::Assistant,
                        "older answer",
                    )],
                )
                .expect("older answer"),
            );
        }
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(100_000, Actor::User, "latest intent")],
        )
        .expect("latest intent");
        let latest_answer = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(100_001, Actor::Assistant, "latest answer")],
        )
        .expect("latest answer");
        segments.push(latest.clone());
        segments.push(latest_answer.clone());
        let first = segments[0].clone();
        let selected_count = segments.len().saturating_sub(2);
        let middle = segments[selected_count / 2].clone();
        let source = ConversationView::from_shared(
            CanonicalHistory::new(segments).expect("large history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("large plan");
        let cut = SemanticCompactionCut::seal(&source, &plan).expect("large cut");

        assert_eq!(cut.selected_segments().len(), selected_count);
        assert_eq!(
            cut.summary_input().history().segments().len(),
            selected_count
        );
        assert_eq!(cut.retained_segment_count(), 2);
        assert!(cut.selected_segments()[0].shares_storage_with(&first));
        assert!(cut.selected_segments()[selected_count / 2].shares_storage_with(&middle));
        assert!(cut.retained_segments()[0].shares_storage_with(&latest));
        assert!(cut.retained_segments()[1].shares_storage_with(&latest_answer));

        let summary = compaction_summary(200_000, "large folded prefix");
        let successor = cut.build_successor(&summary).expect("successor");
        assert_eq!(successor.segments().len(), 3);
        assert!(successor.segments()[1].shares_storage_with(&latest));
        assert!(successor.segments()[2].shares_storage_with(&latest_answer));
    }

    #[test]
    fn stale_or_multiple_model_visible_compaction_layouts_fail_closed() {
        let late = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(1, Actor::User, "retained intent")],
                )
                .expect("intent"),
                compaction(2),
            ])
            .expect("late history"),
            WorkingProjection::default(),
        );
        ContextHistoryPlan::new(&late).expect("compaction-last successor");

        let multiple = ConversationView::from_shared(
            CanonicalHistory::new(vec![compaction(3), compaction(4)]).expect("multiple history"),
            WorkingProjection::default(),
        );
        assert_eq!(
            ContextHistoryPlan::new(&multiple),
            Err(HistoryPlanningError::NoncanonicalCompaction)
        );

        let middle = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(5, Actor::User, "before compaction")],
                )
                .expect("leading intent"),
                compaction(6),
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(7, Actor::User, "after compaction")],
                )
                .expect("trailing intent"),
            ])
            .expect("middle history"),
            WorkingProjection::default(),
        );
        ContextHistoryPlan::new(&middle)
            .expect("native compact successor plus a later user remains plannable");
    }

    #[test]
    fn latest_working_intent_closes_the_last_committed_older_group() {
        let old_intent = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "old")],
        )
        .expect("old intent");
        let old_tool = complete_exchange(600);
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(2, Actor::User, "working latest")],
        )
        .expect("latest intent");
        let latest_answer = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(3, Actor::Assistant, "working tail")],
        )
        .expect("working tail");
        let history = CanonicalHistory::new(vec![old_intent, old_tool]).expect("history");
        let working = WorkingProjection::new(
            vec![latest.clone(), latest_answer.clone()],
            WorkingProjectionLimits::default(),
        )
        .expect("working");
        let source = ConversationView::from_shared(history.clone(), working);
        let plan = ContextHistoryPlan::new(&source).expect("plan");

        assert_eq!(plan.latest_user_intent_position(), Some(2));
        assert_eq!(
            plan.selected_intent_groups()
                .iter()
                .map(|group| (group.start_position(), group.end_position()))
                .collect::<Vec<_>>(),
            vec![(0, 2)]
        );
        assert_eq!(
            plan.retained_tail()
                .map(|tail| (tail.start_position(), tail.segment_count())),
            Some((2, 2))
        );
        assert!(plan.shares_source_history_with(&history));
        let retained = plan
            .summary_replacement_tail()
            .expect("summary replacement tail")
            .segments()
            .collect::<Vec<_>>();
        assert!(retained[0].shares_storage_with(&latest));
        assert!(retained[1].shares_storage_with(&latest_answer));
    }

    #[test]
    fn steering_without_user_intent_anchors_a_protected_tail() {
        let source = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(1, Actor::Assistant, "before")],
                )
                .expect("conversation"),
                HistorySegment::new(
                    HistorySegmentKind::Steering,
                    vec![text(2, Actor::User, "steer")],
                )
                .expect("steering"),
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(3, Actor::Assistant, "after")],
                )
                .expect("conversation"),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        assert_eq!(plan.latest_user_intent_position(), None);
        assert_eq!(plan.selected_segments(), []);
        // Nothing is selected, so the retained set is the whole committed
        // history — the steering anchor does not discard the orphan before it.
        assert_eq!(
            plan.retained_tail()
                .map(|tail| (tail.start_position(), tail.segment_count())),
            Some((0, 3))
        );
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );
    }

    #[test]
    fn only_protected_latest_group_has_no_summary_selection() {
        let source = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                compaction(10),
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(11, Actor::User, "latest")],
                )
                .expect("latest intent"),
                complete_exchange(700),
                reasoning(800),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        assert_eq!(plan.selected_intent_groups(), []);
        assert_eq!(plan.selected_segments(), []);
        assert!(plan.summary_replacement_tail().is_none());
        assert_eq!(
            plan.retained_tail()
                .map(|tail| (tail.start_position(), tail.segment_count())),
            Some((1, 3))
        );
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );
    }

    #[test]
    fn user_bearing_ordinary_segment_is_the_latest_semantic_intent() {
        let mixed = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![
                text(3, Actor::User, "ordinary user intent"),
                text(4, Actor::Assistant, "same atomic segment"),
            ],
        )
        .expect("mixed intent");
        let source = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::RecentIntent,
                    vec![text(1, Actor::User, "old")],
                )
                .expect("old intent"),
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(2, Actor::Assistant, "old answer")],
                )
                .expect("old answer"),
                mixed,
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![text(5, Actor::Assistant, "new tail")],
                )
                .expect("tail"),
            ])
            .expect("history"),
            WorkingProjection::default(),
        );
        let plan = ContextHistoryPlan::new(&source).expect("plan");
        assert_eq!(plan.latest_user_intent_position(), Some(2));
        assert_eq!(
            plan.selected_segments()
                .iter()
                .map(|segment| segment.position())
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(
            plan.retained_tail().map(|tail| tail.start_position()),
            Some(2)
        );
    }

    #[test]
    fn incomplete_tool_exchange_cannot_enter_a_request_projection() {
        let call_id = CallId::try_from(Uuid::from_u128(7)).expect("call ID");
        let call = entry(
            1,
            Actor::Assistant,
            ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    ToolName::new("read").expect("tool"),
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    ToolCorrelation::default(),
                    "{}",
                )
                .expect("call"),
            ),
        );
        assert_eq!(
            HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call.clone()]),
            Err(HistoryError::IncompleteToolExchange)
        );
        let inferred = ConversationView::from_items(vec![call])
            .expect("test conversation items form one valid segment");
        assert_eq!(
            inferred.validate_model_visible(),
            Err(HistoryError::IncompleteToolExchange)
        );
        assert_eq!(
            ContextHistoryPlan::new(&inferred),
            Err(HistoryPlanningError::IncompleteGroup { position: 0 })
        );

        let result = entry(
            2,
            Actor::Tool,
            ContentBlock::ToolResult(
                ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new("ok", None).expect("text"),
                    )],
                    ToolCorrelation::default(),
                )
                .expect("result"),
            ),
        );
        assert!(
            HistorySegment::new(
                HistorySegmentKind::ToolExchange,
                vec![inferred.items()[0].clone(), result,]
            )
            .is_ok()
        );
    }

    #[test]
    fn semantic_prefix_identity_accepts_inside_tool_exchange_item_boundary() {
        let call_id = CallId::try_from(Uuid::from_u128(70)).expect("call ID");
        let correlation = ToolCorrelation::new(
            Some("remote-70".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )
        .expect("correlation");
        let call = entry(
            71,
            Actor::Assistant,
            ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    ToolName::new("read").expect("tool"),
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    "{}",
                )
                .expect("call"),
            ),
        );
        let result = entry(
            72,
            Actor::Tool,
            ContentBlock::ToolResult(
                ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new("ok", None).expect("text"),
                    )],
                    correlation,
                )
                .expect("result"),
            ),
        );
        let segment = HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call, result])
            .expect("complete tool exchange");
        let minimum_retained = segment
            .canonical_bytes()
            .len()
            .saturating_mul(2)
            .saturating_add(size_of::<HistorySegmentInner>())
            .saturating_add(2_usize.saturating_mul(size_of::<Digest>()));
        assert!(segment.retained_bytes_upper_bound() >= minimum_retained);

        let view = ConversationView::from_shared(
            CanonicalHistory::new(vec![segment]).expect("history"),
            WorkingProjection::default(),
        );
        let empty = view.semantic_prefix_identity(0).expect("empty prefix");
        let after_output = view
            .semantic_prefix_identity(1)
            .expect("complete output-item boundary");
        let full = view.semantic_prefix_identity(2).expect("full prefix");
        assert_ne!(empty.identity(), after_output.identity());
        assert_ne!(after_output.identity(), full.identity());
        assert_eq!(full.identity(), view.identity());
        assert_eq!(after_output.item_count(), 1);
        assert_eq!(
            view.semantic_prefix_identity(3),
            Err(HistoryError::PrefixOutsideProjection)
        );
    }

    #[test]
    fn canonical_item_prefix_ignores_regrouping_but_full_projection_does_not() {
        let first = text(80, Actor::Assistant, "one");
        let second = text(81, Actor::Assistant, "two");
        let grouped = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(
                    HistorySegmentKind::Conversation,
                    vec![first.clone(), second.clone()],
                )
                .expect("grouped segment"),
            ])
            .expect("grouped history"),
            WorkingProjection::default(),
        );
        let split = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                HistorySegment::new(HistorySegmentKind::Conversation, vec![first])
                    .expect("first split segment"),
                HistorySegment::new(HistorySegmentKind::Conversation, vec![second])
                    .expect("second split segment"),
            ])
            .expect("split history"),
            WorkingProjection::default(),
        );

        assert_ne!(grouped.identity(), split.identity());
        assert_ne!(
            grouped.semantic_prefix_identity(2).expect("grouped prefix"),
            split.semantic_prefix_identity(2).expect("split prefix")
        );
        let grouped_items = grouped
            .canonical_item_prefix_identity(2)
            .expect("grouped item prefix");
        assert_eq!(
            grouped_items,
            split
                .canonical_item_prefix_identity(2)
                .expect("split item prefix")
        );
        assert_eq!(
            grouped_items.identity().domain(),
            DigestDomain::CanonicalItemPrefix
        );
        assert_eq!(
            grouped_items.identity().version(),
            CANONICAL_ITEM_PREFIX_DIGEST_REVISION
        );
    }

    #[test]
    fn staged_item_prefix_matches_later_complete_tool_exchange() {
        let intent = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(90, Actor::User, "use the tool")],
        )
        .expect("intent");
        let base = ConversationView::from_shared(
            CanonicalHistory::new(vec![intent.clone()]).expect("base history"),
            WorkingProjection::default(),
        );
        let call_id = CallId::try_from(Uuid::from_u128(91)).expect("call ID");
        let correlation = ToolCorrelation::new(
            Some("remote-91".to_owned()),
            None,
            ToolCallerLineage::Direct,
        )
        .expect("correlation");
        let call = entry(
            92,
            Actor::Assistant,
            ContentBlock::ToolCall(
                ToolCallBlock::new(
                    call_id,
                    ToolName::new("read").expect("tool"),
                    ToolExecutionOwner::Local,
                    ToolCallState::Requested,
                    correlation.clone(),
                    "{}",
                )
                .expect("call"),
            ),
        );
        let staged = base
            .staged_canonical_item_prefix_identity(slice::from_ref(&call))
            .expect("staged prefix");
        let result = entry(
            93,
            Actor::Tool,
            ContentBlock::ToolResult(
                ToolResultBlock::new(
                    call_id,
                    ToolExecutionOwner::Local,
                    ToolResultKind::Success,
                    vec![ToolResultPart::Text(
                        TextBlock::new("ok", None).expect("text"),
                    )],
                    correlation,
                )
                .expect("result"),
            ),
        );
        let complete = ConversationView::from_shared(
            CanonicalHistory::new(vec![
                intent,
                HistorySegment::new(HistorySegmentKind::ToolExchange, vec![call.clone(), result])
                    .expect("complete exchange"),
            ])
            .expect("complete history"),
            WorkingProjection::default(),
        );

        assert_eq!(
            staged,
            complete
                .canonical_item_prefix_identity(2)
                .expect("committed prefix")
        );
        assert_eq!(
            base.staged_canonical_item_prefix_identity(&[call.clone(), call]),
            Err(HistoryError::DuplicateEntry)
        );
    }

    #[test]
    fn working_projection_limits_are_enforced_without_flattening() {
        let segment = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(1, Actor::Assistant, "one")],
        )
        .expect("segment");
        let limits = WorkingProjectionLimits::new(1, 1, usize::MAX).expect("limits");
        assert!(WorkingProjection::new(vec![segment.clone()], limits).is_ok());
        assert!(matches!(
            WorkingProjection::new(vec![segment.clone(), segment], limits),
            Err(HistoryError::LimitExceeded {
                field: "working projection segments",
                maximum: 1,
            })
        ));
    }

    #[test]
    fn context_plan_bound_accepts_maximum_and_rejects_one_more_group() {
        let segment = HistorySegment::new(
            HistorySegmentKind::Conversation,
            vec![text(1, Actor::Assistant, "bounded")],
        )
        .expect("segment");
        let limits = WorkingProjectionLimits::new(
            MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1),
            MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1),
            usize::MAX,
        )
        .expect("limits");
        let maximum = ConversationView::from_shared(
            CanonicalHistory::default(),
            WorkingProjection::new(vec![segment.clone(); MAX_CONTEXT_PLANNER_SEGMENTS], limits)
                .expect("maximum working projection"),
        );
        let plan = ContextHistoryPlan::new(&maximum).expect("maximum plan");
        assert_eq!(
            plan.disposition().retained_segment_count(),
            u32::try_from(MAX_CONTEXT_PLANNER_SEGMENTS).expect("bounded count")
        );
        assert_eq!(
            plan.disposition().kind(),
            ContextPlannerDispositionKind::ProtectedOnly
        );

        let excessive = ConversationView::from_shared(
            CanonicalHistory::default(),
            WorkingProjection::new(
                vec![segment; MAX_CONTEXT_PLANNER_SEGMENTS.saturating_add(1)],
                limits,
            )
            .expect("excessive working projection"),
        );
        assert_eq!(
            ContextHistoryPlan::new(&excessive),
            Err(HistoryPlanningError::LimitExceeded)
        );
    }

    #[test]
    fn long_plan_keeps_source_and_retained_payloads_structurally_shared() {
        let first = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "old intent")],
        )
        .expect("first intent");
        let mut history = CanonicalHistory::new(vec![first]).expect("history");
        for value in 2..=2_049_u128 {
            history = history
                .append(
                    HistorySegment::new(
                        HistorySegmentKind::Conversation,
                        vec![text(value, Actor::Assistant, "shared payload")],
                    )
                    .expect("conversation"),
                )
                .expect("append");
        }
        let latest = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(3_000, Actor::User, "latest")],
        )
        .expect("latest intent");
        history = history.append(latest.clone()).expect("append latest");
        let source = ConversationView::from_shared(history.clone(), WorkingProjection::default());
        let left = ContextHistoryPlan::new(&source).expect("left plan");
        let right = ContextHistoryPlan::new(&source).expect("right plan");

        assert!(left.shares_source_history_with(&history));
        assert_eq!(left.identity(), right.identity());
        assert_eq!(
            left.disposition().identity(),
            right.disposition().identity()
        );
        assert_eq!(left.selected_intent_groups().len(), 1);
        assert_eq!(left.selected_segments().len(), 2_049);
        let retained = left
            .summary_replacement_tail()
            .expect("summary replacement tail")
            .segments()
            .collect::<Vec<_>>();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].shares_storage_with(&latest));
    }

    #[test]
    fn segment_bytes_and_identity_are_deterministic() {
        let left = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "same")],
        )
        .expect("left");
        let right = HistorySegment::new(
            HistorySegmentKind::RecentIntent,
            vec![text(1, Actor::User, "same")],
        )
        .expect("right");
        assert_eq!(left.canonical_bytes(), right.canonical_bytes());
        assert_eq!(left.identity(), right.identity());
        assert!(!left.shares_storage_with(&right));
    }
}
