//! Native semantic seeds. These are not another cross-build carrier.
use plakous_recursive_subagents::tree::host_move::DormantTree;
use plakous_recursive_subagents::tree::{CustodyRecord, PersistedTree, TreeStoreError};
use rusqlite::{OptionalExtension as _, Transaction, TransactionBehavior, params};

use crate::SqliteTreeStore;

pub(crate) fn install(
    store: &SqliteTreeStore,
    operation: &str,
    digest: &str,
    seed: &DormantTree,
    source: &CustodyRecord,
) -> Result<(), TreeStoreError> {
    let rows = seed.installed_rows().map_err(TreeStoreError::corrupt)?;
    let seed_digest = seed.digest().map_err(TreeStoreError::corrupt)?;
    let text = serde_json::to_string(seed)
        .map_err(|_| TreeStoreError::corrupt("invalid semantic tree base"))?;
    if operation.is_empty() || digest.len() != 64 || source.fence >= i64::MAX.cast_unsigned() {
        return Err(TreeStoreError::corrupt(
            "invalid semantic tree install identity or fence",
        ));
    }
    store.with_connection(|connection| {
        let unavailable = |_| TreeStoreError::unavailable("could not commit semantic tree seed");
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
        let previous: Option<(String, String, String)> = tx.query_row(
            "SELECT operation,capsule_digest,seed_digest FROM agent_move_seeds WHERE tree_id=?1", [seed.tree_id()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(unavailable)?;
        if let Some(previous) = previous {
            return if previous == (operation.to_owned(), digest.to_owned(), seed_digest) { Ok(()) }
                else { Err(TreeStoreError::corrupt("semantic tree install conflicts with its reservation")) };
        }
        let used: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM agent_trees WHERE tree_id=?1)", [seed.tree_id()], |row| row.get(0)).map_err(unavailable)?;
        if used || crate::custody::read(&tx, seed.tree_id())?.is_some() {
            return Err(TreeStoreError::corrupt("semantic tree identity is already in use"));
        }
        crate::rows::insert_agent_tree_rows(&tx, rows.rows())?;
        let mut floor = source.clone();
        floor.released = true;
        crate::custody::write(&tx, seed.tree_id(), &floor)?;
        tx.execute("INSERT INTO agent_move_seeds VALUES(?1,?2,?3,?4,?5)", params![seed.tree_id(), operation, digest, seed_digest, text]).map_err(unavailable)?;
        tx.commit().map_err(unavailable)
    })
}

pub(crate) fn attach(
    tx: &Transaction<'_>,
    tree_id: &str,
    rows: PersistedTree,
) -> Result<PersistedTree, String> {
    let seed: Option<(String, String)> = tx
        .query_row(
            "SELECT seed,seed_digest FROM agent_move_seeds WHERE tree_id=?1",
            [tree_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "could not read semantic tree base")?;
    let Some((text, digest)) = seed else {
        return Ok(rows);
    };
    let seed: DormantTree =
        serde_json::from_str(&text).map_err(|_| "invalid semantic tree base")?;
    if seed.digest()? != digest {
        return Err("semantic tree base digest changed".into());
    }
    rows.with_move_base(seed)
}
