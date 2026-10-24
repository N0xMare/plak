//! Working projections and the segmented model-visible conversation view.

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Index;
use std::slice;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{ConversationItem, Digest, DigestDomain, DigestError};

use super::{
    CANONICAL_ITEM_PREFIX_DIGEST_REVISION, CanonicalHistory, CanonicalSegmentIter, HistoryError,
    HistorySegment, HistorySegmentKind, projection_item_identity,
};

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

    pub(super) fn segments(&self) -> impl Iterator<Item = &HistorySegment> {
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
