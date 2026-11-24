use std::collections::BTreeMap;

use plakd_wire_types::v13::{
    AnswerV13, Baseline, HostFrameV13, LiveRowLength, PAGE_TARGET_BYTES, PageItem, RetainedRevision,
};
use plakous::sessions::feed::FeedSeq;
use plakous_host_protocol::views::transcript::{
    RowKey, TranscriptItem, TranscriptRow, TranscriptScope,
};
use plakous_host_protocol::wire::{AttachmentId, FramedTranscriptRow, MAX_LINE_BYTES, RequestId};

use crate::store::transcript_history::{HistoryError, body_mut};

use super::{Page, answer};

pub(crate) struct BaselineTarget {
    pub(crate) attachment: AttachmentId,
    pub(crate) scope: TranscriptScope,
    pub(crate) revision: RetainedRevision,
}

/// The fenced resident contribution to the durable page (item A). All live
/// row lengths must match the prefixes already emitted on that revision.
#[derive(Default)]
pub(crate) struct LiveTail {
    /// Product cursor the resident snapshot was taken against. A changed
    /// cursor forces a recapture rather than mixing two revisions.
    pub(crate) applied: Option<FeedSeq>,
    pub(crate) upserts: Vec<TranscriptItem>,
    pub(crate) lengths: BTreeMap<RowKey, u64>,
}

pub(super) fn baseline(
    mut page: Page,
    target: BaselineTarget,
    live: LiveTail,
) -> Result<Vec<HostFrameV13>, HistoryError> {
    let mut by_key: BTreeMap<_, _> = std::mem::take(&mut page.items)
        .into_iter()
        .map(|item| (item.key, item.row))
        .collect();
    by_key.extend(
        live.upserts
            .into_iter()
            .filter(|item| item.key.anchor >= page.from)
            .map(|item| (item.key, item.row)),
    );
    let items = by_key
        .into_iter()
        .map(|(key, row)| TranscriptItem { key, row });
    frames(
        items,
        live.lengths,
        MAX_LINE_BYTES,
        |part, last, items, live_rows| {
            HostFrameV13::Snapshot(Baseline {
                attachment: target.attachment,
                scope: target.scope.clone(),
                revision: target.revision.clone(),
                part,
                last,
                items,
                has_older: page.cursor.is_some(),
                older_rows: page.older.rows,
                older_bytes: page.older.bytes,
                older_cursor: page.cursor.clone(),
                live_rows,
            })
        },
    )
}

pub(super) fn history(
    page: Page,
    attachment: AttachmentId,
    id: RequestId,
) -> Result<Vec<HostFrameV13>, HistoryError> {
    frames(
        page.items,
        BTreeMap::new(),
        MAX_LINE_BYTES,
        |part, last, items, _| HostFrameV13::HistoryPage {
            attachment,
            id,
            part,
            last,
            items,
            has_older: page.cursor.is_some(),
            older_rows: page.older.rows,
            older_bytes: page.older.bytes,
            older_cursor: page.cursor.clone(),
        },
    )
}

fn frames(
    rows: impl IntoIterator<Item = TranscriptItem>,
    mut lengths: BTreeMap<RowKey, u64>,
    cap: usize,
    frame: impl Fn(u32, bool, Vec<PageItem>, Vec<LiveRowLength>) -> HostFrameV13,
) -> Result<Vec<HostFrameV13>, HistoryError> {
    // The largest part number, `false`, and the newline bound all envelopes.
    let overhead = encoded_len(&frame(u32::MAX, false, Vec::new(), Vec::new()))? + 1;
    let room = cap
        .checked_sub(overhead)
        .ok_or(HistoryError::FrameTooLarge)?;
    let mut result = Vec::new();
    let (mut items, mut live_rows) = (Vec::new(), Vec::new());
    let mut size = overhead;
    for row in rows {
        let length = lengths
            .remove(&row.key)
            .map(|len| LiveRowLength { row: row.key, len });
        let length_size = length.as_ref().map(encoded_len).transpose()?.unwrap_or(0);
        let item_room = room
            .checked_sub(length_size)
            .ok_or(HistoryError::FrameTooLarge)?;
        let (mut item, bytes) = fit_item(row, item_room)?;
        if let Some(length) = length
            && body_mut(&mut item.row.0).map_or(0, |body| body.len() as u64) != length.len
        {
            return Err(HistoryError::Unavailable);
        }
        let cost = bytes + length_size;
        let commas =
            usize::from(!items.is_empty()) + usize::from(length.is_some() && !live_rows.is_empty());
        if !items.is_empty() && (size + cost + commas > cap.min(PAGE_TARGET_BYTES)) {
            result.push(frame(
                part_number(&result)?,
                false,
                std::mem::take(&mut items),
                std::mem::take(&mut live_rows),
            ));
            size = overhead + cost;
        } else {
            size += cost + commas;
        }
        items.push(item);
        live_rows.extend(length);
    }
    if !lengths.is_empty() {
        return Err(HistoryError::Unavailable);
    }
    result.push(frame(part_number(&result)?, true, items, live_rows));
    Ok(result)
}

fn part_number(parts: &[HostFrameV13]) -> Result<u32, HistoryError> {
    u32::try_from(parts.len()).map_err(|_| HistoryError::FrameTooLarge)
}

fn fit_item(row: TranscriptItem, room: usize) -> Result<(PageItem, usize), HistoryError> {
    let mut item = PageItem {
        key: row.key,
        row: FramedTranscriptRow(row.row),
        truncated_bytes: None,
    };
    let bytes = encoded_len(&item)?;
    if bytes <= room {
        return Ok((item, bytes));
    }
    if let TranscriptRow::Tool(tool) = &mut item.row.0 {
        tool.wire_operation = Some(tool.operation());
    }
    let body = body_mut(&mut item.row.0)
        .map(std::mem::take)
        .ok_or(HistoryError::FrameTooLarge)?;
    item.truncated_bytes = Some(body.len() as u64);
    let room = room
        .checked_sub(encoded_len(&item)?)
        .ok_or(HistoryError::FrameTooLarge)?;
    let end = fitting_prefix(&body, room);
    item.truncated_bytes = Some((body.len() - end) as u64);
    *body_mut(&mut item.row.0).ok_or(HistoryError::FrameTooLarge)? = body[..end].to_owned();
    let bytes = encoded_len(&item)?;
    Ok((item, bytes))
}

pub(super) fn row_slice(
    id: RequestId,
    row: RowKey,
    text: &str,
    offset: u64,
) -> Result<HostFrameV13, HistoryError> {
    let start = usize::try_from(offset).map_err(|_| HistoryError::InvalidOffset)?;
    let remainder = text.get(start..).ok_or(HistoryError::InvalidOffset)?;
    let slice = |end, text, last| {
        answer(
            id,
            AnswerV13::TranscriptRow {
                row,
                offset,
                end,
                text,
                last,
            },
        )
    };
    let overhead = encoded_len(&slice(u64::MAX, String::new(), false))? + 1;
    let len = fitting_prefix(remainder, PAGE_TARGET_BYTES.saturating_sub(overhead));
    Ok(slice(
        (start + len) as u64,
        remainder[..len].to_owned(),
        len == remainder.len(),
    ))
}

fn encoded_len(value: &impl serde::Serialize) -> Result<usize, HistoryError> {
    serde_json::to_vec(value)
        .map(|bytes| bytes.len())
        .map_err(|_| HistoryError::Unavailable)
}

// serde_json's compact string encoding does not escape non-ASCII scalars.
// Count exactly the additional bytes inside the already-measured quotes.
fn fitting_prefix(text: &str, mut room: usize) -> usize {
    for (index, ch) in text.char_indices() {
        let cost = match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{0008}' | '\u{000c}' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => ch.len_utf8(),
        };
        let Some(left) = room.checked_sub(cost) else {
            return index;
        };
        room = left;
    }
    text.len()
}
