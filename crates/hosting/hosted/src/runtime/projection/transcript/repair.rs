//! Fail-visible recovery of an invalid observational view, never store repair.

use std::collections::BTreeMap;

use plakous_host_protocol::views::PresentationSnapshot;
use plakous_host_protocol::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, row_bytes,
};
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptRow};

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
        rows.insert(RowKey::new(0, Lane::Runtime, 1), TranscriptRow::Truncated);
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
