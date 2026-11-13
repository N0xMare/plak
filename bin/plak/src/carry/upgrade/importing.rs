//! A surrendered seed imports once under the serving destination's custody.

use super::{
    receipt::{Mapping, Phase, Receipt},
    remote,
};
use crate::carry::{CarryDocument, read_document};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

fn document(services: &Services, receipt: &Receipt, mapping: &Mapping) -> CliResult<CarryDocument> {
    let path = receipt
        .path(services.paths()?)
        .with_file_name(format!("{}.json", mapping.source));
    let document = read_document(&path).map_err(|error| CliFailure::new(error.to_string()))?;
    if document.local_digest.as_deref() != Some(mapping.digest.as_str())
        || document.source() != (mapping.source.as_str(), receipt.source_key.as_str())
        || document.runner_id() != mapping.runner
    {
        return Err(CliFailure::new(
            "host upgrade export does not match its reserved digest and provenance",
        ));
    }
    Ok(document)
}

pub(crate) fn one(services: &Services, receipt: &Receipt, mapping: &Mapping) -> CliResult<String> {
    if mapping.abandoned
        || receipt.destination_key != crate::home::state::state_key()
        || receipt.destination_binary != crate::build_identity::binary_id()
        || receipt.phase != Phase::Committed
        || services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
    {
        return Err(CliFailure::new(
            "host_upgrade_import_refused: destination identity or host custody does not match",
        ));
    }
    let records = services.session_records()?;
    match records.load(&mapping.successor) {
        Ok(record) => {
            if record.carried_forward_from()
                != Some((mapping.source.as_str(), receipt.source_key.as_str()))
                || record
                    .runner
                    .as_ref()
                    .map_or("local", |runner| runner.runner_id.as_str())
                    != mapping.runner
            {
                return Err(CliFailure::new(
                    "host upgrade successor conflicts with its reserved provenance",
                ));
            }
            if record.plakous_locator.is_some() {
                return Ok(record
                    .runner
                    .map_or_else(|| "local".to_owned(), |runner| runner.runner_id));
            }
        }
        Err(error) if error == "session not found" => {}
        Err(error) => return Err(CliFailure::new(error)),
    }
    let document = document(services, receipt, mapping)?;
    let id = document.runner_id().to_owned();
    let placed = remote::Placement::for_import(services, &id, document.workspace_root())?;
    let mut prepared = crate::commands::session::PreparedImport::new(document, false)?;
    prepared.upgrade_successor = Some(mapping.successor.clone());
    let (record, _) = crate::commands::session::import_prepared(&placed.services, prepared)?;
    if record.id != mapping.successor
        || records
            .load(&mapping.successor)
            .map_err(CliFailure::new)?
            .plakous_locator
            .is_none()
    {
        return Err(CliFailure::new(
            "host upgrade successor did not finish importing",
        ));
    }
    super::crash_cut("after-import-root");
    Ok(id)
}
