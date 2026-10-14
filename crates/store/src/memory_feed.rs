//! The memory backend's change feed: one bounded in-memory log per session.

use std::collections::{BTreeMap, VecDeque};
use std::num::NonZeroUsize;
use std::time::{SystemTime, UNIX_EPOCH};

use plakous_agent::internal::{
    BranchHead, BranchId, FeedChange, FeedFact, FeedHeader, FeedPage, FeedRecord, FeedSeq,
    FeedTail, MAX_FEED_BYTES_PER_SESSION, MAX_FEED_PAGE_BYTES, MAX_FEED_RECORD_BYTES,
    MAX_FEED_RECORDS_PER_SESSION, MAX_FEED_TRIM_BATCH, SessionId, SessionRef, SessionStoreError,
    StoreCapacityBound,
};

/// Retention bounds of each session's feed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryFeedLimits {
    records: u64,
    bytes: u64,
    trim_batch: NonZeroUsize,
}

impl Default for MemoryFeedLimits {
    fn default() -> Self {
        Self::STANDARD
    }
}

impl MemoryFeedLimits {
    /// The bounds of spec 01 §5.11.
    pub const STANDARD: Self = Self {
        records: MAX_FEED_RECORDS_PER_SESSION,
        bytes: MAX_FEED_BYTES_PER_SESSION,
        trim_batch: match NonZeroUsize::new(MAX_FEED_TRIM_BATCH) {
            Some(batch) => batch,
            None => NonZeroUsize::MIN,
        },
    };

    /// Creates explicit per-session retention bounds and a trim batch size.
    /// Production stores use the defaults; smaller bounds exist so tests can
    /// reach them.
    #[must_use]
    pub const fn new(records: u64, bytes: u64, trim_batch: NonZeroUsize) -> Self {
        Self {
            records,
            bytes,
            trim_batch,
        }
    }

    /// The most records one trim transaction deletes.
    pub(crate) const fn trim_batch(self) -> NonZeroUsize {
        self.trim_batch
    }

    /// The retained counts a session over a bound trims down to: seven
    /// eighths of each exceeded bound, `u64::MAX` for one not exceeded.
    /// `None` when neither is exceeded.
    pub(crate) fn trim_target(self, records: u64, bytes: u64) -> Option<(u64, u64)> {
        let over_records = records > self.records;
        let over_bytes = bytes > self.bytes;
        (over_records || over_bytes).then(|| {
            (
                if over_records {
                    self.records / 8 * 7
                } else {
                    u64::MAX
                },
                if over_bytes {
                    self.bytes / 8 * 7
                } else {
                    u64::MAX
                },
            )
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct SessionFeed {
    records: VecDeque<(FeedRecord, u64)>,
    tail: FeedSeq,
    bytes: u64,
}

impl SessionFeed {
    fn window(&self) -> FeedTail {
        let retained = u64::try_from(self.records.len()).unwrap_or(u64::MAX);
        FeedTail::from_store(
            FeedSeq::new(self.tail.get().saturating_sub(retained).saturating_add(1)),
            self.tail,
        )
    }
}

/// One encoded record whose mutation has passed every check but not yet
/// published. Appending it is infallible, so it commits with its mutation.
pub(crate) struct PendingRecord {
    session_id: SessionId,
    record: FeedRecord,
    bytes: u64,
}

/// Encodes the next record of `session_id`, refusing one above the record
/// bound before anything is published.
pub(crate) fn prepare(
    feeds: &BTreeMap<SessionId, SessionFeed>,
    session_id: SessionId,
    branch_id: BranchId,
    change: FeedChange,
    facts: &[FeedFact],
) -> Result<PendingRecord, SessionStoreError> {
    let tail = feeds
        .get(&session_id)
        .map_or(FeedSeq::ZERO, |feed| feed.tail);
    let seq = tail.checked_next().ok_or_else(|| {
        SessionStoreError::capacity(StoreCapacityBound::FeedRecordBytes, u64::MAX, 0)
    })?;
    let record = FeedRecord::from_store(
        FeedHeader {
            session_id,
            seq,
            branch_id,
            committed_at_ms: wall_clock_ms(),
            change,
        },
        facts.to_vec(),
    );
    let bytes = encode(&record)?.len();
    Ok(PendingRecord {
        session_id,
        record,
        bytes: u64::try_from(bytes).unwrap_or(u64::MAX),
    })
}

/// Encodes one record, refusing one above the record bound as a typed
/// capacity error.
pub(crate) fn encode(record: &FeedRecord) -> Result<Vec<u8>, SessionStoreError> {
    match record.encode() {
        Ok(bytes) => Ok(bytes),
        Err(plakous_agent::internal::FeedCodecError::RecordTooLarge { bytes }) => {
            Err(SessionStoreError::capacity(
                StoreCapacityBound::FeedRecordBytes,
                u64::try_from(bytes).unwrap_or(u64::MAX),
                u64::try_from(MAX_FEED_RECORD_BYTES).unwrap_or(u64::MAX),
            ))
        }
        Err(_) => Err(SessionStoreError::corrupt()),
    }
}

/// [`prepare`] for a store-originated change, which carries no facts.
pub(crate) fn prepare_change(
    feeds: &BTreeMap<SessionId, SessionFeed>,
    session: SessionRef,
    change: FeedChange,
) -> Result<PendingRecord, SessionStoreError> {
    prepare(
        feeds,
        session.session_id(),
        session.branch_id(),
        change,
        &[],
    )
}

impl PendingRecord {
    pub(crate) fn seq(&self) -> FeedSeq {
        self.record.header().seq
    }
}

pub(crate) fn append(feeds: &mut BTreeMap<SessionId, SessionFeed>, pending: PendingRecord) {
    let feed = feeds.entry(pending.session_id).or_default();
    feed.tail = pending.record.header().seq;
    feed.bytes = feed.bytes.saturating_add(pending.bytes);
    feed.records.push_back((pending.record, pending.bytes));
}

pub(crate) fn read(
    feed: &SessionFeed,
    heads: impl FnOnce() -> Vec<BranchHead>,
    after: FeedSeq,
    max_records: NonZeroUsize,
) -> FeedPage {
    let window = feed.window();
    if after.get() < window.retained_from().get().saturating_sub(1) {
        return FeedPage::BehindRetention {
            window,
            heads: heads(),
        };
    }
    let skip = usize::try_from(
        after
            .get()
            .saturating_add(1)
            .saturating_sub(window.retained_from().get()),
    )
    .unwrap_or(usize::MAX);
    let mut records = Vec::new();
    let mut bytes = 0_u64;
    let page_bytes = u64::try_from(MAX_FEED_PAGE_BYTES).unwrap_or(u64::MAX);
    for (record, size) in feed.records.iter().skip(skip).take(max_records.get()) {
        if !records.is_empty() && bytes.saturating_add(*size) > page_bytes {
            break;
        }
        bytes = bytes.saturating_add(*size);
        records.push(record.clone());
    }
    FeedPage::Records { records, window }
}

/// The retained counts one trim reduces `feed` to, when a bound is exceeded.
pub(crate) fn trim_target(feed: &SessionFeed, limits: MemoryFeedLimits) -> Option<(u64, u64)> {
    limits.trim_target(
        u64::try_from(feed.records.len()).unwrap_or(u64::MAX),
        feed.bytes,
    )
}

/// Deletes at most one batch of the oldest records above `target`. Returns
/// whether the feed is still above it.
pub(crate) fn trim_batch(
    feed: &mut SessionFeed,
    target: (u64, u64),
    limits: MemoryFeedLimits,
) -> bool {
    let above = |feed: &SessionFeed| {
        u64::try_from(feed.records.len()).unwrap_or(u64::MAX) > target.0 || feed.bytes > target.1
    };
    for _ in 0..limits.trim_batch.get() {
        if !above(feed) {
            return false;
        }
        let Some((_, bytes)) = feed.records.pop_front() else {
            return false;
        };
        feed.bytes = feed.bytes.saturating_sub(bytes);
    }
    above(feed)
}

pub(crate) fn tail(feed: &SessionFeed) -> FeedTail {
    feed.window()
}

pub(crate) fn wall_clock_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}
