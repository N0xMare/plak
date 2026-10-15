use super::gc_tests::{Branch, advance_branch, backend, fork_branch, seed};
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
