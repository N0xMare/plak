//! CLI lifecycle requests use the live host; only proved absence permits offline work.

use plakous_host_protocol::wire::{Answer, Op, Outcome};

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
            return Err(crate::wire_transport::refusal(reason));
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
