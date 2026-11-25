//! Refuse before surrender when the destination gate could reach a bound.
//!
//! A gate check that times out after surrender is a fix-forward outage, so
//! the requester projects the gate from the same checks it already ran
//! against the live stores and refuses while the source still serves.

use std::time::Duration;

use crate::carry::upgrade::timing;
use crate::{CliFailure, CliResult};

/// Each measured check must fit this many times within its bound.
pub(super) const MARGIN: u32 = 2;

pub(super) struct Measured {
    pub(super) stores: Vec<(&'static str, Duration)>,
    pub(super) preservation: Duration,
    pub(super) bindings: Option<Duration>,
    /// The measured runtime bootstrap rehearsal, plus the revision backup
    /// bound when product or tree revisions are pending.
    pub(super) stopped: Duration,
}

impl Measured {
    fn components(&self) -> Vec<(String, Duration, Duration)> {
        let mut components: Vec<_> = self
            .stores
            .iter()
            .map(|(label, elapsed)| (format!("store_health_{label}"), *elapsed, timing::STORE))
            .collect();
        // Capture dominates the destination's capture and comparison.
        components.push(("preservation".into(), self.preservation, timing::CAPTURE));
        if let Some(bindings) = self.bindings {
            components.push(("bindings".into(), bindings, timing::BINDINGS));
        }
        components
    }

    /// The admission wait, the stopped-store work, then the checks. The
    /// concurrent gate is bounded by their serial sum.
    fn projected(&self) -> Duration {
        self.components().iter().fold(
            timing::ADMISSION.saturating_add(self.stopped),
            |total, (_, elapsed, _)| total.saturating_add(*elapsed),
        )
    }

    pub(super) fn check(&self) -> CliResult<()> {
        let components = self.components();
        let projected = self.projected();
        // Post-gate startup keeps its own reserve of the successor bound.
        let budget = timing::SUCCESSOR.saturating_sub(timing::STARTUP);
        let mut line = components
            .iter()
            .map(|(phase, elapsed, _)| format!("{phase}={}", millis(*elapsed)))
            .collect::<Vec<_>>();
        if self.bindings.is_none() {
            line.push("bindings=unmeasured".into());
        }
        line.push(format!("stopped={}", millis(self.stopped)));
        eprintln!(
            "host upgrade: gate projection {} projected_ms={} budget_ms={}",
            line.join(" "),
            millis(projected),
            millis(budget)
        );
        for (phase, elapsed, bound) in &components {
            if elapsed.saturating_mul(MARGIN) > *bound {
                return Err(refusal(phase, *elapsed, *bound));
            }
        }
        if projected.saturating_mul(MARGIN) > budget {
            return Err(refusal("gate", projected, budget));
        }
        Ok(())
    }
}

pub(super) fn unmeasured(error: &CliFailure) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_gate_projection_refused: destination binding inspection failed before surrender ({}); no surrender was requested",
        error.message()
    ))
}

fn refusal(phase: &str, elapsed: Duration, bound: Duration) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_gate_projection_refused: {phase} measured {} ms before surrender, more than 1/{MARGIN} of its {} ms bound after surrender; no surrender was requested",
        millis(elapsed),
        millis(bound)
    ))
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn measured(store: u64, preservation: u64, bindings: Option<u64>) -> Measured {
        Measured {
            stores: vec![
                ("product", Duration::from_secs(1)),
                ("tree", Duration::from_secs(1)),
                ("runtime", Duration::from_secs(store)),
            ],
            preservation: Duration::from_secs(preservation),
            bindings: bindings.map(Duration::from_secs),
            stopped: Duration::ZERO,
        }
    }

    fn refused(measured: &Measured) -> String {
        measured
            .check()
            .err()
            .map(|error| error.message().to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn a_gate_check_near_its_bound_refuses_before_surrender() {
        assert!(measured(20, 20, Some(5)).check().is_ok());
        assert!(measured(20, 20, None).check().is_ok());
        // The full binding audit took 75 of its 120 seconds in production.
        let bindings = refused(&measured(20, 20, Some(75)));
        assert!(
            bindings
                .starts_with("host_upgrade_gate_projection_refused: bindings measured 75000 ms"),
            "{bindings}"
        );
        assert!(refused(&measured(46, 1, Some(1))).contains(": store_health_runtime measured"));
        assert!(refused(&measured(1, 61, Some(1))).contains(": preservation measured"));
    }

    #[test]
    fn the_whole_gate_refuses_although_each_check_fits_its_own_bound() {
        // Every check is within half of its bound, but the serial sum with
        // the admission wait is more than half of the pre-startup budget.
        let near = Measured {
            stores: vec![
                ("product", Duration::from_secs(45)),
                ("tree", Duration::from_secs(45)),
                ("runtime", Duration::from_secs(45)),
            ],
            ..measured(45, 60, Some(60))
        };
        assert!(
            refused(&near).contains(": gate measured"),
            "{}",
            refused(&near)
        );
        let fits = measured(20, 20, Some(5));
        assert!(fits.check().is_ok());
        // A long stopped bootstrap moves an otherwise passing gate over.
        let stopped = Measured {
            stopped: Duration::from_secs(150),
            ..fits
        };
        assert!(
            refused(&stopped).contains(": gate measured"),
            "{}",
            refused(&stopped)
        );
    }
}
