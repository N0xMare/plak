//! Typed product lifecycle failures at the authenticated wire boundary.

use crate::commands::session::lifecycle::{LifecycleError, LifecycleRefusal};
use plakous_host_protocol::wire::{InvalidTitleKind, LifecycleReason, RequestRefusal};

pub(super) fn refusal(session: &str, error: LifecycleError) -> RequestRefusal {
    let reason = match error {
        LifecycleError::Failure(failure) => {
            return RequestRefusal::SessionUnavailable {
                detail: failure.message().to_owned(),
            };
        }
        LifecycleError::Refused(refusal) => match refusal {
            LifecycleRefusal::NotFound => LifecycleReason::NotFound,
            LifecycleRefusal::InvalidTitle { kind } => LifecycleReason::InvalidTitle {
                kind: match kind {
                    crate::commands::session::lifecycle::InvalidTitleKind::Empty => {
                        InvalidTitleKind::Empty
                    }
                    crate::commands::session::lifecycle::InvalidTitleKind::TooLong => {
                        InvalidTitleKind::TooLong
                    }
                    crate::commands::session::lifecycle::InvalidTitleKind::ControlCharacter => {
                        InvalidTitleKind::ControlCharacter
                    }
                },
            },
            LifecycleRefusal::Busy {
                turn,
                children,
                children_truncated,
                processes,
                processes_truncated,
                state_unknown,
            } => LifecycleReason::Busy {
                turn,
                children,
                children_truncated,
                processes,
                processes_truncated,
                state_unknown,
            },
            LifecycleRefusal::Open => LifecycleReason::Open,
            LifecycleRefusal::LiveTree {
                children,
                children_truncated,
            } => LifecycleReason::LiveTree {
                children,
                children_truncated,
            },
            LifecycleRefusal::RecoveryRequiredTree {
                children,
                children_truncated,
            } => LifecycleReason::RecoveryRequiredTree {
                children,
                children_truncated,
            },
            LifecycleRefusal::ForkDependency {
                dependents,
                dependents_truncated,
            } => LifecycleReason::ForkDependency {
                dependents,
                dependents_truncated,
            },
            LifecycleRefusal::RunningJobs {
                processes,
                processes_truncated,
            } => LifecycleReason::RunningJobs {
                processes,
                processes_truncated,
            },
        },
    };
    RequestRefusal::SessionLifecycleRefused {
        session: session.to_owned(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::refusal;
    use crate::commands::session::lifecycle::{LifecycleError, LifecycleRefusal};
    use plakous_host_protocol::wire::{LifecycleReason, RequestRefusal};

    #[test]
    fn running_jobs_maps_to_the_host_wire_reason() {
        let mapped = refusal(
            "session-1",
            LifecycleError::Refused(LifecycleRefusal::RunningJobs {
                processes: vec!["p1".into(), "p2".into()],
                processes_truncated: false,
            }),
        );
        assert_eq!(
            mapped,
            RequestRefusal::SessionLifecycleRefused {
                session: "session-1".into(),
                reason: LifecycleReason::RunningJobs {
                    processes: vec!["p1".into(), "p2".into()],
                    processes_truncated: false,
                },
            }
        );
    }
}
