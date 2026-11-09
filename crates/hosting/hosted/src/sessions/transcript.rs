//! The bounded in-memory product transcript. Runtime rows are persisted only
//! by the feed projector; this buffer carries no transcript catch-up writes.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use plakous_host_protocol::labelled_chip;
use plakous_host_protocol::views::presentation::row_bytes;
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptItem, TranscriptRow};

use super::projector::{Lane, ProductIntents, RowKey};
use super::{MAX_THINKING_ACTIVITY_BYTES, MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES};

/// Text that replaces the dropped head of a single oversized row.
pub const TRANSCRIPT_TRUNCATION_MARKER: &str = "[older product transcript projection omitted]";

/// Process-local identities are allocated once, not from a window's length.
/// Clones and re-projections carry the key; none is a persisted feed anchor.
pub(crate) fn provisional_key() -> RowKey {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    RowKey::new(
        (1_u64 << 63) + (sequence >> 32),
        Lane::Product,
        u32::try_from(sequence & u64::from(u32::MAX)).unwrap_or(u32::MAX),
    )
}

/// The presented key of the stored truncation sentinel. The presentation-only
/// session header uses `(0, runtime, 0)`.
const MARKER_KEY: RowKey = RowKey::new(0, Lane::Runtime, 1);

/// The transcript chip of one image, from canonical facts alone (history
/// keeps no file name): `[image: png 1280x720 412 KB]`.
pub(crate) fn image_chip(
    media_type: &str,
    byte_len: usize,
    dimensions: Option<(u32, u32)>,
) -> String {
    labelled_chip(
        media_type.strip_prefix("image/").unwrap_or(media_type),
        byte_len,
        dimensions,
        None,
    )
}

/// [`image_chip`] of attached image bytes, as history will show it, plus
/// the dimensions it was attached at when it was downscaled:
/// `[image: png 2048x1324 1.9 MB, downscaled from 3456x2234]`.
pub(crate) fn draft_image_chip(image: &plakous_host_protocol::DraftImage) -> String {
    labelled_chip(
        image
            .media_type
            .strip_prefix("image/")
            .unwrap_or(&image.media_type),
        image.bytes.len(),
        sniffed_dimensions(&image.bytes),
        image.source_dimensions,
    )
}

/// The chip of an attached image under its file name, for surfaces that
/// still hold the attachment: `[image: shot.png 1280x720 412 KB]`.
#[must_use]
pub fn named_image_chip(image: &plakous_host_protocol::DraftImage) -> String {
    labelled_chip(
        &image.name,
        image.bytes.len(),
        sniffed_dimensions(&image.bytes),
        image.source_dimensions,
    )
}

fn sniffed_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    plakous::tools::sniff_image(bytes)
        .ok()
        .and_then(|header| header.dimensions())
}

/// A user or steer row's text: the prompt, then one chip line per image.
pub(crate) fn with_image_chips(text: &str, chips: impl IntoIterator<Item = String>) -> String {
    let mut row = text.to_owned();
    for chip in chips {
        if !row.is_empty() {
            row.push('\n');
        }
        row.push_str(&chip);
    }
    row
}

/// The product transcript: bounded typed rows, their tool-row identities, and
/// separately authored notice intents.
///
/// Reads go through `Deref<Target = [TranscriptRow]>`. Writes go through the
/// methods here, which keep the call identities and bounds exact.
#[derive(Clone, Debug, Default)]
pub struct ProductTranscript {
    lines: Vec<TranscriptRow>,
    /// Encoded size of each row, parallel to `lines`.
    sizes: Vec<usize>,
    /// Whether `lines[0]` is [`TranscriptRow::Truncated`].
    marked: bool,
    /// Sum of `sizes`, marker included.
    bytes: usize,
    /// Absolute sequence of the first line after the marker. Sequences never
    /// move, so a call's row stays addressable while older rows are dropped.
    base: u64,
    /// Tool call id to the absolute sequence of its newest live-updatable row:
    /// a runtime-lane row or a live (provisional) one. A committed product-lane
    /// row, a notice or an imported row, is never updated by call id.
    calls: HashMap<String, u64>,
    /// Ordering key of each row, parallel to `lines` and strictly increasing:
    /// the marker's, a committed row's stored key, or a live row's provisional
    /// key until the next projector publication replaces it.
    keys: Vec<RowKey>,
    /// Operator notices recorded since the last commit: the only rows a store
    /// persists from this projection, as notice intents (spec 02 §2.6).
    notices: Vec<TranscriptRow>,
    notice_keys: Vec<RowKey>,
    notice_ids: HashMap<RowKey, String>,
}

impl PartialEq for ProductTranscript {
    /// Two projections are equal when they show the same rows.
    fn eq(&self, other: &Self) -> bool {
        self.lines == other.lines
    }
}

impl Eq for ProductTranscript {}

impl PartialEq<Vec<TranscriptRow>> for ProductTranscript {
    fn eq(&self, other: &Vec<TranscriptRow>) -> bool {
        &self.lines == other
    }
}

impl From<Vec<TranscriptRow>> for ProductTranscript {
    fn from(rows: Vec<TranscriptRow>) -> Self {
        Self::from_lines(rows)
    }
}

impl FromIterator<TranscriptRow> for ProductTranscript {
    fn from_iter<I: IntoIterator<Item = TranscriptRow>>(rows: I) -> Self {
        Self::from_lines(rows.into_iter().collect())
    }
}

impl std::ops::Deref for ProductTranscript {
    type Target = [TranscriptRow];

    fn deref(&self) -> &[TranscriptRow] {
        &self.lines
    }
}

impl ProductTranscript {
    /// A projection holding `rows` (an import), each its own line in
    /// document order. Rows a store or a document committed are identified
    /// by their position, never by call id, so two rows naming one call stay
    /// two rows. A truncation marker survives only as the first row, a
    /// session header is never stored, tool and thinking rows are bounded,
    /// and rows past the bound are dropped the same way appends drop them.
    #[must_use]
    pub fn from_lines(rows: Vec<TranscriptRow>) -> Self {
        let mut transcript = Self::default();
        let mut rows = rows.into_iter().peekable();
        if rows.next_if_eq(&TranscriptRow::Truncated).is_some() {
            transcript.push_marker();
        }
        for row in rows.filter(is_line) {
            transcript.push_row(bound_row(row), provisional_key());
        }
        let offset = usize::from(transcript.marked);
        for index in offset..transcript.lines.len() {
            let sequence = transcript.base + (index - offset) as u64;
            transcript.keys[index] = RowKey::new(
                0,
                Lane::Product,
                u32::try_from(sequence).unwrap_or(u32::MAX),
            );
        }
        // Imported rows are committed product-lane rows: no call id of theirs
        // is live.
        transcript.calls.clear();
        transcript
    }

    /// The projection a store committed, as `(row, call id)` pairs in order,
    /// with nothing pending. A row without a stored call id takes the one its
    /// tool item names.
    #[must_use]
    pub fn from_rows(rows: impl IntoIterator<Item = (TranscriptRow, Option<String>)>) -> Self {
        let mut transcript = Self::default();
        for (row, call_id) in rows {
            let key = if transcript.lines.is_empty() && row == TranscriptRow::Truncated {
                transcript.marked = true;
                MARKER_KEY
            } else {
                let sequence = transcript.next_sequence();
                let call_id = call_id.or_else(|| row.call_id().map(str::to_owned));
                if let Some(call_id) = call_id {
                    transcript.calls.insert(call_id, sequence);
                }
                provisional_key()
            };
            let size = row_bytes(&row);
            transcript.bytes = transcript.bytes.saturating_add(size);
            transcript.sizes.push(size);
            transcript.keys.push(key);
            transcript.lines.push(row);
        }
        transcript
    }

    /// Every row with the call id it is stored under, for a full rewrite.
    #[must_use]
    pub fn rows(&self) -> Vec<(&TranscriptRow, Option<&str>)> {
        let mut by_sequence = HashMap::with_capacity(self.calls.len());
        for (call_id, sequence) in &self.calls {
            by_sequence.insert(*sequence, call_id.as_str());
        }
        let offset = usize::from(self.marked);
        self.lines
            .iter()
            .enumerate()
            .map(|(index, line)| {
                let call_id = index
                    .checked_sub(offset)
                    .and_then(|body| u64::try_from(body).ok())
                    .and_then(|body| by_sequence.get(&self.base.saturating_add(body)).copied());
                (line, call_id)
            })
            .collect()
    }

    /// Retained rows with their ordering keys. The presentation-only header
    /// uses `(0, runtime, 0)`, so the stored truncation sentinel is presented
    /// at `(0, runtime, 1)` without changing its persisted encoding.
    #[must_use]
    pub fn items(&self) -> Vec<TranscriptItem> {
        self.lines
            .iter()
            .zip(&self.keys)
            .map(|(row, key)| TranscriptItem {
                key: *key,
                row: row.clone(),
            })
            .collect()
    }

    /// Every row but the pending notices, in order: the transcript a carried
    /// session's first commit imports, before it writes those notices as
    /// notices under their own identities.
    #[must_use]
    pub fn imported_rows(&self) -> Vec<&TranscriptRow> {
        let pending: HashSet<&RowKey> = self.notice_keys.iter().collect();
        self.lines
            .iter()
            .zip(&self.keys)
            .filter(|(_, key)| !pending.contains(key))
            .map(|(row, _)| row)
            .collect()
    }

    /// Forget notice intents once a store has committed them.
    pub fn mark_committed(&mut self) {
        self.notices.clear();
        self.notice_keys.clear();
        self.notice_ids.clear();
    }

    /// The operator notices recorded since the last commit, oldest first.
    #[must_use]
    pub fn pending_notices(&self) -> &[TranscriptRow] {
        &self.notices
    }

    /// Shows `row` and records it as an operator notice the next commit
    /// writes as an intent at the cursor the store has applied.
    pub fn record_notice(&mut self, row: TranscriptRow) {
        self.record_notice_item(
            TranscriptItem {
                key: provisional_key(),
                row,
            },
            None,
        );
    }

    pub(crate) fn record_named_notice(&mut self, id: String, row: TranscriptRow) {
        self.record_notice_item(
            TranscriptItem {
                key: provisional_key(),
                row,
            },
            Some(id),
        );
    }

    /// Pending operator intents, with a stable identity when their producer
    /// derives one from durable state. Stores must deduplicate those identities.
    pub fn pending_notice_entries(&self) -> impl Iterator<Item = (Option<&str>, &TranscriptRow)> {
        self.notice_keys
            .iter()
            .zip(&self.notices)
            .map(|(key, row)| (self.notice_ids.get(key).map(String::as_str), row))
    }

    pub(crate) fn pending_notice_items(&self) -> Vec<(TranscriptItem, Option<String>)> {
        self.notice_keys
            .iter()
            .zip(&self.notices)
            .map(|(key, row)| {
                (
                    TranscriptItem {
                        key: *key,
                        row: row.clone(),
                    },
                    self.notice_ids.get(key).cloned(),
                )
            })
            .collect()
    }

    /// Shows `item` as its own row, never as an update of an earlier row with
    /// the same call id, and records it as pending under its key. Callers
    /// show a pending notice after the rows it follows; a release build that
    /// meets an earlier key gives the notice a fresh provisional key, which
    /// sorts after every row shown so far.
    pub(crate) fn record_notice_item(&mut self, item: TranscriptItem, id: Option<String>) {
        let TranscriptItem { key, row } = item;
        let early = self.keys.last().is_some_and(|last| *last >= key);
        debug_assert!(!early, "a pending notice sorts before the rows shown");
        let key = if early { provisional_key() } else { key };
        if let Some(id) = id {
            self.notice_ids.insert(key, id);
        }
        self.notices.push(row.clone());
        self.notice_keys.push(key);
        if is_line(&row) {
            self.push_row(bound_row(row), key);
        }
    }

    /// Shows `pending` (this projection's unsaved notices) on a projection
    /// just rebuilt from the store, after its committed rows in key order. A
    /// named notice `intents` already holds is shown once, at its committed
    /// key, and is no longer pending.
    pub(crate) fn restore_pending_notices(
        &mut self,
        mut pending: Vec<(TranscriptItem, Option<String>)>,
        intents: &ProductIntents,
    ) {
        let committed: HashSet<&str> = intents
            .notices
            .iter()
            .map(|notice| notice.notice_id.as_str())
            .collect();
        pending.retain(|(_, id)| id.as_deref().is_none_or(|id| !committed.contains(id)));
        pending.sort_by_key(|(item, _)| item.key);
        for (item, id) in pending {
            self.record_notice_item(item, id);
        }
    }

    /// The projection of a stored transcript, as
    /// [`SessionViews::transcript`](super::projector::SessionViews::transcript)
    /// merges it, with nothing pending. Every row is its own line under its
    /// stored key, never an in-place update of an earlier row: call ids are
    /// not unique across lanes, since a carried session's imported rows and
    /// its own tree notices can name the same `child-result:` call. Every
    /// row, the projector's included, is bounded again through `bound_row`:
    /// it is the identity on its own output, so a row this build's bound
    /// wrote is shown as stored, and a row an older bound wrote (whatever
    /// build wrote it, whenever) is shown as this build bounds it.
    #[must_use]
    pub fn stored(rows: Vec<(RowKey, TranscriptRow)>) -> Self {
        let mut transcript = Self::default();
        let mut rows = rows.into_iter().peekable();
        if rows
            .next_if(|(_, row)| *row == TranscriptRow::Truncated)
            .is_some()
        {
            transcript.push_marker();
        }
        for (key, row) in rows {
            if !is_line(&row) {
                continue;
            }
            transcript.push_row(bound_row(row), key);
        }
        debug_assert!(
            transcript
                .items()
                .windows(2)
                .all(|items| items[0].key < items[1].key)
        );
        transcript
    }

    /// Append one row, bounding the projection. A tool row goes through
    /// [`Self::upsert_tool`] so its call id stays addressable. The truncation
    /// marker is the projection's own and the session header is derived from
    /// the record, so neither is ever appended; blank thinking is not a row.
    pub fn append(&mut self, row: TranscriptRow) {
        if !is_line(&row) {
            return;
        }
        match row {
            TranscriptRow::Tool(tool) => {
                let call_id = tool.call_id.clone();
                self.upsert_row(bound_row(TranscriptRow::Tool(tool)), call_id);
            }
            row => self.push_row(bound_row(row), provisional_key()),
        }
    }

    /// Write a tool activity row: replace the row of its call id in place, or
    /// append a new one. Returns whether the row is new.
    pub fn upsert_tool(&mut self, activity: &ToolItem) -> bool {
        self.upsert_row(
            bound_row(TranscriptRow::Tool(activity.clone())),
            activity.call_id.clone(),
        )
    }

    /// [`Self::upsert_tool`] of a bounded row under `call_id`.
    fn upsert_row(&mut self, row: TranscriptRow, call_id: String) -> bool {
        if let Some(index) = self.call_index(&call_id) {
            let size = row_bytes(&row);
            self.bytes = self
                .bytes
                .saturating_sub(self.sizes[index])
                .saturating_add(size);
            self.sizes[index] = size;
            self.lines[index] = row;
            self.bound();
            return false;
        }
        self.push_row(row, provisional_key());
        true
    }

    /// The tool row `call_id` last wrote.
    #[must_use]
    pub fn tool_activity(&self, call_id: &str) -> Option<ToolItem> {
        self.lines[self.call_index(call_id)?].as_tool().cloned()
    }

    /// Drop every line.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Push one bounded row under `key`; a live-updatable tool row becomes
    /// its call id's.
    fn push_row(&mut self, row: TranscriptRow, key: RowKey) {
        debug_assert!(self.keys.last().is_none_or(|last| *last < key));
        let updatable = key.lane == Lane::Runtime || key.anchor >= 1_u64 << 63;
        if let Some(call_id) = row.call_id().filter(|_| updatable) {
            let sequence = self.next_sequence();
            self.calls.insert(call_id.to_owned(), sequence);
        }
        let size = row_bytes(&row);
        self.bytes = self.bytes.saturating_add(size);
        self.sizes.push(size);
        self.keys.push(key);
        self.lines.push(row);
        self.bound();
    }

    /// The marker as the first row of an empty projection.
    fn push_marker(&mut self) {
        self.marked = true;
        self.lines.push(TranscriptRow::Truncated);
        self.sizes.push(marker_bytes());
        self.keys.push(MARKER_KEY);
        self.bytes = marker_bytes();
    }

    fn next_sequence(&self) -> u64 {
        let body = self.lines.len().saturating_sub(usize::from(self.marked));
        self.base
            .saturating_add(u64::try_from(body).unwrap_or(u64::MAX))
    }

    fn call_index(&self, call_id: &str) -> Option<usize> {
        let sequence = *self.calls.get(call_id)?;
        let body = usize::try_from(sequence.checked_sub(self.base)?).ok()?;
        let index = body.checked_add(usize::from(self.marked))?;
        (index < self.lines.len()).then_some(index)
    }

    /// Past either bound, drop the oldest rows down to seven eighths of both,
    /// so a transcript at its bound drops rows in batches rather than on every
    /// append. The newest row always stays.
    fn bound(&mut self) {
        if self.lines.len() <= MAX_TRANSCRIPT_LINES && self.bytes <= MAX_TRANSCRIPT_BYTES {
            return;
        }
        let target_lines = MAX_TRANSCRIPT_LINES - MAX_TRANSCRIPT_LINES / 8;
        let target_bytes = MAX_TRANSCRIPT_BYTES - MAX_TRANSCRIPT_BYTES / 8;
        let offset = usize::from(self.marked);
        let (mut lines, mut bytes) = if self.marked {
            (self.lines.len(), self.bytes)
        } else {
            (
                self.lines.len().saturating_add(1),
                self.bytes.saturating_add(marker_bytes()),
            )
        };
        let droppable = self.lines.len().saturating_sub(offset).saturating_sub(1);
        let mut rows = 0;
        while rows < droppable && (lines > target_lines || bytes > target_bytes) {
            bytes = bytes.saturating_sub(self.sizes[offset + rows]);
            lines -= 1;
            rows += 1;
        }
        if rows == 0 {
            return;
        }
        self.lines.drain(offset..offset + rows);
        self.sizes.drain(offset..offset + rows);
        self.keys.drain(offset..offset + rows);
        if !self.marked {
            self.lines.insert(0, TranscriptRow::Truncated);
            self.sizes.insert(0, marker_bytes());
            self.keys.insert(0, MARKER_KEY);
            self.marked = true;
        }
        self.bytes = bytes;
        self.base = self
            .base
            .saturating_add(u64::try_from(rows).unwrap_or(u64::MAX));
        let base = self.base;
        self.calls.retain(|_, sequence| *sequence >= base);
    }
}

/// Whether `row` is a transcript line: the truncation marker is the
/// projection's own, the session header is derived from the record, and blank
/// thinking is not a row.
fn is_line(row: &TranscriptRow) -> bool {
    match row {
        TranscriptRow::Truncated | TranscriptRow::SessionHeader { .. } => false,
        TranscriptRow::Thinking(thinking) => {
            !thinking.location_key.trim().is_empty() && !thinking.text.trim().is_empty()
        }
        _ => true,
    }
}

fn marker_bytes() -> usize {
    row_bytes(&TranscriptRow::Truncated)
}

/// Every stored row is bounded here, live, projected and imported alike: a
/// tool or process row's subject (redacted, then 240 bytes on one line, the
/// same function the live path uses; a process row's command redacted with
/// the runtime's escapes undone), a tool row's detail (redacted, then
/// [`MAX_TOOL_ACTIVITY_DETAIL_BYTES`](super::MAX_TOOL_ACTIVITY_DETAIL_BYTES)),
/// a thinking row's text ([`MAX_THINKING_ACTIVITY_BYTES`]). A single row still
/// larger than the whole byte bound keeps the tail of its text, behind
/// [`TRANSCRIPT_TRUNCATION_MARKER`], sized so the marker row and the row still
/// fit together. Only text-bearing rows can grow that large.
///
/// A bounded row is a fixed point: a cut never leaves text a second redaction
/// would change. [`ProductTranscript::stored`] bounds every stored row again
/// as it reads it, so a change to the display redactor or to these bounds
/// takes effect on the next read of rows stored before it, with no schema
/// bump and no rebuild.
pub(crate) fn bound_row(mut row: TranscriptRow) -> TranscriptRow {
    match &mut row {
        TranscriptRow::Tool(tool) => {
            tool.subject = tool
                .subject
                .take()
                .map(|value| super::live_tool_subject(&value));
            tool.detail = tool
                .detail
                .take()
                .map(|value| super::bound_tool_detail(&value));
        }
        // A process row names the runtime's displayed command, escaped:
        // redacted with the escapes undone, then bounded as a subject.
        TranscriptRow::Process(process) => {
            process.subject = process.subject.take().map(|value| {
                super::live_tool_subject(&crate::jobs::display::redacted_job_command(&value))
            });
        }
        TranscriptRow::Thinking(thinking) => {
            thinking.text = super::bound_activity(
                std::mem::take(&mut thinking.text),
                MAX_THINKING_ACTIVITY_BYTES,
            );
        }
        _ => {}
    }
    let limit = MAX_TRANSCRIPT_BYTES.saturating_sub(marker_bytes());
    let size = row_bytes(&row);
    if size <= limit {
        return row;
    }
    let text = match &mut row {
        TranscriptRow::User { text, .. }
        | TranscriptRow::Steer { text, .. }
        | TranscriptRow::Assistant { text } => text,
        TranscriptRow::Thinking(thinking) => &mut thinking.text,
        TranscriptRow::Notice(notice) => &mut notice.text,
        _ => return row,
    };
    let overhead = size.saturating_sub(text.len());
    let payload = limit
        .saturating_sub(overhead)
        .saturating_sub(TRANSCRIPT_TRUNCATION_MARKER.len() + 1);
    let mut start = text.len().saturating_sub(payload);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    *text = format!("{TRANSCRIPT_TRUNCATION_MARKER}\n{}", &text[start..]);
    row
}
