//! Bounded, incremental reads of retained terminal measurements.

use std::collections::BTreeMap;
use std::time::Duration;

use plakd_wire_types::v13::{Cost, TreeCost};
use plakous::model::{BranchId, SessionId};
use plakous::sessions::SessionLocator;
use rusqlite::{Connection, OpenFlags};

use crate::home::paths::PlakPaths;

const TERMINALS_PER_READ: i64 = 512;
const TERMINALS: &str = "SELECT rowid, branch_id, account IS NOT NULL,
    substr(model_operations,1,21), substr(reporting_operations,1,21), substr(cost_micros,1,21)
    FROM recorded_terminals WHERE session_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Member {
    pub(crate) locator: Option<SessionLocator>,
    pub(crate) started_ms: u64,
    pub(crate) turns: u64,
    pub(crate) remote: bool,
}

#[derive(Default)]
pub(crate) struct Costs {
    connection: Option<Connection>,
    version: Option<i64>,
    evidence: Option<Evidence>,
    sessions: BTreeMap<(SessionId, BranchId), Scan>,
    unfinished: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Evidence {
    started_ms: u64,
    retained_ms: Option<u64>,
    lossless: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Total {
    measured: Option<u64>,
    overflow: bool,
}

impl Total {
    fn add(&mut self, other: Self) {
        self.overflow |= other.overflow;
        if let Some(micros) = other.measured {
            self.measured = self.measured.unwrap_or(0).checked_add(micros);
            self.overflow |= self.measured.is_none();
        }
    }

    fn wire(self) -> Cost {
        match (self.overflow, self.measured) {
            (false, Some(micros)) => Cost::Measured { micros },
            _ => Cost::Unknown,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Scan {
    after: i64,
    turns: u64,
    missing: bool,
    total: Total,
    complete: bool,
}

impl Costs {
    /// A telemetry-only commit must invalidate the draft watch independently
    /// of the product cursor. No usage rows are touched by the poll itself.
    pub(crate) fn poll(&mut self, paths: &PlakPaths) -> bool {
        if self.connection.is_none() {
            self.connection = open(paths).ok();
        }
        let version = self.connection.as_ref().and_then(|connection| {
            connection
                .query_row("PRAGMA data_version", [], |row| row.get(0))
                .ok()
        });
        let changed = version != self.version || std::mem::take(&mut self.unfinished);
        self.version = version;
        changed
    }

    pub(crate) fn read(&mut self, root: Member, children: Option<&[Member]>) -> (Cost, TreeCost) {
        let result = self.read_snapshot(root, children);
        if self.connection.is_some() && result.is_err() {
            self.unfinished = true;
        }
        result.unwrap_or((
            Cost::Unknown,
            TreeCost {
                cost: Cost::Unknown,
                complete: false,
            },
        ))
    }

    fn read_snapshot(
        &mut self,
        root: Member,
        children: Option<&[Member]>,
    ) -> rusqlite::Result<(Cost, TreeCost)> {
        let connection = self
            .connection
            .as_ref()
            .ok_or(rusqlite::Error::InvalidQuery)?;
        let tx = connection.unchecked_transaction()?;
        let evidence = tx.query_row(
            "SELECT started_at_ms,retained_from_ms,dropped=0 AND write_errors=0
             FROM recording_evidence WHERE singleton=1",
            [],
            |row| {
                Ok(Evidence {
                    started_ms: crate::store::from_i64(row.get(0)?)?,
                    retained_ms: row
                        .get::<_, Option<i64>>(1)?
                        .map(crate::store::from_i64)
                        .transpose()?,
                    lossless: row.get(2)?,
                })
            },
        )?;
        // Drop cached subtotals if the retained set may have shrunk.
        if self.evidence.is_none_or(|old| {
            (old.started_ms, old.retained_ms) != (evidence.started_ms, evidence.retained_ms)
        }) {
            self.sessions.clear();
        }
        self.evidence = Some(evidence);
        let mut query = tx.prepare(TERMINALS)?;
        let mut known = BTreeMap::new();
        for member in std::iter::once(&root).chain(children.into_iter().flatten()) {
            let Some(locator) = member.locator else {
                continue;
            };
            let key = (locator.session_id(), locator.branch_id());
            if let std::collections::btree_map::Entry::Vacant(entry) = known.entry(key) {
                let scan = self.sessions.entry(key).or_default();
                let mut next = *scan;
                next.read(&mut query, key)?;
                *scan = next;
                self.unfinished |= !scan.complete;
                entry.insert(next);
            }
        }
        let measurement = |member: Member| {
            let scan = member
                .locator
                .and_then(|loc| known.get(&(loc.session_id(), loc.branch_id())))
                .copied()
                .unwrap_or_default();
            let complete = scan.complete
                && scan.turns == member.turns
                && !scan.missing
                && !member.remote
                && evidence.lossless
                && evidence.started_ms.max(evidence.retained_ms.unwrap_or(0)) <= member.started_ms
                && matches!(scan.total.wire(), Cost::Measured { .. });
            (scan.total, complete)
        };
        let (mut subtotal, root_complete) = measurement(root);
        let root_cost = if root_complete {
            subtotal.wire()
        } else {
            Cost::Unknown
        };
        let mut complete = root_complete && children.is_some();
        let mut seen = std::collections::BTreeSet::new();
        if let Some(loc) = root.locator {
            seen.insert((loc.session_id(), loc.branch_id()));
        }
        for child in children.into_iter().flatten() {
            let (cost, measured) = measurement(*child);
            complete &= measured;
            if let Some(loc) = child.locator
                && seen.insert((loc.session_id(), loc.branch_id()))
            {
                subtotal.add(cost);
            }
        }
        Ok((
            root_cost,
            TreeCost {
                cost: subtotal.wire(),
                complete: complete && !subtotal.overflow,
            },
        ))
    }
}

impl Scan {
    fn read(
        &mut self,
        query: &mut rusqlite::Statement<'_>,
        (session, branch): (SessionId, BranchId),
    ) -> rusqlite::Result<()> {
        let branch = branch.to_string();
        let mut rows = query.query((session.to_string(), self.after, TERMINALS_PER_READ + 1))?;
        for _ in 0..TERMINALS_PER_READ {
            let Some(row) = rows.next()? else {
                self.complete = true;
                return Ok(());
            };
            self.after = row.get(0)?;
            if row.get::<_, String>(1)? != branch {
                continue;
            }
            self.turns = self.turns.saturating_add(1);
            let attributed: bool = row.get(2)?;
            let operations = counter(row, 3)?;
            let reporting = counter(row, 4)?;
            let cost = row
                .get::<_, Option<String>>(5)?
                .map(|value| decimal(&value))
                .transpose()?;
            self.missing |= !attributed || operations != reporting || cost.is_none();
            if attributed && operations == reporting {
                self.total.add(Total {
                    measured: cost,
                    overflow: false,
                });
            }
        }
        self.complete = rows.next()?.is_none();
        Ok(())
    }
}

fn counter(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    decimal(&row.get::<_, String>(index)?)
}

fn decimal(value: &str) -> rusqlite::Result<u64> {
    value.parse().map_err(|_| rusqlite::Error::InvalidQuery)
}

fn open(paths: &PlakPaths) -> rusqlite::Result<Connection> {
    let connection = Connection::open_with_flags(
        paths.telemetry_database(),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    connection.busy_timeout(Duration::from_millis(25))?;
    let version = connection.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))?;
    if version == crate::telemetry_store::TELEMETRY_SCHEMA_VERSION {
        Ok(connection)
    } else {
        Err(rusqlite::Error::InvalidQuery)
    }
}

#[cfg(test)]
mod tests;
