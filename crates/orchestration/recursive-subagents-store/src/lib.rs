//! The agent tree's durable store.
//!
//! One `SQLite` file of its own, `subagents.sqlite3`, with its own schema
//! version and DDL fingerprint under the same fail-closed admission the
//! product store keeps (spec 03 §14). The `TreeStore` port itself lives in
//! `plakous-recursive-subagents`; this is its `SQLite` implementation, kept
//! apart for the same reason `plakous-store` sits beside `plakous-agent`: a
//! tree is not a database, and the tree crate should not link one.
//!
//! [`database`] owns the file and its admission law, [`rows`] the codecs, and
//! [`store`] the `TreeStore` port over both. The tree tables are separate from
//! `product.sqlite3`: the two files carry their own schema
//! versions; no transaction spans both.

#![allow(
    clippy::missing_errors_doc,
    clippy::too_many_lines,
    reason = "the set this crate actually trips"
)]

pub mod database;
pub mod rows;
pub mod store;

pub use database::{
    OpenError, OpenErrorKind, SCHEMA_VERSION, TreeInspection, expected_schema_fingerprint,
};
pub use store::SqliteTreeStore;

/// The `SQLite` integer bound: every count, stamp, and revision a row carries
/// has to survive the round trip through `i64`.
pub(crate) fn to_i64(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "agent tree value exceeds the SQLite integer range".to_owned())
}

/// The reverse: a stored integer that is negative is a corrupt row, not a
/// small number.
pub(crate) fn from_i64(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| {
        rusqlite::Error::InvalidColumnType(0, "u64".to_owned(), rusqlite::types::Type::Integer)
    })
}
