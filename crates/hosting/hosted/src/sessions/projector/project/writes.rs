use super::{
    Applier, BTreeMap, CallView, FeedSeq, Lane, MAX_TRANSCRIPT_BYTES, MAX_TRANSCRIPT_LINES,
    OpenTurn, ProcessTrack, ProductIntents, RowKey, SessionViews, Successor, TranscriptRow,
    bound_row, row_bytes,
};

impl Applier<'_> {
    pub(super) fn next_key(&mut self) -> RowKey {
        let key = RowKey::new(self.seq(), Lane::Runtime, self.ordinal);
        self.ordinal = self.ordinal.saturating_add(1);
        key
    }

    pub(super) fn push(&mut self, row: TranscriptRow) {
        let key = self.next_key();
        self.replace(key, row);
    }

    pub(super) fn replace(&mut self, key: RowKey, row: TranscriptRow) {
        // The head's canonical seed already covers this entire replay window.
        if self
            .views
            .state
            .history_through
            .is_some_and(|through| self.seq() <= through)
        {
            return;
        }
        self.touched
            .entry(key)
            .or_insert_with(|| self.views.rows.contains_key(&key));
        put_row(self.views, key, row);
    }
}

/// The view rows one run changed, and the cursor CAS that commits them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ViewWrites {
    /// The cursor the run read; the store's CAS compares against it.
    pub expected_cursor: FeedSeq,
    /// The cursor after the run.
    pub cursor: FeedSeq,
    /// Rows to insert or replace at their keys.
    pub upserts: Vec<(RowKey, TranscriptRow)>,
    /// Rows the transcript bound dropped.
    pub deletes: Vec<RowKey>,
    /// Changed call bookkeeping; `None` deletes the key.
    pub calls: Vec<(String, Option<CallView>)>,
    /// Changed open turns; `None` deletes the key.
    pub open_turns: Vec<(String, Option<OpenTurn>)>,
    /// Changed process bookkeeping; `None` deletes the key.
    pub processes: Vec<(u64, Option<ProcessTrack>)>,
    /// Successor records to publish under this cursor CAS.
    pub successors: Vec<Successor>,
}

impl ViewWrites {
    /// The writes that replace stored views, including keyed bookkeeping.
    #[must_use]
    pub fn between(from: &SessionViews, to: &SessionViews) -> Self {
        Self {
            expected_cursor: from.cursor,
            cursor: to.cursor,
            upserts: to
                .rows
                .iter()
                .filter(|(key, row)| from.rows.get(key) != Some(row))
                .map(|(key, row)| (*key, row.clone()))
                .collect(),
            deletes: from
                .rows
                .keys()
                .filter(|key| !to.rows.contains_key(key))
                .copied()
                .collect(),
            calls: changed_entries(&from.state.calls, &to.state.calls),
            open_turns: changed_entries(&from.state.open_turns, &to.state.open_turns),
            processes: changed_entries(&from.state.processes, &to.state.processes),
            successors: Vec::new(),
        }
    }

    /// Whether the run changed nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.expected_cursor == self.cursor
            && self.upserts.is_empty()
            && self.deletes.is_empty()
            && self.calls.is_empty()
            && self.open_turns.is_empty()
            && self.processes.is_empty()
            && self.successors.is_empty()
    }
}

pub(super) fn changed_entries<K: Clone + Ord, V: Clone + PartialEq>(
    from: &BTreeMap<K, V>,
    to: &BTreeMap<K, V>,
) -> Vec<(K, Option<V>)> {
    to.iter()
        .filter(|(key, value)| from.get(key) != Some(value))
        .map(|(key, value)| (key.clone(), Some(value.clone())))
        .chain(
            from.keys()
                .filter(|key| !to.contains_key(key))
                .map(|key| (key.clone(), None)),
        )
        .collect()
}

pub(super) fn put_row(views: &mut SessionViews, key: RowKey, row: TranscriptRow) {
    let row = bound_row(row);
    let added = row_bytes(&row) as u64;
    if let Some(old) = views.rows.insert(key, row) {
        views.state.row_bytes = views.state.row_bytes.saturating_sub(row_bytes(&old) as u64);
    }
    views.state.row_bytes = views.state.row_bytes.saturating_add(added);
}

/// Past either transcript bound, drops the oldest rows down to seven eighths
/// of both, in key order; the newest row always stays. A function of the rows
/// alone, so a rebuild trims identically.
/// The product-lane rows the transcript bound counts after record `seq`:
/// imported rows and the operator notices anchored before it, with their
/// encoded sizes. A notice anchored at or after `seq` was written after this
/// record was applied, so it is not counted here, incrementally or in a
/// rebuild.
pub(super) fn counted_notices(intents: &ProductIntents, seq: u64) -> Vec<(RowKey, u64)> {
    let imported = intents.imported.iter().enumerate().map(|(ordinal, row)| {
        (
            RowKey::new(0, Lane::Product, u32::try_from(ordinal).unwrap_or(u32::MAX)),
            row_bytes(row) as u64,
        )
    });
    let notices = intents
        .notices
        .iter()
        .filter(|notice| notice.anchor < seq)
        .map(|notice| (notice.key(), row_bytes(&notice.row) as u64));
    imported.chain(notices).collect()
}

/// The transcript bound over the merged transcript: the projector's rows and
/// the counted product-lane rows, in key order. Only projector rows are
/// dropped; a product row older than the oldest kept projector row of a
/// truncated transcript goes with it ([`SessionViews::transcript`]).
pub(super) fn bound_transcript(
    views: &mut SessionViews,
    touched: &mut BTreeMap<RowKey, bool>,
    notices: &[(RowKey, u64)],
) {
    let measured = |views: &SessionViews| {
        let floor = views
            .state
            .truncated
            .then(|| views.rows.keys().find(|key| **key != RowKey::TRUNCATION))
            .flatten()
            .copied();
        let (lines, bytes) = notices
            .iter()
            .filter(|(key, _)| floor.is_none_or(|floor| *key >= floor))
            .fold((0_usize, 0_u64), |(lines, bytes), (_, size)| {
                (lines + 1, bytes.saturating_add(*size))
            });
        (
            views.rows.len() + lines,
            views.state.row_bytes.saturating_add(bytes),
        )
    };
    let (lines, bytes) = measured(views);
    if lines <= MAX_TRANSCRIPT_LINES && bytes <= MAX_TRANSCRIPT_BYTES as u64 {
        return;
    }
    let target_lines = MAX_TRANSCRIPT_LINES - MAX_TRANSCRIPT_LINES / 8;
    let target_bytes = (MAX_TRANSCRIPT_BYTES - MAX_TRANSCRIPT_BYTES / 8) as u64;
    while views.rows.len() > 1 {
        let (lines, bytes) = measured(views);
        if lines <= target_lines && bytes <= target_bytes {
            break;
        }
        let Some((key, row)) = views.rows.pop_first() else {
            break;
        };
        views.state.row_bytes = views.state.row_bytes.saturating_sub(row_bytes(&row) as u64);
        if let Some(tool) = row.as_tool()
            && let Some(call) = views.state.calls.get_mut(&tool.call_id)
        {
            // Keep a settled tombstone until its canonical result. Late results
            // cannot resurrect this key or count the call a second time.
            call.settled = true;
        }
        touched.entry(key).or_insert(true);
        views.state.truncated = true;
    }
}
