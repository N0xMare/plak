//! Fail-visible recovery of an invalid observational view, never store repair.

use std::collections::BTreeMap;

use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, row_bytes,
};
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptRow};
use plakous_host_protocol::wire::{MAX_LINE_BYTES, MAX_SNAPSHOT_ROW_BYTES};

use crate::runtime::{LiveRowReadError, LiveRowSlice};

// JSON can spend six bytes per body byte. Keep the encoded row within one
// snapshot row (MAX_SNAPSHOT_ROW_BYTES) so a baseline never cuts it; that is
// also well inside a frame line with room for the envelope and other upserts.
pub(super) const MAX_LIVE_ASSISTANT_BYTES: usize = (MAX_SNAPSHOT_ROW_BYTES - 128 * 1024) / 6;
const _: () = assert!(MAX_LIVE_ASSISTANT_BYTES <= MAX_LINE_BYTES / 6 - 128 * 1024);
const ROW_SLICE_BYTES: usize = 64 * 1024;

pub(super) fn live_body(key: RowKey, row: &TranscriptRow) -> Option<&str> {
    if !provisional(key) {
        return None;
    }
    match row {
        TranscriptRow::Assistant { text } => Some(text),
        TranscriptRow::Thinking(thinking) => Some(&thinking.text),
        _ => None,
    }
}

pub(in crate::runtime) const fn provisional(key: RowKey) -> bool {
    key.anchor >= 1_u64 << 63 && matches!(key.lane, Lane::Product)
}

pub(super) fn row_slice(body: &str, offset: u64) -> Result<LiveRowSlice, LiveRowReadError> {
    let start = usize::try_from(offset)
        .ok()
        .filter(|&start| body.is_char_boundary(start))
        .ok_or(LiveRowReadError::InvalidOffset)?;
    let end = body.floor_char_boundary(start.saturating_add(ROW_SLICE_BYTES));
    Ok(LiveRowSlice {
        offset,
        end: end as u64,
        text: body[start..end].to_owned(),
        last: end == body.len(),
    })
}

pub(super) fn snapshot(snapshot: PresentationSnapshot) -> PresentationSnapshot {
    let mut rows: BTreeMap<_, _> = snapshot
        .items
        .into_iter()
        .map(|item| (item.key, crate::sessions::bound_row(item.row)))
        .collect();
    let mut bytes = rows
        .values()
        .map(row_bytes)
        .fold(0_usize, usize::saturating_add);
    let mut truncated = false;
    while rows.len().saturating_add(usize::from(truncated)) > MAX_SNAPSHOT_ROWS
        || bytes.saturating_add(if truncated {
            row_bytes(&TranscriptRow::Truncated)
        } else {
            0
        }) > MAX_PRESENTATION_BYTES
    {
        let Some(key) = rows.iter().find_map(|(key, row)| {
            (!matches!(row, TranscriptRow::SessionHeader { .. })).then_some(*key)
        }) else {
            rows.clear();
            break;
        };
        if let Some(row) = rows.remove(&key) {
            bytes = bytes.saturating_sub(row_bytes(&row));
        }
        truncated = true;
    }
    if truncated {
        rows.insert(
            super::truncation_key(&snapshot.scope),
            TranscriptRow::Truncated,
        );
    }
    PresentationSnapshot {
        scope: snapshot.scope,
        revision: snapshot.revision,
        items: rows
            .into_iter()
            .map(|(key, row)| TranscriptItem { key, row })
            .collect(),
        ..PresentationSnapshot::default()
    }
}
