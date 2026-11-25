//! Identity-only projection of an existing preservation checkpoint.

use std::{
    io,
    path::Path,
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

use super::facts::{Key, texts};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliFailureKind, CliResult};

#[derive(Deserialize)]
pub(in crate::host::upgrade) struct StoreIdentities {
    product: Instance,
    runtime: Option<Instance>,
    trees: TreeIdentity,
}

#[derive(Deserialize)]
struct Instance {
    instance: Key,
}

#[derive(Deserialize)]
struct TreeIdentity {
    schema: Instance,
}

pub(super) fn instance_key(raw: &str) -> Key {
    texts([raw])
}

pub(super) fn runtime_instance(db: &Connection) -> rusqlite::Result<Key> {
    db.query_row(
        "SELECT lease_domain FROM store_meta WHERE id=1",
        [],
        |row| {
            Ok(crate::shipping_artifact::digest(
                &row.get::<_, [u8; 48]>(0)?,
            ))
        },
    )
}

impl StoreIdentities {
    pub(in crate::host::upgrade) fn verify(&self, paths: &PlakPaths) -> CliResult<()> {
        let deadline = Instant::now() + crate::carry::upgrade::timing::STORE;
        check(
            &paths.product_database(),
            "product",
            Some(&self.product.instance),
            deadline,
            |db| {
                schema_instance(
                    db,
                    "SELECT instance_id FROM product_schema WHERE singleton=1",
                )
            },
        )?;
        check(
            &paths.subagents_database(),
            "tree",
            Some(&self.trees.schema.instance),
            deadline,
            |db| {
                schema_instance(
                    db,
                    "SELECT instance_id FROM subagents_schema WHERE singleton=1",
                )
            },
        )?;
        check(
            &paths.runtime_database(),
            "runtime",
            self.runtime.as_ref().map(|runtime| &runtime.instance),
            deadline,
            runtime_instance,
        )
    }
}

fn schema_instance(db: &Connection, query: &str) -> rusqlite::Result<Key> {
    db.query_row(query, [], |row| Ok(instance_key(&row.get::<_, String>(0)?)))
}

fn check(
    path: &Path,
    label: &str,
    expected: Option<&Key>,
    deadline: Instant,
    read: impl FnOnce(&Connection) -> rusqlite::Result<Key>,
) -> CliResult<()> {
    let observed = || -> rusqlite::Result<Key> {
        let mut db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(Duration::from_millis(250))?;
        db.progress_handler(4096, Some(move || Instant::now() >= deadline))?;
        read(&db)
    };
    let matches = match expected {
        Some(key) => Instant::now() < deadline && observed().is_ok_and(|found| &found == key),
        None => path
            .symlink_metadata()
            .is_err_and(|failure| failure.kind() == io::ErrorKind::NotFound),
    };
    if !matches {
        return Err(CliFailure::classified(
            CliFailureKind::RecoveryRequired,
            format!(
                "host_upgrade_acknowledge_identity_mismatch: the {label} store does not match the staged baseline; keep the fence and recover the original home stores"
            ),
        ));
    }
    Ok(())
}
