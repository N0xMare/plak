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
    pub(super) revisions_pending: bool,
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

    /// The concurrent gate cannot take longer than its checks run serially,
    /// after the admission wait and any stopped-store revision backup.
    fn projected(&self) -> Duration {
        let reserved = timing::ADMISSION
            + if self.revisions_pending {
                timing::REVISION_BACKUP
            } else {
                Duration::ZERO
            };
        self.components()
            .iter()
            .fold(reserved, |total, (_, elapsed, _)| {
                total.saturating_add(*elapsed)
            })
    }

    pub(super) fn check(&self) -> CliResult<()> {
        let components = self.components();
        let projected = self.projected();
        let mut line = components
            .iter()
            .map(|(phase, elapsed, _)| format!("{phase}={}", millis(*elapsed)))
            .collect::<Vec<_>>();
        if self.bindings.is_none() {
            line.push("bindings=unmeasured".into());
        }
        eprintln!(
            "host upgrade: gate projection {} projected_ms={} successor_ms={}",
            line.join(" "),
            millis(projected),
            millis(timing::SUCCESSOR)
        );
        for (phase, elapsed, bound) in &components {
            if elapsed.saturating_mul(MARGIN) > *bound {
                return Err(refusal(phase, *elapsed, *bound));
            }
        }
        if projected.saturating_mul(MARGIN) > timing::SUCCESSOR {
            return Err(refusal("gate", projected, timing::SUCCESSOR));
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
            revisions_pending: false,
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
        let total = Measured {
            stores: (0..6)
                .map(|_| ("runtime", Duration::from_secs(45)))
                .collect(),
            ..measured(1, 60, Some(60))
        };
        assert!(
            refused(&total).contains(": gate measured"),
            "{}",
            refused(&total)
        );
        assert!(
            Measured {
                revisions_pending: true,
                ..measured(1, 1, Some(1))
            }
            .projected()
                > measured(1, 1, Some(1)).projected()
        );
    }
}
