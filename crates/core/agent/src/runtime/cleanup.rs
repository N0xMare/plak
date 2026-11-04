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
    if crate::executor::timeout(Duration::from_secs(1), deliveries)
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
    let mut distinct = Vec::new();
    for error in cleanup {
        let message = error.message();
        let already_reported = message == primary.message()
            || primary.message().split("; ").any(|clause| {
                message == clause
                    || message == clause.rsplit_once(": ").map_or(clause, |(_, leaf)| leaf)
            });
        if !already_reported && !distinct.contains(&message) {
            distinct.push(message);
        }
    }
    // Cleanup can report the same failed release through several ownership
    // layers. Keep OutcomeUnknown, but never recursively wrap that evidence.
    let initiating = if primary.kind() == AgentErrorKind::OutcomeUnknown {
        primary.message().to_owned()
    } else {
        format!(
            "initiating {:?} failure: {}",
            primary.kind(),
            primary.message()
        )
    };
    let message = if distinct.is_empty() {
        initiating
    } else {
        format!("{initiating}; cleanup disposition: {}", distinct.join("; "))
    };
    AgentError::new(AgentErrorKind::OutcomeUnknown, message)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cleanup_substring_is_not_the_same_cause() {
        let primary = AgentError::new(AgentErrorKind::Session, "database disk is full");
        let cleanup = AgentError::new(AgentErrorKind::Internal, "disk is full");
        let combined = error_with_cleanup_disposition(primary, &[cleanup]);
        assert_eq!(
            combined.message(),
            "initiating Session failure: database disk is full; cleanup disposition: disk is full"
        );
    }

    #[test]
    fn repeated_full_release_cleanup_retains_uncertainty_without_recursive_causes() {
        let full = AgentError::new(AgentErrorKind::Session, "session store disk is full");
        let first = error_with_cleanup_disposition(full.clone(), &[full.clone(), full.clone()]);
        let second = error_with_cleanup_disposition(first.clone(), &[full, first.clone()]);
        assert_eq!(first, second);
        assert_eq!(second.kind(), AgentErrorKind::OutcomeUnknown);
        assert_eq!(
            second
                .message()
                .matches("session store disk is full")
                .count(),
            1
        );
        let different = AgentError::new(AgentErrorKind::Internal, "worker did not stop");
        let combined = error_with_cleanup_disposition(second, &[different.clone(), different]);
        assert_eq!(combined.message().matches("worker did not stop").count(), 1);
        assert_eq!(
            combined
                .message()
                .matches("session store disk is full")
                .count(),
            1
        );
    }
}
