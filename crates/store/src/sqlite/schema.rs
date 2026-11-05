//! Embedded schema, shape verification, and store-meta maintenance.

#![allow(
    clippy::wildcard_imports,
    reason = "a split of the sqlite module that shares its vocabulary"
)]

use super::*;

pub(super) const APPLICATION_ID: i64 = 0x504c_4b35;

const INCREMENTAL_AUTO_VACUUM: i64 = 2;

pub(super) const SCHEMA_VERSION: i64 = SQLITE_SCHEMA_VERSION;

pub(super) fn bound_domain(c: &Connection) -> Result<LeaseDomain, SessionStoreError> {
    let bytes = c
        .query_row(
            "SELECT lease_domain FROM store_meta WHERE id=1",
            [],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .map_err(map_sqlite)?;
    decode_domain(&bytes)
}

pub(super) fn require_bound_domain(
    c: &Connection,
    domain: LeaseDomain,
) -> Result<(), SessionStoreError> {
    if bound_domain(c)? != domain {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    Ok(())
}

pub(super) fn bind_domain(c: &Connection, domain: LeaseDomain) -> Result<(), SessionStoreError> {
    let previous = bound_domain(c)?;
    if previous == domain {
        return Ok(());
    }
    if previous.epoch() == domain.epoch() {
        return Err(SessionStoreError::LeaseDomainMismatch);
    }
    // Only construction may retire a prior boot, under the same-host contract.
    exactly_one(
        c.execute(
            "UPDATE store_meta SET lease_domain=?1 WHERE id=1",
            [domain_bytes(domain)],
        )
        .map_err(map_sqlite)?,
    )
}

pub(super) fn verify_sqlite_runtime(c: &Connection) -> Result<(), SessionStoreError> {
    let (version, source_id): (String, String) = c
        .query_row("SELECT sqlite_version(), sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .map_err(map_sqlite)?;
    if version != BUNDLED_SQLITE_VERSION || source_id != BUNDLED_SQLITE_SOURCE_ID {
        return Err(SessionStoreError::BackendUnavailable);
    }
    for option in BUNDLED_SQLITE_REQUIRED_COMPILE_OPTIONS {
        let present: i64 = c
            .query_row("SELECT sqlite_compileoption_used(?1)", [option], |row| {
                row.get(0)
            })
            .map_err(map_sqlite)?;
        if present != 1 {
            return Err(SessionStoreError::BackendUnavailable);
        }
    }
    Ok(())
}

pub(super) fn initialize_schema(
    c: &Connection,
    domain: LeaseDomain,
) -> Result<(), SessionStoreError> {
    let app: i64 = c
        .pragma_query_value(None, "application_id", |r| r.get(0))
        .map_err(map_sqlite)?;
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .map_err(map_sqlite)?;
    let tables: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )
        .map_err(map_sqlite)?;
    if app == 0 && version == 0 && tables == 0 {
        c.execute_batch(SCHEMA).map_err(map_sqlite)?;
        c.execute(
            "UPDATE store_meta SET lease_domain=?1 WHERE id=1",
            [domain_bytes(domain)],
        )
        .map_err(map_sqlite)?;
        c.pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(map_sqlite)?;
        c.pragma_update(None, "user_version", SCHEMA_VERSION)
            .map_err(map_sqlite)?;
        return Ok(());
    }
    if app != APPLICATION_ID {
        return Err(SessionStoreError::corrupt());
    }
    if version != SCHEMA_VERSION {
        return Err(SessionStoreError::UnsupportedSchema);
    }
    Ok(())
}

pub(super) const SCHEMA: &str = r"
CREATE TABLE store_meta (
 id INTEGER PRIMARY KEY CHECK(id=1), sessions INTEGER NOT NULL CHECK(sessions>=0),
 branches INTEGER NOT NULL CHECK(branches>=0), checkpoints INTEGER NOT NULL CHECK(checkpoints>=0),
 objects INTEGER NOT NULL CHECK(objects>=0), object_bytes INTEGER NOT NULL CHECK(object_bytes>=0),
 semantic_mutations INTEGER NOT NULL CHECK(semantic_mutations>=0),
 tombstones INTEGER NOT NULL CHECK(tombstones>=0),
 reclaimed_objects INTEGER NOT NULL CHECK(reclaimed_objects>=0),
 reclaimed_object_bytes INTEGER NOT NULL CHECK(reclaimed_object_bytes>=0),
 lease_domain BLOB NOT NULL CHECK(length(lease_domain)=48));
INSERT INTO store_meta VALUES(1,0,0,0,0,0,0,0,0,0,zeroblob(48));
CREATE TABLE runtime_objects (
 id INTEGER PRIMARY KEY,
 kind INTEGER NOT NULL CHECK(kind BETWEEN 1 AND 20),
 codec INTEGER NOT NULL CHECK(codec=36), digest BLOB NOT NULL CHECK(length(digest)=32),
 byte_len INTEGER NOT NULL CHECK(byte_len>=0), UNIQUE(kind,codec,digest));
CREATE TABLE runtime_object_bytes (
 id INTEGER PRIMARY KEY REFERENCES runtime_objects(id), bytes BLOB NOT NULL);
CREATE TABLE gc_candidates (
 kind INTEGER NOT NULL, codec INTEGER NOT NULL, digest BLOB NOT NULL,
 PRIMARY KEY(kind,codec,digest),
 FOREIGN KEY(kind,codec,digest) REFERENCES runtime_objects(kind,codec,digest)) WITHOUT ROWID;
CREATE TABLE runtime_object_edges (
 parent_kind INTEGER NOT NULL, parent_codec INTEGER NOT NULL, parent_digest BLOB NOT NULL,
 position INTEGER NOT NULL CHECK(position>=0), child_kind INTEGER NOT NULL,
 child_codec INTEGER NOT NULL, child_digest BLOB NOT NULL,
 PRIMARY KEY(parent_kind,parent_codec,parent_digest,position),
 FOREIGN KEY(parent_kind,parent_codec,parent_digest) REFERENCES runtime_objects(kind,codec,digest),
 FOREIGN KEY(child_kind,child_codec,child_digest) REFERENCES runtime_objects(kind,codec,digest))
 WITHOUT ROWID;
CREATE TABLE checkpoints (
 checkpoint_id TEXT PRIMARY KEY, root_kind INTEGER NOT NULL, root_codec INTEGER NOT NULL,
 root_digest BLOB NOT NULL, integrity_version INTEGER NOT NULL, integrity BLOB NOT NULL,
 commit_version INTEGER NOT NULL, commit_digest BLOB NOT NULL,
 base_kind INTEGER, base_codec INTEGER, base_digest BLOB,
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL, revision BLOB NOT NULL,
 parent_id TEXT, fork_session TEXT, fork_branch TEXT, fork_checkpoint TEXT, fork_revision BLOB,
 fence BLOB NOT NULL, kind INTEGER NOT NULL CHECK(kind IN(1,2)),
 durability INTEGER NOT NULL CHECK(durability IN(1,2)),
 semantic_key BLOB NOT NULL,
 validation_seal BLOB CHECK(validation_seal IS NULL OR length(validation_seal)=32),
 UNIQUE(root_kind,root_codec,root_digest),
 FOREIGN KEY(root_kind,root_codec,root_digest) REFERENCES runtime_objects(kind,codec,digest),
 FOREIGN KEY(fork_checkpoint) REFERENCES checkpoints(checkpoint_id),
 CHECK((base_kind IS NULL)=(base_codec IS NULL) AND (base_kind IS NULL)=(base_digest IS NULL)),
 CHECK((fork_session IS NULL)=(fork_branch IS NULL)
   AND (fork_session IS NULL)=(fork_checkpoint IS NULL)
   AND (fork_session IS NULL)=(fork_revision IS NULL)));
CREATE TABLE sessions (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, revision BLOB NOT NULL,
 created_s BLOB NOT NULL, created_ns INTEGER NOT NULL CHECK(created_ns BETWEEN 0 AND 999999999),
 durability INTEGER NOT NULL CHECK(durability IN(1,2)), archived INTEGER NOT NULL CHECK(archived IN(0,1)),
 feed_tail INTEGER NOT NULL CHECK(feed_tail>=0),
 feed_retained_from INTEGER NOT NULL CHECK(feed_retained_from>=1),
 feed_bytes INTEGER NOT NULL CHECK(feed_bytes>=0));
CREATE TABLE branches (
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL, revision BLOB NOT NULL,
 checkpoint_id TEXT NOT NULL, last_fence BLOB NOT NULL,
 fork_session TEXT, fork_branch TEXT, fork_checkpoint TEXT, fork_revision BLOB,
 active_owner TEXT, active_fence BLOB, active_time BLOB CHECK(active_time IS NULL OR length(active_time)=56), active_ns INTEGER, active_duration BLOB,
 previous_time BLOB CHECK(previous_time IS NULL OR length(previous_time)=56), previous_ns INTEGER,
 released_owner TEXT, released_fence BLOB, released_time BLOB CHECK(released_time IS NULL OR length(released_time)=56), released_ns INTEGER, released_duration BLOB,
 line_id TEXT NOT NULL, line_generation BLOB NOT NULL CHECK(length(line_generation)=8),
 PRIMARY KEY(session_id,branch_id), FOREIGN KEY(session_id) REFERENCES sessions(session_id),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(fork_checkpoint) REFERENCES checkpoints(checkpoint_id),
 CHECK((fork_session IS NULL)=(fork_branch IS NULL)
   AND (fork_session IS NULL)=(fork_checkpoint IS NULL)
   AND (fork_session IS NULL)=(fork_revision IS NULL))) WITHOUT ROWID;
CREATE TABLE create_receipts (
 session_id TEXT PRIMARY KEY, branch_id TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL, owner TEXT NOT NULL,
 duration BLOB NOT NULL, checkpoint_id TEXT NOT NULL, fence BLOB NOT NULL,
 expires_time BLOB NOT NULL CHECK(length(expires_time)=56), expires_ns INTEGER NOT NULL,
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id));
CREATE TABLE commit_receipts (
 checkpoint_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 owner TEXT NOT NULL, fence BLOB NOT NULL, expires_time BLOB NOT NULL CHECK(length(expires_time)=56), expires_ns INTEGER NOT NULL,
 duration BLOB NOT NULL, expected_revision BLOB NOT NULL, expected_checkpoint TEXT NOT NULL,
 head_revision BLOB NOT NULL, receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 FOREIGN KEY(checkpoint_id) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(expected_checkpoint) REFERENCES checkpoints(checkpoint_id));
CREATE TABLE fork_receipts (
 workflow_id BLOB PRIMARY KEY CHECK(length(workflow_id)=32),
 target_session TEXT NOT NULL, target_branch TEXT NOT NULL,
 source_session TEXT NOT NULL, source_branch TEXT NOT NULL,
 expected_session_revision BLOB NOT NULL, source_revision BLOB NOT NULL,
 semantic_request_version INTEGER NOT NULL CHECK(semantic_request_version BETWEEN 1 AND 4294967295),
 semantic_request BLOB NOT NULL CHECK(length(semantic_request)=32),
 source_checkpoint TEXT NOT NULL, transition INTEGER NOT NULL CHECK(transition IN(1,2)),
 target_checkpoint TEXT NOT NULL, target_owner TEXT NOT NULL, target_duration BLOB NOT NULL,
 target_fence BLOB NOT NULL,
 target_expires_time BLOB NOT NULL CHECK(length(target_expires_time)=56), target_expires_ns INTEGER NOT NULL, initial_branch TEXT NOT NULL,
 created_s BLOB NOT NULL, created_ns INTEGER NOT NULL, durability INTEGER NOT NULL,
 previous_session_revision BLOB NOT NULL, session_revision BLOB NOT NULL,
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 feed_seq INTEGER NOT NULL CHECK(feed_seq>=1),
 UNIQUE(target_session,target_branch),
 FOREIGN KEY(target_session,target_branch) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(source_checkpoint) REFERENCES checkpoints(checkpoint_id),
 FOREIGN KEY(target_checkpoint) REFERENCES checkpoints(checkpoint_id)) WITHOUT ROWID;
CREATE TABLE archive_receipts (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL, expected_revision BLOB NOT NULL,
 session_revision BLOB NOT NULL, receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32),
 FOREIGN KEY(session_id) REFERENCES sessions(session_id));
CREATE TABLE delete_tombstones (
 session_id TEXT PRIMARY KEY, initial_branch TEXT NOT NULL, created_s BLOB NOT NULL,
 created_ns INTEGER NOT NULL, durability INTEGER NOT NULL,
 archived_revision BLOB NOT NULL CHECK(length(archived_revision)=8 AND archived_revision<>zeroblob(8)),
 reclaimed_checkpoints INTEGER NOT NULL CHECK(reclaimed_checkpoints>=0),
 receipt_integrity BLOB NOT NULL CHECK(length(receipt_integrity)=32));
CREATE TABLE branch_horizons (
 session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 horizon_revision BLOB NOT NULL CHECK(length(horizon_revision)=8),
 horizon_checkpoint TEXT NOT NULL,
 pruned_checkpoints INTEGER NOT NULL CHECK(pruned_checkpoints>0),
 integrity BLOB NOT NULL CHECK(length(integrity)=32),
 PRIMARY KEY(session_id,branch_id),
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id),
 FOREIGN KEY(horizon_checkpoint) REFERENCES checkpoints(checkpoint_id)) WITHOUT ROWID;
CREATE TABLE pruned_checkpoints (
 checkpoint_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, branch_id TEXT NOT NULL,
 FOREIGN KEY(session_id,branch_id) REFERENCES branches(session_id,branch_id)) WITHOUT ROWID;
CREATE TABLE session_feed (
 session_id TEXT NOT NULL, seq INTEGER NOT NULL CHECK(seq>=1), branch_id TEXT NOT NULL,
 change INTEGER NOT NULL CHECK(change BETWEEN 1 AND 7), checkpoint_id TEXT,
 record BLOB NOT NULL, integrity BLOB NOT NULL CHECK(length(integrity)=32),
 CHECK((change IN(1,2,3))=(checkpoint_id IS NOT NULL)),
 PRIMARY KEY(session_id,seq), FOREIGN KEY(session_id) REFERENCES sessions(session_id)) WITHOUT ROWID;
CREATE TABLE lines (
 line_id TEXT PRIMARY KEY, head_session TEXT NOT NULL, head_branch TEXT NOT NULL,
 generation BLOB NOT NULL CHECK(length(generation)=8 AND generation<>zeroblob(8)),
 forked_from TEXT CHECK(forked_from IS NULL OR length(forked_from) BETWEEN 1 AND 512),
 FOREIGN KEY(head_session,head_branch) REFERENCES branches(session_id,branch_id)) WITHOUT ROWID;
CREATE TABLE line_job_terminals (
 line_id TEXT NOT NULL, generation BLOB NOT NULL CHECK(length(generation)=8),
 job INTEGER NOT NULL CHECK(job BETWEEN 1 AND 4294967295), evidence BLOB NOT NULL,
 PRIMARY KEY(line_id,generation,job), FOREIGN KEY(line_id) REFERENCES lines(line_id)) WITHOUT ROWID;
CREATE INDEX checkpoints_branch_revision ON checkpoints(session_id,branch_id,revision);
CREATE INDEX checkpoints_root ON checkpoints(root_kind,root_codec,root_digest);
CREATE INDEX checkpoints_parent ON checkpoints(parent_id);
CREATE INDEX checkpoints_fork ON checkpoints(fork_checkpoint);
CREATE INDEX runtime_object_edges_child ON runtime_object_edges(child_kind,child_codec,child_digest);
CREATE INDEX commit_receipts_expected ON commit_receipts(expected_checkpoint);
CREATE INDEX commit_receipts_session ON commit_receipts(session_id);
CREATE INDEX branches_checkpoint ON branches(checkpoint_id);
CREATE INDEX branches_fork ON branches(fork_checkpoint);
CREATE INDEX create_receipts_checkpoint ON create_receipts(checkpoint_id);
CREATE INDEX fork_receipts_source ON fork_receipts(source_checkpoint);
CREATE INDEX fork_receipts_target ON fork_receipts(target_checkpoint);
CREATE INDEX fork_receipts_source_session ON fork_receipts(source_session);
CREATE INDEX branch_horizons_checkpoint ON branch_horizons(horizon_checkpoint);
CREATE INDEX pruned_checkpoints_branch ON pruned_checkpoints(session_id,branch_id);
CREATE INDEX lines_head ON lines(head_session,head_branch);
";

/// Admits shape, schema, domain and counter integrity. Retained state over
/// the configured bounds is not a refusal: an over-limit store still opens so
/// settlement, recovery and deletion can run; its writes are held to their
/// admission class instead.
pub(super) fn admit_open(c: &Connection) -> Result<(), SessionStoreError> {
    verify_shape_pragmas(c)?;
    verify_exact_schema(c)?;
    bound_domain(c)?;
    validate_counts(c)
}

pub(super) fn verify_database(c: &Connection) -> Result<(), SessionStoreError> {
    admit_open(c)?;
    let integrity: String = c
        .query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))
        .map_err(map_sqlite)?;
    if integrity != "ok" {
        return Err(SessionStoreError::corrupt());
    }
    if c.query_row("SELECT 1 FROM pragma_foreign_key_check LIMIT 1", [], |_| {
        Ok(())
    })
    .optional()
    .map_err(map_sqlite)?
    .is_some()
    {
        return Err(SessionStoreError::corrupt());
    }
    load_verified_objects(c)?;
    let orphan: bool = c.query_row("WITH RECURSIVE owned(kind,codec,digest) AS (
        SELECT root_kind,root_codec,root_digest FROM checkpoints
        UNION SELECT kind,codec,digest FROM gc_candidates
        UNION SELECT e.child_kind,e.child_codec,e.child_digest FROM runtime_object_edges e
        JOIN owned o ON e.parent_kind=o.kind AND e.parent_codec=o.codec AND e.parent_digest=o.digest)
        SELECT EXISTS(SELECT 1 FROM runtime_objects r WHERE NOT EXISTS (
        SELECT 1 FROM owned o WHERE o.kind=r.kind AND o.codec=r.codec AND o.digest=r.digest))", [], |r| r.get(0)).map_err(map_sqlite)?;
    if orphan {
        return Err(SessionStoreError::corrupt());
    }
    verify_authority(c)
}

fn load_verified_objects(c: &Connection) -> Result<(), SessionStoreError> {
    let mut children = load_all_children(c)?;
    let mut s = c
        .prepare("SELECT o.kind,o.codec,o.digest,p.bytes,o.byte_len FROM runtime_objects o LEFT JOIN runtime_object_bytes p USING(id) ORDER BY o.kind,o.codec,o.digest")
        .map_err(map_sqlite)?;
    let mut rows = s.query([]).map_err(map_sqlite)?;
    while let Some(r) = rows.next().map_err(map_sqlite)? {
        let a = decode_address(
            r.get(0).map_err(map_sqlite)?,
            r.get(1).map_err(map_sqlite)?,
            r.get(2).map_err(map_sqlite)?,
        )?;
        let bytes: Vec<u8> = r
            .get::<_, Option<Vec<u8>>>(3)
            .map_err(map_sqlite)?
            .ok_or(SessionStoreError::corrupt())?;
        if nonnegative(r.get(4).map_err(map_sqlite)?)? != u64n(bytes.len()) {
            return Err(SessionStoreError::corrupt());
        }
        RuntimeObject::from_store(a, bytes, children.remove(&a).unwrap_or_default())
            .map_err(bundle_error)?;
    }
    if !children.is_empty() {
        return Err(SessionStoreError::corrupt());
    }
    drop(rows);
    drop(s);
    Ok(())
}

pub(super) fn verify_shape_pragmas(c: &Connection) -> Result<(), SessionStoreError> {
    let fk: i64 = c
        .pragma_query_value(None, "foreign_keys", |r| r.get(0))
        .map_err(map_sqlite)?;
    let trusted: i64 = c
        .pragma_query_value(None, "trusted_schema", |r| r.get(0))
        .map_err(map_sqlite)?;
    let journal: String = c
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .map_err(map_sqlite)?;
    let sync: i64 = c
        .pragma_query_value(None, "synchronous", |r| r.get(0))
        .map_err(map_sqlite)?;
    let vacuum: i64 = c
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .map_err(map_sqlite)?;
    if fk != 1
        || trusted != 0
        || !journal.eq_ignore_ascii_case("wal")
        || sync != 2
        || vacuum != INCREMENTAL_AUTO_VACUUM
    {
        return Err(SessionStoreError::BackendUnavailable);
    }
    Ok(())
}

pub(super) fn verify_exact_schema(c: &Connection) -> Result<(), SessionStoreError> {
    let reference = Connection::open_in_memory().map_err(map_sqlite)?;
    reference.execute_batch(SCHEMA).map_err(map_sqlite)?;
    if schema_objects(c)? == schema_objects(&reference)? {
        return Ok(());
    }
    // The object table's DDL spells the format epoch, so a store another
    // epoch's build wrote at this schema version differs here first: that is
    // an incompatible build, not damage.
    let epoch = i64::from(plakous_agent::internal::RUNTIME_FORMAT_EPOCH);
    if object_table_epoch(c)?.is_some_and(|written| written != epoch) {
        return Err(SessionStoreError::UnsupportedCodec);
    }
    Err(SessionStoreError::corrupt())
}

/// The format epoch the `runtime_objects` codec CHECK admits, when the table
/// exists and spells one.
fn object_table_epoch(c: &Connection) -> Result<Option<i64>, SessionStoreError> {
    let sql: Option<String> = c
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE type='table' AND name='runtime_objects'",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(map_sqlite)?;
    Ok(sql.and_then(|sql| {
        let (_, rest) = sql.split_once("CHECK(codec=")?;
        rest.split_once(')')?.0.parse().ok()
    }))
}

fn schema_objects(
    c: &Connection,
) -> Result<Vec<(String, String, String, String)>, SessionStoreError> {
    let mut statement = c
        .prepare(
            "SELECT type,name,tbl_name,sql FROM sqlite_schema
             WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
             ORDER BY type,name,tbl_name,sql",
        )
        .map_err(map_sqlite)?;
    statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)
}

fn verify_authority(c: &Connection) -> Result<(), SessionStoreError> {
    verify_receipt_completeness(c)?;
    verify_lines(c)?;
    let snapshot = AuthoritySnapshot::load(c)?;
    snapshot.verify()?;
    feed::verify(c)
}

/// Every line head names a branch of that line at the line's generation, and
/// every recorded terminal belongs to its line's current generation.
fn verify_lines(c: &Connection) -> Result<(), SessionStoreError> {
    let broken = c
        .query_row(
            "SELECT 1 WHERE
             EXISTS(SELECT 1 FROM lines l JOIN branches b
                    ON b.session_id=l.head_session AND b.branch_id=l.head_branch
                    WHERE b.line_id<>l.line_id OR b.line_generation<>l.generation)
             OR EXISTS(SELECT 1 FROM line_job_terminals t JOIN lines l USING(line_id)
                    WHERE t.generation<>l.generation)",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(map_sqlite)?;
    if broken.is_some() {
        return Err(SessionStoreError::corrupt());
    }
    Ok(())
}

fn verify_receipt_completeness(c: &Connection) -> Result<(), SessionStoreError> {
    let gap = c
        .query_row(
            ("SELECT 1 WHERE
             EXISTS(SELECT 1 FROM sessions s LEFT JOIN create_receipts r USING(session_id)
                    WHERE r.session_id IS NULL)
             OR EXISTS(SELECT 1 FROM sessions s LEFT JOIN archive_receipts r USING(session_id)
                    WHERE (s.archived=1 AND r.session_id IS NULL)
                       OR (s.archived=0 AND r.session_id IS NOT NULL))
             OR EXISTS(SELECT 1 FROM branches b LEFT JOIN fork_receipts r
                    ON r.target_session=b.session_id AND r.target_branch=b.branch_id
                    WHERE (b.fork_checkpoint IS NOT NULL AND r.target_session IS NULL)
                       OR (b.fork_checkpoint IS NULL AND r.target_session IS NOT NULL))
             OR EXISTS(SELECT 1 FROM checkpoints c WHERE
                    (c.revision=zeroblob(8)
                     AND NOT EXISTS(SELECT 1 FROM create_receipts r
                                    WHERE r.checkpoint_id=c.checkpoint_id)
                     AND NOT EXISTS(SELECT 1 FROM fork_receipts r
                                    WHERE r.target_checkpoint=c.checkpoint_id))
                    OR (c.revision<>zeroblob(8)
                        AND NOT EXISTS(SELECT 1 FROM commit_receipts r
                                       WHERE r.checkpoint_id=c.checkpoint_id)
                        AND NOT EXISTS(SELECT 1 FROM branch_horizons h
                                       WHERE h.session_id=c.session_id
                                         AND h.branch_id=c.branch_id
                                         AND c.revision<=h.horizon_revision)))
             OR EXISTS(SELECT 1 FROM commit_receipts r
                    JOIN checkpoints c ON c.checkpoint_id=r.checkpoint_id
                    JOIN branch_horizons h ON h.session_id=c.session_id AND h.branch_id=c.branch_id
                    WHERE c.revision<=h.horizon_revision)
             OR EXISTS(SELECT 1 FROM pruned_checkpoints p
                    JOIN checkpoints c ON c.checkpoint_id=p.checkpoint_id)
             OR EXISTS(SELECT 1 FROM checkpoints c WHERE c.parent_id IS NOT NULL
                    AND NOT EXISTS(SELECT 1 FROM checkpoints p
                                   WHERE p.checkpoint_id=c.parent_id)
                    AND NOT EXISTS(SELECT 1 FROM pruned_checkpoints p
                                   WHERE p.checkpoint_id=c.parent_id))"
                .to_owned()
                + feed::COMPLETENESS)
                .as_str(),
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(map_sqlite)?;
    if gap.is_some() {
        Err(SessionStoreError::corrupt())
    } else {
        Ok(())
    }
}

pub(super) fn read_stats(c: &Connection) -> Result<SqliteStoreStats, SessionStoreError> {
    let v = c.query_row(
        "SELECT sessions,branches,checkpoints,objects,object_bytes,semantic_mutations,tombstones,reclaimed_objects,reclaimed_object_bytes FROM store_meta WHERE id=1",
        [], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,
                    r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?))
    ).map_err(map_sqlite)?;
    Ok(SqliteStoreStats {
        sessions: nonnegative(v.0)?,
        branches: nonnegative(v.1)?,
        checkpoints: nonnegative(v.2)?,
        objects: nonnegative(v.3)?,
        object_bytes: nonnegative(v.4)?,
        semantic_mutations: nonnegative(v.5)?,
        tombstones: nonnegative(v.6)?,
        reclaimed_objects: nonnegative(v.7)?,
        reclaimed_object_bytes: nonnegative(v.8)?,
    })
}

pub(super) fn validate_counts(c: &Connection) -> Result<(), SessionStoreError> {
    let s = read_stats(c)?;
    let count = |table: &str| -> Result<u64, SessionStoreError> {
        c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .map_err(map_sqlite)
        .and_then(nonnegative)
    };
    let bytes=c.query_row(
        "SELECT COALESCE(sum(byte_len),0)+(SELECT count(*)*37 FROM runtime_object_edges) FROM runtime_objects",
        [],|r|r.get::<_,i64>(0)).map_err(map_sqlite).and_then(nonnegative)?;
    if (
        s.sessions,
        s.branches,
        s.checkpoints,
        s.objects,
        s.object_bytes,
        s.tombstones,
    ) != (
        count("sessions")?,
        count("branches")?,
        count("checkpoints")?,
        count("runtime_objects")?,
        bytes,
        count("delete_tombstones")?,
    ) {
        return Err(SessionStoreError::corrupt());
    }
    Ok(())
}

pub(super) fn meta(
    tx: &Transaction<'_>,
    sessions: u64,
    branches: u64,
) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE store_meta SET sessions=sessions+?1,branches=branches+?2,
        semantic_mutations=semantic_mutations+1 WHERE id=1",
        params![i64n(sessions)?, i64n(branches)?],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

pub(super) fn bump(tx: &Transaction<'_>) -> Result<(), SessionStoreError> {
    tx.execute(
        "UPDATE store_meta SET semantic_mutations=semantic_mutations+1 WHERE id=1",
        [],
    )
    .map_err(map_sqlite)?;
    Ok(())
}

pub(super) fn checkpoint_wal(c: &Connection, truncate: bool) -> Result<(), SessionStoreError> {
    let sql = if truncate {
        "PRAGMA wal_checkpoint(TRUNCATE)"
    } else {
        "PRAGMA wal_checkpoint(PASSIVE)"
    };
    let busy: i64 = c.query_row(sql, [], |r| r.get(0)).map_err(map_sqlite)?;
    if busy == 0 {
        Ok(())
    } else {
        Err(SessionStoreError::BackendBusy)
    }
}

pub(super) fn report() -> DurabilityReport {
    DurabilityReport::from_store(RuntimeDurability::Required, 0, None)
}

pub(super) fn enforce(
    name: StoreCapacityBound,
    value: u64,
    max: usize,
) -> Result<(), SessionStoreError> {
    let max = u64n(max);
    if value > max {
        Err(SessionStoreError::capacity(name, value, max))
    } else {
        Ok(())
    }
}

/// Holds an aggregate bound to `admission`'s share of `limit`.
pub(super) fn admit(
    bound: StoreCapacityBound,
    attempted: u64,
    limit: usize,
    admission: StoreAdmission,
) -> Result<(), SessionStoreError> {
    let limit = u64n(limit);
    if attempted > admission.threshold(limit) {
        Err(SessionStoreError::admission_capacity(
            bound, attempted, limit, admission,
        ))
    } else {
        Ok(())
    }
}

pub(super) fn usage(
    c: &Connection,
    limits: MemoryStoreLimits,
) -> Result<StoreUsage, SessionStoreError> {
    let stats = read_stats(c)?;
    Ok(StoreUsage::new(
        (stats.checkpoints, u64n(limits.max_checkpoints().get())),
        (stats.objects, u64n(limits.max_objects().get())),
        (stats.object_bytes, u64n(limits.max_object_bytes().get())),
    ))
}

/// Allocated database bytes and reusable free-list bytes.
pub(super) fn file_usage(c: &Connection) -> Result<SqliteFileUsage, SessionStoreError> {
    let pragma = |name: &str| {
        c.pragma_query_value(None, name, |r| r.get::<_, i64>(0))
            .map_err(map_sqlite)
            .and_then(nonnegative)
    };
    let page_size = pragma("page_size")?;
    Ok(SqliteFileUsage {
        file_bytes: pragma("page_count")?.saturating_mul(page_size),
        free_bytes: pragma("freelist_count")?.saturating_mul(page_size),
    })
}
