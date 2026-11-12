//! Product wording and exit classes for authenticated host refusals.

use plakous_host_protocol::wire::{
    BindingRefusalKind, ConsoleCapability, LifecycleReason, PlacementReason, RequestRefusal,
};
use plakous_hosted::error::{CliFailure, CliFailureKind};

pub(crate) fn refusal(reason: RequestRefusal) -> CliFailure {
    let kind = match &reason {
        RequestRefusal::Malformed { .. }
        | RequestRefusal::CapabilityDisabled { .. }
        | RequestRefusal::BindingRefused { .. }
        | RequestRefusal::Unsupported { .. }
        | RequestRefusal::UnknownNode
        | RequestRefusal::UnknownSession => CliFailureKind::Usage,
        RequestRefusal::TreeHostedElsewhere { .. } => CliFailureKind::HostedElsewhere,
        RequestRefusal::SessionLifecycleRefused {
            reason: LifecycleReason::InvalidTitle { .. },
            ..
        } => CliFailureKind::Usage,
        RequestRefusal::PlacementRefused { reason, .. }
            if matches!(reason.as_ref(), PlacementReason::WorkspaceRefused { .. }) =>
        {
            CliFailureKind::Usage
        }
        _ => CliFailureKind::Agent,
    };
    let text = match &reason {
        RequestRefusal::CapabilityDisabled { capability } => match capability {
            ConsoleCapability::Terminal => "operator terminals are disabled by this host".into(),
            ConsoleCapability::CredentialsWrite => "credential writes are disabled by this host".into(),
            ConsoleCapability::ConfigWrite => "configuration writes are disabled by this host".into(),
            ConsoleCapability::AccountsRead => "account reads are disabled by this host".into(),
            ConsoleCapability::ConfigRead => "configuration reads are disabled by this host".into(),
            ConsoleCapability::FilesRead => "workspace reads are disabled by this host".into(),
        },
        RequestRefusal::RateLimited { retry_after_ms } => format!("host console is busy; retry in {retry_after_ms} ms"),
        RequestRefusal::DeadlineExceeded => "the request took too long; retry".to_owned(),
        RequestRefusal::PlacementRefused { runner, reason } => format!("runner {}: {}", runner.name, placement(reason)),
        RequestRefusal::BindingRefused { kind } => match kind {
            BindingRefusalKind::UnknownProfile => "unknown profile; choose an exact profile id from the host's catalog",
            BindingRefusalKind::UnknownTemplate => "unknown template; choose a template from the host's catalog",
            BindingRefusalKind::ModelUnavailable => "model unavailable; choose an admitted account and model",
            BindingRefusalKind::IncompatibleComposition => "the model cannot use this composition; choose another profile",
            BindingRefusalKind::Configuration => "the host configuration is invalid; run `plak config edit`",
        }.to_owned(),
        RequestRefusal::SessionLifecycleRefused { session, reason } => format!("session {session}: {}", lifecycle(reason)),
        RequestRefusal::SessionArchived { session } => format!("session {session} is archived and read-only; unarchive it before sending work"),
        RequestRefusal::TreeHostedElsewhere { host_name, pid, .. } => format!("the tree is open on {host_name} (pid {pid}); release it there first"),
        RequestRefusal::Malformed { detail } | RequestRefusal::Unsupported { detail }
        | RequestRefusal::SessionUnavailable { detail } | RequestRefusal::MailRefused { detail } => detail.clone(),
        RequestRefusal::DraftPending { .. } => "the draft is still awaiting durable admission; inspect its outcome before retrying".into(),
        RequestRefusal::DraftRegistryFull { .. } => "this host's draft identity registry is full; restart the host before sending a new draft".into(),
        RequestRefusal::Backpressure => "the session mailbox is full; retry when it drains".into(),
        RequestRefusal::OneInFlight => "another operation is in flight; wait for its outcome".into(),
        RequestRefusal::Disconnected => "the session is not running; attach again".into(),
        RequestRefusal::RefusedForRole { row } => format!("this node's role does not admit {row}"),
        RequestRefusal::UnknownNode => "the node does not exist".into(),
        RequestRefusal::UnknownSession => "the session does not exist on this host".into(),
        RequestRefusal::Moved { .. } => "the session moved; attach to its current root".into(),
        RequestRefusal::NodeClosed => "the node is closed and cannot run again".into(),
        RequestRefusal::NodeRetiring => "the node is releasing; retry after release".into(),
        RequestRefusal::OutOfReach => "the node is outside this operator's reach".into(),
        RequestRefusal::NotAttached => "attach to a session before sending this request".into(),
    };
    CliFailure::classified(kind, text).with_wire_refusal(reason)
}

fn lifecycle(reason: &LifecycleReason) -> String {
    lifecycle_refusal(reason.clone()).to_string()
}

fn lifecycle_refusal(
    reason: LifecycleReason,
) -> crate::commands::session::lifecycle::LifecycleRefusal {
    use crate::commands::session::lifecycle::{InvalidTitleKind, LifecycleRefusal as R};
    match reason {
        LifecycleReason::NotFound => R::NotFound,
        LifecycleReason::InvalidTitle { kind } => R::InvalidTitle {
            kind: match kind {
                plakous_host_protocol::wire::InvalidTitleKind::Empty => InvalidTitleKind::Empty,
                plakous_host_protocol::wire::InvalidTitleKind::TooLong => InvalidTitleKind::TooLong,
                plakous_host_protocol::wire::InvalidTitleKind::ControlCharacter => {
                    InvalidTitleKind::ControlCharacter
                }
            },
        },
        LifecycleReason::Busy {
            turn,
            children,
            children_truncated,
            processes,
            processes_truncated,
            state_unknown,
        } => R::Busy {
            turn,
            children,
            children_truncated,
            processes,
            processes_truncated,
            state_unknown,
        },
        LifecycleReason::Open => R::Open,
        LifecycleReason::LiveTree {
            children,
            children_truncated,
        } => R::LiveTree {
            children,
            children_truncated,
        },
        LifecycleReason::RecoveryRequiredTree {
            children,
            children_truncated,
        } => R::RecoveryRequiredTree {
            children,
            children_truncated,
        },
        LifecycleReason::ForkDependency {
            dependents,
            dependents_truncated,
        } => R::ForkDependency {
            dependents,
            dependents_truncated,
        },
    }
}

fn placement(reason: &PlacementReason) -> String {
    use plakous_host_protocol::wire::WorkspaceRefusalKind as W;
    match reason {
        PlacementReason::RunnerNotConfigured { saved_id, .. } => {
            format!("saved runner {saved_id} is not configured; restore its configuration")
        }
        PlacementReason::RunnerNotPaired => {
            "not paired; pair this runner before creating a session".into()
        }
        PlacementReason::RunnerIdentityMismatch { .. } => {
            "machine identity differs from its pin; verify the machine and pair it again".into()
        }
        PlacementReason::RunnerUnreachable { .. } => {
            "runner unavailable; check its connection and retry after reconciliation".into()
        }
        PlacementReason::RunnerVersionMismatch { .. } => {
            "runner version differs from this host; install matching binaries".into()
        }
        PlacementReason::RunnerRootsOverlap { path, .. } => {
            format!("workspace roots overlap at {path}; configure disjoint roots")
        }
        PlacementReason::WorkspaceRefused { kind, canonical } => match kind {
            W::InvalidPath => "workspace must be an absolute path without parent traversal".into(),
            W::NotCanonical => format!(
                "use the canonical workspace {}",
                canonical.as_deref().unwrap_or("reported by the runner")
            ),
            W::NotFound => "workspace does not exist on this runner".into(),
            W::NotDirectory => "workspace is not a directory".into(),
            W::OutsideRoots => "workspace is outside the runner's allowed roots".into(),
            W::ProductHome => "the product home cannot be a workspace".into(),
            W::PermissionDenied => {
                "the runner cannot access this workspace; check its permissions".into()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_host_refusals_share_product_prose_and_keep_exit_classes() {
        let blank = refusal(RequestRefusal::SessionLifecycleRefused {
            session: "session".into(),
            reason: LifecycleReason::InvalidTitle {
                kind: plakous_host_protocol::wire::InvalidTitleKind::Empty,
            },
        });
        assert_eq!(blank.exit_code(), 2);
        assert!(blank.message().contains("title is blank"));
        assert!(blank.message().contains("--clear"));
        let busy = refusal(RequestRefusal::SessionLifecycleRefused {
            session: "session".into(),
            reason: LifecycleReason::Busy {
                turn: false,
                children: vec!["child".into()],
                children_truncated: false,
                processes: Vec::new(),
                processes_truncated: false,
                state_unknown: true,
            },
        });
        assert_eq!(busy.exit_code(), 5);
        assert!(busy.message().contains("children: child"));
        assert!(busy.message().contains("activity unknown: true"));
    }

    #[test]
    fn host_refusals_keep_exit_classes_without_debug_text() {
        assert_eq!(
            refusal(RequestRefusal::DeadlineExceeded).message(),
            "the request took too long; retry"
        );
        let usage = refusal(RequestRefusal::BindingRefused {
            kind: BindingRefusalKind::UnknownProfile,
        });
        assert_eq!(usage.exit_code(), 2);
        assert!(usage.message().contains("exact profile id"));
        assert!(!usage.message().contains("BindingRefused"));
        let owner = refusal(RequestRefusal::TreeHostedElsewhere {
            host_name: "machine".into(),
            pid: 42,
            binary: None,
        });
        assert_eq!(owner.exit_code(), 7);
        assert!(owner.message().contains("release it there"));
        assert_eq!(
            refusal(RequestRefusal::SessionArchived {
                session: "session".into()
            })
            .exit_code(),
            5
        );
    }
}
