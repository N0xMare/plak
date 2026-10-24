//! Open-time authority snapshot.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) struct AuthoritySnapshot {
    pub(super) checkpoints: BTreeMap<CheckpointId, CheckpointRow>,
    pub(super) branches: BTreeMap<SessionRef, BranchRow>,
}

impl AuthoritySnapshot {
    pub(super) fn load(c: &Connection) -> Result<Self, SessionStoreError> {
        let sessions = load_authority_sessions(c)?;
        let checkpoints = load_authority_checkpoint_rows(c)?;
        for checkpoint in checkpoints.values() {
            verify_checkpoint_validation_seal(checkpoint)?;
        }
        let branches = load_authority_branches(c, &checkpoints)?;
        load_authority_create_receipts(c, &sessions, &checkpoints, &branches)?;
        load_authority_commit_receipts(c, &checkpoints)?;
        load_authority_fork_receipts(c, &sessions, &checkpoints)?;
        load_authority_archive_receipts(c, &sessions)?;
        load_authority_delete_receipts(c, &sessions)?;
        load_authority_horizons(c, &checkpoints, &branches)?;
        Ok(Self {
            checkpoints,
            branches,
        })
    }

    pub(super) fn verify(&self) -> Result<(), SessionStoreError> {
        for (selector, branch) in &self.branches {
            let checkpoint = self
                .checkpoints
                .get(&branch.head.checkpoint().checkpoint_id())
                .ok_or(SessionStoreError::corrupt())?;
            if checkpoint.reference != branch.head.checkpoint()
                || checkpoint.selector != *selector
                || checkpoint.revision != branch.head.revision()
                || checkpoint.lineage != branch.lineage
                || checkpoint.fence > branch.last_fence
                || branch
                    .active
                    .as_ref()
                    .is_some_and(|lease| lease.fence().get() != branch.last_fence)
            {
                return Err(SessionStoreError::corrupt());
            }
        }
        Ok(())
    }
}

fn load_authority_sessions(
    c: &Connection,
) -> Result<BTreeMap<SessionId, SessionHead>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,revision,created_s,created_ns,durability,archived
         FROM sessions ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    let mut sessions = BTreeMap::new();
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let head = SessionHead::from_store(
            session_id,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?)?),
            timestamp(
                &row.get::<_, Vec<u8>>(3).map_err(map_sqlite)?,
                row.get(4).map_err(map_sqlite)?,
            )?,
            decode_durability(row.get(5).map_err(map_sqlite)?)?,
            boolv(row.get(6).map_err(map_sqlite)?)?,
        );
        if sessions.insert(session_id, head).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(sessions)
}

fn load_authority_checkpoint_rows(
    c: &Connection,
) -> Result<BTreeMap<CheckpointId, CheckpointRow>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT checkpoint_id,root_kind,root_codec,root_digest,integrity_version,integrity,
         commit_version,commit_digest,session_id,branch_id,revision,parent_id,fork_session,
         fork_branch,fork_checkpoint,fork_revision,fence,kind,durability,semantic_key,
         base_kind,base_codec,base_digest,validation_seal FROM checkpoints ORDER BY checkpoint_id",
        )
        .map_err(map_sqlite)?;
    let mut records = statement.query([]).map_err(map_sqlite)?;
    let mut rows = BTreeMap::new();
    while let Some(record) = records.next().map_err(map_sqlite)? {
        let checkpoint_id = parse(&record.get::<_, String>(0).map_err(map_sqlite)?)?;
        let root = decode_address(
            record.get(1).map_err(map_sqlite)?,
            record.get(2).map_err(map_sqlite)?,
            record.get(3).map_err(map_sqlite)?,
        )?;
        let base = match (
            record.get::<_, Option<i64>>(20).map_err(map_sqlite)?,
            record.get::<_, Option<i64>>(21).map_err(map_sqlite)?,
            record.get::<_, Option<Vec<u8>>>(22).map_err(map_sqlite)?,
        ) {
            (None, None, None) => None,
            (Some(kind), Some(codec), Some(digest)) => Some(decode_address(kind, codec, digest)?),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let fork_session = record.get::<_, Option<String>>(12).map_err(map_sqlite)?;
        let fork_branch = record.get::<_, Option<String>>(13).map_err(map_sqlite)?;
        let fork_checkpoint = record.get::<_, Option<String>>(14).map_err(map_sqlite)?;
        let fork_revision = record.get::<_, Option<Vec<u8>>>(15).map_err(map_sqlite)?;
        let row = CheckpointRow {
            reference: CheckpointObjectRef::from_store(
                checkpoint_id,
                root,
                decode_digest(
                    DigestDomain::Blob,
                    record.get(4).map_err(map_sqlite)?,
                    record.get(5).map_err(map_sqlite)?,
                )?,
            ),
            base,
            commit: decode_digest(
                DigestDomain::ProviderOperation,
                record.get(6).map_err(map_sqlite)?,
                record.get(7).map_err(map_sqlite)?,
            )?,
            selector: SessionRef::new(
                parse(&record.get::<_, String>(8).map_err(map_sqlite)?)?,
                parse(&record.get::<_, String>(9).map_err(map_sqlite)?)?,
            ),
            revision: BranchRevision::from_store(read_u64(
                &record.get::<_, Vec<u8>>(10).map_err(map_sqlite)?,
            )?),
            parent: record
                .get::<_, Option<String>>(11)
                .map_err(map_sqlite)?
                .as_deref()
                .map(parse)
                .transpose()?,
            lineage: lineage(
                fork_session.as_deref(),
                fork_branch.as_deref(),
                fork_checkpoint.as_deref(),
                fork_revision.as_deref(),
            )?,
            fence: read_u64(&record.get::<_, Vec<u8>>(16).map_err(map_sqlite)?)?,
            kind: decode_checkpoint_kind(record.get(17).map_err(map_sqlite)?)?,
            durability: decode_durability(record.get(18).map_err(map_sqlite)?)?,
            key: record.get(19).map_err(map_sqlite)?,
            validation_seal: record
                .get::<_, Option<Vec<u8>>>(23)
                .map_err(map_sqlite)?
                .unwrap_or_default(),
        };
        if rows.insert(checkpoint_id, row).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(rows)
}

fn load_authority_branches(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<BTreeMap<SessionRef, BranchRow>, SessionStoreError> {
    let mut statement = c.prepare(
        "SELECT session_id,branch_id,revision,checkpoint_id,last_fence,fork_session,fork_branch,
         fork_checkpoint,fork_revision,active_owner,active_fence,active_time,active_ns,
         active_duration,previous_time,previous_ns,released_owner,released_fence,released_time,
         released_ns,released_duration FROM branches ORDER BY session_id,branch_id",
    ).map_err(map_sqlite)?;
    let mut records = statement.query([]).map_err(map_sqlite)?;
    let mut branches = BTreeMap::new();
    while let Some(record) = records.next().map_err(map_sqlite)? {
        let selector = SessionRef::new(
            parse(&record.get::<_, String>(0).map_err(map_sqlite)?)?,
            parse(&record.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let checkpoint_id = parse(&record.get::<_, String>(3).map_err(map_sqlite)?)?;
        let checkpoint = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        let active_owner = record.get::<_, Option<String>>(9).map_err(map_sqlite)?;
        let active_fence = record.get::<_, Option<Vec<u8>>>(10).map_err(map_sqlite)?;
        let active_seconds = record.get::<_, Option<Vec<u8>>>(11).map_err(map_sqlite)?;
        let active_nanos = record.get::<_, Option<i64>>(12).map_err(map_sqlite)?;
        let active_duration = record.get::<_, Option<Vec<u8>>>(13).map_err(map_sqlite)?;
        let active = lease(
            selector,
            active_owner.as_deref(),
            active_fence.as_deref(),
            active_seconds.as_deref(),
            active_nanos,
            active_duration.as_deref(),
        )?;
        let previous_seconds = record.get::<_, Option<Vec<u8>>>(14).map_err(map_sqlite)?;
        let previous_nanos = record.get::<_, Option<i64>>(15).map_err(map_sqlite)?;
        let previous = match (&previous_seconds, previous_nanos, &active) {
            (None, None, _) => None,
            (Some(seconds), Some(nanos), Some(active)) => Some(BranchLease::from_store(
                selector,
                active.owner(),
                active.fence(),
                lease_time(seconds, nanos)?,
                active.lease_seconds(),
            )),
            _ => return Err(SessionStoreError::corrupt()),
        };
        let released_owner = record.get::<_, Option<String>>(16).map_err(map_sqlite)?;
        let released_fence = record.get::<_, Option<Vec<u8>>>(17).map_err(map_sqlite)?;
        let released_seconds = record.get::<_, Option<Vec<u8>>>(18).map_err(map_sqlite)?;
        let released_nanos = record.get::<_, Option<i64>>(19).map_err(map_sqlite)?;
        let released_duration = record.get::<_, Option<Vec<u8>>>(20).map_err(map_sqlite)?;
        let released = lease(
            selector,
            released_owner.as_deref(),
            released_fence.as_deref(),
            released_seconds.as_deref(),
            released_nanos,
            released_duration.as_deref(),
        )?;
        if active.is_some() && released.is_some() {
            return Err(SessionStoreError::corrupt());
        }
        let fork_session = record.get::<_, Option<String>>(5).map_err(map_sqlite)?;
        let fork_branch = record.get::<_, Option<String>>(6).map_err(map_sqlite)?;
        let fork_checkpoint = record.get::<_, Option<String>>(7).map_err(map_sqlite)?;
        let fork_revision = record.get::<_, Option<Vec<u8>>>(8).map_err(map_sqlite)?;
        let branch = BranchRow {
            head: BranchHead::from_store(
                selector,
                BranchRevision::from_store(read_u64(
                    &record.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
                )?),
                checkpoint.reference,
            ),
            last_fence: read_u64(&record.get::<_, Vec<u8>>(4).map_err(map_sqlite)?)?,
            lineage: lineage(
                fork_session.as_deref(),
                fork_branch.as_deref(),
                fork_checkpoint.as_deref(),
                fork_revision.as_deref(),
            )?,
            active,
            previous,
            released,
        };
        if branches.insert(selector, branch).is_some() {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(branches)
}

fn load_authority_create_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
    branches: &BTreeMap<SessionRef, BranchRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,branch_id,created_s,created_ns,durability,owner,duration,
         checkpoint_id,fence,expires_time,expires_ns,receipt_integrity,feed_seq
         FROM create_receipts ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let selector = SessionRef::new(
            session_id,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let created = timestamp(
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(4).map_err(map_sqlite)?)?;
        let duration = nz(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?;
        let checkpoint_id = parse(&row.get::<_, String>(7).map_err(map_sqlite)?)?;
        let checkpoint = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        let lease = BranchLease::from_store(
            selector,
            parse(&row.get::<_, String>(5).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(8).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(9).map_err(map_sqlite)?,
                row.get(10).map_err(map_sqlite)?,
            )?,
            duration,
        );
        let persisted_session = sessions
            .get(&session_id)
            .ok_or(SessionStoreError::corrupt())?;
        let persisted_branch = branches
            .get(&selector)
            .ok_or(SessionStoreError::corrupt())?;
        if checkpoint.selector != selector
            || checkpoint.revision != BranchRevision::INITIAL
            || checkpoint.parent.is_some()
            || checkpoint.lineage.is_some()
            || checkpoint.fence != lease.fence().get()
            || checkpoint.kind != RuntimeCheckpointKind::Quiescent
            || checkpoint.durability != durability
            || persisted_session.initial_branch() != selector.branch_id()
            || persisted_session.created_at() != created
            || persisted_session.durability() != durability
            || persisted_branch.lineage.is_some()
        {
            return Err(SessionStoreError::corrupt());
        }
        let receipt = CreateReceipt::from_store(
            CreateOperation::from_store(
                selector,
                created,
                lease.owner(),
                duration,
                checkpoint.reference,
                checkpoint.commit,
            ),
            SessionHead::from_store(
                session_id,
                selector.branch_id(),
                SessionRevision::INITIAL,
                created,
                durability,
                false,
            ),
            BranchHead::from_store(selector, BranchRevision::INITIAL, checkpoint.reference),
            lease,
            feed::receipt_seq(row.get(12).map_err(map_sqlite)?)?,
        );
        if create_receipt_integrity(&receipt)? != row.get::<_, Vec<u8>>(11).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_commit_receipts(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT checkpoint_id,session_id,branch_id,owner,fence,expires_time,expires_ns,duration,
         expected_revision,expected_checkpoint,head_revision,receipt_integrity,feed_seq
         FROM commit_receipts ORDER BY checkpoint_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let checkpoint_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let selector = SessionRef::new(
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(2).map_err(map_sqlite)?)?,
        );
        let lease = BranchLease::from_store(
            selector,
            parse(&row.get::<_, String>(3).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(4).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?,
                row.get(6).map_err(map_sqlite)?,
            )?,
            nz(&row.get::<_, Vec<u8>>(7).map_err(map_sqlite)?)?,
        );
        let previous_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(8).map_err(map_sqlite)?)?);
        let previous_id = parse(&row.get::<_, String>(9).map_err(map_sqlite)?)?;
        let head_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(10).map_err(map_sqlite)?)?);
        let old = checkpoints
            .get(&previous_id)
            .ok_or(SessionStoreError::corrupt())?;
        let new = checkpoints
            .get(&checkpoint_id)
            .ok_or(SessionStoreError::corrupt())?;
        if previous_revision.checked_next() != Some(head_revision)
            || old.selector != selector
            || new.selector != selector
            || old.revision != previous_revision
            || new.revision != head_revision
            || new.parent != Some(old.reference.checkpoint_id())
            || new.lineage != old.lineage
            || new.fence != lease.fence().get()
            || new.durability != old.durability
        {
            return Err(SessionStoreError::corrupt());
        }
        let previous = BranchHead::from_store(selector, previous_revision, old.reference);
        let head = BranchHead::from_store(selector, head_revision, new.reference);
        let committed = CommittedCheckpoint::from_store(
            CommitOperation::from_store(lease, previous, new.reference, new.commit),
            CommitReceipt::from_store(
                new.reference,
                previous,
                head,
                feed::receipt_seq(row.get(12).map_err(map_sqlite)?)?,
            ),
        );
        if commit_receipt_integrity(&committed)? != row.get::<_, Vec<u8>>(11).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "ordered bulk receipt decoding keeps exact fork authority visible"
)]
fn load_authority_fork_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT workflow_id,target_session,target_branch,source_session,source_branch,
         expected_session_revision,source_revision,semantic_request_version,semantic_request,
         source_checkpoint,transition,target_checkpoint,target_owner,target_duration,target_fence,
         target_expires_time,target_expires_ns,initial_branch,created_s,created_ns,durability,
         previous_session_revision,session_revision,receipt_integrity,feed_seq
         FROM fork_receipts ORDER BY workflow_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let workflow_id = decode_workflow_id(&row.get::<_, Vec<u8>>(0).map_err(map_sqlite)?)?;
        let target = SessionRef::new(
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(2).map_err(map_sqlite)?)?,
        );
        let source_ref = SessionRef::new(
            parse(&row.get::<_, String>(3).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(4).map_err(map_sqlite)?)?,
        );
        let expected_session_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        let source_revision =
            BranchRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?);
        let semantic_request = decode_digest(
            DigestDomain::RequestPlan,
            row.get(7).map_err(map_sqlite)?,
            row.get(8).map_err(map_sqlite)?,
        )?;
        let source_id = parse(&row.get::<_, String>(9).map_err(map_sqlite)?)?;
        let target_id = parse(&row.get::<_, String>(11).map_err(map_sqlite)?)?;
        let source_checkpoint = checkpoints
            .get(&source_id)
            .ok_or(SessionStoreError::corrupt())?;
        let target_checkpoint = checkpoints
            .get(&target_id)
            .ok_or(SessionStoreError::corrupt())?;
        let duration = nz(&row.get::<_, Vec<u8>>(13).map_err(map_sqlite)?)?;
        let lease = BranchLease::from_store(
            target,
            parse(&row.get::<_, String>(12).map_err(map_sqlite)?)?,
            nz(&row.get::<_, Vec<u8>>(14).map_err(map_sqlite)?)?,
            lease_time(
                &row.get::<_, Vec<u8>>(15).map_err(map_sqlite)?,
                row.get(16).map_err(map_sqlite)?,
            )?,
            duration,
        );
        let initial = parse(&row.get::<_, String>(17).map_err(map_sqlite)?)?;
        let created = timestamp(
            &row.get::<_, Vec<u8>>(18).map_err(map_sqlite)?,
            row.get(19).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(20).map_err(map_sqlite)?)?;
        let previous_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(21).map_err(map_sqlite)?)?);
        let session_revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(22).map_err(map_sqlite)?)?);
        let source =
            BranchHead::from_store(source_ref, source_revision, source_checkpoint.reference);
        let receipt = ForkReceipt::from_store(
            ForkOperation::from_store(
                workflow_id,
                semantic_request,
                expected_session_revision,
                source,
                target,
                decode_transition(row.get(10).map_err(map_sqlite)?)?,
                target_checkpoint.reference,
                target_checkpoint.commit,
            ),
            SessionHead::from_store(
                target.session_id(),
                initial,
                previous_revision,
                created,
                durability,
                false,
            ),
            SessionHead::from_store(
                target.session_id(),
                initial,
                session_revision,
                created,
                durability,
                false,
            ),
            source,
            BranchHead::from_store(target, BranchRevision::INITIAL, target_checkpoint.reference),
            lease.clone(),
            feed::receipt_seq(row.get(24).map_err(map_sqlite)?)?,
        );
        let persisted = sessions
            .get(&target.session_id())
            .ok_or(SessionStoreError::corrupt())?;
        if persisted.initial_branch() != initial
            || persisted.created_at() != created
            || persisted.durability() != durability
            || source_checkpoint.selector != source_ref
            || source_checkpoint.revision != source_revision
            || target_checkpoint.selector != target
            || target_checkpoint.revision != BranchRevision::INITIAL
            || target_checkpoint.parent != Some(source_id)
            || target_checkpoint.base != Some(source_checkpoint.reference.root())
            || target_checkpoint.lineage
                != Some(ForkLineage {
                    session_id: source_ref.session_id(),
                    branch_id: source_ref.branch_id(),
                    checkpoint_id: source_id,
                    branch_revision: source_revision.get(),
                })
            || target_checkpoint.fence != lease.fence().get()
            || target_checkpoint.kind != RuntimeCheckpointKind::Quiescent
            || target_checkpoint.durability != source_checkpoint.durability
            || fork_receipt_integrity(&receipt)? != row.get::<_, Vec<u8>>(23).map_err(map_sqlite)?
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_archive_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,created_s,created_ns,durability,expected_revision,
         session_revision,receipt_integrity FROM archive_receipts ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let initial = parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?;
        let created = timestamp(
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            row.get(3).map_err(map_sqlite)?,
        )?;
        let durability = decode_durability(row.get(4).map_err(map_sqlite)?)?;
        let expected =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        let next =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(6).map_err(map_sqlite)?)?);
        let receipt = ArchiveReceipt::from_store(
            ArchiveSession::new(session_id, expected),
            SessionHead::from_store(session_id, initial, expected, created, durability, false),
            SessionHead::from_store(session_id, initial, next, created, durability, true),
        );
        if expected.checked_next() != Some(next)
            || sessions.get(&session_id).copied() != Some(receipt.session())
            || archive_receipt_integrity(receipt)?
                != row.get::<_, Vec<u8>>(7).map_err(map_sqlite)?
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_delete_receipts(
    c: &Connection,
    sessions: &BTreeMap<SessionId, SessionHead>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,initial_branch,created_s,created_ns,durability,archived_revision,
         reclaimed_checkpoints,receipt_integrity
         FROM delete_tombstones ORDER BY session_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let session_id = parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?;
        let revision =
            SessionRevision::from_store(read_u64(&row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?)?);
        if revision == SessionRevision::INITIAL || sessions.contains_key(&session_id) {
            return Err(SessionStoreError::corrupt());
        }
        let request = DeleteSession::new(session_id, revision);
        let receipt = DeleteReceipt::from_store(
            request,
            SessionHead::from_store(
                session_id,
                parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
                revision,
                timestamp(
                    &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
                    row.get(3).map_err(map_sqlite)?,
                )?,
                decode_durability(row.get(4).map_err(map_sqlite)?)?,
                true,
            ),
            nonnegative(row.get(6).map_err(map_sqlite)?)?,
        );
        if delete_receipt_integrity(receipt)? != row.get::<_, Vec<u8>>(7).map_err(map_sqlite)? {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}

fn load_authority_horizons(
    c: &Connection,
    checkpoints: &BTreeMap<CheckpointId, CheckpointRow>,
    branches: &BTreeMap<SessionRef, BranchRow>,
) -> Result<(), SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT session_id,branch_id,horizon_revision,horizon_checkpoint,pruned_checkpoints,integrity FROM branch_horizons
             ORDER BY session_id,branch_id",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        let selector = SessionRef::new(
            parse(&row.get::<_, String>(0).map_err(map_sqlite)?)?,
            parse(&row.get::<_, String>(1).map_err(map_sqlite)?)?,
        );
        let horizon = decode_horizon(
            selector,
            &row.get::<_, Vec<u8>>(2).map_err(map_sqlite)?,
            &row.get::<_, String>(3).map_err(map_sqlite)?,
            row.get(4).map_err(map_sqlite)?,
            &row.get::<_, Vec<u8>>(5).map_err(map_sqlite)?,
        )?;
        let checkpoint = checkpoints
            .get(&horizon.checkpoint)
            .ok_or(SessionStoreError::corrupt())?;
        let branch = branches
            .get(&selector)
            .ok_or(SessionStoreError::corrupt())?;
        if checkpoint.selector != selector
            || checkpoint.revision != horizon.revision
            || horizon.revision >= branch.head.revision()
        {
            return Err(SessionStoreError::corrupt());
        }
    }
    Ok(())
}
