#![allow(
    clippy::too_many_lines,
    reason = "Telemetry fixtures construct every observation kind so schema and binding coverage stays exhaustive."
)]

use super::*;
use std::sync::Arc;
use std::time::Duration;

use plakous::agent::{
    DurabilityAttemptKind, DurabilityAttemptOutcome, TelemetryContext, TelemetryObservation,
    TelemetryObservationKind, TelemetryTerminalStatus, TurnUsage,
};
use plakous::model::{
    AgentId, AttemptId, BranchId, CallId, RunId, SessionId, Timestamp, TurnId, Usage,
};
use uuid::Uuid;

use crate::telemetry_sink::TelemetryWriter;
use plakous_hosted::product_observation::ProductObservation;

pub(super) type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

pub(super) fn fixture_context() -> TestResult<TelemetryContext> {
    Ok(TelemetryContext::new(
        AgentId::try_from(Uuid::from_u128(1))?,
        SessionId::try_from(Uuid::from_u128(2))?,
        BranchId::try_from(Uuid::from_u128(3))?,
        RunId::try_from(Uuid::from_u128(4))?,
    )
    .with_turn(TurnId::try_from(Uuid::from_u128(5))?)
    .with_attempt(AttemptId::try_from(Uuid::from_u128(6))?)
    .with_call(CallId::try_from(Uuid::from_u128(7))?))
}

pub(super) fn fixture_timestamp() -> TestResult<Timestamp> {
    Ok(Timestamp::new(8, 9)?)
}

#[test]
fn clean_commit_has_one_row_but_failed_and_recovery_attempts_are_retained() -> TestResult {
    use plakous::agent::{DurabilityAttemptKind as K, DurabilityAttemptOutcome as O};
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let at = fixture_timestamp()?;
    store.write_batch(&[
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::DurabilityAttemptFinished {
                elapsed: Duration::ZERO,
                attempt: K::InitialCommit,
                outcome: O::Committed,
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::DurabilityFinished {
                elapsed: Duration::ZERO,
                status: TelemetryTerminalStatus::Completed,
            },
        )),
    ])?;
    assert_eq!(
        store.query_i64("SELECT count(*) FROM durability_attempts"),
        Some(1)
    );
    for (attempt, outcome) in [
        (K::InitialCommit, O::Failed),
        (
            K::InitialAcknowledgementRecovery,
            O::RecoveredAcknowledgement,
        ),
    ] {
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::DurabilityAttemptFinished {
                elapsed: Duration::ZERO,
                attempt,
                outcome,
            },
        ))])?;
    }
    assert_eq!(
        store.query_i64("SELECT count(*) FROM durability_attempts"),
        Some(3)
    );
    Ok(())
}

#[test]
fn presented_tool_and_delta_fingerprint_are_recorded() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let at = fixture_timestamp()?;
    let tool = plakous::model::ToolName::new("functions.exec")?;
    store.write_batch(&[
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::ToolStarted {
                tool: (&tool).into(),
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::ModelFinished {
                elapsed: Duration::ZERO,
                status: TelemetryTerminalStatus::Completed,
                usage: Some(Usage::new(10, 20, 0, 0, 0)),
                served_by: None,
                output_deltas: 4,
                measurement: plakous::agent::ModelMeasurement::absent(),
            },
        )),
    ])?;
    let name: String =
        store
            .connection
            .query_row("SELECT tool_name FROM tool_calls", [], |row| row.get(0))?;
    let fingerprint: String = store.connection.query_row(
        "SELECT served_path_fingerprint FROM model_attempts",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(name, "functions.exec");
    assert_eq!(fingerprint, "dpt-v1:200");
    Ok(())
}

#[test]
fn turn_origin_is_stored_for_user_prompts_and_host_wakes() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    for origin in [
        plakous::model::TurnOrigin::UserPrompt,
        plakous::model::TurnOrigin::HostWake,
    ] {
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            fixture_context()?,
            fixture_timestamp()?,
            TelemetryObservationKind::TurnStarted {
                origin,
                queue_time: Duration::ZERO,
            },
        ))])?;
        let stored: String = store
            .connection
            .query_row("SELECT origin FROM turns", [], |row| row.get(0))?;
        assert_eq!(stored, origin.as_str());
    }
    Ok(())
}

pub(super) fn all_observations() -> TestResult<Vec<TelemetryObservation>> {
    let context = fixture_context()?;
    let timestamp = fixture_timestamp()?;
    Ok(vec![
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::TurnStarted {
                origin: plakous::model::TurnOrigin::UserPrompt,
                queue_time: Duration::from_millis(10),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::TurnFinished {
                elapsed: Duration::from_millis(20),
                status: TelemetryTerminalStatus::Completed,
                usage: TurnUsage::new(Usage::new(11, 12, 13, 14, 15), 1, 1, 2)?,
                model_wall: Duration::from_millis(8),
                tool_wall: Duration::from_millis(6),
                compaction_wall: Duration::from_millis(2),
                model_attempts: 1,
                tool_calls: 1,
                compactions: 0,
                prepared_input_high_water: None,
            },
        ),
        TelemetryObservation::new(context, timestamp, TelemetryObservationKind::ModelStarted),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ModelFirstEvent {
                elapsed: Duration::from_millis(30),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ModelFinished {
                elapsed: Duration::from_millis(40),
                status: TelemetryTerminalStatus::Completed,
                usage: Some(Usage::new(21, 22, 23, 24, 25)),
                served_by: None,
                output_deltas: 0,
                measurement: plakous::agent::ModelMeasurement::absent(),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ToolStarted {
                tool: (&plakous::model::ToolName::new("test").unwrap()).into(),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ToolFinished {
                elapsed: Duration::from_millis(50),
                status: TelemetryTerminalStatus::Completed,
                output_bytes: 128,
                measurement: plakous::agent::ToolMeasurement::absent(),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ToolObservationClipped {
                bytes_kept: 64,
                bytes_dropped: 64,
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::CompactionStarted {
                reason: CompactionEventReason::Manual,
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::CompactionFinished {
                refusal: None,
                elapsed: Duration::from_millis(55),
                outcome: CompactionEventOutcome::Committed,
                reason: CompactionEventReason::Manual,
                tokens_before: Some(100),
                tokens_after: Some(40),
                usage: Some(Usage::new(1, 2, 3, 4, 5)),
                measurement: plakous::agent::CompactionMeasurement::absent(),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::DurabilityFinished {
                elapsed: Duration::from_millis(60),
                status: TelemetryTerminalStatus::Completed,
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::RetentionFinished {
                elapsed: Duration::from_millis(7),
                status: TelemetryTerminalStatus::Failed,
                pruned_checkpoints: 0,
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::ChildProcessExited {
                child: plakous::agent::ChildProcessKind::JsEvalSidecar,
                user_cpu: Duration::from_millis(2),
                system_cpu: Duration::from_millis(1),
                peak_rss_bytes: Some(4096),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::CredentialRefresh {
                provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                phase: CredentialRefreshPhase::OutcomeUnknown,
                failure_class: Some(CredentialRefreshFailureClass::Timeout),
                transport_phase: Some(plakous::model::ModelRetryPhase::Resolution),
                http_status: Some(503),
                elapsed: Some(Duration::from_millis(30)),
                generation: 40,
                detail: plakous::agent::CredentialRefreshDetail::default(),
            },
        ),
        TelemetryObservation::new(
            context,
            timestamp,
            TelemetryObservationKind::PromptCacheDiagnosis {
                reason: plakous::model::PromptCacheMissReason::MessagesChanged,
                missed_input_tokens: Some(587),
            },
        ),
    ])
}

#[test]
fn durability_variants_map_to_stable_snake_case() {
    assert_eq!(
        durability_attempt_kind_text(DurabilityAttemptKind::InitialCommit),
        "initial_commit"
    );
    assert_eq!(
        durability_attempt_kind_text(DurabilityAttemptKind::InitialAcknowledgementRecovery),
        "initial_acknowledgement_recovery"
    );
    assert_eq!(
        durability_attempt_outcome_text(DurabilityAttemptOutcome::Committed),
        "committed"
    );
    assert_eq!(
        durability_attempt_outcome_text(DurabilityAttemptOutcome::Failed),
        "failed"
    );
    assert_eq!(
        durability_attempt_outcome_text(DurabilityAttemptOutcome::RecoveredAcknowledgement),
        "recovered_acknowledgement"
    );
}

#[test]
fn turn_finished_cannot_write_projected_usage() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        context,
        fixture_timestamp()?,
        TelemetryObservationKind::TurnFinished {
            elapsed: Duration::from_millis(20),
            status: TelemetryTerminalStatus::Completed,
            usage: TurnUsage::new(
                Usage::new(1, 2, 0, 0, 0).with_cost_micros(Some(42_000)),
                1,
                1,
                0,
            )?,
            model_wall: Duration::ZERO,
            tool_wall: Duration::ZERO,
            compaction_wall: Duration::ZERO,
            model_attempts: 1,
            tool_calls: 0,
            compactions: 0,
            prepared_input_high_water: None,
        },
    ))])?;
    assert_eq!(store.query_i64("SELECT cost_micros FROM turns"), None);
    assert_eq!(store.query_i64("SELECT input_tokens FROM turns"), None);
    Ok(())
}

#[test]
fn same_call_id_in_two_turns_keeps_two_tool_rows() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let first = fixture_context()?;
    let second = first.with_turn(TurnId::try_from(Uuid::from_u128(55))?);
    let finished = |context| {
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            fixture_timestamp().expect("fixture timestamp"),
            TelemetryObservationKind::ToolFinished {
                elapsed: Duration::from_millis(5),
                status: TelemetryTerminalStatus::Completed,
                output_bytes: 1,
                measurement: plakous::agent::ToolMeasurement::absent(),
            },
        ))
    };
    let started = |context| {
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            fixture_timestamp().expect("fixture timestamp"),
            TelemetryObservationKind::ToolStarted {
                tool: (&plakous::model::ToolName::new("test").unwrap()).into(),
            },
        ))
    };
    store.write_batch(&[
        started(first),
        finished(first),
        started(second),
        finished(second),
    ])?;
    assert_eq!(store.query_i64("SELECT count(*) FROM tool_calls"), Some(2));
    assert_eq!(
        store.query_i64("SELECT count(DISTINCT turn_id) FROM tool_calls"),
        Some(2)
    );
    Ok(())
}

#[test]
fn every_observation_kind_lands_as_one_row() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    let writer = TelemetryWriter::spawn(store, 4096)?;
    let sink: Arc<dyn plakous::agent::TelemetrySink> =
        Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
    let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
    let context = fixture_context()?;
    writer.emit_product(ProductObservation::TurnUsage {
        at: fixture_timestamp()?,
        session_id: context.session_id(),
        branch_id: context.branch_id(),
        turn_id: context.turn_id().unwrap(),
        usage: TurnUsage::new(Usage::new(11, 12, 13, 14, 15), 1, 1, 2)?,
    });
    for observation in all_observations()? {
        emitter.emit(observation);
    }
    let status = writer.close_and_join()?;
    assert_eq!(status.write_errors(), 0);

    let store = TelemetryStore::open(&paths)?;
    assert_eq!(store.count_rows("turns"), 1);
    assert_eq!(store.count_rows("model_attempts"), 1);
    assert_eq!(store.count_rows("tool_calls"), 1);
    assert_eq!(store.count_rows("durability_attempts"), 1);
    assert_eq!(store.count_rows("retention_runs"), 1);
    assert_eq!(
        TelemetryStore::unresolved_retention_failure(&paths)?,
        store.query_i64("SELECT at FROM retention_runs")
    );

    assert_eq!(store.count_rows("compactions"), 1);
    assert_eq!(store.count_rows("resource_samples"), 4);
    assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
    assert_eq!(
        store.query_text("SELECT reason FROM prompt_cache_diagnoses"),
        Some("messages_changed".to_owned())
    );
    assert_eq!(
        store.query_i64("SELECT missed_input_tokens FROM prompt_cache_diagnoses"),
        Some(587)
    );

    assert_eq!(
        store.query_i64("SELECT model_wall_ns FROM turns").unwrap(),
        duration_ns(Duration::from_millis(8))
    );
    assert_eq!(
        store
            .query_i64("SELECT output_bytes FROM tool_calls")
            .unwrap(),
        128
    );
    assert_eq!(
        store
            .query_i64("SELECT ttft_ns FROM model_attempts")
            .unwrap(),
        duration_ns(Duration::from_millis(30))
    );
    assert_eq!(
        store
            .query_i64("SELECT ttlb_ns FROM model_attempts")
            .unwrap(),
        duration_ns(Duration::from_millis(40))
    );
    assert_eq!(
        store.query_i64("SELECT cache_write_tokens FROM model_attempts"),
        Some(25)
    );
    assert_eq!(
        store.query_i64("SELECT cache_write_tokens FROM turns"),
        Some(15)
    );
    assert_eq!(
        store.query_text("SELECT session_id FROM turns").unwrap(),
        SessionId::try_from(Uuid::from_u128(2))?.to_string()
    );
    Ok(())
}

#[test]
fn version_mismatch_refuses_without_deleting_rows() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    {
        let store = TelemetryStore::open(&paths)?;
        let writer = TelemetryWriter::spawn(store, 16)?;
        let sink: Arc<dyn plakous::agent::TelemetrySink> =
            Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
        let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
        emitter.emit(all_observations()?[0]);
        writer.close_and_join()?;
    }
    {
        let connection = Connection::open(paths.telemetry_database())?;
        connection.pragma_update(None, "user_version", 99)?;
    }
    assert!(TelemetryStore::open(&paths).is_err());
    let connection = Connection::open(paths.telemetry_database())?;
    let rows: i64 = connection.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
    assert_eq!(rows, 1);
    Ok(())
}

#[test]
fn compaction_finished_without_start_inserts_finished_only_row() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        context,
        fixture_timestamp()?,
        TelemetryObservationKind::CompactionFinished {
            refusal: None,
            elapsed: Duration::from_millis(12),
            outcome: CompactionEventOutcome::NotNeeded,
            reason: CompactionEventReason::Scheduled,
            tokens_before: None,
            tokens_after: None,
            usage: None,
            measurement: plakous::agent::CompactionMeasurement::absent(),
        },
    ))])?;
    assert_eq!(store.count_rows("compactions"), 1);
    assert_eq!(
        store.query_text("SELECT outcome FROM compactions").unwrap(),
        "not_needed"
    );
    assert!(
        store
            .query_i64("SELECT started_at FROM compactions")
            .is_none()
    );
    Ok(())
}

#[test]
fn compaction_cost_tracks_reported_completed_and_unknown_responses() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let at = fixture_timestamp()?;
    for (outcome, usage) in [
        (
            CompactionEventOutcome::Committed,
            Some(Usage::new(11, 2, 1, 3, 4).with_cost_micros(Some(125))),
        ),
        (
            CompactionEventOutcome::Failed,
            Some(Usage::new(21, 5, 4, 6, 0).with_cost_micros(Some(666))),
        ),
        (
            CompactionEventOutcome::Committed,
            Some(Usage::new(31, 8, 0, 1, 0)),
        ),
        (CompactionEventOutcome::Failed, None),
    ] {
        store.write_batch(&[
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::CompactionStarted {
                    reason: CompactionEventReason::Scheduled,
                },
            )),
            TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::CompactionFinished {
                    refusal: None,
                    elapsed: Duration::from_millis(1),
                    outcome,
                    reason: CompactionEventReason::Scheduled,
                    tokens_before: None,
                    tokens_after: None,
                    usage,
                    measurement: plakous::agent::CompactionMeasurement::absent(),
                },
            )),
        ])?;
    }
    let mut rows = store.connection.prepare(
        "SELECT outcome, input_tokens, cost_micros, cost_status FROM compactions ORDER BY id",
    )?;
    let actual = rows
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<i64>>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(
        actual,
        vec![
            (
                "committed".to_owned(),
                Some(11),
                Some(125),
                "reported".to_owned()
            ),
            (
                "failed".to_owned(),
                Some(21),
                Some(666),
                "reported".to_owned()
            ),
            ("committed".to_owned(), Some(31), None, "unknown".to_owned()),
            ("failed".to_owned(), None, None, "unknown".to_owned()),
        ]
    );
    Ok(())
}

#[test]
fn product_observations_land_in_product_events() -> TestResult {
    use plakous_hosted::product_observation::{
        BoundedText64, JsonlCommandKind, ProductObservation, ResourceSamplePhase,
        session_id_from_text, wall_timestamp,
    };

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session_id = SessionId::try_from(Uuid::from_u128(2))?;
    let tree_id = BoundedText64::new("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap();
    let node_id = BoundedText64::new("root").unwrap();
    store.write_batch(&[
        TelemetryItem::Product(ProductObservation::TreeNodeSpawned {
            at: wall_timestamp(),
            session_id,
            tree_id,
            node_id,
            parent_node_id: None,
            template: BoundedText64::new("review").unwrap(),
        }),
        TelemetryItem::Product(ProductObservation::JsonlCommand {
            at: wall_timestamp(),
            session_id,
            kind: JsonlCommandKind::Prompt,
            admitted_to_completed: Duration::from_millis(4),
        }),
        TelemetryItem::Product(ProductObservation::SessionOpened {
            at: wall_timestamp(),
            session_id,
            elapsed: Duration::from_millis(5),
        }),
        TelemetryItem::Product(ProductObservation::ResourceSample {
            at: wall_timestamp(),
            session_id,
            agent_id: Some(AgentId::try_from(Uuid::from_u128(1))?),
            phase: ResourceSamplePhase::TurnStarted,
            child_kind: None,
            rss_bytes: Some(1024),
            peak_rss_bytes: Some(2048),
            user_cpu: Duration::from_millis(1),
            system_cpu: Duration::from_millis(1),
            threads: Some(2),
            fds: Some(3),
        }),
    ])?;
    assert_eq!(store.count_rows("product_events"), 3);
    assert_eq!(store.count_rows("resource_samples"), 1);
    assert!(session_id_from_text("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").is_some());
    Ok(())
}

#[test]
fn semantic_activity_and_declined_retry_columns_are_available() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    assert_eq!(store.query_i64("SELECT COUNT(*) FROM pragma_table_info('model_attempts') WHERE name = 'keepalive_count'"), Some(1));
    assert_eq!(
        store.query_i64(
            "SELECT COUNT(*) FROM pragma_table_info('model_retries') WHERE name = 'declined_reason'"
        ),
        Some(1)
    );
    Ok(())
}

#[test]
fn semantic_activity_rows_retain_counts_and_the_last_semantic_time() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        fixture_context()?,
        Timestamp::new(500, 0)?,
        TelemetryObservationKind::ModelStreamActivity {
            activity: plakous::model::ModelStreamActivity {
                output_started: true,
                last_semantic_event_elapsed_ms: Some(100),
                semantic_gap_ms: 300_000,
                keepalive_count: 7,
                max_semantic_gap_ms: 300_000,
            },
        },
    ))])?;
    assert_eq!(
        store.query_i64("SELECT last_semantic_event_at FROM model_attempts"),
        Some(200_000_000_000)
    );
    assert_eq!(
        store.query_i64("SELECT keepalive_count FROM model_attempts"),
        Some(7)
    );
    assert_eq!(
        store.query_i64("SELECT max_semantic_gap_ms FROM model_attempts"),
        Some(300_000)
    );
    Ok(())
}

#[test]
fn replay_loss_rows_are_content_free_and_pruned_with_their_session() -> TestResult {
    use plakous::model::{ModelRetryCause, ModelRetryLoss, ModelRetryPhase};
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session_id = SessionId::try_from(Uuid::from_u128(31))?;
    for next_attempt in [2, 3] {
        store.write_batch(&[TelemetryItem::Product(ProductObservation::ModelRetry {
            at: Timestamp::new(1, 0)?,
            session_id,
            branch_id: BranchId::try_from(Uuid::from_u128(32))?,
            agent_id: AgentId::try_from(Uuid::from_u128(33))?,
            turn_id: TurnId::try_from(Uuid::from_u128(34))?,
            next_attempt,
            max_attempts: 4,
            cause: if next_attempt == 2 {
                ModelRetryCause::RecordRejectedInTransit
            } else {
                ModelRetryCause::StreamInterrupted {
                    fault: plakous::model::ModelTransportFault::EndOfStream,
                }
            },
            loss: ModelRetryLoss {
                assistant_text_bytes: 7,
                reasoning_bytes: 11,
                completed_items: 2,
                elapsed_ms: 17,
                server_effect: false,
                transport_phase: Some(if next_attempt == 2 {
                    ModelRetryPhase::RequestWriteOrResponseHead
                } else {
                    ModelRetryPhase::ResponseBody
                }),
                declined_reason: (next_attempt == 3)
                    .then_some(plakous::model::ModelRetryDeclineReason::Budget),
            },
        })])?;
    }
    assert_eq!(
        store.query_i64("SELECT COUNT(*) FROM v_replay_loss"),
        Some(2)
    );
    assert_eq!(store.query_i64("PRAGMA user_version"), Some(21));
    assert_eq!(
        store.query_i64(
            "SELECT COUNT(*) FROM model_retries WHERE cause_kind = 'record_rejected_in_transit'"
        ),
        Some(1)
    );
    assert_eq!(
        store.query_i64("SELECT COUNT(*) FROM model_retries WHERE declined_reason = 'budget'"),
        Some(1)
    );

    assert_eq!(
        store.query_i64("SELECT SUM(assistant_text_bytes + reasoning_bytes) FROM v_replay_loss"),
        Some(36)
    );
    assert_eq!(
        store
            .query_text("SELECT replay_safety FROM v_replay_loss LIMIT 1")
            .as_deref(),
        Some("cost_only")
    );
    assert_eq!(
        store
            .query_text("SELECT transport_phase FROM v_replay_loss WHERE cause_kind = 'record_rejected_in_transit'")
            .as_deref(),
        Some("request_write_or_response_head")
    );
    delete_session_rows(&store.connection, &id_text(session_id))?;
    assert_eq!(store.count_rows("model_retries"), 0);
    Ok(())
}

#[test]
fn tree_node_detached_status_lands_in_product_events() -> TestResult {
    use plakous_hosted::product_observation::{
        BoundedText64, NodeDetachStatus, ProductObservation, wall_timestamp,
    };

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session_id = SessionId::try_from(Uuid::from_u128(3))?;
    let tree_id = BoundedText64::new("bbbbbbbb-cccc-dddd-eeee-ffffffffffff").unwrap();
    let node_id = BoundedText64::new("c1").unwrap();
    store.write_batch(&[TelemetryItem::Product(
        ProductObservation::TreeNodeDetached {
            at: wall_timestamp(),
            session_id,
            tree_id,
            node_id,
            parent_node_id: BoundedText64::new("root"),
            status: NodeDetachStatus::Closed,
        },
    )])?;
    assert_eq!(store.count_rows("product_events"), 1);
    assert_eq!(
        store
            .query_text("SELECT status FROM product_events WHERE kind = 'tree_node_detached'")
            .unwrap(),
        "closed"
    );
    assert!(
        store
            .query_text("SELECT spawn_class FROM product_events WHERE kind = 'tree_node_detached'")
            .is_none()
    );
    Ok(())
}

#[test]
fn resource_sample_has_nonzero_cpu_and_peak_rss() -> TestResult {
    use plakous_hosted::product_observation::{
        ProductObservation, ResourceSamplePhase, wall_timestamp,
    };
    use plakous_hosted::runtime::sample_process_resources;

    let snapshot = sample_process_resources();
    assert!(
        snapshot.user_cpu > Duration::ZERO || snapshot.system_cpu > Duration::ZERO,
        "expected non-zero CPU sample on this OS"
    );
    assert!(
        snapshot.peak_rss_bytes.unwrap_or(0) > 0,
        "expected peak RSS on this OS"
    );

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session_id = SessionId::try_from(Uuid::from_u128(2))?;
    store.write_batch(
        &[TelemetryItem::Product(ProductObservation::ResourceSample {
            at: wall_timestamp(),
            session_id,
            agent_id: None,
            phase: ResourceSamplePhase::TurnFinished,
            child_kind: None,
            rss_bytes: snapshot.rss_bytes,
            peak_rss_bytes: snapshot.peak_rss_bytes,
            user_cpu: snapshot.user_cpu,
            system_cpu: snapshot.system_cpu,
            threads: snapshot.threads,
            fds: snapshot.fds,
        })],
    )?;
    let user_cpu = store
        .query_i64("SELECT user_cpu_ns FROM resource_samples")
        .unwrap();
    let system_cpu = store
        .query_i64("SELECT system_cpu_ns FROM resource_samples")
        .unwrap();
    let peak = store
        .query_i64("SELECT peak_rss_bytes FROM resource_samples")
        .unwrap_or(0);
    assert!((user_cpu + system_cpu) > 0);
    assert!(peak > 0);
    Ok(())
}

#[test]
fn child_exited_sample_persists_child_kind() -> TestResult {
    use plakous::agent::{
        ChildProcessKind, TelemetryContext, TelemetryObservation, TelemetryObservationKind,
    };
    use plakous::model::{AgentId, BranchId, RunId, SessionId, Timestamp};

    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session_id = SessionId::try_from(Uuid::from_u128(42))?;
    let agent_id = AgentId::try_from(Uuid::from_u128(43))?;
    let branch_id = BranchId::try_from(Uuid::from_u128(44))?;
    let run_id = RunId::try_from(Uuid::from_u128(45))?;
    let context = TelemetryContext::new(agent_id, session_id, branch_id, run_id);
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        context,
        Timestamp::new(100, 0)?,
        TelemetryObservationKind::ChildProcessExited {
            child: ChildProcessKind::JsEvalSidecar,
            user_cpu: Duration::from_millis(3),
            system_cpu: Duration::from_millis(1),
            peak_rss_bytes: Some(8192),
        },
    ))])?;
    assert_eq!(store.count_rows("resource_samples"), 1);
    assert_eq!(
        store
            .query_text("SELECT phase FROM resource_samples")
            .unwrap(),
        "child_exited"
    );
    assert_eq!(
        store
            .query_text("SELECT child_kind FROM resource_samples")
            .unwrap(),
        "js_eval_sidecar"
    );
    Ok(())
}

#[test]
fn prune_by_age_removes_whole_old_sessions() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let old_session = SessionId::try_from(Uuid::from_u128(10))?;
    let new_session = SessionId::try_from(Uuid::from_u128(11))?;
    let mut store = TelemetryStore::open(&paths)?;
    store.write_batch(&[
        TelemetryItem::Product(ProductObservation::SessionOpened {
            at: Timestamp::new(1, 0)?,
            session_id: old_session,
            elapsed: Duration::from_millis(1),
        }),
        TelemetryItem::Product(ProductObservation::SessionOpened {
            at: Timestamp::new(9_999_999_999, 0)?,
            session_id: new_session,
            elapsed: Duration::from_millis(1),
        }),
    ])?;
    store.prune(
        TelemetryRetentionConfig {
            max_bytes: u64::MAX,
            max_age_days: 1,
        },
        &paths.telemetry_database(),
    )?;
    assert_eq!(store.count_rows("product_events"), 1);
    assert_eq!(
        store
            .query_text("SELECT session_id FROM product_events")
            .unwrap(),
        new_session.to_string()
    );
    Ok(())
}

fn session_context(session: SessionId) -> TestResult<TelemetryContext> {
    Ok(TelemetryContext::new(
        AgentId::try_from(Uuid::from_u128(1))?,
        session,
        BranchId::try_from(Uuid::from_u128(3))?,
        RunId::try_from(Uuid::from_u128(4))?,
    ))
}

fn cache_diagnosis_item(session: SessionId, at: Timestamp) -> TestResult<TelemetryItem> {
    Ok(TelemetryItem::Agent(TelemetryObservation::new(
        session_context(session)?,
        at,
        TelemetryObservationKind::PromptCacheDiagnosis {
            reason: plakous::model::PromptCacheMissReason::MessagesChanged,
            missed_input_tokens: Some(4),
        },
    )))
}

fn job_event_item(session: SessionId, at: Timestamp) -> TestResult<TelemetryItem> {
    use plakous::agent::{JobHintSource, JobTool};
    Ok(TelemetryItem::Agent(TelemetryObservation::new(
        session_context(session)?,
        at,
        TelemetryObservationKind::JobPromoted {
            tool: JobTool::Start,
            nested: false,
            hint_source: JobHintSource::Default,
            hint_ms: 1,
        },
    )))
}

#[test]
fn prompt_cache_diagnoses_are_counted_and_nullable() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let columns = {
        let mut statement = store.connection.prepare(
            "SELECT name, \"notnull\" FROM pragma_table_info('prompt_cache_diagnoses') ORDER BY cid",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    assert_eq!(
        columns,
        [
            ("id", 1),
            ("at", 1),
            ("session_id", 1),
            ("branch_id", 1),
            ("run_id", 1),
            ("agent_id", 1),
            ("turn_id", 0),
            ("attempt_id", 0),
            ("reason", 1),
            ("missed_input_tokens", 0),
        ]
        .map(|(name, required)| (name.to_owned(), required))
    );
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        fixture_context()?,
        fixture_timestamp()?,
        TelemetryObservationKind::PromptCacheDiagnosis {
            reason: plakous::model::PromptCacheMissReason::Unavailable,
            missed_input_tokens: None,
        },
    ))])?;
    assert_eq!(
        store.query_i64("SELECT missed_input_tokens IS NULL FROM prompt_cache_diagnoses"),
        Some(1)
    );
    let status = store.status_snapshot()?;
    assert!(
        status
            .table_counts
            .iter()
            .any(|(name, count)| { name == "prompt_cache_diagnoses" && *count == 1 })
    );
    assert_eq!(TelemetryStore::count_all_rows(&paths)?, 1);
    Ok(())
}

#[test]
fn prune_by_age_removes_prompt_cache_diagnoses_and_job_events() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let old_session = SessionId::try_from(Uuid::from_u128(30))?;
    let new_session = SessionId::try_from(Uuid::from_u128(31))?;
    let old_at = Timestamp::new(1, 0)?;
    let new_at = Timestamp::new(9_999_999_999, 0)?;
    let mut store = TelemetryStore::open(&paths)?;
    store.write_batch(&[
        cache_diagnosis_item(old_session, old_at)?,
        job_event_item(old_session, old_at)?,
        cache_diagnosis_item(new_session, new_at)?,
        job_event_item(new_session, new_at)?,
    ])?;
    store.prune(
        TelemetryRetentionConfig {
            max_bytes: u64::MAX,
            max_age_days: 1,
        },
        &paths.telemetry_database(),
    )?;
    assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
    assert_eq!(store.count_rows("job_events"), 1);
    assert_eq!(
        store.query_text("SELECT session_id FROM prompt_cache_diagnoses"),
        Some(new_session.to_string())
    );
    assert_eq!(
        store.query_text("SELECT session_id FROM job_events"),
        Some(new_session.to_string())
    );
    Ok(())
}

#[test]
fn prune_by_bytes_removes_prompt_cache_diagnoses_and_job_events() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let old_session = SessionId::try_from(Uuid::from_u128(32))?;
    let new_session = SessionId::try_from(Uuid::from_u128(33))?;
    let mut store = TelemetryStore::open(&paths)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    let old_at = Timestamp::new(now.as_secs().saturating_sub(3_600), 0)?;
    let new_at = Timestamp::new(now.as_secs(), 0)?;
    fill_session(&mut store, old_session, old_at, 400)?;
    fill_session(&mut store, new_session, new_at, 400)?;
    store.write_batch(&[
        cache_diagnosis_item(old_session, old_at)?,
        job_event_item(old_session, old_at)?,
        cache_diagnosis_item(new_session, new_at)?,
        job_event_item(new_session, new_at)?,
    ])?;
    assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
    let database = paths.telemetry_database();
    let size_both = TelemetryStore::database_size_bytes(&database);
    let outcome = store.prune(
        TelemetryRetentionConfig {
            max_bytes: size_both.saturating_sub(1),
            max_age_days: 90,
        },
        &database,
    )?;
    assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
    assert_eq!(store.count_rows("prompt_cache_diagnoses"), 1);
    assert_eq!(store.count_rows("job_events"), 1);
    assert_eq!(
        store.query_text("SELECT session_id FROM prompt_cache_diagnoses"),
        Some(new_session.to_string())
    );
    assert_eq!(
        store.query_text("SELECT session_id FROM job_events"),
        Some(new_session.to_string())
    );
    Ok(())
}

fn recent_timestamp() -> TestResult<Timestamp> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    Ok(Timestamp::new(now.as_secs(), 0)?)
}

fn session_opened(session: SessionId, at: Timestamp) -> TelemetryItem {
    TelemetryItem::Product(ProductObservation::SessionOpened {
        at,
        session_id: session,
        elapsed: Duration::from_millis(1),
    })
}

fn wal_file_len(database: &Path) -> u64 {
    sqlite_file_len(&sqlite_sidecar(database, "-wal"))
}

fn fill_session(
    store: &mut TelemetryStore,
    session: SessionId,
    at: Timestamp,
    rows: usize,
) -> TestResult {
    for _ in 0..rows {
        store.write_batch(&[session_opened(session, at)])?;
    }
    Ok(())
}

#[test]
fn telemetry_wal_growth_counts_toward_the_byte_cap() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session = SessionId::try_from(Uuid::from_u128(21))?;
    fill_session(&mut store, session, recent_timestamp()?, 128)?;
    let database = paths.telemetry_database();
    let main = sqlite_file_len(&database);
    let wal = wal_file_len(&database);
    let shm = sqlite_file_len(&sqlite_sidecar(&database, "-shm"));
    let total = TelemetryStore::database_size_bytes(&database);
    assert!(
        wal > 0,
        "expected WAL growth after uncheckpointed writes, wal={wal}"
    );
    assert_eq!(total, main.saturating_add(wal).saturating_add(shm));
    assert!(
        total > main,
        "byte cap must count WAL/SHM, main={main} total={total}"
    );
    Ok(())
}

#[test]
fn telemetry_prune_checkpoint_truncates_wal() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session = SessionId::try_from(Uuid::from_u128(22))?;
    fill_session(&mut store, session, recent_timestamp()?, 128)?;
    let database = paths.telemetry_database();
    let wal_before = wal_file_len(&database);
    assert!(wal_before > 0, "expected a WAL file before prune");
    let outcome = store.prune(
        TelemetryRetentionConfig {
            max_bytes: u64::MAX,
            max_age_days: 90,
        },
        &database,
    )?;
    assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
    let wal_after = wal_file_len(&database);
    assert!(
        wal_after < wal_before,
        "TRUNCATE should shrink the WAL, before={wal_before} after={wal_after}"
    );
    assert_eq!(store.count_rows("product_events"), 128);
    Ok(())
}

#[test]
fn telemetry_byte_cap_is_met_without_over_deleting() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let old_session = SessionId::try_from(Uuid::from_u128(23))?;
    let new_session = SessionId::try_from(Uuid::from_u128(24))?;
    let mut store = TelemetryStore::open(&paths)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    let old_at = Timestamp::new(now.as_secs().saturating_sub(3_600), 0)?;
    let new_at = Timestamp::new(now.as_secs(), 0)?;
    fill_session(&mut store, old_session, old_at, 400)?;
    fill_session(&mut store, new_session, new_at, 400)?;
    assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
    let database = paths.telemetry_database();
    let size_both = TelemetryStore::database_size_bytes(&database);
    assert!(size_both > 1, "expected allocated pages for two sessions");
    let outcome = store.prune(
        TelemetryRetentionConfig {
            max_bytes: size_both.saturating_sub(1),
            max_age_days: 90,
        },
        &database,
    )?;
    assert_eq!(outcome.wal_checkpoint, WalCheckpointOutcome::Completed);
    assert_eq!(store.count_rows("product_events"), 400);
    assert_eq!(
        store.query_text("SELECT DISTINCT session_id FROM product_events"),
        Some(new_session.to_string())
    );
    assert!(
        TelemetryStore::database_size_bytes(&database) <= size_both.saturating_sub(1),
        "cap must be satisfied after prune+checkpoint"
    );
    Ok(())
}

#[test]
fn telemetry_wal_checkpoint_reports_busy_readers() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session = SessionId::try_from(Uuid::from_u128(25))?;
    fill_session(&mut store, session, recent_timestamp()?, 8)?;
    let reader = Connection::open(paths.telemetry_database())?;
    reader.execute_batch("BEGIN")?;
    let _held: i64 =
        reader.query_row("SELECT COUNT(*) FROM product_events", [], |row| row.get(0))?;
    assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Busy);
    reader.execute_batch("COMMIT")?;
    assert_eq!(store.checkpoint_wal()?, WalCheckpointOutcome::Completed);
    Ok(())
}

#[test]
fn telemetry_busy_reader_does_not_delete_sessions_to_chase_wal_bytes() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session = SessionId::try_from(Uuid::from_u128(26))?;
    fill_session(&mut store, session, recent_timestamp()?, 8)?;
    let reader = Connection::open(paths.telemetry_database())?;
    reader.execute_batch("BEGIN")?;
    let _: i64 = reader.query_row("SELECT COUNT(*) FROM product_events", [], |row| row.get(0))?;
    let outcome = store.prune(
        TelemetryRetentionConfig {
            max_bytes: 1,
            max_age_days: 90,
        },
        &paths.telemetry_database(),
    )?;
    assert!(outcome.wal_checkpoint.is_busy());
    assert_eq!(store.count_rows("product_events"), 8);
    reader.execute_batch("COMMIT")?;
    Ok(())
}

#[test]
fn telemetry_prune_keeps_sessions_with_unfinished_turns() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let session = SessionId::try_from(Uuid::from_u128(27))?;
    fill_session(&mut store, session, recent_timestamp()?, 8)?;
    store.connection.execute(
        "INSERT INTO turns(session_id, branch_id, run_id, agent_id, turn_id, started_at) VALUES (?1,'branch','run','agent','turn',1)",
        [session.to_string()],
    )?;
    store.prune(
        TelemetryRetentionConfig {
            max_bytes: 1,
            max_age_days: 1,
        },
        &paths.telemetry_database(),
    )?;
    assert_eq!(store.count_rows("product_events"), 8);
    assert_eq!(store.count_rows("turns"), 1);
    Ok(())
}

#[test]
fn writer_refuses_foreign_schema_without_wiping() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    {
        let connection = Connection::open(paths.telemetry_database())?;
        connection.execute_batch(
            "CREATE TABLE turns (
                    session_id TEXT, branch_id TEXT, run_id TEXT, agent_id TEXT,
                    turn_id TEXT PRIMARY KEY, started_at INTEGER
                 );
                 PRAGMA user_version = 1;",
        )?;
        connection.execute("INSERT INTO turns VALUES ('s','b','r','a','t', 1)", [])?;
    }
    assert!(TelemetryStore::open(&paths).is_err());
    let connection = Connection::open(paths.telemetry_database())?;
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM turns", [], |row| row.get(0))?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn writer_leaves_legacy_and_other_schema_files_untouched() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    for (name, version) in [
        ("telemetry.sqlite3", 9),
        ("telemetry-v9.sqlite3", 9),
        ("telemetry-v18.sqlite3", 18),
    ] {
        let database = paths.telemetry_dir().join(name);
        let connection = Connection::open(&database)?;
        connection.execute_batch(&format!("CREATE TABLE evidence (value TEXT); INSERT INTO evidence VALUES ('retained'); PRAGMA user_version = {version};"))?;
        drop(connection);
        let before = std::fs::read(&database)?;
        let store = TelemetryStore::open(&paths)?;
        assert_eq!(store.query_i64("PRAGMA user_version"), Some(21));
        assert_eq!(
            store.query_i64("SELECT count(*) FROM sqlite_master WHERE name = 'evidence'"),
            Some(0)
        );
        assert_eq!(std::fs::read(&database)?, before);
    }
    Ok(())
}

#[test]
fn read_paths_refuse_stale_schema_without_wiping() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    {
        let connection = Connection::open(paths.telemetry_database())?;
        connection.execute_batch(
            "CREATE TABLE turns (
                    session_id TEXT, branch_id TEXT, run_id TEXT, agent_id TEXT,
                    turn_id TEXT PRIMARY KEY, started_at INTEGER
                 );
                 PRAGMA user_version = 3;",
        )?;
        connection.execute("INSERT INTO turns VALUES ('s','b','r','a','t', 1)", [])?;
    }
    let error = TelemetryStore::query_attached(&paths, "SELECT COUNT(*) FROM turns")
        .expect_err("stale schema must fail closed");
    assert!(error.to_string().contains("file is left untouched"));
    assert_eq!(
        TelemetryStore::count_all_rows(&paths)
            .expect_err("stale")
            .to_string(),
        error.to_string()
    );
    let count: i64 = Connection::open(paths.telemetry_database())?.query_row(
        "SELECT COUNT(*) FROM turns",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}

#[test]
fn turn_started_rows_default_cost_status_unknown() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
        context.with_turn(TurnId::try_from(Uuid::from_u128(0x201)).expect("turn")),
        fixture_timestamp()?,
        TelemetryObservationKind::TurnStarted {
            origin: plakous::model::TurnOrigin::UserPrompt,
            queue_time: Duration::from_millis(1),
        },
    ))])?;
    assert_eq!(
        store.query_text("SELECT cost_status FROM turns"),
        Some("unknown".to_owned())
    );
    Ok(())
}

#[test]
fn credential_refresh_transport_phase_fixture() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout()?;
    let store = TelemetryStore::open(&paths)?;
    store.connection.execute(
        "INSERT INTO credential_refreshes (at, provider, phase, failure_class, transport_phase, generation)
             VALUES (1720000100000000000, 'anthropic', 'restored', 'timeout', 'resolution', 2)",
        [],
    )?;
    assert_eq!(
        store.query_text("SELECT transport_phase FROM v_credential_refreshes"),
        Some("resolution".to_owned())
    );
    Ok(())
}

#[test]
fn credential_refresh_round_trips_and_latest_query() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let older = Timestamp::new(1_720_000_000, 0).expect("older");
    let newer = Timestamp::new(1_720_000_100, 0).expect("newer");
    let oauth_error = plakous::agent::CredentialRefreshDiagnostic::new(
        Some("invalid_grant"),
        Some("Grant revoked [redacted]"),
    );
    store.write_batch(&[
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            older,
            TelemetryObservationKind::CredentialRefresh {
                provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                phase: CredentialRefreshPhase::Began,
                failure_class: None,
                transport_phase: None,
                http_status: None,
                elapsed: None,
                generation: 39,
                detail: plakous::agent::CredentialRefreshDetail::default(),
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            newer,
            TelemetryObservationKind::CredentialRefresh {
                provider: plakous::agent::CredentialProviderLabel::new("xai").unwrap(),
                phase: CredentialRefreshPhase::OutcomeUnknown,
                failure_class: Some(CredentialRefreshFailureClass::Timeout),
                transport_phase: Some(plakous::model::ModelRetryPhase::Resolution),
                http_status: Some(503),
                elapsed: Some(Duration::from_millis(30_000)),
                generation: 40,
                detail: plakous::agent::CredentialRefreshDetail {
                    reason: Some(plakous::agent::CredentialRefreshFailureReason::IdTokenExp),
                    expires_in_secs: Some(21_600),
                    id_token_iat_age_secs: Some(3_700),
                    id_token_exp_in_secs: Some(-100),
                    oauth_error,
                },
            },
        )),
    ])?;
    assert_eq!(
        store.query_i64("SELECT COUNT(*) FROM v_credential_refreshes"),
        Some(2)
    );
    assert_eq!(
        store.query_text(
            "SELECT http_status_class FROM v_credential_refreshes ORDER BY at DESC LIMIT 1"
        ),
        Some("5xx".to_owned())
    );
    let latest = TelemetryStore::latest_credential_refresh(&paths, "xai")
        .expect("query")
        .expect("row");
    assert_eq!(latest.phase, "outcome_unknown");
    assert_eq!(latest.failure_class.as_deref(), Some("timeout"));
    assert_eq!(latest.http_status, Some(503));
    assert_eq!(latest.generation, 40);
    assert_eq!(latest.transport_phase.as_deref(), Some("resolution"));
    assert_eq!(latest.failure_reason.as_deref(), Some("id_token_exp"));
    assert_eq!(latest.oauth_error_code.as_deref(), Some("invalid_grant"));
    let description = latest.oauth_error_description.as_deref().unwrap_or("");
    assert!(description.starts_with("Grant revoked"));
    assert!(!description.contains("private"));
    assert!(!format!("{latest:?}").contains("private"));
    Ok(())
}

/// Unlinking an open sqlite file on Unix leaves the inode writable, so this
/// cannot assert write errors; the contract is only that the agent-side
/// emitter keeps accepting and the writer shuts down cleanly.
#[test]
fn deleting_database_mid_run_keeps_emitter_enabled_and_does_not_panic() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let store = TelemetryStore::open(&paths)?;
    let database = paths.telemetry_database();
    let writer = TelemetryWriter::spawn(store, 16)?;
    let sink: Arc<dyn plakous::agent::TelemetrySink> =
        Arc::clone(&writer) as Arc<dyn plakous::agent::TelemetrySink>;
    let emitter = Arc::new(plakous::agent::TelemetryEmitter::enabled(sink));
    let observations = all_observations()?;
    emitter.emit(observations[0]);
    std::fs::remove_file(&database)?;
    let _ = std::fs::remove_file(database.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(database.with_extension("sqlite3-shm"));
    for observation in &observations[1..] {
        emitter.emit(*observation);
    }
    assert!(emitter.status().enabled());
    let _status = writer.close_and_join()?;
    assert!(emitter.status().enabled());
    Ok(())
}

#[test]
fn compaction_refusal_reason_is_persisted_with_and_without_a_start() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let at = fixture_timestamp()?;
    for started in [false, true] {
        if started {
            store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
                context,
                at,
                TelemetryObservationKind::CompactionStarted {
                    reason: CompactionEventReason::Scheduled,
                },
            ))])?;
        }
        store.write_batch(&[TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::CompactionFinished {
                refusal: Some(CompactionRefusalReason::Rejected),
                elapsed: Duration::from_millis(1),
                outcome: CompactionEventOutcome::Unavailable,
                reason: CompactionEventReason::Scheduled,
                tokens_before: Some(38_000),
                tokens_after: None,
                usage: None,
                measurement: plakous::agent::CompactionMeasurement::absent(),
            },
        ))])?;
    }
    let count: i64 = store.connection.query_row(
        "SELECT count(*) FROM compactions WHERE refusal_reason = 'rejected' AND tokens_before = 38000", [], |row| row.get(0),
    )?;
    assert_eq!(count, 2);
    Ok(())
}

#[test]
fn measurement_columns_hold_typed_facts_and_clips_update_their_call() -> TestResult {
    use plakous::agent::{
        CompactionMeasurement, ModelMeasurement, ProgramBasename, ToolMeasurement, WaitOutcome,
        WaitTargetKind,
    };
    let (_guard, paths) = PlakPaths::isolated_test_home();
    paths.ensure_layout().map_err(|error| error.to_string())?;
    let mut store = TelemetryStore::open(&paths)?;
    let context = fixture_context()?;
    let at = fixture_timestamp()?;
    let digest = [0xab; 32];
    let tool = |call: u128, measurement| -> TestResult<TelemetryItem> {
        Ok(TelemetryItem::Agent(TelemetryObservation::new(
            context.with_call(CallId::try_from(Uuid::from_u128(call))?),
            at,
            TelemetryObservationKind::ToolFinished {
                elapsed: Duration::from_millis(4),
                status: TelemetryTerminalStatus::Completed,
                output_bytes: 12,
                measurement,
            },
        )))
    };
    store.write_batch(&[
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::TurnFinished {
                elapsed: Duration::from_millis(20),
                status: TelemetryTerminalStatus::Completed,
                usage: TurnUsage::new(Usage::new(1, 2, 0, 0, 0), 2, 1, 3)?,
                model_wall: Duration::from_millis(1),
                tool_wall: Duration::from_millis(1),
                compaction_wall: Duration::from_millis(1),
                model_attempts: 2,
                tool_calls: 3,
                compactions: 1,
                prepared_input_high_water: Some(4_321),
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::ModelFinished {
                elapsed: Duration::from_millis(10),
                status: TelemetryTerminalStatus::Completed,
                usage: Some(Usage::new(9, 1, 0, 0, 0)),
                served_by: None,
                output_deltas: 0,
                measurement: ModelMeasurement::new(
                    Some(1_000_000),
                    Some(4_321),
                    Some((128, 256)),
                    Some(Duration::from_secs(5)),
                ),
            },
        )),
        tool(
            7,
            ToolMeasurement::access(Some(digest), Some((10, 20)), true),
        )?,
        tool(
            8,
            ToolMeasurement::command(ProgramBasename::from_basename("rg").ok_or("label")?),
        )?,
        tool(
            9,
            ToolMeasurement::wait(2, WaitTargetKind::Agent, WaitOutcome::Interrupted),
        )?,
        TelemetryItem::Agent(TelemetryObservation::new(
            context.with_call(CallId::try_from(Uuid::from_u128(7))?),
            at,
            TelemetryObservationKind::ToolObservationClipped {
                bytes_kept: 100,
                bytes_dropped: 900,
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::CompactionStarted {
                reason: CompactionEventReason::Scheduled,
            },
        )),
        TelemetryItem::Agent(TelemetryObservation::new(
            context,
            at,
            TelemetryObservationKind::CompactionFinished {
                refusal: None,
                elapsed: Duration::from_millis(7),
                outcome: CompactionEventOutcome::Committed,
                reason: CompactionEventReason::Scheduled,
                tokens_before: Some(900),
                tokens_after: Some(100),
                usage: None,
                measurement: CompactionMeasurement::new(
                    Some(1_000_000),
                    Some(800_000),
                    Some(900_000),
                    Some(Duration::from_secs(2)),
                ),
            },
        )),
    ])?;
    let row = |sql: &str| store.query_text(sql);
    assert_eq!(
        store.query_i64("SELECT prepared_input_high_water FROM turns"),
        Some(4_321)
    );
    assert_eq!(
        row(
            "SELECT context_window_tokens || ',' || prepared_input_tokens || ',' || \
             instruction_bytes || ',' || tool_schema_bytes || ',' || idle_gap_ns \
             FROM model_attempts"
        )
        .as_deref(),
        Some("1000000,4321,128,256,5000000000")
    );
    assert_eq!(
        row(
            "SELECT access_digest || ',' || range_first_line || ',' || range_lines || ',' || \
             page_partial || ',' || observation_clipped_bytes FROM tool_calls \
             WHERE access_digest IS NOT NULL"
        )
        .as_deref(),
        Some(format!("{},10,20,1,900", "ab".repeat(32)).as_str())
    );
    assert_eq!(
        row("SELECT command_class FROM tool_calls WHERE command_class IS NOT NULL").as_deref(),
        Some("rg")
    );
    assert_eq!(
        row(
            "SELECT wait_count || ':' || wait_kind || ':' || wait_outcome FROM tool_calls \
             WHERE wait_kind IS NOT NULL"
        )
        .as_deref(),
        Some("2:agent:interrupted")
    );
    assert_eq!(
        store.query_i64(
            "SELECT COUNT(*) FROM tool_calls WHERE observation_clipped_bytes IS NOT NULL"
        ),
        Some(1),
        "a clip updates only its own call"
    );
    assert_eq!(
        row(
            "SELECT context_window_tokens || ',' || threshold_tokens || ',' || \
             occupancy_tokens || ',' || idle_gap_ns FROM compactions"
        )
        .as_deref(),
        Some("1000000,800000,900000,2000000000")
    );
    Ok(())
}
