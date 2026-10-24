//! Checkpoint candidates, object rows, and child loads.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) struct Candidate {
    pub(super) reference: CheckpointObjectRef,
    pub(super) base: Option<RuntimeObjectAddress>,
    pub(super) checkpoint: Arc<RuntimeCheckpoint>,
    pub(super) objects: Vec<RuntimeObject>,
    pub(super) semantic_key: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct InsertEvidence {
    pub(super) objects: u64,
    pub(super) bytes: u64,
}

pub(super) fn candidate(b: &CheckpointObjectBundle) -> Result<Candidate, SessionStoreError> {
    let checkpoint = b.validated_checkpoint().map_err(bundle_error)?;
    let reference = bundle_ref(b);
    if checkpoint.manifest().topology().checkpoint_id != reference.checkpoint_id()
        || checkpoint.integrity() != reference.integrity()
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::CheckpointObjectBinding,
        ));
    }
    let objects = b.supplied_objects().to_vec();
    if objects.windows(2).any(|p| p[0].address() >= p[1].address()) {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::NoncanonicalCheckpointObjects,
        ));
    }
    let semantic_key = semantic_key(checkpoint.manifest(), &objects)?;
    Ok(Candidate {
        reference,
        base: b.base_root(),
        checkpoint,
        objects,
        semantic_key,
    })
}

pub(super) fn bundle_ref(b: &CheckpointObjectBundle) -> CheckpointObjectRef {
    CheckpointObjectRef::from_store(b.checkpoint_id(), b.root(), b.checkpoint_integrity())
}

pub(super) fn semantic_key(
    m: &RuntimeManifest,
    objects: &[RuntimeObject],
) -> Result<Vec<u8>, SessionStoreError> {
    let roots = objects
        .iter()
        .filter(|o| o.address().kind() == RuntimeObjectKind::ManifestRoot)
        .collect::<Vec<_>>();
    if roots.len() != 1
        || roots[0].children().first().map(|a| a.kind())
            != Some(RuntimeObjectKind::ManifestMetadata)
    {
        return Err(SessionStoreError::Invalid(
            StoreInvalidReason::ManifestRootDelta,
        ));
    }
    let mut out = b"plakous-store/semantic-key-v5".to_vec();
    out.push(durability_tag(m.durability())?);
    for a in roots[0].children().iter().skip(1) {
        encode_address(&mut out, *a);
    }
    Ok(out)
}

pub(super) fn insert_candidate(
    tx: &Transaction<'_>,
    x: &Candidate,
    limits: MemoryStoreLimits,
    admission: StoreAdmission,
) -> Result<InsertEvidence, SessionStoreError> {
    if checkpoint_row(tx, x.reference.checkpoint_id())?.is_some() {
        return Err(SessionStoreError::IdentityConflict);
    }
    if let Some(base) = x.base
        && !object_exists(tx, base)?
    {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingBase,
        ));
    }
    let supplied = x
        .objects
        .iter()
        .map(RuntimeObject::address)
        .collect::<BTreeSet<_>>();
    if !supplied.contains(&x.reference.root()) && !object_exists(tx, x.reference.root())? {
        return Err(SessionStoreError::Corrupt(
            plakous_agent::internal::StoreIntegrityReason::MissingRoot,
        ));
    }
    let mut missing = Vec::new();
    let mut added = 0_u64;
    for object in &x.objects {
        for child in object.children() {
            if !supplied.contains(child) && !object_exists(tx, *child)? {
                return Err(SessionStoreError::Corrupt(
                    plakous_agent::internal::StoreIntegrityReason::MissingChild,
                ));
            }
        }
        if object_exists(tx, object.address())? {
            if load_object(tx, object.address())?.as_ref() != Some(object) {
                return Err(SessionStoreError::corrupt());
            }
        } else {
            added = added
                .checked_add(u64n(object.retained_bytes()))
                .ok_or_else(|| {
                    SessionStoreError::capacity(StoreCapacityBound::ObjectBytes, u64::MAX, 0)
                })?;
            missing.push(object);
        }
    }
    let stats = read_stats(tx)?;
    let objects = stats
        .objects
        .checked_add(u64n(missing.len()))
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::Objects, u64::MAX, 0))?;
    let bytes = stats
        .object_bytes
        .checked_add(added)
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::ObjectBytes, u64::MAX, 0))?;
    let checkpoints = stats
        .checkpoints
        .checked_add(1)
        .ok_or_else(|| SessionStoreError::capacity(StoreCapacityBound::Checkpoints, u64::MAX, 0))?;
    admit(
        StoreCapacityBound::Objects,
        objects,
        limits.max_objects().get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::ObjectBytes,
        bytes,
        limits.max_object_bytes().get(),
        admission,
    )?;
    admit(
        StoreCapacityBound::Checkpoints,
        checkpoints,
        limits.max_checkpoints().get(),
        admission,
    )?;
    let missing_count = u64n(missing.len());
    for object in &missing {
        insert_object_row(tx, object)?;
    }
    for object in missing {
        insert_object_edges(tx, object)?;
    }
    insert_checkpoint(tx, x)?;
    tx.execute(
        "UPDATE store_meta SET checkpoints=?1,objects=?2,object_bytes=?3 WHERE id=1",
        params![i64n(checkpoints)?, i64n(objects)?, i64n(bytes)?],
    )
    .map_err(map_sqlite)?;
    Ok(InsertEvidence {
        objects: missing_count,
        bytes: added,
    })
}

fn insert_object_row(tx: &Transaction<'_>, o: &RuntimeObject) -> Result<(), SessionStoreError> {
    let a = o.address();
    tx.execute(
        "INSERT INTO runtime_objects(kind,codec,digest,byte_len) VALUES(?1,?2,?3,?4)",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice(),
            i64n(u64n(o.canonical_bytes().len()))?
        ],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO runtime_object_bytes(id,bytes) VALUES(?1,?2)",
        params![tx.last_insert_rowid(), o.canonical_bytes()],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

fn insert_object_edges(tx: &Transaction<'_>, o: &RuntimeObject) -> Result<(), SessionStoreError> {
    let a = o.address();
    for (p, c) in o.children().iter().enumerate() {
        tx.execute(
            "INSERT INTO runtime_object_edges VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                kind_tag(a.kind()),
                i64::from(a.codec_revision()),
                a.digest().bytes().as_slice(),
                i64n(u64n(p))?,
                kind_tag(c.kind()),
                i64::from(c.codec_revision()),
                c.digest().bytes().as_slice()
            ],
        )
        .map_err(map_sqlite)?;
    }
    Ok(())
}

fn insert_checkpoint(tx: &Transaction<'_>, x: &Candidate) -> Result<(), SessionStoreError> {
    let m = x.checkpoint.manifest();
    let t = m.topology();
    let r = x.reference.root();
    let integrity = x.reference.integrity();
    let commit = x.checkpoint.checkpoint_commit_identity();
    let fork = t.fork;
    let base = x.base;
    let checkpoint_row = CheckpointRow {
        reference: x.reference,
        base,
        commit,
        selector: SessionRef::new(t.session_id, t.branch_id),
        revision: BranchRevision::from_store(t.branch_revision),
        parent: t.parent_checkpoint_id,
        lineage: fork,
        fence: t.lease_fence,
        kind: x.checkpoint.kind(),
        durability: m.durability(),
        key: x.semantic_key.clone(),
        validation_seal: Vec::new(),
    };
    let validation_seal = checkpoint_validation_seal(&checkpoint_row)?;
    tx.execute(
        "INSERT INTO checkpoints VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,
         ?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24)",
        params![
            id(x.reference.checkpoint_id()),
            kind_tag(r.kind()),
            i64::from(r.codec_revision()),
            r.digest().bytes().as_slice(),
            i64::from(integrity.version()),
            integrity.bytes().as_slice(),
            i64::from(commit.version()),
            commit.bytes().as_slice(),
            base.map(|a| kind_tag(a.kind())),
            base.map(|a| i64::from(a.codec_revision())),
            base.map(|a| a.digest().bytes().to_vec()),
            id(t.session_id),
            id(t.branch_id),
            u64b(t.branch_revision),
            t.parent_checkpoint_id.map(id),
            fork.map(|v| id(v.session_id)),
            fork.map(|v| id(v.branch_id)),
            fork.map(|v| id(v.checkpoint_id)),
            fork.map(|v| u64b(v.branch_revision)),
            u64b(t.lease_fence),
            checkpoint_kind_tag(x.checkpoint.kind()),
            durability_tag(m.durability())?,
            &x.semantic_key,
            validation_seal
        ],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

#[derive(Clone)]
pub(super) struct CheckpointRow {
    pub(super) reference: CheckpointObjectRef,
    pub(super) base: Option<RuntimeObjectAddress>,
    pub(super) commit: Digest,
    pub(super) selector: SessionRef,
    pub(super) revision: BranchRevision,
    pub(super) parent: Option<CheckpointId>,
    pub(super) lineage: Option<ForkLineage>,
    pub(super) fence: u64,
    pub(super) kind: RuntimeCheckpointKind,
    pub(super) durability: RuntimeDurability,
    pub(super) key: Vec<u8>,
    pub(super) validation_seal: Vec<u8>,
}

fn checkpoint_validation_seal(row: &CheckpointRow) -> Result<Vec<u8>, SessionStoreError> {
    let mut selector = text_bytes(row.selector.session_id());
    selector.extend_from_slice(&text_bytes(row.selector.branch_id()));
    let mut base = vec![u8::from(row.base.is_some())];
    if let Some(address) = row.base {
        encode_address(&mut base, address);
    }
    let mut parent = vec![u8::from(row.parent.is_some())];
    if let Some(checkpoint_id) = row.parent {
        parent.extend_from_slice(&text_bytes(checkpoint_id));
    }
    let mut lineage = vec![u8::from(row.lineage.is_some())];
    if let Some(fork) = row.lineage {
        lineage.extend_from_slice(&text_bytes(fork.session_id));
        lineage.extend_from_slice(&text_bytes(fork.branch_id));
        lineage.extend_from_slice(&text_bytes(fork.checkpoint_id));
        lineage.extend_from_slice(&fork.branch_revision.to_be_bytes());
    }
    receipt_integrity(
        b"checkpoint-row-v1",
        &[
            checkpoint_ref_bytes(row.reference),
            base,
            row.commit.identity_bytes(),
            selector,
            row.revision.get().to_be_bytes().to_vec(),
            parent,
            lineage,
            row.fence.to_be_bytes().to_vec(),
            checkpoint_kind_tag(row.kind).to_be_bytes().to_vec(),
            vec![durability_tag(row.durability)?],
            row.key.clone(),
        ],
    )
}

pub(super) fn verify_checkpoint_validation_seal(
    row: &CheckpointRow,
) -> Result<(), SessionStoreError> {
    if checkpoint_validation_seal(row)? != row.validation_seal {
        return Err(SessionStoreError::corrupt());
    }
    Ok(())
}

pub(super) fn checkpoint_row(
    c: &Connection,
    idv: CheckpointId,
) -> Result<Option<CheckpointRow>, SessionStoreError> {
    c.query_row(
        "SELECT root_kind,root_codec,root_digest,integrity_version,integrity,
        commit_version,commit_digest,session_id,branch_id,revision,parent_id,fork_session,
        fork_branch,fork_checkpoint,fork_revision,fence,kind,durability,semantic_key,
        base_kind,base_codec,base_digest,validation_seal
        FROM checkpoints WHERE checkpoint_id=?1",
        [id(idv)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Vec<u8>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, Vec<u8>>(9)?,
                r.get::<_, Option<String>>(10)?,
                r.get::<_, Option<String>>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, Option<String>>(13)?,
                r.get::<_, Option<Vec<u8>>>(14)?,
                r.get::<_, Vec<u8>>(15)?,
                r.get::<_, i64>(16)?,
                r.get::<_, i64>(17)?,
                r.get::<_, Vec<u8>>(18)?,
                r.get::<_, Option<i64>>(19)?,
                r.get::<_, Option<i64>>(20)?,
                r.get::<_, Option<Vec<u8>>>(21)?,
                r.get::<_, Option<Vec<u8>>>(22)?,
            ))
        },
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|v| {
        let root = decode_address(v.0, v.1, v.2)?;
        let base = match (v.19, v.20, v.21) {
            (None, None, None) => None,
            (Some(kind), Some(codec), Some(digest)) => Some(decode_address(kind, codec, digest)?),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let row = CheckpointRow {
            reference: CheckpointObjectRef::from_store(
                idv,
                root,
                decode_digest(DigestDomain::Blob, v.3, v.4)?,
            ),
            base,
            commit: decode_digest(DigestDomain::ProviderOperation, v.5, v.6)?,
            selector: SessionRef::new(parse(&v.7)?, parse(&v.8)?),
            revision: BranchRevision::from_store(read_u64(&v.9)?),
            parent: v.10.as_deref().map(parse).transpose()?,
            lineage: lineage(
                v.11.as_deref(),
                v.12.as_deref(),
                v.13.as_deref(),
                v.14.as_deref(),
            )?,
            fence: read_u64(&v.15)?,
            kind: decode_checkpoint_kind(v.16)?,
            durability: decode_durability(v.17)?,
            key: v.18,
            validation_seal: v.22.unwrap_or_default(),
        };
        verify_checkpoint_validation_seal(&row)?;
        Ok(row)
    })
    .transpose()
}

pub(super) fn load_object(
    c: &Connection,
    a: RuntimeObjectAddress,
) -> Result<Option<RuntimeObject>, SessionStoreError> {
    c.query_row(
        "SELECT p.bytes FROM runtime_objects o JOIN runtime_object_bytes p USING(id) WHERE kind=?1 AND codec=?2 AND digest=?3",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ],
        |r| r.get::<_, Vec<u8>>(0),
    )
    .optional()
    .map_err(map_sqlite)?
    .map(|b| RuntimeObject::from_store(a, b, load_children(c, a)?).map_err(bundle_error))
    .transpose()
}

fn object_exists(c: &Connection, a: RuntimeObjectAddress) -> Result<bool, SessionStoreError> {
    Ok(c.query_row(
        "SELECT 1 FROM runtime_objects WHERE kind=?1 AND codec=?2 AND digest=?3",
        params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ],
        |_| Ok(()),
    )
    .optional()
    .map_err(map_sqlite)?
    .is_some())
}

fn load_children(
    c: &Connection,
    a: RuntimeObjectAddress,
) -> Result<Vec<RuntimeObjectAddress>, SessionStoreError> {
    let mut s = c
        .prepare(
            "SELECT position,child_kind,child_codec,child_digest FROM runtime_object_edges
        WHERE parent_kind=?1 AND parent_codec=?2 AND parent_digest=?3 ORDER BY position",
        )
        .map_err(map_sqlite)?;
    let mut rows = s
        .query(params![
            kind_tag(a.kind()),
            i64::from(a.codec_revision()),
            a.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    let mut out = Vec::new();
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        if r.get::<_, i64>(0).map_err(map_sqlite)? != i64n(u64n(out.len()))? {
            return Err(SessionStoreError::corrupt());
        }
        out.push(decode_address(
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
            r.get(3).map_err(map_sqlite)?,
        )?);
    }
    Ok(out)
}

pub(super) fn load_all_children(
    c: &Connection,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT parent_kind,parent_codec,parent_digest,position,
             child_kind,child_codec,child_digest FROM runtime_object_edges
             ORDER BY parent_kind,parent_codec,parent_digest,position",
        )
        .map_err(map_sqlite)?;
    let rows = statement.query([]).map_err(map_sqlite)?;
    collect_children(rows)
}

fn collect_children(
    mut rows: rusqlite::Rows<'_>,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut children: BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>> = BTreeMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let parent = decode_address(
            row.get(0).map_err(map_sqlite)?,
            row.get(1).map_err(map_sqlite)?,
            row.get(2).map_err(map_sqlite)?,
        )?;
        let values = children.entry(parent).or_default();
        if row.get::<_, i64>(3).map_err(map_sqlite)? != i64n(u64n(values.len()))? {
            return Err(SessionStoreError::corrupt());
        }
        values.push(decode_address(
            row.get(4).map_err(map_sqlite)?,
            row.get(5).map_err(map_sqlite)?,
            row.get(6).map_err(map_sqlite)?,
        )?);
    }
    Ok(children)
}

pub(super) fn load_reachable_children(
    c: &Connection,
    root: RuntimeObjectAddress,
) -> Result<BTreeMap<RuntimeObjectAddress, Vec<RuntimeObjectAddress>>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "WITH RECURSIVE reach(kind,codec,digest) AS(
             VALUES(?1,?2,?3) UNION SELECT e.child_kind,e.child_codec,e.child_digest
             FROM runtime_object_edges e JOIN reach p ON e.parent_kind=p.kind
             AND e.parent_codec=p.codec AND e.parent_digest=p.digest)
             SELECT e.parent_kind,e.parent_codec,e.parent_digest,e.position,
             e.child_kind,e.child_codec,e.child_digest FROM reach
             JOIN runtime_object_edges e ON e.parent_kind=reach.kind
             AND e.parent_codec=reach.codec AND e.parent_digest=reach.digest
             ORDER BY e.parent_kind,e.parent_codec,e.parent_digest,e.position",
        )
        .map_err(map_sqlite)?;
    let rows = statement
        .query(params![
            kind_tag(root.kind()),
            i64::from(root.codec_revision()),
            root.digest().bytes().as_slice()
        ])
        .map_err(map_sqlite)?;
    collect_children(rows)
}
