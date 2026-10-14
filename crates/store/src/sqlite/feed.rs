//! The `SQLite` change feed (spec 01 §5.2 and §5.11): one `session_feed` row
//! per record, appended inside the transaction that publishes its mutation,
//! with the session's window kept on its `sessions` row.

use std::num::NonZeroUsize;

use plakous_agent::internal::{
    BranchHead, CheckpointId, FeedChange, FeedFact, FeedHeadKind, FeedHeader, FeedPage, FeedRecord,
    FeedSeq, FeedTail, MAX_FEED_PAGE_BYTES, RuntimeCheckpointKind, SessionId, SessionRef,
    SessionStoreError, StoreCapacityBound,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{
    Backend, branch_row, exactly_one, i64n, id, map_sqlite, nonnegative, parse, receipt_integrity,
    require_bound_domain, text_bytes, u64n,
};
use crate::memory_feed;

/// One session's retained window as its `sessions` row records it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Window {
    retained_from: u64,
    tail: u64,
    bytes: u64,
}

impl Window {
    fn decode(retained_from: i64, tail: i64, bytes: i64) -> Result<Self, SessionStoreError> {
        let window = Self {
            retained_from: nonnegative(retained_from)?,
            tail: nonnegative(tail)?,
            bytes: nonnegative(bytes)?,
        };
        if window.retained_from == 0 || window.retained_from > window.tail.saturating_add(1) {
            return Err(SessionStoreError::corrupt());
        }
        Ok(window)
    }

    const fn records(self) -> u64 {
        self.tail + 1 - self.retained_from
    }

    const fn observed(self) -> FeedTail {
        FeedTail::from_store(FeedSeq::new(self.retained_from), FeedSeq::new(self.tail))
    }
}

fn load_window(c: &Connection, session_id: SessionId) -> Result<Option<Window>, SessionStoreError> {
    c.query_row(
        "SELECT feed_retained_from,feed_tail,feed_bytes FROM sessions WHERE session_id=?1",
        [id(session_id)],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|(retained_from, tail, bytes)| Window::decode(retained_from, tail, bytes))
    .transpose()
}

pub(super) const fn head_kind(kind: RuntimeCheckpointKind) -> FeedHeadKind {
    match kind {
        RuntimeCheckpointKind::Quiescent => FeedHeadKind::Quiescent,
        RuntimeCheckpointKind::Working => FeedHeadKind::Working,
    }
}

/// The stored tag of a change; none for a change this schema cannot store.
const fn change_tag(change: &FeedChange) -> Option<i64> {
    match change {
        FeedChange::Created { .. } => Some(1),
        FeedChange::Advanced { .. } => Some(2),
        FeedChange::Branched { .. } => Some(3),
        FeedChange::LeaseReleased { .. } => Some(4),
        FeedChange::Pruned { .. } => Some(5),
        FeedChange::Archived { .. } => Some(6),
        _ => None,
    }
}

/// The checkpoint a record publishes; none for a release, prune or archive.
const fn published_checkpoint(change: &FeedChange) -> Option<CheckpointId> {
    match change {
        FeedChange::Created { genesis, .. } => Some(*genesis),
        FeedChange::Advanced { checkpoint, .. } => Some(*checkpoint),
        FeedChange::Branched { target, .. } => Some(*target),
        _ => None,
    }
}

fn seal(
    session: SessionRef,
    seq: u64,
    tag: i64,
    checkpoint: Option<CheckpointId>,
    record: &[u8],
) -> Result<Vec<u8>, SessionStoreError> {
    receipt_integrity(
        b"feed-record-v1",
        &[
            text_bytes(session.session_id()),
            text_bytes(session.branch_id()),
            seq.to_be_bytes().to_vec(),
            tag.to_be_bytes().to_vec(),
            checkpoint.map_or_else(Vec::new, text_bytes),
            record.to_vec(),
        ],
    )
}

/// Appends the next record of `session` inside the publishing transaction
/// `c` and returns its sequence. A record above the record bound is refused
/// before anything commits; a rolled-back transaction consumes no sequence.
pub(super) fn append(
    c: &Connection,
    session: SessionRef,
    change: FeedChange,
    facts: &[FeedFact],
) -> Result<FeedSeq, SessionStoreError> {
    let window = load_window(c, session.session_id())?.ok_or(SessionStoreError::corrupt())?;
    let seq = FeedSeq::new(window.tail).checked_next().ok_or_else(|| {
        SessionStoreError::capacity(StoreCapacityBound::SqliteInteger, window.tail, 0)
    })?;
    let record = FeedRecord::from_store(
        FeedHeader {
            session_id: session.session_id(),
            seq,
            branch_id: session.branch_id(),
            committed_at_ms: memory_feed::wall_clock_ms(),
            change,
        },
        facts.to_vec(),
    );
    let bytes = memory_feed::encode(&record)?;
    let tag = change_tag(&change).ok_or(SessionStoreError::UnsupportedCodec)?;
    let checkpoint = published_checkpoint(&change);
    let integrity = seal(session, seq.get(), tag, checkpoint, &bytes)?;
    let size = i64n(u64n(bytes.len()))?;
    exactly_one(
        c.execute(
            "INSERT INTO session_feed VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                id(session.session_id()),
                i64n(seq.get())?,
                id(session.branch_id()),
                tag,
                checkpoint.map(id),
                bytes,
                integrity
            ],
        )
        .map_err(map_sqlite)?,
    )?;
    exactly_one(
        c.execute(
            "UPDATE sessions SET feed_tail=?2,feed_bytes=feed_bytes+?3 WHERE session_id=?1",
            params![id(session.session_id()), i64n(seq.get())?, size],
        )
        .map_err(map_sqlite)?,
    )?;
    Ok(seq)
}

/// Decodes one row, refusing a seal, encoding or header that disagrees with
/// the row that stores it.
fn decode_row(
    session_id: SessionId,
    seq: u64,
    branch: &str,
    tag: i64,
    checkpoint: Option<&str>,
    bytes: &[u8],
    integrity: &[u8],
) -> Result<FeedRecord, SessionStoreError> {
    let selector = SessionRef::new(session_id, parse(branch)?);
    let checkpoint = checkpoint.map(parse::<CheckpointId>).transpose()?;
    if seal(selector, seq, tag, checkpoint, bytes)? != integrity {
        return Err(SessionStoreError::corrupt());
    }
    let record = FeedRecord::decode(bytes).map_err(|_| SessionStoreError::corrupt())?;
    let header = record.header();
    if header.session_id != session_id
        || header.seq.get() != seq
        || header.branch_id != selector.branch_id()
        || change_tag(&header.change) != Some(tag)
        || published_checkpoint(&header.change) != checkpoint
    {
        return Err(SessionStoreError::corrupt());
    }
    Ok(record)
}

fn heads(c: &Connection, session_id: SessionId) -> Result<Vec<BranchHead>, SessionStoreError> {
    let mut statement = c
        .prepare("SELECT branch_id FROM branches WHERE session_id=?1 ORDER BY branch_id")
        .map_err(map_sqlite)?;
    let branches = statement
        .query_map([id(session_id)], |row| row.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    drop(statement);
    branches
        .iter()
        .map(|branch| {
            branch_row(c, SessionRef::new(session_id, parse(branch)?))?
                .map(|row| row.head)
                .ok_or(SessionStoreError::corrupt())
        })
        .collect()
}

/// Reads one bounded page after `after`. The caller holds one read
/// snapshot, so the window and the records are observed together.
pub(super) fn read(
    c: &Connection,
    session_id: SessionId,
    after: FeedSeq,
    max_records: NonZeroUsize,
) -> Result<FeedPage, SessionStoreError> {
    let window = load_window(c, session_id)?.ok_or(SessionStoreError::NotFound)?;
    if after.get() < window.retained_from - 1 {
        return Ok(FeedPage::BehindRetention {
            window: window.observed(),
            heads: heads(c, session_id)?,
        });
    }
    let mut records = Vec::new();
    if after.get() >= window.tail {
        return Ok(FeedPage::Records {
            records,
            window: window.observed(),
        });
    }
    let mut statement = c
        .prepare(
            "SELECT seq,branch_id,change,checkpoint_id,record,integrity FROM session_feed
             WHERE session_id=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement
        .query(params![
            id(session_id),
            i64n(after.get())?,
            i64::try_from(max_records.get()).unwrap_or(i64::MAX)
        ])
        .map_err(map_sqlite)?;
    let page_bytes = u64n(MAX_FEED_PAGE_BYTES);
    let mut bytes = 0_u64;
    let mut expected = after.get() + 1;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let seq = nonnegative(row.get(0).map_err(map_sqlite)?)?;
        if seq != expected {
            return Err(SessionStoreError::corrupt());
        }
        let encoded = row
            .get_ref(4)
            .map_err(map_sqlite)?
            .as_blob()
            .map_err(|_| SessionStoreError::corrupt())?;
        let size = u64n(encoded.len());
        if !records.is_empty() && bytes.saturating_add(size) > page_bytes {
            break;
        }
        let record = decode_row(
            session_id,
            seq,
            &row.get::<_, String>(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
            row.get::<_, Option<String>>(3)
                .map_err(map_sqlite)?
                .as_deref(),
            encoded,
            &row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?,
        )?;
        bytes = bytes.saturating_add(size);
        expected += 1;
        records.push(record);
    }
    if records.is_empty() {
        // The window names a record after the cursor that is not stored.
        return Err(SessionStoreError::corrupt());
    }
    Ok(FeedPage::Records {
        records,
        window: window.observed(),
    })
}

/// Reads one session's retained window.
pub(super) fn tail(c: &Connection, session_id: SessionId) -> Result<FeedTail, SessionStoreError> {
    Ok(load_window(c, session_id)?
        .ok_or(SessionStoreError::NotFound)?
        .observed())
}

/// Retained records and their encoded bytes summed over every session's
/// counters, without reading the feed.
pub(super) fn totals(c: &Connection) -> Result<(u64, u64), SessionStoreError> {
    let (records, bytes) = c
        .query_row(
            "SELECT COALESCE(sum(feed_tail+1-feed_retained_from),0),COALESCE(sum(feed_bytes),0)
             FROM sessions",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(map_sqlite)?;
    Ok((nonnegative(records)?, nonnegative(bytes)?))
}

/// Trims one session's feed after a publishing commit, never inside it: at
/// most one batch of the oldest records per transaction until the session is
/// back under seven eighths of each exceeded bound. Best effort: a failed
/// batch leaves the feed over its bound for the next publication to trim.
pub(super) fn trim(b: &mut Backend, session_id: SessionId) {
    #[cfg(feature = "commit-trace")]
    let _label = crate::commit_trace::label("feed_trim");
    let _ = trim_batches(b, session_id);
}

fn trim_batches(b: &mut Backend, session_id: SessionId) -> Result<(), SessionStoreError> {
    let limits = b.limits.feed_limits();
    let Some(window) = load_window(&b.connection, session_id)? else {
        return Ok(());
    };
    let Some((records_target, bytes_target)) = limits.trim_target(window.records(), window.bytes)
    else {
        return Ok(());
    };
    let above = |records: u64, bytes: u64| records > records_target || bytes > bytes_target;
    let batch = i64::try_from(limits.trim_batch().get()).unwrap_or(i64::MAX);
    loop {
        let tx = b
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;
        require_bound_domain(&tx, b.domain)?;
        let Some(window) = load_window(&tx, session_id)? else {
            return Ok(());
        };
        let mut records = window.records();
        let mut bytes = window.bytes;
        let mut next = window.retained_from;
        {
            let mut statement = tx
                .prepare(
                    "SELECT seq,length(record) FROM session_feed
                     WHERE session_id=?1 AND seq>=?2 ORDER BY seq LIMIT ?3",
                )
                .map_err(map_sqlite)?;
            let mut rows = statement
                .query(params![id(session_id), i64n(next)?, batch])
                .map_err(map_sqlite)?;
            while above(records, bytes) {
                let Some(row) = rows.next().map_err(map_sqlite)? else {
                    break;
                };
                if nonnegative(row.get(0).map_err(map_sqlite)?)? != next {
                    return Err(SessionStoreError::corrupt());
                }
                let size = nonnegative(row.get(1).map_err(map_sqlite)?)?;
                bytes = bytes
                    .checked_sub(size)
                    .ok_or(SessionStoreError::corrupt())?;
                records -= 1;
                next += 1;
            }
        }
        if next == window.retained_from {
            return Ok(());
        }
        tx.execute(
            "DELETE FROM session_feed WHERE session_id=?1 AND seq<?2",
            params![id(session_id), i64n(next)?],
        )
        .map_err(map_sqlite)?;
        exactly_one(
            tx.execute(
                "UPDATE sessions SET feed_retained_from=?2,feed_bytes=?3 WHERE session_id=?1",
                params![id(session_id), i64n(next)?, i64n(bytes)?],
            )
            .map_err(map_sqlite)?,
        )?;
        tx.commit().map_err(map_sqlite)?;
        if !above(records, bytes) {
            return Ok(());
        }
    }
}

/// Full verification of every stored record, in one ordered scan: each
/// session's rows run gap-free from its `feed_retained_from` to its
/// `feed_tail`, sum to its `feed_bytes`, and carry a seal, encoding and header
/// that agree with their row. Window sanity, empty windows and the receipts'
/// records are checked by the receipt-completeness statement.
pub(super) fn verify(c: &Connection) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT f.session_id,f.seq,f.branch_id,f.change,f.checkpoint_id,f.record,f.integrity,
             s.feed_retained_from,s.feed_tail,s.feed_bytes
             FROM session_feed f JOIN sessions s USING(session_id) ORDER BY f.session_id,f.seq",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    // (session, its window, next expected sequence, bytes so far)
    let mut current: Option<(SessionId, Window, u64, u64)> = None;
    let finish = |done: Option<(SessionId, Window, u64, u64)>| match done {
        Some((_, window, next, bytes)) if next != window.tail + 1 || bytes != window.bytes => {
            Err(SessionStoreError::corrupt())
        }
        _ => Ok(()),
    };
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id: SessionId = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        if current.is_none_or(|(session, ..)| session != session_id) {
            finish(current.take())?;
            let window = Window::decode(
                row.get(7).map_err(map_sqlite)?,
                row.get(8).map_err(map_sqlite)?,
                row.get(9).map_err(map_sqlite)?,
            )?;
            current = Some((session_id, window, window.retained_from, 0));
        }
        let Some((_, _, next, bytes)) = current.as_mut() else {
            return Err(SessionStoreError::corrupt());
        };
        let seq = nonnegative(row.get(1).map_err(map_sqlite)?)?;
        if seq != *next {
            return Err(SessionStoreError::corrupt());
        }
        let encoded = row
            .get_ref(5)
            .map_err(map_sqlite)?
            .as_blob()
            .map_err(|_| SessionStoreError::corrupt())?;
        decode_row(
            session_id,
            seq,
            &row.get::<_, String>(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
            row.get::<_, Option<String>>(4)
                .map_err(map_sqlite)?
                .as_deref(),
            encoded,
            &row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?,
        )?;
        *bytes = bytes
            .checked_add(u64n(encoded.len()))
            .ok_or(SessionStoreError::corrupt())?;
        *next += 1;
    }
    finish(current)
}

/// The receipt-completeness clauses for the feed: a window that is not
/// `1 <= feed_retained_from <= feed_tail + 1`, a row outside its session's
/// window, an empty window that claims bytes, and a create, commit or fork
/// receipt whose sequence is above the tail or whose record is missing (or is
/// not that receipt's change) at or above `feed_retained_from`.
pub(super) const COMPLETENESS: &str = "
             OR EXISTS(SELECT 1 FROM sessions s
                    WHERE s.feed_retained_from<1 OR s.feed_retained_from>s.feed_tail+1
                       OR (s.feed_retained_from=s.feed_tail+1 AND s.feed_bytes<>0))
             OR EXISTS(SELECT 1 FROM session_feed f JOIN sessions s USING(session_id)
                    WHERE f.seq<s.feed_retained_from OR f.seq>s.feed_tail)
             OR EXISTS(SELECT 1 FROM create_receipts r JOIN sessions s USING(session_id)
                    WHERE r.feed_seq>s.feed_tail OR (r.feed_seq>=s.feed_retained_from
                      AND NOT EXISTS(SELECT 1 FROM session_feed f
                          WHERE f.session_id=r.session_id AND f.seq=r.feed_seq AND f.change=1
                            AND f.branch_id=r.branch_id AND f.checkpoint_id=r.checkpoint_id)))
             OR EXISTS(SELECT 1 FROM commit_receipts r JOIN sessions s USING(session_id)
                    WHERE r.feed_seq>s.feed_tail OR (r.feed_seq>=s.feed_retained_from
                      AND NOT EXISTS(SELECT 1 FROM session_feed f
                          WHERE f.session_id=r.session_id AND f.seq=r.feed_seq AND f.change=2
                            AND f.branch_id=r.branch_id AND f.checkpoint_id=r.checkpoint_id)))
             OR EXISTS(SELECT 1 FROM fork_receipts r JOIN sessions s ON s.session_id=r.target_session
                    WHERE r.feed_seq>s.feed_tail OR (r.feed_seq>=s.feed_retained_from
                      AND NOT EXISTS(SELECT 1 FROM session_feed f
                          WHERE f.session_id=r.target_session AND f.seq=r.feed_seq AND f.change=3
                            AND f.branch_id=r.target_branch
                            AND f.checkpoint_id=r.target_checkpoint)))";

/// Decodes a receipt's stored record sequence.
pub(super) fn receipt_seq(value: i64) -> Result<FeedSeq, SessionStoreError> {
    match nonnegative(value)? {
        0 => Err(SessionStoreError::corrupt()),
        seq => Ok(FeedSeq::new(seq)),
    }
}
