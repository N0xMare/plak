use super::tests::{TestResult, all_observations, fixture_context, fixture_timestamp};
use super::*;
use plakous::agent::TurnUsage;
use std::sync::Arc;

use crate::store::session_records::{SessionRecordStore, SqliteSessionStore};
use plakous::agent::CommandId;
use plakous::sessions::{
    AgentSessions, CreateSessionOutcome, CreateSessionRequest, SessionBackend, SessionOperationId,
};
use plakous_hosted::runtime::project_session;
use plakous_hosted::sessions::SessionRecord;
use plakous_offline_crash_harness::protocol::CrashBoundary;
use plakous_offline_crash_harness::script::{SCRIPT_VERSION, ScriptPlan};
use plakous_offline_crash_harness::{BoundaryController, scripted_runtime_binding_with_cursor};
use std::sync::atomic::AtomicU64;

#[test]
fn projected_terminal_batch_is_one_atomic_telemetry_write() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    store.connection.execute_batch(
        "CREATE TRIGGER reject_second BEFORE INSERT ON turns WHEN (SELECT count(*) FROM turns)=1 BEGIN SELECT RAISE(ABORT,'second terminal'); END;",
    )?;
    let context = fixture_context()?;
    let first = plakous_hosted::sessions::projector::ProjectedTurnUsage {
        session_id: context.session_id(),
        branch_id: context.branch_id(),
        turn_id: context.turn_id().unwrap(),
        committed_at_ms: 0,
        usage: TurnUsage::default(),
    };
    let second = plakous_hosted::sessions::projector::ProjectedTurnUsage {
        turn_id: plakous::model::TurnId::try_from(uuid::Uuid::new_v4())?,
        ..first
    };
    emit_projected_turn_usages(&paths, &[first, second]);
    assert_eq!(
        store.count_rows("turns"),
        0,
        "a failed batch must roll back its first terminal too"
    );
    Ok(())
}

#[test]
fn later_diagnostics_cannot_replace_known_turn_identity() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let observations = all_observations()?;
    store.write_batch(&[TelemetryItem::Agent(observations[0])])?;
    store.connection.execute(
        "UPDATE turns SET run_id='original-run',agent_id='original-agent'",
        [],
    )?;
    for observation in &observations[..2] {
        store.write_batch(&[TelemetryItem::Agent(*observation)])?;
        assert_eq!(
            store.query_text("SELECT run_id FROM turns"),
            Some("original-run".to_owned())
        );
        assert_eq!(
            store.query_text("SELECT agent_id FROM turns"),
            Some("original-agent".to_owned())
        );
    }
    Ok(())
}

#[test]
fn terminal_diagnostics_without_projected_usage_do_not_pin_retention() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let diagnostics: Vec<_> = all_observations()?[..2]
        .iter()
        .copied()
        .map(TelemetryItem::Agent)
        .collect();
    store.write_batch(&diagnostics)?;
    assert_eq!(store.count_rows("turns"), 1);
    assert_eq!(
        store.query_i64("SELECT finished_at IS NULL FROM turns"),
        Some(1)
    );
    assert_eq!(
        store.query_i64("SELECT status IS NOT NULL FROM turns"),
        Some(1)
    );
    store.prune(
        TelemetryRetentionConfig {
            max_bytes: u64::MAX,
            max_age_days: 1,
        },
        &paths.telemetry_database(),
    )?;
    assert_eq!(store.count_rows("turns"), 0);
    Ok(())
}

#[test]
fn projected_usage_keeps_unknown_identity_empty_and_preserves_live_identity() -> TestResult {
    for live_first in [false, true] {
        let (_guard, paths) = PlakPaths::isolated_test_home();
        paths.ensure_layout().map_err(|error| error.to_string())?;
        let mut store = TelemetryStore::open(&paths)?;
        let context = fixture_context()?;
        if live_first {
            store.write_batch(&[TelemetryItem::Agent(all_observations()?[0])])?;
        }
        let usage = TelemetryItem::Product(ProductObservation::TurnUsage {
            at: fixture_timestamp()?,
            session_id: context.session_id(),
            branch_id: context.branch_id(),
            turn_id: context.turn_id().unwrap(),
            usage: TurnUsage::new(Usage::new(123, 45, 6, 7, 8), 2, 1, 0)?,
        });
        store.write_batch(&[usage, usage])?;
        assert_eq!(store.query_i64("SELECT input_tokens FROM turns"), Some(123));
        assert_eq!(
            store.query_text("SELECT run_id FROM turns"),
            Some(if live_first {
                context.run_id().to_string()
            } else {
                String::new()
            })
        );
        assert_eq!(
            store.query_text("SELECT agent_id FROM turns"),
            Some(if live_first {
                context.agent_id().to_string()
            } else {
                String::new()
            })
        );
        // A late live observation fills unknown diagnostics, without writing usage.
        store.write_batch(&[TelemetryItem::Agent(all_observations()?[0]), usage])?;
        assert_eq!(
            store.query_text("SELECT run_id FROM turns"),
            Some(context.run_id().to_string())
        );
        assert_eq!(
            store.query_text("SELECT agent_id FROM turns"),
            Some(context.agent_id().to_string())
        );
        assert_eq!(store.query_i64("SELECT input_tokens FROM turns"), Some(123));
    }
    Ok(())
}

#[test]
fn projected_usage_upsert_is_identical_and_live_timings_cannot_replace_it() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let observation = TelemetryItem::Product(ProductObservation::TurnUsage {
        at: fixture_timestamp()?,
        session_id: context.session_id(),
        branch_id: context.branch_id(),
        turn_id: context.turn_id().unwrap(),
        usage: TurnUsage::new(
            Usage::new(123, 45, 6, 7, 8).with_cost_micros(Some(42_000)),
            2,
            1,
            0,
        )?,
    });
    store.write_batch(&[observation])?;
    let rows = store.query_i64("SELECT count(*) FROM turns");
    let total = store.query_i64("SELECT sum(input_tokens) FROM turns");
    store.write_batch(&[observation])?;
    for event in all_observations()?.into_iter().filter(|event| {
        matches!(
            event.kind(),
            TelemetryObservationKind::TurnStarted { .. }
                | TelemetryObservationKind::TurnFinished { .. }
        )
    }) {
        store.write_batch(&[TelemetryItem::Agent(event)])?;
    }
    assert_eq!(store.query_i64("SELECT count(*) FROM turns"), rows);
    assert_eq!(
        store.query_i64("SELECT sum(input_tokens) FROM turns"),
        total
    );
    assert_eq!(
        store.query_i64("SELECT cost_micros FROM turns"),
        Some(42_000)
    );
    assert_eq!(
        store.query_i64("SELECT model_wall_ns FROM turns"),
        Some(8_000_000)
    );
    Ok(())
}

#[test]
fn usage_totals_survive_deleted_views_and_behind_retention_rebuilds() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let workspace = tempfile::tempdir()?;
    let controller = BoundaryController::quiet_from_evidence_path(
        CrashBoundary::None,
        workspace.path().join("usage-evidence.jsonl"),
    )
    .map_err(|error| error.to_string())?;
    let script: Arc<ScriptPlan> = Arc::new(serde_json::from_value(serde_json::json!({
        "version": SCRIPT_VERSION,
        "steps": [{"output":{"text":"done"},"usage":{"input":123,"output":45}}]
    }))?);
    let binding = scripted_runtime_binding_with_cursor(
        workspace.path(),
        controller,
        script,
        Arc::new(AtomicU64::new(1)),
    )
    .map_err(|error| error.to_string())?;
    let sessions = AgentSessions::new(SessionBackend::sqlite(paths.runtime_database())?)?;
    let product = Arc::new(SqliteSessionStore::new(paths.clone()));
    let records: Arc<dyn SessionRecordStore> = product.clone();
    let executor = plakous::application::ApplicationExecutor::new()?;
    executor.block_on(async {
        let CreateSessionOutcome::Created { session, .. } = sessions
            .create(CreateSessionRequest::new(
                SessionOperationId::new("usage:create").unwrap(),
                binding,
            ))
            .await
            .unwrap()
        else {
            panic!("created")
        };
        let locator = session.locator();
        let mut record = SessionRecord::new(workspace.path().to_owned());
        record.plakous_locator = Some(locator);
        product.save(&mut record).unwrap();
        // Seed an old cursor before the terminal: a lease-free catch-up is the emitter.
        project_session(&sessions, &records, &record).await.unwrap();
        let telemetry = TelemetryStore::open(&paths).unwrap();
        telemetry.connection.execute_batch(
            "CREATE TABLE test_usage_emissions (turn_id TEXT NOT NULL);
             CREATE TRIGGER count_usage_insert AFTER INSERT ON turns WHEN NEW.input_tokens IS NOT NULL
             BEGIN INSERT INTO test_usage_emissions VALUES (NEW.turn_id); END;
             CREATE TRIGGER count_usage_update AFTER UPDATE OF input_tokens ON turns
             BEGIN INSERT INTO test_usage_emissions VALUES (NEW.turn_id); END;",
        ).unwrap();
        let result = session
            .agent()
            .start(CommandId::new("usage:first").unwrap(), "first")
            .await
            .unwrap()
            .into_turn()
            .unwrap()
            .result()
            .await
            .unwrap();
        let duplicate = session
            .agent()
            .start(CommandId::new("usage:first").unwrap(), "first")
            .await
            .unwrap();
        assert_eq!(
            duplicate.acknowledgement().disposition(),
            plakous::agent::CommandAckDisposition::Duplicate
        );
        session.release().await.unwrap();
        project_session(&sessions, &records, &record).await.unwrap();
        let snapshot = || {
            (
                telemetry.query_i64("SELECT count(*) FROM turns"),
                telemetry.query_i64("SELECT sum(input_tokens) FROM turns"),
                telemetry.query_i64("SELECT sum(output_tokens) FROM turns"),
                telemetry.query_i64("SELECT finished_at FROM turns"),
            )
        };
        let expected = snapshot();
        let rows = || {
            let mut statement = telemetry
                .connection
                .prepare("SELECT * FROM turns ORDER BY turn_id")
                .unwrap();
            let columns = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<Result<Vec<_>, _>>()
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let expected_rows = rows();
        assert_eq!(expected.0, Some(1));
        assert_eq!(telemetry.query_i64("SELECT count(*) FROM test_usage_emissions"), Some(1));
        assert_eq!(
            expected.1,
            Some(i64::try_from(result.usage().model().input_tokens()).unwrap())
        );
        project_session(&sessions, &records, &record).await.unwrap();
        assert_eq!(telemetry.query_i64("SELECT count(*) FROM test_usage_emissions"), Some(1), "an already applied terminal must not invoke the telemetry emitter again");
        assert_eq!(
            snapshot(),
            expected,
            "an already applied terminal emits nothing"
        );
        let mut store = crate::store::ProductStore::open(&paths).unwrap();
        store.delete_views(&record.id).unwrap();
        project_session(&sessions, &records, &record).await.unwrap();
        assert_eq!(
            snapshot(),
            expected,
            "retained facts upsert identical turn usage"
        );
        assert_eq!(rows(), expected_rows);
        let connection = Connection::open(paths.runtime_database()).unwrap();
        connection.execute("DELETE FROM session_feed", []).unwrap();
        connection
            .execute(
                "UPDATE sessions SET feed_retained_from=feed_tail+1,feed_bytes=0",
                [],
            )
            .unwrap();
        store.delete_views(&record.id).unwrap();
        project_session(&sessions, &records, &record).await.unwrap();
        assert!(
            product
                .load_views(&record.id)
                .unwrap()
                .unwrap()
                .state
                .retained_from
                .is_some()
        );
        assert_eq!(
            snapshot(),
            expected,
            "pruned history never invents or removes usage"
        );
        assert_eq!(rows(), expected_rows);
    })?;
    Ok(())
}
