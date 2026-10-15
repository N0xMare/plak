use super::gc_tests::{
    Branch, advance_branch, advance_branch_with_payload, backend, fork_branch, seed,
};
use super::*;
use std::time::{Duration, Instant};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;
type Graph = BTreeMap<RuntimeObjectAddress, RuntimeObject>;

fn reference() -> TestResult<Connection> {
    let c = Connection::open_in_memory()?;
    c.execute_batch(SCHEMA)?;
    Ok(c)
}

fn plan(c: &Connection, sql: &str) -> TestResult<Vec<String>> {
    let mut statement = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    let unbound = std::iter::repeat_n(rusqlite::types::Null, statement.parameter_count());
    let details = statement
        .query_map(rusqlite::params_from_iter(unbound), |row| {
            row.get::<_, String>(3)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(details)
}

fn scans(details: &[String]) -> Vec<&String> {
    details.iter().filter(|d| d.starts_with("SCAN")).collect()
}

/// SQLite checks every child table on each parent-key DELETE or UPDATE. Each
/// such probe is a lookup on the child columns; without an index whose
/// leading columns cover them it is a full scan of the child per parent row.
#[test]
fn every_foreign_key_child_probe_searches_an_index() -> TestResult {
    let c = reference()?;
    let tables = c
        .prepare("SELECT name FROM sqlite_schema WHERE type='table' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut probes = 0;
    let mut unindexed = Vec::new();
    for table in tables {
        let mut keys = BTreeMap::<i64, Vec<String>>::new();
        let mut statement =
            c.prepare("SELECT id, \"from\" FROM pragma_foreign_key_list(?1) ORDER BY id, seq")?;
        let rows = statement.query_map([&table], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (key, column) = row?;
            keys.entry(key).or_default().push(column);
        }
        for columns in keys.into_values() {
            let predicate = columns
                .iter()
                .map(|column| format!("{column}=?"))
                .collect::<Vec<_>>()
                .join(" AND ");
            let details = plan(&c, &format!("SELECT 1 FROM {table} WHERE {predicate}"))?;
            probes += 1;
            if details.iter().any(|d| !d.starts_with("SEARCH")) {
                unindexed.push(format!("{table}({}): {details:?}", columns.join(",")));
            }
        }
    }
    assert!(probes >= 20, "schema foreign keys not enumerated: {probes}");
    assert!(
        unindexed.is_empty(),
        "unindexed FK children: {unindexed:#?}"
    );
    Ok(())
}

#[test]
fn prune_candidate_query_probes_only_indexes() -> TestResult {
    let c = reference()?;
    let details = plan(&c, PRUNE_CANDIDATES)?;
    assert!(
        scans(&details).is_empty(),
        "prune candidates scan: {details:#?}"
    );
    for index in [
        "checkpoints_branch_revision",
        "fork_receipts_source",
        "fork_receipts_target",
        "checkpoints_fork",
        "branches_fork",
        "checkpoints_parent",
    ] {
        assert!(
            details
                .iter()
                .any(|d| d.starts_with("SEARCH") && d.contains(&format!(" {index} "))),
            "{index} not probed: {details:#?}"
        );
    }
    Ok(())
}

#[test]
fn session_delete_scopes_its_per_session_rows_by_index() -> TestResult {
    let c = reference()?;
    let statements = [SESSION_FOREIGN_DEPENDENCY]
        .into_iter()
        .chain(SESSION_ROW_DELETES)
        .chain([
            "DELETE FROM checkpoints WHERE session_id=?1",
            "DELETE FROM sessions WHERE session_id=?1",
        ]);
    for statement in statements {
        let details = plan(&c, statement)?;
        assert!(
            scans(&details).is_empty(),
            "{statement} scans: {details:#?}"
        );
    }
    Ok(())
}

fn open_session(b: &mut Backend, graph: &mut Graph, identity: u128) -> TestResult<Branch> {
    let request = seed(identity)?;
    graph.extend(
        request
            .initial()
            .supplied_objects()
            .iter()
            .map(|o| (o.address(), o.clone())),
    );
    let created = create(b, &request)?;
    Ok(Branch {
        head: created.branch(),
        lease: created.lease().clone(),
        bundle: request.initial().clone(),
        value: 1,
    })
}

fn chain(
    b: &mut Backend,
    graph: &mut Graph,
    identity: &mut u128,
    checkpoints: u32,
) -> TestResult<Branch> {
    *identity += 10;
    let mut branch = open_session(b, graph, *identity)?;
    for _ in 0..checkpoints {
        *identity += 10;
        advance_branch(&mut branch, b, graph, *identity)?;
    }
    Ok(branch)
}

fn timed_delete(b: &mut Backend, branch: &Branch) -> TestResult<Duration> {
    release(b, &branch.lease)?;
    let session =
        session_head(&b.connection, branch.head.session().session_id())?.ok_or("session")?;
    let archived = archive(
        b,
        ArchiveSession::new(session.session_id(), session.revision()),
    )?;
    let started = Instant::now();
    delete(
        b,
        DeleteSession::new(session.session_id(), archived.session().revision()),
    )?;
    Ok(started.elapsed())
}

/// Prune and session-delete cost against the number of fork receipts and
/// branches elsewhere in the store. Run explicitly (beyond the nextest slow
/// timeout): `cargo test -p plakous-store --all-features --lib --
/// --ignored --nocapture measure_prune_and_delete_against_fork_count`.
#[test]
#[ignore = "measurement, not a gate"]
fn measure_prune_and_delete_against_fork_count() -> TestResult {
    const CHECKPOINTS: u32 = 500;
    for forks in [0_u32, 1_000, 3_000] {
        let dir = tempfile::tempdir()?;
        let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
        let mut graph = Graph::new();
        let mut identity = 1_000_000_u128;
        let started = Instant::now();
        // One session holds at most 1024 branches.
        for session in 0..forks.div_ceil(1_000) {
            identity += 10;
            let source = open_session(&mut b, &mut graph, identity)?;
            for _ in 0..(forks - session * 1_000).min(1_000) {
                identity += 10;
                fork_branch(&source, &mut b, &mut graph, identity)?;
            }
        }
        let pruned = chain(&mut b, &mut graph, &mut identity, CHECKPOINTS)?;
        let deleted = chain(&mut b, &mut graph, &mut identity, CHECKPOINTS)?;
        let setup_elapsed = started.elapsed();
        let started = Instant::now();
        let receipt = prune(
            &mut b,
            &PruneBranch::new(pruned.lease.clone(), pruned.head, BTreeSet::new(), 1),
        )?;
        let prune_elapsed = started.elapsed();
        let delete_elapsed = timed_delete(&mut b, &deleted)?;
        eprintln!(
            "store-index measurement: schema={SCHEMA_VERSION} forks={forks} checkpoints={CHECKPOINTS} pruned={} setup_ms={} prune_ms={} delete_ms={}",
            receipt.pruned_checkpoints(),
            setup_elapsed.as_millis(),
            prune_elapsed.as_millis(),
            delete_elapsed.as_millis(),
        );
        assert!(receipt.pruned_checkpoints() > 0);
    }
    Ok(())
}

fn chain_with_payload(
    b: &mut Backend,
    graph: &mut Graph,
    identity: &mut u128,
    checkpoints: u32,
    payload: &str,
) -> TestResult<Branch> {
    *identity += 10;
    let mut branch = open_session(b, graph, *identity)?;
    for _ in 0..checkpoints {
        *identity += 10;
        advance_branch_with_payload(&mut branch, b, graph, *identity, payload)?;
    }
    Ok(branch)
}

/// Appends `records` opaque feed records of `bytes` each to one session.
fn inflate_feed(c: &Connection, branch: &Branch, records: u32, bytes: u32) -> TestResult {
    let session = branch.head.session();
    c.execute(
        "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<?3)
         INSERT INTO session_feed(session_id,seq,branch_id,change,checkpoint_id,record,integrity)
         SELECT ?1,s.feed_tail+n.i,?2,4,NULL,randomblob(?4),zeroblob(32)
         FROM n, sessions s WHERE s.session_id=?1",
        params![
            id(session.session_id()),
            id(session.branch_id()),
            records,
            bytes
        ],
    )?;
    c.execute(
        "UPDATE sessions SET feed_tail=feed_tail+?2,feed_bytes=feed_bytes+?2*?3
         WHERE session_id=?1",
        params![id(session.session_id()), records, bytes],
    )?;
    Ok(())
}

fn timed<T>(
    jobs: &mut Vec<(String, Duration)>,
    label: &str,
    job: impl FnOnce() -> Result<T, SessionStoreError>,
) -> TestResult<T> {
    let started = Instant::now();
    let result = job()?;
    jobs.push((label.to_owned(), started.elapsed()));
    Ok(result)
}

/// The single actor jobs that retiring one child runs, in a store shaped like
/// a long root session: forks and live children beside the root, and a
/// retiring child with large history items and a large feed. Retirement is
/// the child's retention prune, lease release, archive with feed trim and
/// session delete, then every background step until the store is idle (GC
/// reclaim, WAL checkpoint, incremental vacuum). Run explicitly:
/// `cargo test -p plakous-store --all-features --lib -- --ignored
/// --nocapture measure_child_retirement_actor_jobs`.
#[test]
#[ignore = "measurement, not a gate"]
fn measure_child_retirement_actor_jobs() -> TestResult {
    const FORKS: u32 = 1_000;
    const ROOT_CHECKPOINTS: u32 = 500;
    const CHILDREN: u32 = 16;
    const CHILD_CHECKPOINTS: u32 = 64;
    const RETIRED_CHECKPOINTS: u32 = 128;
    const FEED_RECORD_BYTES: u32 = 4 * 1024;
    let payload = "x".repeat(64 * 1024);
    for feed_records in [1_024_u32, 14_336] {
        let dir = tempfile::tempdir()?;
        let mut b = backend(&dir.path().join("runtime.sqlite3"))?;
        let mut graph = Graph::new();
        let mut identity = 1_000_000_u128;
        let started = Instant::now();
        identity += 10;
        let root = open_session(&mut b, &mut graph, identity)?;
        for _ in 0..FORKS {
            identity += 10;
            fork_branch(&root, &mut b, &mut graph, identity)?;
        }
        chain(&mut b, &mut graph, &mut identity, ROOT_CHECKPOINTS)?;
        for _ in 0..CHILDREN {
            chain_with_payload(&mut b, &mut graph, &mut identity, CHILD_CHECKPOINTS, "c")?;
        }
        let retired = chain_with_payload(
            &mut b,
            &mut graph,
            &mut identity,
            RETIRED_CHECKPOINTS,
            &payload,
        )?;
        inflate_feed(&b.connection, &retired, feed_records, FEED_RECORD_BYTES)?;
        checkpoint_wal(&b.connection, true)?;
        b.gc_pending = false;
        b.vacuum_pending = false;
        b.wal_pending = false;
        let setup_elapsed = started.elapsed();

        let mut jobs = Vec::new();
        let session_id = retired.head.session().session_id();
        timed(&mut jobs, "prune", || {
            prune(
                &mut b,
                &PruneBranch::new(retired.lease.clone(), retired.head, BTreeSet::new(), 1),
            )
        })?;
        timed(&mut jobs, "release_many", || {
            release_many(&mut b, std::slice::from_ref(&retired.lease))
        })?;
        let head = session_head(&b.connection, session_id)?.ok_or("session")?;
        let archived = timed(&mut jobs, "archive", || {
            let receipt = archive(&mut b, ArchiveSession::new(session_id, head.revision()))?;
            feed::trim(&mut b, session_id);
            Ok(receipt)
        })?;
        timed(&mut jobs, "delete", || {
            delete(
                &mut b,
                DeleteSession::new(session_id, archived.session().revision()),
            )
        })?;
        let mut steps = 0_u32;
        loop {
            let label = if b.gc_pending {
                "gc_reclaim"
            } else if b.wal_pending {
                "wal_checkpoint"
            } else {
                "vacuum"
            };
            let started = Instant::now();
            if !b.background_step() {
                break;
            }
            jobs.push((label.to_owned(), started.elapsed()));
            steps += 1;
            assert!(steps < 10_000, "background work did not settle");
        }
        let (longest, longest_ms) = jobs
            .iter()
            .max_by_key(|(_, elapsed)| *elapsed)
            .map(|(label, elapsed)| (label.clone(), elapsed.as_millis()))
            .ok_or("jobs")?;
        let mut by_kind = BTreeMap::<&str, (u32, Duration)>::new();
        for (label, elapsed) in &jobs {
            let entry = by_kind.entry(label.as_str()).or_default();
            entry.0 += 1;
            entry.1 = entry.1.max(*elapsed);
        }
        let kinds: Vec<_> = by_kind
            .iter()
            .map(|(label, (count, max))| format!("{label}:{count}x/max{}ms", max.as_millis()))
            .collect();
        eprintln!(
            "retirement measurement: schema={SCHEMA_VERSION} forks={FORKS} root_checkpoints={ROOT_CHECKPOINTS} children={CHILDREN} retired_checkpoints={RETIRED_CHECKPOINTS} feed_records={feed_records} feed_mib={} setup_ms={} longest={longest} longest_ms={longest_ms} jobs=[{}]",
            u64::from(feed_records) * u64::from(FEED_RECORD_BYTES) / (1024 * 1024),
            setup_elapsed.as_millis(),
            kinds.join(" "),
        );
        assert!(by_kind.contains_key("gc_reclaim"));
    }
    Ok(())
}
