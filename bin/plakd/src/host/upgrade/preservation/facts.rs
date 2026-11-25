use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use rusqlite::types::ValueRef;
use rusqlite::{Connection, Params, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::check;
use crate::CliResult;

use super::unreadable;

pub(super) type Key = String;
pub(super) type Map<T> = BTreeMap<Key, T>;

// Counts rows, edges, and sequence elements, not payload bytes. One SQLite
// row is borrowed at a time; a large or damaged store fails closed at the cap.
const MAX_FACTS: usize = 1_000_000;
const MAX_VALUE_BYTES: usize = 64 << 20;

pub(super) fn configure_reader(db: &Connection) -> CliResult<()> {
    // Recursive reachability and ordered scans may need a temporary index.
    db.execute_batch("PRAGMA temp_store=FILE; PRAGMA cache_size=-8192;")
        .map_err(|_| unreadable("SQLite inspection bound"))?;
    Ok(())
}

pub(super) struct Budget {
    facts: usize,
    deadline: Instant,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            facts: 0,
            deadline: Instant::now() + Duration::from_secs(60),
        }
    }
}

impl Budget {
    pub(super) const fn deadline(&self) -> Instant {
        self.deadline
    }

    pub(super) fn take(&mut self) -> CliResult<()> {
        self.facts += 1;
        check(
            Instant::now() < self.deadline,
            "preservation exceeded its time bound",
        )?;
        check(
            self.facts <= MAX_FACTS,
            "preservation snapshot exceeds its fact budget",
        )
    }
}

pub(super) fn scan(
    conn: &Connection,
    sql: &str,
    parameters: impl Params,
    scope: &str,
    mut read: impl FnMut(&Row<'_>) -> CliResult<()>,
) -> CliResult<()> {
    let mut statement = conn.prepare(sql).map_err(|_| unreadable(scope))?;
    let mut rows = statement.query(parameters).map_err(|_| unreadable(scope))?;
    while let Some(row) = rows.next().map_err(|_| unreadable(scope))? {
        read(row)?;
    }
    Ok(())
}

pub(super) fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, index: usize) -> CliResult<T> {
    cell(row, index)?;
    row.get(index).map_err(|_| unreadable("SQLite row"))
}

fn cell<'row>(row: &'row Row<'_>, index: usize) -> CliResult<ValueRef<'row>> {
    let value = row.get_ref(index).map_err(|_| unreadable("SQLite row"))?;
    if let ValueRef::Blob(bytes) | ValueRef::Text(bytes) = value {
        check(
            bytes.len() <= MAX_VALUE_BYTES,
            "preservation value exceeds its byte bound",
        )?;
    }
    Ok(value)
}

pub(super) fn revision(row: &Row<'_>, index: usize) -> CliResult<u64> {
    Ok(u64::from_be_bytes(get(row, index)?))
}

pub(super) fn nonnegative(row: &Row<'_>, index: usize) -> CliResult<u64> {
    u64::try_from(get::<i64>(row, index)?).map_err(|_| unreadable("SQLite integer"))
}

pub(super) fn columns(row: &Row<'_>, indexes: impl IntoIterator<Item = usize>) -> CliResult<Key> {
    let mut digest = Sha256::new();
    for index in indexes {
        let value = cell(row, index)?;
        match value {
            ValueRef::Null => digest.update([0]),
            ValueRef::Integer(value) => {
                digest.update([1]);
                digest.update(value.to_be_bytes());
            }
            ValueRef::Real(value) => {
                digest.update([2]);
                digest.update(value.to_bits().to_be_bytes());
            }
            ValueRef::Text(value) => {
                std::str::from_utf8(value).map_err(|_| unreadable("SQLite text"))?;
                digest.update([3]);
                frame(&mut digest, value);
            }
            ValueRef::Blob(value) => {
                digest.update([4]);
                frame(&mut digest, value);
            }
        }
    }
    Ok(finish(digest))
}

pub(super) fn texts<'a>(fields: impl IntoIterator<Item = &'a str>) -> Key {
    let mut digest = Sha256::new();
    for field in fields {
        digest.update([3]);
        frame(&mut digest, field.as_bytes());
    }
    finish(digest)
}

fn frame(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

pub(super) fn finish(digest: Sha256) -> Key {
    crate::shipping_artifact::hex_digest(&digest.finalize())
}

pub(super) fn insert<T>(map: &mut Map<T>, key: Key, value: T) -> CliResult<()> {
    check(
        map.insert(key, value).is_none(),
        "duplicate preservation identity",
    )
}

pub(super) fn subset<T: PartialEq>(before: &Map<T>, after: &Map<T>, scope: &str) -> CliResult<()> {
    check(
        before
            .iter()
            .all(|(key, value)| after.get(key) == Some(value)),
        &format!("preservation lost or changed pre-existing {scope}"),
    )
}

pub(super) fn locator_branch(locator: &str) -> CliResult<plakous::sessions::SessionLocator> {
    locator.parse().map_err(|_| unreadable("runtime locator"))
}

pub(super) fn branch_key(locator: plakous::sessions::SessionLocator) -> Key {
    texts([
        locator.session_id().to_string().as_str(),
        locator.branch_id().to_string().as_str(),
    ])
}

#[derive(Clone, Copy)]
pub(super) struct Table {
    pub name: &'static str,
    pub key: &'static [&'static str],
    pub live: &'static [&'static str],
    pub monotone: &'static [&'static str],
    pub once: &'static [&'static str],
    pub prefix: &'static [&'static str],
}

impl Table {
    pub const fn stable(name: &'static str, key: &'static [&'static str]) -> Self {
        Self {
            name,
            key,
            live: &[],
            monotone: &[],
            once: &[],
            prefix: &[],
        }
    }
}

#[derive(Deserialize, Serialize)]
pub(super) struct Fact {
    stable: Key,
    monotone: Vec<u64>,
    once: Vec<Option<Key>>,
    prefix: Vec<Vec<Key>>,
}

impl Fact {
    pub(super) fn preserved_by(&self, after: &Self) -> bool {
        self.stable == after.stable
            && self.monotone.len() == after.monotone.len()
            && self.once.len() == after.once.len()
            && self.prefix.len() == after.prefix.len()
            && self
                .monotone
                .iter()
                .zip(&after.monotone)
                .all(|(old, new)| new >= old)
            && self
                .once
                .iter()
                .zip(&after.once)
                .all(|(old, new)| old.is_none() || old == new)
            && self
                .prefix
                .iter()
                .zip(&after.prefix)
                .all(|(old, new)| new.starts_with(old))
    }
}

pub(super) fn tables(
    db: &Connection,
    descriptions: &[Table],
    budget: &mut Budget,
) -> CliResult<Map<Map<Fact>>> {
    descriptions
        .iter()
        .map(|rule| Ok((rule.name.to_owned(), table(db, rule, budget)?)))
        .collect()
}

fn table(db: &Connection, rule: &Table, budget: &mut Budget) -> CliResult<Map<Fact>> {
    let corrupt = || unreadable(rule.name);
    let mut query = db
        .prepare(&format!("SELECT * FROM {}", rule.name))
        .map_err(|_| corrupt())?;
    let names: Vec<_> = query.column_names().collect();
    let indices = |cols: &[&str]| {
        cols.iter()
            .map(|col| {
                names
                    .iter()
                    .position(|name| name == col)
                    .ok_or_else(corrupt)
            })
            .collect::<CliResult<Vec<_>>>()
    };
    let key = indices(rule.key)?;
    let monotone = indices(rule.monotone)?;
    let once = indices(rule.once)?;
    let prefix = indices(rule.prefix)?;
    let ignored = indices(rule.live)?;
    let stable = (0..names.len())
        .filter(|index| {
            !ignored.contains(index)
                && !monotone.contains(index)
                && !once.contains(index)
                && !prefix.contains(index)
        })
        .collect::<Vec<_>>();
    let node = (rule.name == "agent_nodes")
        .then(|| indices(&["session_locator"]))
        .transpose()?;
    let mut result = Map::new();
    let mut rows = query.query([]).map_err(|_| corrupt())?;
    while let Some(row) = rows.next().map_err(|_| corrupt())? {
        budget.take()?;
        let fact = |index| columns(row, [index]);
        let mut once = once
            .iter()
            .map(|&index| {
                if row.get_ref(index).map_err(|_| corrupt())? == ValueRef::Null {
                    Ok(None)
                } else {
                    fact(index).map(Some)
                }
            })
            .collect::<CliResult<Vec<_>>>()?;
        if let Some(index) = node.as_ref().and_then(|indexes| indexes.first()) {
            once.push(
                get::<Option<String>>(row, *index)?
                    .as_deref()
                    .map(locator_branch)
                    .transpose()?
                    .map(branch_key),
            );
        }
        let prefix = prefix
            .iter()
            .map(|&index| {
                let value = cell(row, index)?.as_str().map_err(|_| corrupt())?;
                ordered_prefix(db, rule.name, value, budget)
            })
            .collect::<CliResult<Vec<_>>>()?;
        insert(
            &mut result,
            columns(row, key.iter().copied())?,
            Fact {
                stable: columns(row, stable.iter().copied())?,
                monotone: monotone
                    .iter()
                    .map(|&index| nonnegative(row, index))
                    .collect::<CliResult<_>>()?,
                once,
                prefix,
            },
        )?;
    }
    Ok(result)
}

fn ordered_prefix(
    db: &Connection,
    table: &str,
    value: &str,
    budget: &mut Budget,
) -> CliResult<Vec<Key>> {
    check(
        value.trim_start().starts_with('['),
        "invalid preservation prefix array",
    )?;
    let mut list = Vec::new();
    scan(
        db,
        "SELECT type,atom FROM json_each(?1) ORDER BY key",
        [value],
        table,
        |entry| {
            budget.take()?;
            let kind: String = get(entry, 0)?;
            if table == "agent_attempts" {
                check(
                    kind == "integer" && nonnegative(entry, 1).is_ok(),
                    "invalid applied terminal sequence",
                )?;
            } else {
                check(kind == "text", "invalid preservation prefix element")?;
            }
            list.push(columns(entry, [1])?);
            Ok(())
        },
    )?;
    Ok(list)
}

pub(super) fn compare_tables(
    before: &Map<Map<Fact>>,
    after: &Map<Map<Fact>>,
    exempt: impl Fn(&str, &str) -> bool,
) -> CliResult<()> {
    for (table, rows) in before {
        let current = after.get(table).ok_or_else(|| unreadable(table))?;
        check(
            rows.iter().all(|(key, fact)| {
                current
                    .get(key)
                    .map_or_else(|| exempt(table, key), |after| fact.preserved_by(after))
            }),
            &format!("preservation lost, changed or regressed a {table} record"),
        )?;
    }
    Ok(())
}
