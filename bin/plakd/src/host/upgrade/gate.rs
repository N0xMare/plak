//! The inherited destination's read-only checks before it serves.
//!
//! Store health, preservation and binding inspection read independent
//! snapshots, so they run together under the caller's stand-alone writer
//! exclusion. Each keeps its own bound. Failures are reported in a fixed
//! order, not in completion order, so the failure stage is stable.

use std::thread::{Scope, ScopedJoinHandle};
use std::time::Instant;

use super::checkpoint::{Checkpoint, EvidenceLevel, Failure, GatePhase, GateTiming};
use super::{bindings, health};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

/// Logs a destination step when it completes; the verdict carries the same timing.
pub(in crate::host) fn timed<T>(phase: GatePhase, step: impl FnOnce() -> T) -> (T, GateTiming) {
    let started = Instant::now();
    let result = step();
    let timing = GateTiming::new(phase, started.elapsed());
    eprintln!("host upgrade: {}", timing.detail());
    (result, timing)
}

/// Returns the timings of the checks that ran, in reporting order, beside
/// the verdict. On failure `stage` names the failed check.
pub(in crate::host) fn verify(
    paths: &PlakPaths,
    checkpoint: &mut Checkpoint,
    stage: &mut Failure,
) -> (Vec<GateTiming>, CliResult<EvidenceLevel>) {
    let deadline = health::deadline();
    let phases = [
        GatePhase::StoreHealthProduct,
        GatePhase::StoreHealthTree,
        GatePhase::StoreHealthRuntime,
    ];
    std::thread::scope(|scope| {
        let stores = health::databases(paths)
            .into_iter()
            .zip(phases)
            .map(|((label, path), phase)| {
                spawn(scope, "store health", move || {
                    timed(phase, || health::store(label, &path, deadline))
                })
            })
            .collect::<Vec<_>>();
        let preservation = spawn(scope, "preservation", || {
            timed(GatePhase::Preservation, || {
                checkpoint.verify_destination(paths)
            })
        });
        let bindings = spawn(scope, "binding inspection", || {
            timed(GatePhase::Bindings, || bindings::inspect_destination(paths))
        });
        // Join every check before reporting, so none outlives the exclusion.
        let mut timings = Vec::new();
        let stores = stores
            .into_iter()
            .map(|store| finish(&mut timings, store))
            .collect::<Vec<_>>();
        let preservation = finish(&mut timings, preservation);
        let bindings = finish(&mut timings, bindings);
        let verdict = first_failure(stage, stores, preservation, bindings, |baseline, report| {
            bindings::health(baseline, report)
        });
        (timings, verdict)
    })
}

fn first_failure<E, B, R>(
    stage: &mut Failure,
    stores: impl IntoIterator<Item = CliResult<()>>,
    preservation: CliResult<(E, B)>,
    bindings: CliResult<R>,
    regressions: impl FnOnce(&B, &R) -> CliResult<()>,
) -> CliResult<E> {
    *stage = Failure::Health;
    for store in stores {
        store?;
    }
    *stage = Failure::Preservation;
    let (evidence, baseline) = preservation?;
    *stage = Failure::Health;
    bindings.and_then(|report| regressions(&baseline, &report))?;
    Ok(evidence)
}

type Check<'scope, T> = CliResult<ScopedJoinHandle<'scope, (CliResult<T>, GateTiming)>>;

fn spawn<'scope, T: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    name: &str,
    check: impl FnOnce() -> (CliResult<T>, GateTiming) + Send + 'scope,
) -> Check<'scope, T> {
    std::thread::Builder::new()
        .name(format!("upgrade {name}"))
        .spawn_scoped(scope, check)
        .map_err(|error| CliFailure::new(format!("could not start the {name} check: {error}")))
}

fn finish<T>(timings: &mut Vec<GateTiming>, check: Check<'_, T>) -> CliResult<T> {
    let (result, timing) = check?
        .join()
        .map_err(|_| CliFailure::new("an upgrade gate check panicked"))?;
    timings.push(timing);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed(label: &str) -> CliResult<()> {
        Err(CliFailure::new(label))
    }

    fn stage(check: impl FnOnce(&mut Failure) -> CliResult<()>) -> (&'static str, String) {
        let mut stage = Failure::Startup;
        let Err(error) = check(&mut stage) else {
            panic!("the gate passed");
        };
        let stage = match stage {
            Failure::Health => "health",
            Failure::Preservation => "preservation",
            Failure::Startup => "startup",
        };
        (stage, error.message().to_owned())
    }

    #[test]
    fn concurrent_gate_failures_are_reported_in_a_fixed_order() {
        let regression = |(): &(), (): &()| failed("bindings");
        let all = stage(|stage| {
            first_failure(
                stage,
                [failed("product"), failed("tree"), failed("runtime")],
                Err(CliFailure::new("preservation")),
                Err(CliFailure::new("inspection")),
                regression,
            )
        });
        assert_eq!(all, ("health", "product".to_owned()));
        let runtime = stage(|stage| {
            first_failure(
                stage,
                [Ok(()), Ok(()), failed("runtime")],
                Err(CliFailure::new("preservation")),
                Ok(()),
                regression,
            )
        });
        assert_eq!(runtime, ("health", "runtime".to_owned()));
        let preservation = stage(|stage| {
            first_failure(
                stage,
                [Ok(()), Ok(()), Ok(())],
                Err(CliFailure::new("preservation")),
                Err(CliFailure::new("inspection")),
                regression,
            )
        });
        assert_eq!(preservation, ("preservation", "preservation".to_owned()));
        let inspection = stage(|stage| {
            first_failure(
                stage,
                [Ok(()), Ok(()), Ok(())],
                Ok(((), ())),
                Err::<(), _>(CliFailure::new("inspection")),
                regression,
            )
        });
        assert_eq!(inspection, ("health", "inspection".to_owned()));
        assert_eq!(
            stage(|stage| first_failure(
                stage,
                [Ok(()), Ok(()), Ok(())],
                Ok(((), ())),
                Ok(()),
                regression,
            )),
            ("health", "bindings".to_owned())
        );
        assert!(
            first_failure(
                &mut Failure::Startup,
                [Ok(()), Ok(()), Ok(())],
                Ok(((), ())),
                Ok(()),
                |(), ()| Ok(()),
            )
            .is_ok()
        );
    }
}
