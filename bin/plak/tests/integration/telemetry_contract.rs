//! `plak telemetry` CLI contracts.

#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "integration test crate may unwrap"
)]

use std::path::PathBuf;
use std::process::Command;

use plak::home::paths::PlakPaths;
use plak::telemetry_store::TelemetryStore;
use rusqlite::Connection;

fn plak_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_plak"))
}

fn private_home(home: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))
            .expect("private home");
    }
}

/// Mirrors the product schema shape the provenance view depends on: root
/// sessions carry a `SessionLocator` string in `sessions.locator`; spawned
/// children carry theirs in `agent_nodes.session_locator`. Node ids are not
/// plakous agent ids.
fn seed_minimal_product_provenance(database: &std::path::Path, subagents: &std::path::Path) {
    let connection = Connection::open(database).expect("open product db");
    connection
        .execute_batch(
            "CREATE TABLE sessions (
              id TEXT PRIMARY KEY,
              account TEXT,
              model TEXT,
              effort TEXT,
              locator TEXT,
              composition TEXT NOT NULL DEFAULT 'plakous-default'
            );
            INSERT INTO sessions VALUES (
              'node-root','openai','gpt-test','low','sess-1/br/ckpt','direct'
            );
            ",
        )
        .expect("seed product provenance");
    let connection = Connection::open(subagents).expect("open subagents db");
    connection
        .execute_batch(
            "CREATE TABLE agent_nodes (
              tree_id TEXT NOT NULL,
              agent_id TEXT NOT NULL,
              tuple_id TEXT NOT NULL,
              account TEXT NOT NULL,
              effort TEXT,
              session_locator TEXT,
              PRIMARY KEY(tree_id, agent_id)
            );
            INSERT INTO agent_nodes VALUES (
              'node-root','node-root','gpt-test','openai','low',NULL
            );
            INSERT INTO agent_nodes VALUES (
              'node-root','node-child','gpt-child','openai',NULL,'sess-2/br/ckpt'
            );",
        )
        .expect("seed product provenance");
}

#[test]
fn telemetry_sql_with_product_attach() {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    let _store = TelemetryStore::open(&paths).expect("open telemetry");
    let connection = Connection::open(paths.telemetry_database()).expect("telemetry db");
    connection
        .execute(
            "INSERT INTO turns (session_id, branch_id, run_id, agent_id, turn_id, input_tokens, cached_input_tokens)
             VALUES ('sess-1','br','run','01a0-root-agent','turn-1',100,20),
                    ('sess-2','br','run','01a0-child-agent','turn-2',50,5)",
            [],
        )
        .expect("seed turn");
    seed_minimal_product_provenance(&paths.product_database(), &paths.subagents_database());
    for sql in [
        "SELECT * FROM v_session_models",
        "SELECT s.tuple_id, s.effort, s.composition, SUM(t.cached_input_tokens) FROM v_turn_costs t JOIN v_session_models s ON s.session_id = t.session_id GROUP BY s.tuple_id, s.effort, s.composition",
        "SELECT s.tuple_id, s.effort, AVG(m.ttft_ns), AVG(m.ttlb_ns) FROM model_attempts m JOIN v_session_models s ON s.session_id = m.session_id GROUP BY s.tuple_id, s.effort",
    ] {
        TelemetryStore::query_attached(&paths, sql).expect("canned provenance query");
    }

    private_home(paths.home());
    let output = Command::new(plak_bin())
        .env("PLAK_HOME", paths.home())
        .env("HOME", paths.home())
        .current_dir(paths.home())
        .args([
            "telemetry",
            "sql",
            "SELECT s.tuple_id, s.origin, t.cached_input_tokens FROM turns t JOIN v_session_models s ON s.session_id = t.session_id ORDER BY t.turn_id",
        ])
        .output()
        .expect("spawn plak telemetry sql");
    assert!(
        output.status.success(),
        "telemetry sql failed: status={:?} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gpt-test"), "stdout: {stdout}");
    assert!(stdout.contains("root"), "stdout: {stdout}");
    assert!(stdout.contains("20"), "stdout: {stdout}");
    assert!(stdout.contains("gpt-child"), "stdout: {stdout}");
    assert!(stdout.contains("child"), "stdout: {stdout}");
}

#[test]
fn telemetry_status_reports_schema_version() {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    let _store = TelemetryStore::open(&paths).expect("open telemetry");

    private_home(paths.home());
    let output = Command::new(plak_bin())
        .env("PLAK_HOME", paths.home())
        .env("HOME", paths.home())
        .current_dir(paths.home())
        .args(["telemetry", "status"])
        .output()
        .expect("spawn plak telemetry status");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Schema version 19"),
        "expected schema v19 in status: {stdout}"
    );
}

#[test]
fn jobs_telemetry_schema_is_content_free_and_route_joinable() {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    let _store = TelemetryStore::open(&paths).expect("open telemetry");
    assert!(
        paths
            .telemetry_database()
            .ends_with("telemetry-v19.sqlite3")
    );
    let connection = Connection::open(paths.telemetry_database()).expect("telemetry db");
    let mut query = connection
        .prepare("SELECT name FROM pragma_table_info('job_events')")
        .unwrap();
    let columns = query
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        columns,
        [
            "id",
            "at",
            "session_id",
            "branch_id",
            "run_id",
            "agent_id",
            "turn_id",
            "call_id",
            "kind",
            "tool",
            "nested",
            "hint_source",
            "hint_ms",
            "delivery_point",
            "count"
        ]
    );
}

fn job_telemetry_kinds() -> Vec<plakous::agent::TelemetryObservationKind> {
    use plakous::agent::{
        JobHintSource, JobNoticeDelivery, JobPreemption, JobTool, TelemetryObservationKind,
    };
    let mut kinds = Vec::new();
    for tool in [JobTool::Start] {
        for nested in [false, true] {
            for hint_source in [
                JobHintSource::Default,
                JobHintSource::ModelHint,
                JobHintSource::Background,
            ] {
                kinds.push(TelemetryObservationKind::JobPromoted {
                    tool,
                    nested,
                    hint_source,
                    hint_ms: 600_000,
                });
            }
        }
    }
    for tool in [JobTool::Poll, JobTool::WriteWait, JobTool::Await] {
        kinds.push(TelemetryObservationKind::JobWaited { tool });
    }
    for kind in [
        JobPreemption::Promoted,
        JobPreemption::Superseded,
        JobPreemption::WaitYielded,
    ] {
        kinds.push(TelemetryObservationKind::JobPreempted { kind });
    }
    for point in [
        JobNoticeDelivery::Boundary,
        JobNoticeDelivery::Admission,
        JobNoticeDelivery::IdleWake,
    ] {
        kinds.push(TelemetryObservationKind::JobNoticesDelivered { point, count: 2 });
    }
    kinds
}

#[test]
fn jobs_telemetry_records_only_closed_classifications_and_numeric_hints() {
    use plakous::agent::{TelemetryContext, TelemetryObservation};
    use plakous::model::{AgentId, BranchId, RunId, SessionId, Timestamp, TurnId};
    use uuid::Uuid;

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let mut store = TelemetryStore::open(&paths).unwrap();
    let context = TelemetryContext::new(
        AgentId::try_from(Uuid::from_u128(1)).unwrap(),
        SessionId::try_from(Uuid::from_u128(2)).unwrap(),
        BranchId::try_from(Uuid::from_u128(3)).unwrap(),
        RunId::try_from(Uuid::from_u128(4)).unwrap(),
    )
    .with_turn(TurnId::try_from(Uuid::from_u128(5)).unwrap());
    let at = Timestamp::new(1_700_000_000, 0).unwrap();
    let observations = job_telemetry_kinds()
        .into_iter()
        .map(|kind| TelemetryObservation::new(context, at, kind))
        .collect::<Vec<_>>();
    store.write_observations_for_test(&observations).unwrap();
    let connection = Connection::open(paths.telemetry_database()).unwrap();
    let count = |sql: &str| {
        connection
            .query_row(sql, [], |row| row.get::<_, u32>(0))
            .unwrap()
    };
    assert_eq!(
        count("SELECT count(*) FROM job_events WHERE kind = 'promoted' AND hint_ms = 600000"),
        6
    );
    assert_eq!(
        count("SELECT count(*) FROM job_events WHERE kind = 'promoted' AND nested = 1"),
        3
    );
    assert_eq!(
        count("SELECT count(*) FROM job_events WHERE kind = 'waited'"),
        3
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM job_events WHERE kind IN ('preempt_superseded', 'preempt_wait_yielded')"
        ),
        2
    );
    assert_eq!(
        count("SELECT sum(count) FROM job_events WHERE kind = 'notices'"),
        6
    );
    assert_eq!(
        count("SELECT count(DISTINCT delivery_point) FROM job_events WHERE kind = 'notices'"),
        3
    );
    assert_eq!(
        count(
            "SELECT count(*) FROM job_events WHERE session_id IS NOT NULL AND turn_id IS NOT NULL"
        ),
        15
    );
}

#[test]
fn run_bound_exits_record_a_closed_product_status() {
    use plak::telemetry_store::TelemetryItem;
    use plakous::model::{SessionId, Timestamp};
    use plakous_hosted::product_observation::{ProductObservation, RunBound};
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().unwrap();
    let mut store = TelemetryStore::open(&paths).unwrap();
    let session_id = SessionId::try_from(uuid::Uuid::from_u128(1)).unwrap();
    let at = Timestamp::new(1_700_000_000, 0).unwrap();
    store
        .write_batch(&[RunBound::Timeout, RunBound::MaxTurns].map(|bound| {
            TelemetryItem::Product(ProductObservation::RunBoundExit {
                at,
                session_id,
                bound,
            })
        }))
        .unwrap();
    let connection = Connection::open(paths.telemetry_database()).unwrap();
    let mut statement = connection
        .prepare("SELECT status FROM product_events WHERE kind = 'run_bound_exit' ORDER BY status")
        .unwrap();
    let statuses = statement
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(statuses, ["max_turns", "timeout"]);
}

#[test]
fn model_finished_persists_served_endpoint_and_cost() {
    use std::time::Duration;

    use plakous::agent::{
        TelemetryContext, TelemetryObservation, TelemetryObservationKind, TelemetryTerminalStatus,
    };
    use plakous::model::{
        AgentId, AttemptId, BranchId, RunId, ServedBy, SessionId, Timestamp, TurnId, Usage,
    };
    use uuid::Uuid;

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().expect("layout");
    let mut store = TelemetryStore::open(&paths).expect("open telemetry");

    let session_id = SessionId::try_from(Uuid::from_u128(0x100)).expect("session");
    let turn_id = TurnId::try_from(Uuid::from_u128(0x101)).expect("turn");
    let attempt_reported = AttemptId::try_from(Uuid::from_u128(0x102)).expect("attempt");
    let attempt_unknown = AttemptId::try_from(Uuid::from_u128(0x103)).expect("attempt");
    let timestamp = Timestamp::new(1_700_000_000, 0).expect("timestamp");
    let base = TelemetryContext::new(
        AgentId::try_from(Uuid::from_u128(0x104)).expect("agent"),
        session_id,
        BranchId::try_from(Uuid::from_u128(0x105)).expect("branch"),
        RunId::try_from(Uuid::from_u128(0x106)).expect("run"),
    )
    .with_turn(turn_id);

    store
        .write_observations_for_test(&[
            TelemetryObservation::new(
                base.with_attempt(attempt_reported),
                timestamp,
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_millis(10),
                    status: TelemetryTerminalStatus::Completed,
                    usage: Some(Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(123_456))),
                    served_by: Some(ServedBy::new("Provider").expect("served_by")),
                    output_deltas: 3,
                },
            ),
            TelemetryObservation::new(
                base.with_attempt(attempt_unknown),
                timestamp,
                TelemetryObservationKind::ModelFinished {
                    elapsed: Duration::from_millis(20),
                    status: TelemetryTerminalStatus::Completed,
                    usage: Some(Usage::new(3, 4, 0, 0, 0)),
                    served_by: None,
                    output_deltas: 0,
                },
            ),
        ])
        .expect("write observations");

    let connection = Connection::open(paths.telemetry_database()).expect("telemetry db");
    let columns: Vec<String> = connection
        .prepare("PRAGMA table_info(model_attempts)")
        .expect("pragma")
        .query_map([], |row| row.get::<_, String>(1))
        .expect("columns")
        .filter_map(Result::ok)
        .collect();
    for column in ["served_endpoint", "cost_micros", "cost_status"] {
        assert!(
            columns.iter().any(|name| name == column),
            "missing model_attempts.{column}: {columns:?}"
        );
    }

    let reported: (Option<String>, Option<i64>, String) = connection
        .query_row(
            "SELECT served_endpoint, cost_micros, cost_status FROM model_attempts WHERE attempt_id = ?1",
            [attempt_reported.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("reported row");
    assert_eq!(reported.0.as_deref(), Some("Provider"));
    assert_eq!(reported.1, Some(123_456));
    assert_eq!(reported.2, "reported");

    let unknown: (Option<String>, Option<i64>, String) = connection
        .query_row(
            "SELECT served_endpoint, cost_micros, cost_status FROM model_attempts WHERE attempt_id = ?1",
            [attempt_unknown.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("unknown row");
    assert!(unknown.0.is_none());
    assert!(unknown.1.is_none());
    assert_eq!(unknown.2, "unknown");
}
