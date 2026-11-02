use super::*;
use std::error::Error;
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};

use uuid::Uuid;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

struct ScriptedSink {
    mode: AtomicU8,
    observations: Mutex<Vec<TelemetryObservation>>,
}

impl TelemetrySink for ScriptedSink {
    fn try_emit(&self, observation: TelemetryObservation) -> Result<(), TelemetrySinkError> {
        match self.mode.load(AtomicOrdering::Acquire) {
            0 => {
                lock(&self.observations).push(observation);
                Ok(())
            }
            1 => Err(TelemetrySinkError::Full),
            2 => Err(TelemetrySinkError::Closed),
            _ => panic!("contained telemetry panic"),
        }
    }
}

fn observation() -> TestResult<TelemetryObservation> {
    Ok(TelemetryObservation::new(
        TelemetryContext::new(
            DriverInstanceId::try_from(Uuid::from_u128(1))?,
            SessionId::try_from(Uuid::from_u128(2))?,
            BranchId::try_from(Uuid::from_u128(3))?,
            RunId::try_from(Uuid::from_u128(4))?,
        )
        .with_turn(TurnId::try_from(Uuid::from_u128(5))?)
        .with_attempt(AttemptId::try_from(Uuid::from_u128(6))?)
        .with_call(CallId::try_from(Uuid::from_u128(7))?),
        Timestamp::new(8, 9)?,
        TelemetryObservationKind::ModelFinished {
            elapsed: Duration::from_millis(10),
            status: TelemetryTerminalStatus::Completed,
            usage: Some(Usage::new(11, 12, 13, 14, 15)),
            served_by: None,
            output_deltas: 0,
            measurement: ModelMeasurement::absent(),
        },
    ))
}

#[test]
fn retry_logging_panics_are_contained_and_retire_only_the_hook() -> TestResult {
    struct PanickingHook(AtomicU8);
    impl TelemetrySink for PanickingHook {
        fn try_emit(&self, _: TelemetryObservation) -> Result<(), TelemetrySinkError> {
            Ok(())
        }
        fn observe_model_retry(&self, _: TelemetryContext, _: &plakous_model::ModelRetry) {
            self.0.fetch_add(1, AtomicOrdering::SeqCst);
            panic!("contained retry logging panic");
        }
    }
    let sink = Arc::new(PanickingHook(AtomicU8::new(0)));
    let emitter = TelemetryEmitter::enabled(sink.clone());
    let retry = plakous_model::ModelRetry::new(
        2,
        4,
        Duration::ZERO,
        plakous_model::ModelRetryCause::RecordRejectedInTransit,
        false,
        "peer_rejected_record",
    );
    let observation = observation()?;
    emitter.observe_model_retry(observation.context(), &retry);
    emitter.observe_model_retry(observation.context(), &retry);
    emitter.emit(observation);
    assert_eq!(sink.0.load(AtomicOrdering::SeqCst), 1);
    assert_eq!(emitter.status().panics(), 1);
    assert_eq!(emitter.status().emitted(), 1);
    assert!(emitter.status().enabled());
    Ok(())
}

#[test]
fn disabled_emitter_is_a_true_opt_out() -> TestResult {
    let emitter = TelemetryEmitter::disabled();
    emitter.emit(observation()?);
    assert_eq!(emitter.status(), TelemetryStatus::default());
    Ok(())
}

#[test]
fn full_is_dropped_while_closed_disables_the_sink() -> TestResult {
    let sink = Arc::new(ScriptedSink {
        mode: AtomicU8::new(1),
        observations: Mutex::new(Vec::new()),
    });
    let emitter = TelemetryEmitter::enabled(sink.clone());
    emitter.emit(observation()?);
    assert_eq!(emitter.status().dropped(), 1);
    assert!(emitter.status().enabled());

    sink.mode.store(2, AtomicOrdering::Release);
    emitter.emit(observation()?);
    emitter.emit(observation()?);
    assert_eq!(emitter.status().dropped(), 2);
    assert!(!emitter.status().enabled());
    Ok(())
}

#[test]
fn sink_panic_is_contained_counted_and_disabled() -> TestResult {
    let sink = Arc::new(ScriptedSink {
        mode: AtomicU8::new(3),
        observations: Mutex::new(Vec::new()),
    });
    let emitter = TelemetryEmitter::enabled(sink);
    emitter.emit(observation()?);
    emitter.emit(observation()?);
    assert_eq!(emitter.status().dropped(), 1);
    assert_eq!(emitter.status().panics(), 1);
    assert!(!emitter.status().enabled());
    Ok(())
}

#[test]
fn turn_finished_compartments_are_copy_and_typed() -> TestResult {
    let kind = TelemetryObservationKind::TurnFinished {
        elapsed: Duration::from_millis(100),
        status: TelemetryTerminalStatus::Completed,
        usage: TurnUsage::default(),
        model_wall: Duration::from_millis(40),
        tool_wall: Duration::from_millis(30),
        compaction_wall: Duration::from_millis(10),
        model_attempts: 1,
        tool_calls: 2,
        compactions: 1,
        prepared_input_high_water: None,
    };
    let copied = kind;
    let TelemetryObservationKind::TurnFinished {
        model_attempts,
        tool_calls,
        ..
    } = copied
    else {
        panic!("expected TurnFinished");
    };
    assert_eq!(model_attempts, 1);
    assert_eq!(tool_calls, 2);
    Ok(())
}

#[test]
fn tool_finished_carries_output_bytes() -> TestResult {
    let kind = TelemetryObservationKind::ToolFinished {
        elapsed: Duration::from_millis(5),
        status: TelemetryTerminalStatus::Completed,
        output_bytes: 42,
        measurement: ToolMeasurement::absent(),
    };
    let TelemetryObservationKind::ToolFinished { output_bytes, .. } = kind else {
        panic!("expected ToolFinished");
    };
    assert_eq!(output_bytes, 42);
    Ok(())
}

#[test]
fn compaction_finished_carries_tokens_and_usage() -> TestResult {
    let kind = TelemetryObservationKind::CompactionFinished {
        refusal: None,
        elapsed: Duration::from_millis(50),
        outcome: CompactionEventOutcome::Committed,
        reason: CompactionEventReason::Manual,
        tokens_before: Some(100),
        tokens_after: Some(40),
        usage: Some(Usage::new(1, 2, 3, 4, 5)),
        measurement: CompactionMeasurement::absent(),
    };
    let TelemetryObservationKind::CompactionFinished {
        tokens_before,
        tokens_after,
        usage,
        ..
    } = kind
    else {
        panic!("expected CompactionFinished");
    };
    assert_eq!(tokens_before, Some(100));
    assert_eq!(tokens_after, Some(40));
    assert_eq!(usage, Some(Usage::new(1, 2, 3, 4, 5)));
    Ok(())
}

#[test]
fn durability_attempt_finished_is_copy() -> TestResult {
    let kind = TelemetryObservationKind::DurabilityAttemptFinished {
        elapsed: Duration::from_millis(3),
        attempt: DurabilityAttemptKind::InitialCommit,
        outcome: DurabilityAttemptOutcome::Committed,
    };
    let copied = kind;
    let TelemetryObservationKind::DurabilityAttemptFinished {
        attempt, outcome, ..
    } = copied
    else {
        panic!("expected DurabilityAttemptFinished");
    };
    assert_eq!(attempt, DurabilityAttemptKind::InitialCommit);
    assert_eq!(outcome, DurabilityAttemptOutcome::Committed);
    Ok(())
}

#[test]
fn telemetry_tool_name_preserves_the_full_validated_name_and_is_copyable() -> TestResult {
    let name = plakous_model::ToolName::new("x".repeat(128))?;
    let token = TelemetryToolName::from(&name);
    let copied = token;
    assert_eq!(copied.as_str(), name.as_str());
    assert_eq!(token, copied);
    Ok(())
}

#[test]
fn observation_surface_contains_only_typed_content_free_values() -> TestResult {
    let observation = observation()?;
    let TelemetryObservationKind::ModelFinished {
        elapsed,
        status,
        usage,
        served_by,
        ..
    } = observation.kind()
    else {
        panic!("fixture kind changed");
    };
    assert_eq!(elapsed, Duration::from_millis(10));
    assert_eq!(status, TelemetryTerminalStatus::Completed);
    assert_eq!(usage, Some(Usage::new(11, 12, 13, 14, 15)));
    assert_eq!(served_by, None);
    assert_eq!(
        observation.context().call_id(),
        Some(CallId::try_from(Uuid::from_u128(7))?)
    );
    Ok(())
}

#[test]
fn credential_refresh_is_copy_and_content_free() -> TestResult {
    let kind = TelemetryObservationKind::CredentialRefresh {
        provider: CredentialProviderLabel::new("acme")?,
        phase: CredentialRefreshPhase::OutcomeUnknown,
        failure_class: Some(CredentialRefreshFailureClass::Timeout),
        transport_phase: Some(plakous_model::ModelRetryPhase::Resolution),
        http_status: Some(503),
        elapsed: Some(Duration::from_millis(30)),
        generation: 40,
        detail: CredentialRefreshDetail::default(),
    };
    let copied = kind;
    let TelemetryObservationKind::CredentialRefresh {
        provider,
        phase,
        failure_class,
        http_status,
        generation,
        ..
    } = copied
    else {
        panic!("expected CredentialRefresh");
    };
    assert_eq!(provider.as_str(), "acme");
    assert_eq!(phase, CredentialRefreshPhase::OutcomeUnknown);
    assert_eq!(failure_class, Some(CredentialRefreshFailureClass::Timeout));
    assert_eq!(http_status, Some(503));
    assert_eq!(generation, 40);
    assert_eq!(
        HttpStatusClass::from_status(503),
        HttpStatusClass::ServerError
    );
    assert!(CredentialProviderLabel::new("").is_err());
    Ok(())
}

#[test]
fn only_an_attached_sink_turns_measurement_on() {
    struct Discard;
    impl TelemetrySink for Discard {
        fn try_emit(&self, _: TelemetryObservation) -> Result<(), TelemetrySinkError> {
            Ok(())
        }
    }
    assert!(!TelemetryEmitter::disabled().is_enabled());
    assert!(TelemetryEmitter::enabled(Arc::new(Discard)).is_enabled());
}
