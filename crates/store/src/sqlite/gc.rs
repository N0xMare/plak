#[cfg(test)]
use super::test_crash_cut;
use super::{
    Connection, OptionalExtension, SessionStoreError, StoreReclaimReport, TransactionBehavior,
    exactly_one, i64n, map_sqlite, nonnegative, params,
};

pub(super) fn pending(c: &Connection) -> Result<bool, SessionStoreError> {
    c.query_row("SELECT EXISTS(SELECT 1 FROM gc_candidates)", [], |r| {
        r.get(0)
    })
    .map_err(map_sqlite)
}

pub(super) fn step(
    c: &mut Connection,
    object_budget: usize,
    byte_budget: u64,
) -> Result<StoreReclaimReport, SessionStoreError> {
    let tx = c
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let mut objects = 0_u64;
    let mut bytes = 0_u64;
    for _ in 0..object_budget {
        let candidate = tx
            .query_row(
                "SELECT q.kind,q.codec,q.digest,o.id,o.byte_len FROM gc_candidates q
             JOIN runtime_objects o USING(kind,codec,digest)
             ORDER BY q.kind,q.codec,q.digest LIMIT 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, Vec<u8>>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(map_sqlite)?;
        let Some((kind, codec, digest, id, byte_len)) = candidate else {
            break;
        };
        let address = params![kind, codec, digest];
        let referenced: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM checkpoints WHERE root_kind=?1 AND root_codec=?2 AND root_digest=?3)
             OR EXISTS(SELECT 1 FROM runtime_object_edges WHERE child_kind=?1 AND child_codec=?2 AND child_digest=?3)",
            address, |r| r.get(0),
        ).map_err(map_sqlite)?;
        let edges: i64 = if referenced {
            0
        } else {
            tx.query_row("SELECT count(*) FROM runtime_object_edges WHERE parent_kind=?1 AND parent_codec=?2 AND parent_digest=?3", address, |r| r.get(0)).map_err(map_sqlite)?
        };
        let weight = nonnegative(byte_len)?
            .checked_add(
                nonnegative(edges)?
                    .checked_mul(37)
                    .ok_or(SessionStoreError::corrupt())?,
            )
            .ok_or(SessionStoreError::corrupt())?;
        // One oversized object must make progress; every later object fits the batch.
        if !referenced && objects > 0 && weight > byte_budget.saturating_sub(bytes) {
            break;
        }
        tx.execute(
            "DELETE FROM gc_candidates WHERE kind=?1 AND codec=?2 AND digest=?3",
            address,
        )
        .map_err(map_sqlite)?;
        if referenced {
            continue;
        }
        tx.execute("INSERT OR IGNORE INTO gc_candidates SELECT child_kind,child_codec,child_digest FROM runtime_object_edges WHERE parent_kind=?1 AND parent_codec=?2 AND parent_digest=?3", address).map_err(map_sqlite)?;
        tx.execute("DELETE FROM runtime_object_edges WHERE parent_kind=?1 AND parent_codec=?2 AND parent_digest=?3", address).map_err(map_sqlite)?;
        exactly_one(
            tx.execute("DELETE FROM runtime_object_bytes WHERE id=?1", [id])
                .map_err(map_sqlite)?,
        )?;
        exactly_one(
            tx.execute("DELETE FROM runtime_objects WHERE id=?1", [id])
                .map_err(map_sqlite)?,
        )?;
        objects += 1;
        bytes = bytes
            .checked_add(weight)
            .ok_or(SessionStoreError::corrupt())?;
        if bytes >= byte_budget {
            break;
        }
    }
    tx.execute("UPDATE store_meta SET objects=objects-?1,object_bytes=object_bytes-?2,
                reclaimed_objects=reclaimed_objects+?1,reclaimed_object_bytes=reclaimed_object_bytes+?2 WHERE id=1",
        params![i64n(objects)?, i64n(bytes)?]).map_err(map_sqlite)?;
    let pending = pending(&tx)?;
    #[cfg(test)]
    test_crash_cut("gc-before-commit");
    tx.commit().map_err(map_sqlite)?;
    #[cfg(test)]
    test_crash_cut("gc-after-commit");
    Ok(StoreReclaimReport::from_store(objects, bytes, pending))
}
