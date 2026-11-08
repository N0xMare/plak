//! Before/after pins for the complete SQLite and OTel observation surface.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "observation contract fixtures"
)]

use super::tests::{TestResult, all_observations, fixture_context, fixture_timestamp};
use super::*;
use std::sync::{Arc, Mutex};

use opentelemetry_sdk::trace::{SpanData, SpanProcessor};
use plakous::agent::{
    DurabilityAttemptKind, DurabilityAttemptOutcome, JobHintSource, JobNoticeDelivery,
    JobPreemption, JobTool, TelemetryContext, TelemetryObservationKind, TelemetrySink,
};

#[derive(Debug, Default)]
struct Capture(Mutex<Vec<SpanData>>);

impl SpanProcessor for Capture {
    fn on_start(&self, _: &mut opentelemetry_sdk::trace::Span, _: &opentelemetry::Context) {}
    fn on_end(&self, span: SpanData) {
        self.0.lock().unwrap().push(span);
    }
    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
    fn shutdown_with_timeout(&self, _: Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        Ok(())
    }
    fn set_resource(&mut self, _: &opentelemetry_sdk::Resource) {}
}

fn observations() -> TestResult<Vec<TelemetryObservation>> {
    let mut observations = all_observations()?;
    for kind in [
        TelemetryObservationKind::JobPromoted {
            tool: JobTool::Start,
            nested: true,
            hint_source: JobHintSource::ModelHint,
            hint_ms: u64::MAX,
        },
        TelemetryObservationKind::JobWaited {
            tool: JobTool::Poll,
        },
        TelemetryObservationKind::JobPreempted {
            kind: JobPreemption::WaitYielded,
        },
        TelemetryObservationKind::JobNoticesDelivered {
            point: JobNoticeDelivery::Boundary,
            count: 3,
        },
        TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed: Duration::from_millis(2),
            attempt: DurabilityAttemptKind::CleanupFlush,
            outcome: DurabilityAttemptOutcome::Failed,
        },
        TelemetryObservationKind::ModelStreamActivity {
            activity: plakous::model::ModelStreamActivity {
                output_started: true,
                last_semantic_event_elapsed_ms: Some(10),
                semantic_gap_ms: 5,
                keepalive_count: 7,
                max_semantic_gap_ms: 20,
            },
        },
        TelemetryObservationKind::ModelReresolved { accepted: true },
        TelemetryObservationKind::ModelArtifactPrefixDrift {
            fields: plakous::model::StableRequestPrefixDrift::default(),
        },
    ] {
        observations.push(TelemetryObservation::new(
            fixture_context()?,
            fixture_timestamp()?,
            kind,
        ));
    }
    assert_eq!(
        observations.len(),
        23,
        "one fixture for every observation kind"
    );
    // Also cover missing correlation ids, finish-without-start, and clean commit suppression.
    let bare = TelemetryContext::new(
        fixture_context()?.agent_id(),
        fixture_context()?.session_id(),
        fixture_context()?.branch_id(),
        fixture_context()?.run_id(),
    );
    let missing: Vec<_> = observations
        .iter()
        .map(|item| TelemetryObservation::new(bare, item.timestamp(), item.kind()))
        .collect();
    observations.extend(missing);
    observations.push(TelemetryObservation::new(
        fixture_context()?,
        fixture_timestamp()?,
        TelemetryObservationKind::DurabilityAttemptFinished {
            elapsed: Duration::ZERO,
            attempt: DurabilityAttemptKind::InitialCommit,
            outcome: DurabilityAttemptOutcome::Committed,
        },
    ));
    Ok(observations)
}

fn rows(connection: &Connection) -> TestResult<serde_json::Value> {
    // New terminal facts and recording evidence have their own contract. This
    // pin still proves that the legacy agent diagnostics and spans did not change.
    let tables = connection.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT IN ('recorded_terminals', 'recording_evidence') ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    let mut snapshot = serde_json::Map::new();
    for table in tables {
        let mut query = connection.prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))?;
        let columns: Vec<_> = query
            .column_names()
            .iter()
            .enumerate()
            .filter_map(|(index, name)| {
                (table != "credential_refreshes" || *name != "account").then_some(index)
            })
            .collect();
        let values = query
            .query_map([], |row| {
                columns
                    .iter()
                    .map(|&column| {
                        row.get::<_, rusqlite::types::Value>(column)
                            .map(|value| format!("{value:?}"))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })?
            .collect::<Result<Vec<_>, _>>()?;
        snapshot.insert(table, serde_json::to_value(values)?);
    }
    Ok(snapshot.into())
}

#[test]
fn every_observation_keeps_the_before_refactor_rows_and_spans() -> TestResult {
    let (_guard, paths) = PlakPaths::isolated_test_home();
    let mut store = TelemetryStore::open(&paths)?;
    let capture = Arc::new(Capture::default());
    let processor = Arc::clone(&capture);
    let bridge = Arc::new(
        crate::otel_bridge::OtelBridge::with_processor(processor).expect("capture bridge"),
    );
    let mut snapshots = Vec::new();
    for observation in observations()? {
        store.write_batch(&[TelemetryItem::Agent(observation)])?;
        snapshots.push(rows(&store.connection)?);
        bridge.try_emit(observation)?;
    }
    bridge.drain_for_test();
    // SpanData's Debug covers identity, parents, timing, ordered typed attributes,
    // events, status and instrumentation scope without wall-clock inputs.
    let spans: Vec<_> = capture
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|span| format!("{span:?}"))
        .collect();
    let bytes = serde_json::to_vec(&(snapshots, spans))?;
    let digest: String = plakous::model::sha256_bytes(&[&bytes])
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(
        digest,
        "959825181c64882f571b9e88f47d29e0dc730626991fd92208a84df0b5d597da"
    );
    Ok(())
}
