//! Bounded joined cleanup and preservation of primary failure evidence.

use crate::local_composition::LocalCompositionShutdownResult;
use crate::runtime::composition::projection::local_composition_started_error;
use crate::runtime::composition::{LocalCompositionShutdownTelemetry, PendingNestedDeliveries};
use crate::runtime::terminal::actor_panic_error;
use crate::{AgentError, AgentErrorKind, TelemetryObservation, TelemetryObservationKind};
use futures_util::FutureExt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::time::Duration;
pub(super) async fn settle_local_composition_deliveries(
    pending: &mut PendingNestedDeliveries,
) -> Vec<AgentError> {
    let mut errors = Vec::new();
    let deliveries = async {
        while let Some(result) = pending.next().await {
            if let Err(error) = result {
                errors.push(error);
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(1), deliveries)
        .await
        .is_err()
    {
        errors.push(local_composition_started_error(
            "nested delivery acknowledgement did not settle after host shutdown",
        ));
        pending.clear();
    }
    errors
}

pub(super) fn error_with_cleanup_disposition(
    primary: AgentError,
    cleanup: &[AgentError],
) -> AgentError {
    if cleanup.is_empty() {
        return primary;
    }
    AgentError::new(
        AgentErrorKind::OutcomeUnknown,
        format!(
            "initiating {:?} failure: {}; cleanup disposition: {}",
            primary.kind(),
            primary.message(),
            cleanup
                .iter()
                .map(AgentError::message)
                .collect::<Vec<_>>()
                .join("; ")
        ),
    )
}

pub(super) fn local_composition_shutdown_result(
    result: LocalCompositionShutdownResult,
) -> Result<(), AgentError> {
    match result {
        LocalCompositionShutdownResult::Joined { .. } => Ok(()),
        LocalCompositionShutdownResult::Lost(error) => Err(local_composition_started_error(error)),
    }
}

pub(super) fn emit_shutdown_child_exit(
    telemetry: Option<&LocalCompositionShutdownTelemetry>,
    result: &LocalCompositionShutdownResult,
) {
    let Some(telemetry) = telemetry else {
        return;
    };
    let LocalCompositionShutdownResult::Joined {
        child_exit: Some(exit),
    } = result
    else {
        return;
    };
    telemetry.emitter.emit(TelemetryObservation::new(
        telemetry.context,
        telemetry.clock.now(),
        TelemetryObservationKind::ChildProcessExited {
            child: exit.child,
            user_cpu: exit.user_cpu,
            system_cpu: exit.system_cpu,
            peak_rss_bytes: exit.peak_rss_bytes,
        },
    ));
}

pub(super) async fn contain_cleanup_panic<T>(
    future: impl Future<Output = Result<T, AgentError>>,
) -> Result<T, AgentError> {
    AssertUnwindSafe(future)
        .catch_unwind()
        .await
        .unwrap_or_else(|_| Err(actor_panic_error()))
}

pub(super) fn combine_cleanup_results(
    results: impl IntoIterator<Item = Result<(), AgentError>>,
) -> Result<(), AgentError> {
    let mut errors = results.into_iter().filter_map(Result::err);
    let Some(primary) = errors.next() else {
        return Ok(());
    };
    let cleanup = errors.collect::<Vec<_>>();
    Err(error_with_cleanup_disposition(primary, &cleanup))
}
