//! [`TreeCustody`] over `agent_tree_custody`: each call reads and writes the
//! one record in a single `Immediate` transaction, so two processes taking
//! the same tree serialize on the file's write lock and exactly one of them
//! claims it. The transitions are the tree's ([`CustodyRecord`]); this only
//! stores them.

use plakous_recursive_subagents::tree::{
    CustodyClaim, CustodyError, CustodyHolding, CustodyHost, CustodyRecord, CustodyTake,
    HolderGone, HostInstance, TreeCustody, TreeStoreError,
};
use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, Transaction, TransactionBehavior};

use crate::store::{SqliteTreeStore, sqlite_unavailable, sqlite_write_error};
use crate::{from_i64, to_i64};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

fn read(tx: &Transaction<'_>, tree_id: &str) -> Result<Option<CustodyRecord>, TreeStoreError> {
    let invalid = || TreeStoreError::corrupt("agent tree custody row is invalid");
    let row = tx
        .query_row(
            "SELECT fence, host_id, host_instance, host_binary, host_name, claimed_at, renewed_at, released FROM agent_tree_custody WHERE tree_id=?1",
            [tree_id],
            |row| {
                Ok((
                    from_i64(row.get(0)?)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    from_i64(row.get(5)?)?,
                    from_i64(row.get(6)?)?,
                    row.get::<_, i64>(7)? == 1,
                ))
            },
        )
        .optional()
        .map_err(|_| invalid())?;
    let Some((fence, host_id, instance, binary, host_name, claimed_at_ms, renewed_at_ms, released)) =
        row
    else {
        return Ok(None);
    };
    Ok(Some(CustodyRecord {
        fence,
        holding: CustodyHolding {
            host: CustodyHost {
                host_id,
                instance: HostInstance::parse(&instance).map_err(|_| invalid())?,
                binary,
                host_name,
            },
            claimed_at_ms,
            renewed_at_ms,
        },
        released,
    }))
}

fn write(
    tx: &Transaction<'_>,
    tree_id: &str,
    record: &CustodyRecord,
) -> Result<(), TreeStoreError> {
    let holding = &record.holding;
    let changed = tx
        .execute(
            "INSERT INTO agent_tree_custody(tree_id, fence, host_id, host_instance, host_binary, host_name, claimed_at, renewed_at, released) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(tree_id) DO UPDATE SET fence=excluded.fence, host_id=excluded.host_id, host_instance=excluded.host_instance, host_binary=excluded.host_binary, host_name=excluded.host_name, claimed_at=excluded.claimed_at, renewed_at=excluded.renewed_at, released=excluded.released",
            (
                tree_id,
                to_i64(record.fence)?,
                holding.host.host_id.as_str(),
                holding.host.instance.to_string(),
                holding.host.binary.as_deref(),
                holding.host.host_name.as_str(),
                to_i64(holding.claimed_at_ms)?,
                to_i64(holding.renewed_at_ms)?,
                i64::from(record.released),
            ),
        )
        .map_err(|error| sqlite_write_error(error, "could not write agent tree custody".to_owned()))?;
    if changed != 1 {
        return Err(TreeStoreError::unavailable(
            "agent tree custody write was not exact",
        ));
    }
    Ok(())
}

/// Proves `claim` against `tree_id`'s stored record inside the write's own
/// transaction.
pub(crate) fn prove_in(
    tx: &Transaction<'_>,
    tree_id: &str,
    claim: &CustodyClaim,
) -> Result<(), TreeStoreError> {
    plakous_recursive_subagents::tree::prove_write(read(tx, tree_id)?.as_ref(), tree_id, claim)
}

/// Removes `tree_id`'s custody record with its deleted rows.
pub(crate) fn delete_record(tx: &Transaction<'_>, tree_id: &str) -> Result<(), TreeStoreError> {
    tx.execute("DELETE FROM agent_tree_custody WHERE tree_id=?1", [tree_id])
        .map_err(|error| {
            sqlite_write_error(error, "could not delete agent tree custody".to_owned())
        })?;
    Ok(())
}

impl SqliteTreeStore {
    /// Runs one custody transition of `tree_id` inside one `Immediate`
    /// transaction: `step` maps the stored record to the record to write, if
    /// any, and its answer.
    fn custody_step<T>(
        &self,
        tree_id: &str,
        step: impl FnOnce(Option<&CustodyRecord>) -> Result<(Option<CustodyRecord>, T), CustodyError>,
    ) -> Result<T, CustodyError> {
        self.with_connection(|connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(|error| {
                    sqlite_unavailable("could not begin agent tree custody", &error)
                })?;
            let (record, answer) = step(read(&tx, tree_id)?.as_ref())?;
            if let Some(record) = record {
                write(&tx, tree_id, &record)?;
                tx.commit().map_err(|error| {
                    sqlite_unavailable("could not durably commit agent tree custody", &error)
                })?;
            }
            Ok(answer)
        })
    }
}

impl TreeCustody for SqliteTreeStore {
    fn custody_scope(&self) -> String {
        format!("sqlite:{}", self.custody_path().display())
    }

    fn liveness_dir(&self) -> Option<std::path::PathBuf> {
        self.custody_path()
            .parent()
            .map(|directory| directory.join("custody-hosts"))
    }

    fn custody(&self, tree_id: &str) -> Result<Option<CustodyRecord>, TreeStoreError> {
        self.with_connection(|connection| {
            let tx = connection.transaction().map_err(|error| {
                sqlite_unavailable("could not begin agent tree custody", &error)
            })?;
            read(&tx, tree_id)
        })
    }

    fn held_custody_instances(&self) -> Result<BTreeSet<HostInstance>, TreeStoreError> {
        self.with_connection(|connection| {
            let invalid = || TreeStoreError::corrupt("agent tree custody row is invalid");
            let mut statement = connection
                .prepare("SELECT DISTINCT host_instance FROM agent_tree_custody WHERE released=0")
                .map_err(|error| sqlite_unavailable("could not read agent tree custody", &error))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| sqlite_unavailable("could not read agent tree custody", &error))?;
            rows.map(|row| HostInstance::parse(&row.map_err(|_| invalid())?).map_err(|_| invalid()))
                .collect()
        })
    }

    fn take_custody(
        &self,
        tree_id: &str,
        host: &CustodyHost,
        takeover: Option<&HolderGone>,
    ) -> Result<CustodyClaim, CustodyError> {
        let now = now_ms();
        self.custody_step(tree_id, |current| {
            Ok(
                match CustodyRecord::take(current, tree_id, host, takeover, now)? {
                    CustodyTake::Held(claim) => (None, claim),
                    CustodyTake::Claimed { record, claim } => (Some(record), claim),
                },
            )
        })
    }

    fn renew_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
        let now = now_ms();
        self.custody_step(&claim.tree_id, |current| {
            Ok((Some(CustodyRecord::renew(current, claim, now)?), ()))
        })
    }

    fn release_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
        self.custody_step(&claim.tree_id, |current| {
            Ok((CustodyRecord::release(current, claim)?, ()))
        })
    }

    fn prove_custody(&self, claim: &CustodyClaim) -> Result<(), CustodyError> {
        let current = self.custody(&claim.tree_id)?;
        CustodyRecord::prove(current.as_ref(), claim).map(|_| ())
    }
}
