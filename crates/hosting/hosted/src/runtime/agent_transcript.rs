//! One agent's committed history as the typed rows the root transcript shows
//! (spec 03 §16). Read from the agent's own bound session, or from a live
//! agent's atomic attach, newest first and bounded; tool calls and results are joined by call id into one tool row
//! whose subject, detail and state come from the same functions the root's
//! live path calls.

use super::transcript_rows::canonical_items as items_from_reads;
use crate::sessions::projector::transcript_tail;
#[cfg(test)]
use crate::sessions::projector::{TranscriptPart, TranscriptRead};
use crate::sessions::{TranscriptRow, bound_row};
use plakous::sessions::{AgentSessions, SessionLocator, SessionTranscriptSource};
use plakous_host_protocol::views::ProcessView;
use plakous_host_protocol::views::presentation::{
    MAX_PRESENTATION_BYTES, MAX_SNAPSHOT_ROWS, row_bytes,
};
#[cfg(test)]
use plakous_host_protocol::views::transcript::Lane;
use plakous_host_protocol::views::transcript::{RowKey, TranscriptItem};

#[cfg(test)]
pub(super) fn rows_from_items(
    items: &[TranscriptRead<'_>],
    locator: SessionLocator,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptRow> {
    items_from_reads(items, locator, processes, task_tools)
        .into_iter()
        .map(|item| item.row)
        .collect()
}

/// Bounds every row like a stored row, then keeps the newest rows that fit
/// `max_rows` and `max_bytes`, behind one `Truncated` row when anything older
/// was dropped here or by the read. The marker takes a row and its bytes only
/// when something is dropped: rows that exactly fit are all kept.
fn bound_items(
    rows: Vec<TranscriptItem>,
    mut truncated: bool,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<TranscriptItem> {
    if max_rows == 0 {
        return Vec::new();
    }
    let mut kept: Vec<(TranscriptItem, usize)> = Vec::new();
    let mut bytes = 0_usize;
    for mut row in rows.into_iter().rev() {
        row.row = bound_row(row.row);
        let size = row_bytes(&row.row);
        if kept.len() >= max_rows || bytes.saturating_add(size) > max_bytes {
            truncated = true;
            break;
        }
        bytes = bytes.saturating_add(size);
        kept.push((row, size));
    }
    let mut kept = if truncated {
        let marker = row_bytes(&TranscriptRow::Truncated);
        while kept.len() >= max_rows || bytes.saturating_add(marker) > max_bytes {
            let Some((_, size)) = kept.pop() else {
                break;
            };
            bytes = bytes.saturating_sub(size);
        }
        let mut kept: Vec<TranscriptItem> = kept.into_iter().map(|(row, _)| row).collect();
        kept.push(TranscriptItem {
            key: RowKey::TRUNCATION,
            row: TranscriptRow::Truncated,
        });
        kept
    } else {
        kept.into_iter().map(|(row, _)| row).collect()
    };
    kept.reverse();
    // A composition parent outside the window cannot own a visible row.
    let calls: std::collections::BTreeSet<String> = kept
        .iter()
        .filter_map(|item| item.row.call_id().map(str::to_owned))
        .collect();
    for item in &mut kept {
        if let TranscriptRow::Tool(tool) = &mut item.row
            && tool
                .parent_call_id
                .as_ref()
                .is_some_and(|parent| !calls.contains(parent))
        {
            tool.parent_call_id = None;
            tool.cell_id = None;
        }
    }
    kept
}

/// The most committed items one read projects. Each item becomes at most a
/// few rows, so 512 items cover the last few hundred calls and messages of an
/// agent, which is what a reader scrolls through in a view; anything older is
/// behind the `Truncated` row and still readable by call detail. Each
/// argument or result string is redacted whole up to 64 KiB and past that over
/// the agent library's history window (`HISTORY_WINDOW_BYTES`, 20 KiB), so one
/// read's work stays in the tens of MiB however long the history is.
pub(super) const MAX_AGENT_TRANSCRIPT_ITEMS: usize = 512;

/// Projects `source`'s newest items to bounded rows. Synchronous CPU work:
/// callers run it off the async workers.
pub(super) fn items_from_source(
    source: &SessionTranscriptSource,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptItem> {
    rows_within(
        source,
        processes,
        task_tools,
        MAX_AGENT_TRANSCRIPT_ITEMS,
        MAX_SNAPSHOT_ROWS,
        MAX_PRESENTATION_BYTES,
    )
}

fn rows_within(
    source: &SessionTranscriptSource,
    processes: &[ProcessView],
    task_tools: bool,
    max_items: usize,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<TranscriptItem> {
    let tail = source.tail(max_items, max_bytes);
    let rows = items_from_reads(
        &transcript_tail(&tail),
        tail.locator(),
        processes,
        task_tools,
    );
    bound_items(rows, tail.truncated(), max_rows, max_bytes)
}

#[cfg(test)]
pub(super) fn rows_from_source(
    source: &SessionTranscriptSource,
    processes: &[ProcessView],
    task_tools: bool,
) -> Vec<TranscriptRow> {
    items_from_source(source, processes, task_tools)
        .into_iter()
        .map(|item| item.row)
        .collect()
}

#[cfg(test)]
fn bound_rows(
    rows: Vec<TranscriptRow>,
    truncated: bool,
    max_rows: usize,
    max_bytes: usize,
) -> Vec<TranscriptRow> {
    let items = rows
        .into_iter()
        .enumerate()
        .map(|(index, row)| TranscriptItem {
            key: RowKey::new(index as u64 + 1, Lane::Runtime, 0),
            row,
        })
        .collect();
    bound_items(items, truncated, max_rows, max_bytes)
        .into_iter()
        .map(|item| item.row)
        .collect()
}

/// Reads `locator`'s committed history at the current head of its session
/// and branch, never another's.
pub(super) async fn read_source(
    sessions: &AgentSessions,
    locator: SessionLocator,
) -> Result<SessionTranscriptSource, String> {
    sessions
        .transcript_source(locator)
        .await
        .map_err(|error| format!("transcript unavailable: {error}"))
}

#[cfg(test)]
pub(super) async fn read_rows(
    sessions: &AgentSessions,
    locator: SessionLocator,
    processes: &[ProcessView],
) -> Result<Vec<TranscriptRow>, String> {
    Ok(rows_from_source(
        &read_source(sessions, locator).await?,
        processes,
        false,
    ))
}

#[cfg(test)]
pub(super) async fn read_rows_within(
    sessions: &AgentSessions,
    locator: SessionLocator,
    processes: &[ProcessView],
    max_rows: usize,
    max_bytes: usize,
) -> Result<Vec<TranscriptRow>, String> {
    Ok(rows_within(
        &read_source(sessions, locator).await?,
        processes,
        false,
        max_rows.min(MAX_AGENT_TRANSCRIPT_ITEMS),
        max_rows,
        max_bytes,
    )
    .into_iter()
    .map(|item| item.row)
    .collect())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(super) use tests::assert_full_and_narrow_source_keys;
