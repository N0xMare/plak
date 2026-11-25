//! The inherited destination's read-only checks before it serves.
//!
//! Store health, preservation and binding inspection read independent
//! snapshots, so they run together under the caller's stand-alone writer
//! exclusion. Each keeps its own bound. Failures are reported in a fixed
//! order, not in completion order, so the failure stage is stable.

use std::thread::{Scope, ScopedJoinHandle};

use super::checkpoint::{Checkpoint, EvidenceLevel, Failure};
use super::{bindings, health};
use crate::home::paths::PlakPaths;
use crate::{CliFailure, CliResult};

/// On failure `stage` names the failed check.
pub(in crate::host) fn verify(
    paths: &PlakPaths,
    checkpoint: &mut Checkpoint,
    stage: &mut Failure,
) -> CliResult<EvidenceLevel> {
    let deadline = health::deadline();
    std::thread::scope(|scope| {
        let stores = health::databases(paths).map(|(label, path)| {
            spawn(scope, "store health", move || {
                health::store(label, &path, deadline)
            })
        });
        let preservation = spawn(scope, "preservation", || {
            checkpoint.verify_destination(paths)
        });
        let bindings = spawn(scope, "binding inspection", || {
            bindings::inspect_destination(paths)
        });
        // Join every check before reporting, so none outlives the exclusion.
        let stores = stores.map(join);
        let preservation = join(preservation);
        let bindings = join(bindings);
        first_failure(stage, stores, preservation, bindings, |baseline, report| {
            bindings::health(baseline, report)
        })
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

type Check<'scope, T> = CliResult<ScopedJoinHandle<'scope, CliResult<T>>>;

fn spawn<'scope, T: Send + 'scope>(
    scope: &'scope Scope<'scope, '_>,
    name: &str,
    check: impl FnOnce() -> CliResult<T> + Send + 'scope,
) -> Check<'scope, T> {
    std::thread::Builder::new()
        .name(format!("upgrade {name}"))
        .spawn_scoped(scope, check)
        .map_err(|error| CliFailure::new(format!("could not start the {name} check: {error}")))
}

fn join<T>(check: Check<'_, T>) -> CliResult<T> {
    check?
        .join()
        .map_err(|_| CliFailure::new("an upgrade gate check panicked"))?
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
