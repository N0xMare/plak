use std::{num::NonZeroUsize, path::Path};

use plakd_wire_types::v13::PAGE_TARGET_BYTES;
use plakous::model::SessionId;
use plakous::sessions::AgentSessions;
use plakous::sessions::feed::{
    FeedCommandKind, FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedRecord, FeedSeq,
};

use super::{History, HistoryError};

#[derive(Default)]
pub(super) struct Turns {
    pub(super) starts: Vec<u64>,
    pub(super) retained_from: u64,
}

pub(super) struct OlderCut {
    pub(super) from: u64,
    budget: u64,
    bytes: u64,
    any_rows: bool,
}

impl OlderCut {
    pub(super) const fn new(before: u64, budget: u64) -> Self {
        Self {
            from: before,
            budget,
            bytes: 0,
            any_rows: false,
        }
    }

    pub(super) fn include(
        &mut self,
        history: &History<'_>,
        start: u64,
    ) -> Result<bool, HistoryError> {
        let size = history.totals(start, Some(self.from))?;
        let bytes = self.bytes.saturating_add(size.bytes);
        if self.any_rows && bytes > self.budget {
            return Ok(false);
        }
        self.from = start;
        self.bytes = bytes;
        self.any_rows |= size.rows != 0;
        Ok(true)
    }
}

pub(super) fn turns_at(
    path: &Path,
    history: &History<'_>,
    before: Option<u64>,
) -> Result<Turns, HistoryError> {
    let mut upper = before.unwrap_or(history.applied.get());
    let Some(locator) = history.locator.filter(|_| upper != 0) else {
        return Ok(Turns::default());
    };
    let mut turns = Turns::default();
    let mut older = before.map(|before| OlderCut::new(before, PAGE_TARGET_BYTES as u64));
    let window_size = FeedPageRequest::after(FeedCursor::start(locator.session_id()))
        .max_records
        .get() as u64;
    // Walk only the windows needed by this page. The product snapshot fixes
    // the applied upper bound and the lineage; the records themselves are
    // immutable. An imported or pruned prefix without boundary facts remains
    // indivisible.
    'windows: while upper > 0 && upper >= turns.retained_from {
        let lower = upper.saturating_sub(window_size);
        let records = window(
            path,
            locator.session_id(),
            lower,
            upper,
            &mut turns.retained_from,
        )?;
        for record in records.iter().rev() {
            let header = record.header();
            let seq = header.seq.get();
            let branch = history
                .lineage
                .iter()
                .rev()
                .find(|step| step.from_seq <= seq)
                .map_or_else(|| locator.branch_id(), |step| step.branch_id);
            if header.branch_id != branch
                || !record.facts().iter().any(|fact| {
                    matches!(
                        fact,
                        FeedFact::CommandAccepted {
                            kind: FeedCommandKind::Root { .. },
                            turn_id: Some(_),
                            ..
                        }
                    )
                })
            {
                continue;
            }
            turns.starts.push(seq);
            let Some(older) = &mut older else {
                break 'windows;
            };
            if seq < older.from && !older.include(history, seq)? {
                break 'windows;
            }
        }
        upper = lower;
    }
    turns.starts.retain(|&start| start >= turns.retained_from);
    turns.starts.reverse();
    Ok(turns)
}

fn window(
    path: &Path,
    session: SessionId,
    mut after: u64,
    upper: u64,
    retained: &mut u64,
) -> Result<Vec<FeedRecord>, HistoryError> {
    let mut out = Vec::new();
    // The feed may cap a page by bytes before it reaches the requested
    // record count. Finish the window before consuming it in reverse.
    while let Some(limit) =
        NonZeroUsize::new(usize::try_from(upper - after).map_err(|_| HistoryError::Unavailable)?)
    {
        let request = FeedPageRequest {
            cursor: FeedCursor::new(session, FeedSeq::new(after)),
            max_records: limit,
        };
        match AgentSessions::inspect_feed_sqlite(path, request)
            .map_err(|_| HistoryError::Unavailable)?
        {
            FeedPage::BehindRetention { retained_from, .. } => {
                *retained = retained_from.get();
                after = upper.min(retained_from.get().saturating_sub(1));
            }
            FeedPage::Records {
                records,
                retained_from,
                ..
            } => {
                *retained = retained_from.get();
                let Some(last) = records.last() else { break };
                after = last.header().seq.get().min(upper);
                out.extend(
                    records
                        .into_iter()
                        .take_while(|record| record.header().seq.get() <= upper),
                );
            }
            _ => return Err(HistoryError::Unavailable),
        }
    }
    out.retain(|record| record.header().seq.get() >= *retained);
    Ok(out)
}
