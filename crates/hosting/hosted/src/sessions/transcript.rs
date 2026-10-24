//! The bounded in-memory product transcript. Runtime rows are persisted only
//! by the feed projector; this buffer carries no transcript catch-up writes.

use std::collections::HashMap;

use plakous_host_protocol::views::presentation::row_bytes;
use plakous_host_protocol::views::transcript::{ToolItem, TranscriptRow};

use super::projector::{Lane, RowKey};
use super::{MAX_THINKING_ACTIVITY_BYTES, MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES};

/// Text that replaces the dropped head of a single oversized row.
pub const TRANSCRIPT_TRUNCATION_MARKER: &str = "[older product transcript projection omitted]";

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

fn labelled_chip(
    label: &str,
    byte_len: usize,
    dimensions: Option<(u32, u32)>,
    source: Option<(u32, u32)>,
) -> String {
    let size = if byte_len < 1024 * 1024 {
        format!("{} KB", byte_len.div_ceil(1024))
    } else {
        let tenths = (byte_len * 10).div_ceil(1024 * 1024);
        format!("{}.{} MB", tenths / 10, tenths % 10)
    };
    let source = source.map_or_else(String::new, |(width, height)| {
        format!(", downscaled from {width}x{height}")
    });
    match dimensions {
        Some((width, height)) => format!("[image: {label} {width}x{height} {size}{source}]"),
        None => format!("[image: {label} {size}{source}]"),
    }
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
    /// Tool call id to the absolute sequence of its row.
    calls: HashMap<String, u64>,
    /// Operator notices recorded since the last commit: the only rows a store
    /// persists from this projection, as notice intents (spec 02 §2.6).
    notices: Vec<TranscriptRow>,
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
    /// A projection holding `rows` (an import).
    /// Every row goes through [`Self::append`], the path live rows take: a
    /// truncation marker survives only as the first row, a session header is
    /// never stored, tool and thinking rows are bounded, and rows past the
    /// bound are dropped the same way appends drop them.
    #[must_use]
    pub fn from_lines(rows: Vec<TranscriptRow>) -> Self {
        let mut transcript = Self::default();
        let mut rows = rows.into_iter().peekable();
        if rows.next_if_eq(&TranscriptRow::Truncated).is_some() {
            transcript.marked = true;
            transcript.lines.push(TranscriptRow::Truncated);
            transcript.sizes.push(marker_bytes());
            transcript.bytes = marker_bytes();
        }
        for row in rows {
            transcript.append(row);
        }
        transcript
    }

    /// The projection a store committed, as `(row, call id)` pairs in order,
    /// with nothing pending. A row without a stored call id takes the one its
    /// tool item names.
    #[must_use]
    pub fn from_rows(rows: impl IntoIterator<Item = (TranscriptRow, Option<String>)>) -> Self {
        let mut transcript = Self::default();
        for (row, call_id) in rows {
            if transcript.lines.is_empty() && row == TranscriptRow::Truncated {
                transcript.marked = true;
            } else {
                let sequence = transcript.next_sequence();
                let call_id = call_id.or_else(|| row.call_id().map(str::to_owned));
                if let Some(call_id) = call_id {
                    transcript.calls.entry(call_id).or_insert(sequence);
                }
            }
            let size = row_bytes(&row);
            transcript.bytes = transcript.bytes.saturating_add(size);
            transcript.sizes.push(size);
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

    /// Forget notice intents once a store has committed them.
    pub fn mark_committed(&mut self) {
        self.notices.clear();
    }

    /// The operator notices recorded since the last commit, oldest first.
    #[must_use]
    pub fn pending_notices(&self) -> &[TranscriptRow] {
        &self.notices
    }

    /// Shows `row` and records it as an operator notice the next commit
    /// writes as an intent at the cursor the store has applied.
    pub fn record_notice(&mut self, row: TranscriptRow) {
        self.notices.push(row.clone());
        self.append(row);
    }

    /// The projection of a stored transcript, as
    /// [`SessionViews::transcript`](super::projector::SessionViews::transcript)
    /// merges it, with nothing pending. It equals [`Self::from_lines`] of the
    /// same rows without bounding each projector row again: the projector
    /// stored each one through [`bound_row`], which is the identity on its
    /// output. Notices and imported rows (the product lane) are bounded as an
    /// append bounds them.
    #[must_use]
    pub fn stored(rows: Vec<(RowKey, TranscriptRow)>) -> Self {
        let mut transcript = Self::default();
        let mut rows = rows.into_iter().peekable();
        if rows
            .next_if(|(_, row)| *row == TranscriptRow::Truncated)
            .is_some()
        {
            transcript.marked = true;
            transcript.lines.push(TranscriptRow::Truncated);
            transcript.sizes.push(marker_bytes());
            transcript.bytes = marker_bytes();
        }
        for (key, row) in rows {
            match key.lane {
                Lane::Runtime => transcript.insert(row, std::convert::identity),
                Lane::Product => transcript.insert(row, bound_row),
            }
        }
        transcript
    }

    /// Append one row, bounding the projection. A tool row goes through
    /// [`Self::upsert_tool`] so its call id stays addressable. The truncation
    /// marker is the projection's own and the session header is derived from
    /// the record, so neither is ever appended; blank thinking is not a row.
    pub fn append(&mut self, row: TranscriptRow) {
        self.insert(row, bound_row);
    }

    /// [`Self::append`] with `bound` applied to each row it keeps.
    fn insert(&mut self, row: TranscriptRow, bound: fn(TranscriptRow) -> TranscriptRow) {
        match row {
            TranscriptRow::Tool(tool) => {
                let call_id = tool.call_id.clone();
                self.upsert_row(bound(TranscriptRow::Tool(tool)), call_id);
            }
            TranscriptRow::Truncated | TranscriptRow::SessionHeader { .. } => {}
            TranscriptRow::Thinking(thinking)
                if thinking.location_key.trim().is_empty() || thinking.text.trim().is_empty() => {}
            row => self.push_row(bound(row), None),
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
        self.push_row(row, Some(call_id));
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

    /// Push one bounded row.
    fn push_row(&mut self, row: TranscriptRow, call_id: Option<String>) {
        if let Some(call_id) = call_id {
            let sequence = self.next_sequence();
            self.calls.insert(call_id, sequence);
        }
        let size = row_bytes(&row);
        self.bytes = self.bytes.saturating_add(size);
        self.sizes.push(size);
        self.lines.push(row);
        self.bound();
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
        if !self.marked {
            self.lines.insert(0, TranscriptRow::Truncated);
            self.sizes.insert(0, marker_bytes());
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

fn marker_bytes() -> usize {
    row_bytes(&TranscriptRow::Truncated)
}

/// Every stored row is bounded here, live, projected and imported alike: a
/// tool or process row's subject (redacted, then 240 bytes on one line, the
/// same function the live path uses), a tool row's detail (redacted, then
/// [`MAX_TOOL_ACTIVITY_DETAIL_BYTES`](super::MAX_TOOL_ACTIVITY_DETAIL_BYTES)),
/// a thinking row's text ([`MAX_THINKING_ACTIVITY_BYTES`]). A single row still
/// larger than the whole byte bound keeps the tail of its text, behind
/// [`TRANSCRIPT_TRUNCATION_MARKER`], sized so the marker row and the row still
/// fit together. Only text-bearing rows can grow that large.
///
/// A bounded row is a fixed point: a cut never leaves text a second redaction
/// would change. [`ProductTranscript::stored`] relies on it and shows projector
/// rows as persisted, so a change to the display redactor or to these bounds
/// must bump the product schema or drop the views to force a rebuild.
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
        TranscriptRow::Process(process) => {
            process.subject = process
                .subject
                .take()
                .map(|value| super::live_tool_subject(&value));
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
