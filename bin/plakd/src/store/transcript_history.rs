//! Indexed reads of the durable part of a transcript, under one product snapshot.

use std::collections::HashMap;

use plakous::sessions::{SessionLocator, feed::FeedSeq};
use plakous_host_protocol::views::transcript::{Lane, RowKey, TranscriptItem, TranscriptRow};
use plakous_hosted::sessions::{
    MAX_TOOL_ACTIVITY_DETAIL_BYTES, ProductTranscript, projector::LineageStep,
};
use rusqlite::{Connection, OptionalExtension as _, types::ValueRef};
use serde::{Deserialize, Serialize};

use super::{ProductStore, from_i64};
use crate::home::paths::PlakPaths;

mod index;
pub(crate) use index::HistoryReader;

const MARKER: RowKey = RowKey::new(0, Lane::Runtime, 1);

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HistoryError {
    CursorExpired,
    UnknownSession,
    Unavailable,
    StaleCapture,
    UnknownRow,
    InvalidOffset,
    FrameTooLarge,
}

// Checkpoint ids are deliberately excluded: advancing within the same branch
// is an append, not a line-head change.
#[derive(Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Line {
    instance: String,
    session: String,
    head: String,
    branch: Option<(String, String)>,
}

impl Line {
    pub(crate) fn session(&self) -> &str {
        &self.session
    }
}

#[derive(Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct Prune {
    floor: Option<RowKey>,
    retained_from: Option<u64>,
    truncated: bool,
}

pub(crate) struct History<'a> {
    connection: &'a Connection,
    prefix: Option<&'a std::cell::RefCell<Option<index::Prefix>>>,
    pub(crate) line: Line,
    pub(crate) prune: Prune,
    pub(crate) applied: FeedSeq,
    pub(crate) locator: Option<SessionLocator>,
    pub(crate) lineage: Vec<LineageStep>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Totals {
    pub(crate) rows: u64,
    pub(crate) bytes: u64,
}

// A BLOB length counts unescaped UTF-8 bytes past NULs. Only rows whose
// previews may need rendering again are returned to Rust as JSON.
const BODY_SQL: &str = "coalesce(length(CAST(coalesce(json_extract(row,'$.text'),
    json_extract(row,'$.cause')) AS BLOB)),0)";
const TOOL_SQL: &str = "json_extract(row,'$.kind')='tool'";
const REBOUND_SQL: &str = "json_extract(row,'$.kind') IN ('process','thinking')";

const RANGE_SQL: &str = "SELECT anchor,lane,ordinal,row FROM transcript_rows
    WHERE session_id=?1 AND anchor BETWEEN ?2 AND ?5
      AND (anchor,lane,ordinal)>=(?2,?3,?4)
    UNION ALL
    SELECT anchor,1 AS lane,ordinal,row FROM operator_notices
    WHERE session_id=?1 AND anchor BETWEEN ?2 AND ?5
      AND (anchor,1,ordinal)>=(?2,?3,?4)";

const VISIBLE_SQL: &str =
    "json_extract(row,'$.kind') NOT IN ('truncated','session_header','auto_continue')";

fn unavailable(_: impl std::error::Error) -> HistoryError {
    HistoryError::Unavailable
}

#[cfg(test)]
pub(crate) fn read<T>(
    paths: &PlakPaths,
    session: &str,
    read: impl FnOnce(History<'_>) -> Result<T, HistoryError>,
) -> Result<T, HistoryError> {
    ProductStore::read_only_snapshot(paths, |connection| {
        Ok(History::open(connection, session).and_then(read))
    })
    .map_err(unavailable)?
}

impl<'a> History<'a> {
    fn open(connection: &'a Connection, session: &str) -> Result<Self, HistoryError> {
        let Some((instance, tree, agent, locator, applied, lineage, retained_from, truncated)) =
            connection
                .query_row(
                    "SELECT ps.instance_id,s.node_tree_id,s.node_agent_id,
                       CASE WHEN v.session_id IS NULL THEN s.locator ELSE v.head_locator END,
                       p.applied_seq,json_extract(v.state,'$.lineage'),
                       json_extract(v.state,'$.retained_from'),
                       coalesce(json_extract(v.state,'$.truncated'),0)
                     FROM sessions s CROSS JOIN product_schema ps
                     LEFT JOIN session_views v ON v.session_id=s.id
                     LEFT JOIN projector_cursors p ON p.session_id=s.id
                     WHERE s.id=?1",
                    [session],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<i64>>(4)?.map(from_i64).transpose()?,
                            r.get::<_, Option<String>>(5)?,
                            r.get::<_, Option<i64>>(6)?.map(from_i64).transpose()?,
                            r.get::<_, bool>(7)?,
                        ))
                    },
                )
                .optional()
                .map_err(unavailable)?
        else {
            return Err(HistoryError::UnknownSession);
        };
        let locator: Option<SessionLocator> = locator
            .map(|value| value.parse())
            .transpose()
            .map_err(unavailable)?;
        let head: Option<String> = connection
            .query_row(
                "SELECT CASE count(*) WHEN 1 THEN min(id) END FROM (
                   SELECT s.id FROM sessions s WHERE s.node_tree_id=?1 AND s.node_agent_id=?2
                   AND NOT EXISTS (SELECT 1 FROM sessions successor
                      WHERE successor.source_session_id=s.id AND successor.node_tree_id=?1
                        AND successor.node_agent_id=?2) LIMIT 2)",
                (tree, agent),
                |r| r.get(0),
            )
            .map_err(unavailable)?;
        let floor = if truncated {
            connection
                .query_row(
                    "SELECT anchor,lane,ordinal FROM transcript_rows WHERE session_id=?1
                       AND (anchor,lane,ordinal)!=(0,0,0)
                     ORDER BY anchor,lane,ordinal LIMIT 1",
                    [session],
                    key_of,
                )
                .optional()
                .map_err(unavailable)?
        } else {
            None
        };
        Ok(Self {
            connection,
            prefix: None,
            line: Line {
                instance,
                session: session.to_owned(),
                head: head.ok_or(HistoryError::UnknownSession)?,
                branch: locator.map(|value| {
                    (
                        value.session_id().to_string(),
                        value.branch_id().to_string(),
                    )
                }),
            },
            prune: Prune {
                floor,
                retained_from,
                truncated,
            },
            applied: FeedSeq::new(applied.unwrap_or(0)),
            locator,
            lineage: lineage
                .map(|value| serde_json::from_str(&value))
                .transpose()
                .map_err(unavailable)?
                .unwrap_or_default(),
        })
    }

    fn range(&self, from: u64, to: Option<u64>) -> Result<(i64, u8, u32, i64), HistoryError> {
        let from = RowKey::new(from, Lane::Runtime, 0);
        let lower = self.prune.floor.map_or(from, |floor| from.max(floor));
        let upper = to
            .map_or(Ok(i64::MAX), |to| i64::try_from(to.saturating_sub(1)))
            .map_err(unavailable)?;
        Ok((
            i64::try_from(lower.anchor).map_err(unavailable)?,
            lower.lane.number(),
            lower.ordinal,
            upper,
        ))
    }

    pub(crate) fn totals(&self, from: u64, to: Option<u64>) -> Result<Totals, HistoryError> {
        if to.is_some_and(|to| to <= from) {
            return Ok(Totals::default());
        }
        if let (Some(prefix), Some(to)) = (self.prefix, to)
            && let Some(total) = prefix
                .borrow()
                .as_ref()
                .and_then(|prefix| prefix.range(from, to))
        {
            return Ok(total);
        }
        let mut build = match (self.prefix, from, to) {
            (Some(_), 0, Some(to)) => Some(index::Prefix::new(to)),
            _ => None,
        };
        let (anchor, lane, ordinal, upper) = self.range(from, to)?;
        let mut query = self
            .connection
            .prepare(&format!(
                "SELECT anchor, CASE WHEN {REBOUND_SQL} THEN row END, {BODY_SQL},
                        {TOOL_SQL}, CASE WHEN {TOOL_SQL} THEN json_extract(row,'$.detail') END
                 FROM ({RANGE_SQL}) WHERE {VISIBLE_SQL} ORDER BY anchor,lane,ordinal"
            ))
            .map_err(unavailable)?;
        let mut rows = query
            .query((&self.line.session, anchor, lane, ordinal, upper))
            .map_err(unavailable)?;
        let mut totals = Totals {
            rows: u64::from(from == 0 && self.prune.truncated),
            bytes: 0,
        };
        let mut detail_lengths = HashMap::new();
        while let Some(row) = rows.next().map_err(unavailable)? {
            if let Some(bytes) = counted_body(row, &mut detail_lengths)? {
                totals.rows += 1;
                totals.bytes += bytes;
            }
            if let Some(prefix) = &mut build {
                prefix.push(
                    from_i64(row.get(0).map_err(unavailable)?).map_err(unavailable)?,
                    totals,
                );
            }
        }
        if let (Some(cache), Some(prefix)) = (self.prefix, build) {
            *cache.borrow_mut() = Some(prefix.with_marker(self.prune.truncated));
        }
        Ok(totals)
    }

    pub(crate) fn items(
        &self,
        from: u64,
        to: Option<u64>,
    ) -> Result<Vec<TranscriptItem>, HistoryError> {
        if to.is_some_and(|to| to <= from) {
            return Ok(Vec::new());
        }
        let (anchor, lane, ordinal, upper) = self.range(from, to)?;
        let mut query = self
            .connection
            .prepare(&format!(
                "SELECT * FROM ({RANGE_SQL}) WHERE {VISIBLE_SQL}
                                     ORDER BY anchor,lane,ordinal"
            ))
            .map_err(unavailable)?;
        let mut items = Vec::new();
        let rows = query
            .query_map((&self.line.session, anchor, lane, ordinal, upper), |r| {
                Ok(TranscriptItem {
                    key: key_of(r)?,
                    row: decode_row(r, 3)?,
                })
            })
            .map_err(unavailable)?;
        for item in rows {
            let TranscriptItem { key, row } = item.map_err(unavailable)?;
            if let Some(row) = render(row) {
                items.push(TranscriptItem { key, row });
            }
        }
        if from == 0 && self.prune.truncated {
            items.insert(
                items.partition_point(|item| item.key < MARKER),
                TranscriptItem {
                    key: MARKER,
                    row: TranscriptRow::Truncated,
                },
            );
        }
        Ok(items)
    }

    pub(crate) fn notice_keys(
        &self,
        ids: &[String],
    ) -> Result<impl Iterator<Item = RowKey>, HistoryError> {
        super::views::intents::operator_notice_keys(self.connection, &self.line.session, ids)
            .map(|keys| keys.into_iter().map(|(_, key)| key))
            .map_err(unavailable)
    }

    pub(crate) fn row(&self, key: RowKey) -> Result<TranscriptRow, HistoryError> {
        if key == MARKER && self.prune.truncated {
            return Ok(TranscriptRow::Truncated);
        }
        let anchor = i64::try_from(key.anchor).map_err(|_| HistoryError::UnknownRow)?;
        if self.prune.floor.is_some_and(|floor| key < floor) {
            return Err(HistoryError::UnknownRow);
        }
        let table = match key.lane {
            Lane::Runtime => "transcript_rows",
            Lane::Product => "operator_notices",
        };
        self.connection
            .query_row(
                &format!("SELECT row FROM {table} WHERE session_id=?1 AND anchor=?2 AND ordinal=?3 AND {VISIBLE_SQL}"),
                (&self.line.session, anchor, key.ordinal),
                |r| decode_row(r, 0),
            )
            .optional()
            .map_err(unavailable)?
            .and_then(render)
            .ok_or(HistoryError::UnknownRow)
    }
}

fn counted_body(
    row: &rusqlite::Row<'_>,
    detail_lengths: &mut HashMap<String, u64>,
) -> Result<Option<u64>, HistoryError> {
    if row.get::<_, bool>(3).map_err(unavailable)? {
        let value = row.get_ref(4).map_err(unavailable)?;
        if matches!(value, ValueRef::Null) {
            return Ok(Some(0));
        }
        let detail = value.as_str().map_err(unavailable)?;
        if let Some(&len) = detail_lengths.get(detail) {
            return Ok(Some(len));
        }
        let len = ProductTranscript::rendered_tool_detail_len(detail) as u64;
        // Repeated tool results need one normalization per cached body.
        // Keep only a small memo within this query, never raw text in the index.
        if detail_lengths.len() < 64 && detail.len() <= MAX_TOOL_ACTIVITY_DETAIL_BYTES {
            detail_lengths.insert(detail.to_owned(), len);
        }
        return Ok(Some(len));
    }
    if matches!(row.get_ref(1).map_err(unavailable)?, ValueRef::Null) {
        return Ok(Some(
            from_i64(row.get(2).map_err(unavailable)?).map_err(unavailable)?,
        ));
    }
    Ok(render(decode_row(row, 1).map_err(unavailable)?)
        .map(|mut row| body_mut(&mut row).map_or(0, |body| body.len() as u64)))
}

fn key_of(row: &rusqlite::Row<'_>) -> rusqlite::Result<RowKey> {
    Ok(RowKey::new(
        from_i64(row.get(0)?)?,
        Lane::from_number(row.get(1)?).ok_or(rusqlite::Error::InvalidQuery)?,
        row.get(2)?,
    ))
}

fn decode_row(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<TranscriptRow> {
    let text = row
        .get_ref(index)?
        .as_str()
        .map_err(|_| rusqlite::Error::InvalidQuery)?;
    serde_json::from_str(text).map_err(|_| rusqlite::Error::InvalidQuery)
}

// Keep restore-time redaction of old tool/process previews and reasoning,
// without a second transcript-window cut over the complete page.
fn render(row: TranscriptRow) -> Option<TranscriptRow> {
    if matches!(
        row,
        TranscriptRow::Tool(_) | TranscriptRow::Process(_) | TranscriptRow::Thinking(_)
    ) {
        return ProductTranscript::render_stored_line(row);
    }
    Some(row)
}

pub(crate) fn body_mut(row: &mut TranscriptRow) -> Option<&mut String> {
    match row {
        TranscriptRow::User { text, .. }
        | TranscriptRow::Steer { text, .. }
        | TranscriptRow::Assistant { text } => Some(text),
        TranscriptRow::Thinking(thinking) => Some(&mut thinking.text),
        TranscriptRow::Notice(notice) => Some(&mut notice.text),
        TranscriptRow::Tool(tool) => tool.detail.as_mut(),
        TranscriptRow::Process(process) => process.subject.as_mut(),
        TranscriptRow::Retry(retry) => Some(&mut retry.cause),
        TranscriptRow::Milestone(milestone) => milestone.text.as_mut(),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "offline SQLite fixtures")]
mod tests;
