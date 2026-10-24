//! Branch and checkpoint reads.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) fn load_branch(
    c: &Connection,
    s: SessionRef,
) -> Result<LoadedBranch, SessionStoreError> {
    let session = live_session(c, s.session_id())?;
    let row = branch_row(c, s)?.ok_or(SessionStoreError::NotFound)?;
    let checkpoint = load_checkpoint(c, row.head.checkpoint().checkpoint_id())?;
    if checkpoint.reference() != row.head.checkpoint() {
        return Err(SessionStoreError::corrupt());
    }
    with_loaded_horizon(
        c,
        s,
        LoadedBranch::from_store(session, row.head, checkpoint),
    )
}

pub(super) fn with_loaded_horizon(
    c: &Connection,
    selector: SessionRef,
    loaded: LoadedBranch,
) -> Result<LoadedBranch, SessionStoreError> {
    Ok(match load_horizon(c, selector)? {
        Some(horizon) => loaded.with_horizon(horizon.revision),
        None => loaded,
    })
}

pub(super) fn load_checkpoint(
    c: &Connection,
    idv: CheckpointId,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let Some(row) = checkpoint_row(c, idv)? else {
        return Err(if pruned_checkpoint(c, idv)? {
            SessionStoreError::Pruned
        } else {
            SessionStoreError::NotFound
        });
    };
    load_checkpoint_contents(c, idv, &row)
}

pub(super) fn load_checkpoint_contents(
    c: &Connection,
    idv: CheckpointId,
    row: &CheckpointRow,
) -> Result<StoredCheckpoint, SessionStoreError> {
    let root = row.reference.root();
    let mut children = load_reachable_children(c, root)?;
    let mut s=c.prepare("WITH RECURSIVE reach(kind,codec,digest) AS(
        VALUES(?1,?2,?3) UNION SELECT e.child_kind,e.child_codec,e.child_digest
        FROM runtime_object_edges e JOIN reach p ON e.parent_kind=p.kind
        AND e.parent_codec=p.codec AND e.parent_digest=p.digest)
        SELECT o.kind,o.codec,o.digest,p.bytes FROM reach JOIN runtime_objects o USING(kind,codec,digest) JOIN runtime_object_bytes p USING(id)
        ORDER BY o.kind,o.codec,o.digest").map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            kind_tag(root.kind()),
            i64::from(root.codec_revision()),
            root.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    let mut objects = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let a = decode_address(
            r.get(0).map_err(map_sqlite)?,
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
        )?;
        objects.push(
            RuntimeObject::from_store(
                a,
                r.get(3).map_err(map_sqlite)?,
                children.remove(&a).unwrap_or_default(),
            )
            .map_err(bundle_error)?,
        );
    }
    if !children.is_empty() {
        return Err(SessionStoreError::corrupt());
    }
    let bundle = CheckpointObjectBundle::from_store(idv, row.reference.integrity(), root, objects)
        .map_err(bundle_error)?;
    let checkpoint = bundle.validated_checkpoint().map_err(bundle_error)?;
    let t = checkpoint.manifest().topology();
    let key = semantic_key(checkpoint.manifest(), bundle.supplied_objects())?;
    // A parent reclaimed by retention leaves the sealed base as the only
    // record of what this checkpoint was built on.
    let expected_base = match row.parent {
        Some(parent) => match checkpoint_row(c, parent)? {
            Some(parent) => Some(parent.reference.root()),
            None if pruned_checkpoint(c, parent)? => row.base,
            None => return Err(SessionStoreError::corrupt()),
        },
        None => None,
    };
    if t.session_id != row.selector.session_id()
        || t.branch_id != row.selector.branch_id()
        || t.branch_revision != row.revision.get()
        || t.parent_checkpoint_id != row.parent
        || t.fork != row.lineage
        || t.lease_fence != row.fence
        || checkpoint.kind() != row.kind
        || checkpoint.manifest().durability() != row.durability
        || checkpoint.checkpoint_commit_identity() != row.commit
        || key != row.key
        || row.base != expected_base
    {
        return Err(SessionStoreError::corrupt());
    }
    StoredCheckpoint::from_store(row.reference, bundle).map_err(bundle_error)
}

pub(super) fn list(
    c: &Connection,
    limits: MemoryStoreLimits,
    q: SessionQuery,
) -> Result<Page<SessionSummary>, SessionStoreError> {
    if q.limit().get() > limits.max_page_size().get() {
        return Err(SessionStoreError::capacity(
            StoreCapacityBound::PageSize,
            u64n(q.limit().get()),
            u64n(limits.max_page_size().get()),
        ));
    }
    let mut s = c
        .prepare(
            "SELECT session_id,initial_branch,revision,created_s,created_ns,durability,
        archived,(SELECT count(*) FROM branches b WHERE b.session_id=sessions.session_id)
        FROM sessions WHERE (?1 IS NULL OR session_id>?1) AND (?2=1 OR archived=0)
        ORDER BY session_id LIMIT ?3",
        )
        .map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            q.after_session().map(id),
            i64::from(q.includes_archived()),
            i64n(u64n(q.limit().get()) + 1)?
        ])
        .map_err(map_sqlite)?;
    let mut items = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let sid = parse(&r.get::<_, String>(0).map_err(map_sqlite)?)?;
        let h = SessionHead::from_store(
            sid,
            parse(&r.get::<_, String>(1).map_err(map_sqlite)?)?,
            SessionRevision::from_store(read_u64(&r.get::<_, Vec<u8>>(2).map_err(map_sqlite)?)?),
            timestamp(
                &r.get::<_, Vec<u8>>(3).map_err(map_sqlite)?,
                r.get(4).map_err(map_sqlite)?,
            )?,
            decode_durability(r.get(5).map_err(map_sqlite)?)?,
            boolv(r.get(6).map_err(map_sqlite)?)?,
        );
        items.push(SessionSummary::from_store(
            h,
            nonnegative(r.get(7).map_err(map_sqlite)?)?,
        ));
    }
    let more = items.len() > q.limit().get();
    if more {
        items.pop();
    }
    let next = if more {
        items.last().map(|x| x.head().session_id())
    } else {
        None
    };
    Ok(Page::from_store(items, next))
}
