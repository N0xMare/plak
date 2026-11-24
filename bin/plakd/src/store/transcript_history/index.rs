//! A derived prefix index for one paging attachment, never a source of rows.

use std::cell::RefCell;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use plakous_recursive_subagents_store::admission::BUSY_TIMEOUT;
use rusqlite::Connection;

use super::{History, HistoryError, PlakPaths, ProductStore, Totals, unavailable};

pub(crate) struct HistoryReader {
    path: PathBuf,
    session: String,
    identity: (u64, u64),
    read: Mutex<Read>,
}

struct Read {
    connection: Connection,
    version: i64,
    prefix: RefCell<Option<Prefix>>,
}

impl HistoryReader {
    pub(crate) fn open(paths: &PlakPaths, session: &str) -> Result<Self, HistoryError> {
        let path = paths.product_database();
        let before = identity(&path)?;
        let connection =
            ProductStore::inspection_connection(paths, BUSY_TIMEOUT).map_err(unavailable)?;
        if identity(&path)? != before {
            return Err(HistoryError::Unavailable);
        }
        Ok(Self {
            path,
            session: session.to_owned(),
            identity: before,
            read: Mutex::new(Read {
                connection,
                version: -1,
                prefix: RefCell::default(),
            }),
        })
    }

    pub(crate) fn read<T>(
        &self,
        paths: &PlakPaths,
        read: impl FnOnce(History<'_>) -> Result<T, HistoryError>,
    ) -> Result<T, HistoryError> {
        if self.path != paths.product_database() || identity(&self.path) != Ok(self.identity) {
            return Err(HistoryError::CursorExpired);
        }
        let mut guard = self.read.lock().unwrap_or_else(PoisonError::into_inner);
        let Read {
            connection,
            version,
            prefix,
        } = &mut *guard;
        let snapshot = connection.unchecked_transaction().map_err(unavailable)?;
        ProductStore::admit_read_only(&snapshot).map_err(unavailable)?;
        // data_version is meaningful only on this same read-only connection.
        // Ask after admitting the snapshot so the pager observes its WAL end.
        let observed = snapshot
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .map_err(unavailable)?;
        if observed != *version {
            *prefix.get_mut() = None;
            *version = observed;
        }
        let mut history = History::open(&snapshot, &self.session)?;
        history.prefix = Some(prefix);
        let result = read(history);
        if identity(&self.path) != Ok(self.identity) {
            return Err(HistoryError::CursorExpired);
        }
        result
    }
}

fn identity(path: &Path) -> Result<(u64, u64), HistoryError> {
    let metadata = path.symlink_metadata().map_err(unavailable)?;
    if !metadata.is_file() {
        return Err(HistoryError::Unavailable);
    }
    Ok((metadata.dev(), metadata.ino()))
}

// The numeric prefix is bounded even if an offline import contains many
// more anchors than the ordinary transcript window. No user text is cached.
const MAX_PREFIX_ANCHORS: usize = 10_000;

pub(super) struct Prefix {
    before: u64,
    marker: bool,
    entries: Vec<(u64, Totals)>,
}

impl Prefix {
    pub(super) const fn new(before: u64) -> Self {
        Self {
            before,
            marker: false,
            entries: Vec::new(),
        }
    }

    pub(super) const fn with_marker(mut self, marker: bool) -> Self {
        self.marker = marker;
        self
    }

    pub(super) fn push(&mut self, anchor: u64, totals: Totals) {
        if let Some((last_anchor, entry)) = self.entries.last_mut()
            && *last_anchor == anchor
        {
            *entry = totals;
        } else if self.entries.len() < MAX_PREFIX_ANCHORS {
            self.entries.push((anchor, totals));
        } else {
            self.before = self.before.min(anchor);
        }
    }

    pub(super) fn range(&self, from: u64, to: u64) -> Option<Totals> {
        if to > self.before {
            return None;
        }
        let through = |anchor: u64| {
            let count = self.entries.partition_point(|(key, _)| *key < anchor);
            count.checked_sub(1).map_or(
                Totals {
                    rows: u64::from(self.marker && anchor != 0),
                    bytes: 0,
                },
                |last| self.entries[last].1,
            )
        };
        let start = through(from);
        let end = through(to);
        Some(Totals {
            rows: end.rows - start.rows,
            bytes: end.bytes - start.bytes,
        })
    }
}
