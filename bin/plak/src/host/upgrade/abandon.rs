//! Explicit disposition never revives a reserved successor or releases source fencing.

use super::{
    Answer, CliFailure, CliResult, Identity, PROBE_BOUND, Phase, ProcessOutcome, Request, Services,
    io_failure, read, receipt, socket, startup, write,
};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Deserialize, Serialize)]
pub(super) struct Abandoned {
    successor: String,
    export: PathBuf,
}

pub(in crate::host) fn request(services: &Services, target: &str) -> CliResult<ProcessOutcome> {
    let paths = services.paths()?;
    let mut stream = crate::wire_transport::hosts::socket_until(
        &socket(paths),
        std::time::Instant::now() + PROBE_BOUND,
    )
    .map_err(|_| {
        CliFailure::new("start the recorded destination host before abandoning a successor")
    })?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(600)))
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity: Identity = read(&stream)?;
    if identity.protocol != crate::carry::upgrade::PROTOCOL
        || identity.key != crate::home::state::state_key()
        || identity.binary != crate::build_identity::binary_id()
    {
        return Err(CliFailure::new(
            "abandon requires the serving destination executable and state key",
        ));
    }
    write(
        &mut stream,
        &Request {
            source_instance: identity.instance,
            source_key: identity.key.clone(),
            destination: std::env::current_exe().map_err(io_failure)?,
            destination_key: identity.key,
            expected_binary: identity.binary,
            wait_seconds: 0,
            drain: crate::command::HostDrain::default(),
            abandon: Some(target.to_owned()),
        },
    )?;
    match read::<Answer>(&stream)? {
        Answer::Abandoned { mappings } => {
            let mut out = std::io::stdout().lock();
            for mapping in mappings {
                writeln!(
                    out,
                    "abandoned {}; retained export: {}",
                    mapping.successor,
                    mapping.export.display()
                )
                .map_err(|_| CliFailure::new("stdout failed"))?;
            }
            Ok(ProcessOutcome::Success)
        }
        Answer::Refused { detail } => Err(CliFailure::new(detail)),
        Answer::Ready { .. } => Err(CliFailure::new("unexpected upgrade reply to abandon")),
    }
}

pub(super) fn apply(services: &Services, request: &Request) -> CliResult<Vec<Abandoned>> {
    let identity = plakous_recursive_subagents::tree::custody_host();
    let own = crate::home::state::state_key();
    if request.source_instance != identity.instance.to_string()
        || request.source_key != own
        || request.destination_key != own
        || request.expected_binary != crate::build_identity::binary_id()
    {
        return Err(CliFailure::new("abandon source identity changed"));
    }
    let target = request
        .abandon
        .as_deref()
        .ok_or_else(|| CliFailure::new("abandon selector missing"))?;
    let paths = services.paths()?;
    let _lock = receipt::lock(paths)?;
    let mut abandoned = Vec::new();
    for mut receipt in receipt::all(paths)?.into_iter().filter(|receipt| {
        receipt.destination_key == own
            && receipt.destination_binary == crate::build_identity::binary_id()
            && matches!(receipt.phase, Phase::Committed | Phase::Acknowledged)
    }) {
        let selected = abandoned.len();
        for mapping in &mut receipt.mappings {
            if mapping.runner == "local"
                || mapping.ready
                || (mapping.successor != target && mapping.runner != target)
            {
                continue;
            }
            mapping.abandoned = true;
            abandoned.push(Abandoned {
                successor: mapping.successor.clone(),
                export: receipt::root(paths)
                    .join(receipt.id.to_string())
                    .join(format!("{}.json", mapping.source)),
            });
        }
        if abandoned.len() == selected {
            continue;
        }
        startup::acknowledge(&mut receipt);
        receipt.save(paths)?;
    }
    if abandoned.is_empty() {
        return Err(CliFailure::new(
            "no pending or already-abandoned remote successor matches the exact UUID or runner ID; Ready and local roots cannot be abandoned",
        ));
    }
    Ok(abandoned)
}
