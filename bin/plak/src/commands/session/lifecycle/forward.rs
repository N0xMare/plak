//! CLI lifecycle requests use the live host; only proved absence permits offline work.

use plakous_host_protocol::wire::{Answer, LifecycleReason, Op, Outcome, RequestRefusal};

use crate::shell::services::Services;
use crate::surface_protocol::PlakExtension;
use plakous_hosted::error::{CliFailure, CliResult};

pub(super) enum Change {
    Rename(Option<String>),
    Archive(bool),
    Delete(bool),
}

pub(super) fn request(services: &Services, id: &str, change: Change) -> CliResult<bool> {
    let operation: Op<PlakExtension> = match &change {
        Change::Rename(title) => Op::RenameSession {
            session: id.to_owned(),
            title: title.clone(),
        },
        Change::Archive(archived) => Op::ArchiveSession {
            session: id.to_owned(),
            archived: *archived,
        },
        Change::Delete(abandon_children) => Op::DeleteSession {
            session: id.to_owned(),
            abandon_children: *abandon_children,
        },
    };
    let Some(outcome) = crate::wire_transport::forward_lifecycle(services.paths()?, operation)?
    else {
        return Ok(false);
    };
    let answer = match outcome {
        Outcome::Ok(answer) => answer,
        Outcome::Refused(reason) => {
            return Err(CliFailure::new(host_refusal(reason)));
        }
    };
    let matched = match (&change, &answer) {
        (Change::Rename(_), Answer::Renamed { session, .. })
        | (Change::Delete(_), Answer::Deleted { session }) => session == id,
        (Change::Archive(expected), Answer::ArchiveUpdated { session, archived }) => {
            session == id && expected == archived
        }
        _ => false,
    };
    if !matched {
        return Err(CliFailure::new(
            "host lifecycle outcome did not match the request; inspect the session before retrying; no offline mutation attempted",
        ));
    }
    Ok(true)
}

fn host_refusal(reason: RequestRefusal) -> String {
    match reason {
        RequestRefusal::SessionLifecycleRefused { reason, .. } => product_refusal(reason).to_string(),
        RequestRefusal::SessionUnavailable { detail } => format!("{detail}; refresh the session list and retry"),
        RequestRefusal::SessionArchived { .. } => "the session is archived; unarchive it before writing".to_owned(),
        RequestRefusal::UnknownSession => super::LifecycleRefusal::NotFound.to_string(),
        _ => "the host refused the request; refresh the session list and retry; no offline mutation attempted".to_owned(),
    }
}

fn product_refusal(reason: LifecycleReason) -> super::LifecycleRefusal {
    use super::{InvalidTitleKind, LifecycleRefusal as R};
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
