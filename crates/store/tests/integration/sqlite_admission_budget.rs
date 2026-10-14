//! Count-based critical-lane budget, compiled beside the private actor to trace its connection.
use super::*;
use crate::integrity_fixture as fixture;
use rusqlite::StatementStatus;
use rusqlite::trace::{TraceEvent, TraceEventCodes};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Metrics {
    statements: usize,
    vm_steps: i64,
    fullscan_steps: i64,
    autoindex: i64,
    payload_reads: usize,
    recursive_or_sum: usize,
}

thread_local! {
    static METRICS: std::cell::RefCell<Metrics> = std::cell::RefCell::default();
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "SQLite trace callbacks receive events by value"
)]
fn profile(event: TraceEvent<'_>) {
    let TraceEvent::Profile(statement, _) = event else {
        return;
    };
    let sql = statement.sql().to_ascii_lowercase();
    METRICS.with(|metrics| {
        let mut m = metrics.borrow_mut();
        m.statements += 1;
        m.vm_steps += i64::from(statement.get_status(StatementStatus::VmStep));
        m.fullscan_steps += i64::from(statement.get_status(StatementStatus::FullscanStep));
        m.autoindex += i64::from(statement.get_status(StatementStatus::AutoIndex));
        m.payload_reads +=
            usize::from(sql.contains("select") && sql.contains("runtime_object_bytes"));
        m.recursive_or_sum += usize::from(sql.contains("with recursive") || sql.contains("sum("));
    });
}

fn checkpoint(
    base: u128,
    revision: u64,
    prior: Option<&CheckpointObjectBundle>,
    payload: &str,
) -> TestResult<CheckpointObjectBundle> {
    let topology = fixture::topology(
        fixture::id(base)?,
        fixture::id(base + 1)?,
        fixture::id(base + 2 + u128::from(revision))?,
        prior.map(CheckpointObjectBundle::checkpoint_id),
        revision,
        None,
        1,
    )?;
    let state = if let Some(prior) = prior {
        fixture::checkpoint_with_payload(
            topology,
            1,
            RuntimeDurability::Required,
            prior
                .validated_checkpoint()?
                .manifest()
                .canonical()
                .context()
                .clone(),
            payload,
        )?
    } else {
        let initial = fixture::checkpoint(topology, 1, RuntimeDurability::Required)?;
        fixture::checkpoint_with_payload(
            topology,
            1,
            RuntimeDurability::Required,
            initial.manifest().canonical().context().clone(),
            payload,
        )?
    };
    fixture::bundle(&state, prior)
}

fn populate(b: &mut Backend) -> TestResult {
    let padding = "x".repeat(64 * 1024);
    for session in 0_u128..200 {
        let base = 10_000_000 + session * 100;
        let mut bundle = checkpoint(base, 0, None, &format!("{session}:0:{padding}"))?;
        let created = create(
            b,
            &CreateSession::new(
                SessionRef::new(fixture::id(base)?, fixture::id(base + 1)?),
                Timestamp::new(50, 0)?,
                fixture::id(base + 90)?,
                NonZeroU64::new(3600).ok_or("lease")?,
                bundle.clone(),
            ),
        )?;
        let mut head = created.branch();
        let mut lease = created.lease().clone();
        for revision in 1..10 {
            let next = checkpoint(
                base,
                revision,
                Some(&bundle),
                &format!("{session}:{revision}:{padding}"),
            )?;
            head = commit(b, &CommitCheckpoint::new(lease.clone(), head, next.clone()))?.head();
            bundle = next;
        }
        for _ in 0..40 {
            release(b, &lease)?;
            lease = acquire(
                b,
                AcquireBranch::new(
                    head.session(),
                    created.lease().owner(),
                    created.lease().lease_seconds(),
                ),
            )?;
        }
        // The final active grant stays authoritative, with a 50-record feed.
    }
    b.connection.execute("INSERT INTO gc_candidates SELECT kind,codec,digest FROM runtime_objects ORDER BY kind,codec,digest LIMIT 5000", [])?;
    let stats = read_stats(&b.connection)?;
    assert_eq!(stats.sessions(), 200);
    assert_eq!(stats.checkpoints(), 2000);
    assert_eq!(
        b.connection
            .query_row("SELECT count(*) FROM session_feed", [], |r| r
                .get::<_, i64>(0))?,
        10_000
    );
    assert_eq!(
        b.connection
            .query_row("SELECT count(*) FROM gc_candidates", [], |r| r
                .get::<_, i64>(0))?,
        5000
    );
    assert!(
        b.connection
            .query_row("SELECT max(byte_len) FROM runtime_objects", [], |r| r
                .get::<_, i64>(0))?
            >= 64 * 1024
    );
    validate_counts(&b.connection)?;
    Ok(())
}

fn measure(large: bool) -> TestResult<Metrics> {
    let dir = tempfile::tempdir()?;
    let mut b = gc_tests::backend(&dir.path().join("runtime.sqlite3"))?;
    if large {
        populate(&mut b)?;
    }
    let request = gc_tests::seed(500_000)?;
    let created = create(&mut b, &request)?;
    let next = fixture::checkpoint(
        fixture::topology(
            created.branch().session().session_id(),
            created.branch().session().branch_id(),
            fixture::id(500_050)?,
            Some(request.initial().checkpoint_id()),
            1,
            None,
            1,
        )?,
        2,
        RuntimeDurability::Required,
    )?;
    let delta = fixture::bundle(&next, Some(request.initial()))?;
    b.connection
        .trace_v2(TraceEventCodes::SQLITE_TRACE_PROFILE, Some(profile));
    METRICS.with(|metrics| *metrics.borrow_mut() = Metrics::default());
    assert!(
        usage(&b.connection, b.limits)?
            .ordinary_exhausted()
            .is_none()
    );
    let renewed = renew(&mut b, created.lease())?;
    commit(
        &mut b,
        &CommitCheckpoint::new(renewed, created.branch(), delta),
    )?;
    b.connection.trace_v2(TraceEventCodes::empty(), None);
    Ok(METRICS.with(|metrics| metrics.borrow().clone()))
}

#[test]
fn large_store_admission_has_equal_statement_and_vm_budgets() -> TestResult {
    let small = measure(false)?;
    let large = measure(true)?;
    assert_eq!(
        small, large,
        "critical-lane SQL work grew with retained history or GC backlog"
    );
    assert!(large.statements > 0);
    assert_eq!(large.fullscan_steps, 0);
    assert_eq!(large.autoindex, 0);
    assert_eq!(large.payload_reads, 0);
    assert_eq!(large.recursive_or_sum, 0);
    Ok(())
}
