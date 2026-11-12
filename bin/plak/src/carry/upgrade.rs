//! Read-only destination admission for host-owned upgrades, separate from client reload.

use serde::{Deserialize, Serialize};

use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

#[cfg(feature = "web")]
pub(crate) mod process;
pub(crate) mod remote;

pub(crate) const PROTOCOL: u32 = 2;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ServiceProbe {
    pub(crate) arguments: Vec<String>,
    pub(crate) placements: Vec<PlacementProbe>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlacementProbe {
    pub(crate) id: String,
    pub(crate) workspace: std::path::PathBuf,
}

#[cfg(feature = "web")]
pub(crate) fn crash_cut(name: &str) {
    #[cfg(feature = "test-hooks")]
    if std::env::var("PLAK_TEST_HOST_UPGRADE_CUT").is_ok_and(|selected| selected == name) {
        std::process::exit(86);
    }
    #[cfg(not(feature = "test-hooks"))]
    let _ = name;
}

/// Embedded/headless writers hold this shared; a host upgrade takes it
/// exclusively before its final quiescence check. Host client faces do not.
pub(crate) fn admission(
    paths: &crate::home::paths::PlakPaths,
    exclusive: bool,
) -> CliResult<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;
    crate::commands::runner::private_directory(&paths.home().join("run"))?;
    let descriptor = rustix::fs::open(
        paths.home().join("run/upgrade-admission.lock"),
        OFlags::CREATE | OFlags::RDWR | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| CliFailure::new(error.to_string()))?;
    let file = std::fs::File::from(descriptor);
    let metadata = file
        .metadata()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(CliFailure::new(
            "host upgrade admission lock is not a private owned regular file",
        ));
    }
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|_| CliFailure::new("host_upgrade_admission_busy: an embedded writer or another upgrade owns this home; settle it first"))?;
    if !exclusive
        && receipt::all(paths)?
            .last()
            .is_some_and(|receipt| receipt.phase == receipt::Phase::Committed)
    {
        return Err(CliFailure::new(
            "host_upgrade_pending_ack: recover the destination before admitting standalone work or pruning source state",
        ));
    }
    Ok(file)
}
pub(crate) mod receipt;

pub(crate) fn print_probe(
    services: &Services,
    capacity: Option<&std::path::Path>,
    service: Option<&std::path::Path>,
    out: &mut dyn std::io::Write,
) -> CliResult<crate::ProcessOutcome> {
    if let Some(service) = service {
        probe_service(services, service)?;
    }
    match capacity {
        Some(path) => serde_json::to_writer(
            &mut *out,
            &crate::commands::session::probe_upgrade_capacity(services, path)?,
        ),
        None => serde_json::to_writer(&mut *out, &destination_probe(services)?),
    }
    .map_err(|_| CliFailure::new("stdout failed"))?;
    writeln!(out).map_err(|_| CliFailure::new("stdout failed"))?;
    Ok(crate::ProcessOutcome::Success)
}

fn probe_service(services: &Services, path: &std::path::Path) -> CliResult<()> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_end(&mut bytes))
        .map_err(|error| CliFailure::new(error.to_string()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(CliFailure::new(
            "host upgrade service arguments exceed their bound",
        ));
    }
    let probe: ServiceProbe = serde_json::from_slice(&bytes)
        .map_err(|_| CliFailure::new("invalid host upgrade service arguments"))?;
    if probe.arguments.len() > 64 || probe.placements.len() > 4096 {
        return Err(CliFailure::new(
            "host upgrade probe exceeds its argument or placement bound",
        ));
    }
    let crate::command::Command::Host(command) =
        crate::cli::parse(probe.arguments.into_iter().map(std::ffi::OsString::from))?
    else {
        return Err(CliFailure::new(
            "destination cannot serve the recorded host arguments",
        ));
    };
    #[cfg(feature = "web")]
    if let Some(web) = &command.web {
        crate::web::preflight(services, web)?;
    }
    #[cfg(not(feature = "web"))]
    let _ = (services, command);
    for placement in probe.placements {
        let _qualified = remote::Placement::prepare(services, &placement.id, &placement.workspace)?;
    }
    Ok(())
}

#[cfg(feature = "web")]
pub(crate) fn import(
    services: &Services,
    receipt_path: &std::path::Path,
) -> CliResult<crate::ProcessOutcome> {
    let paths = services.paths()?;
    let receipt = receipt::read(receipt_path)?;
    if receipt.path(paths) != receipt_path
        || receipt.destination_key != crate::home::state::state_key()
        || receipt.destination_binary != crate::build_identity::binary_id()
        || receipt.phase != receipt::Phase::Committed
        || services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
    {
        return Err(CliFailure::new(
            "host_upgrade_import_refused: destination or receipt identity does not match",
        ));
    }
    process::verify_image(&receipt.destination, &receipt.destination_binary)?;
    crate::home::state::ensure_usable_state(paths, &mut std::io::stderr())
        .map_err(CliFailure::new)?;
    for mapping in &receipt.mappings {
        let file = receipt_path
            .parent()
            .ok_or_else(|| CliFailure::new("upgrade receipt has no directory"))?
            .join(format!("{}.json", mapping.source));
        let document =
            super::read_document(&file).map_err(|error| CliFailure::new(error.to_string()))?;
        if document.read_digest() != Some(mapping.digest.as_str()) {
            return Err(CliFailure::new(
                "host upgrade export digest does not match its receipt",
            ));
        }
        if document.source() != (mapping.source.as_str(), receipt.source_key.as_str()) {
            return Err(CliFailure::new(
                "host upgrade export provenance does not match its receipt",
            ));
        }
        crate::commands::session::probe_upgrade_capacity(services, &file)?;
        let placed = remote::Placement::for_import(
            services,
            document.runner_id(),
            document.workspace_root(),
        )?;
        let mut prepared = crate::commands::session::PreparedImport::new(document, false)?;
        prepared.upgrade_successor = Some(mapping.successor.clone());
        let (record, _) = crate::commands::session::import_prepared(&placed.services, prepared)?;
        let saved = services
            .session_records()?
            .load(&mapping.successor)
            .map_err(CliFailure::new)?;
        if record.id != mapping.successor || saved.plakous_locator.is_none() {
            return Err(CliFailure::new(
                "host upgrade successor did not finish importing",
            ));
        }
        crash_cut("after-import-root");
    }
    Ok(crate::ProcessOutcome::Success)
}

pub(crate) fn binary_id_valid(value: &str) -> bool {
    value.len() == 12
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn state_key_valid(value: &str) -> bool {
    let mut parts = value.split('.');
    for prefix in ['r', 'e', 'p', 't'] {
        if parts
            .next()
            .and_then(|part| part.strip_prefix(prefix))
            .is_none_or(|number| {
                number.is_empty()
                    || !number.bytes().all(|byte| byte.is_ascii_digit())
                    || number.parse::<u32>().is_err()
            })
        {
            return false;
        }
    }
    parts.next().is_none()
}

/// Capability probe, not a session carrier or permission to surrender a source.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DestinationProbe {
    pub(crate) protocol: u32,
    pub(crate) binary_id: String,
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) host_available: bool,
    pub(crate) runner_protocol: u32,
    pub(crate) config_error: Option<String>,
}

pub(crate) fn destination_probe(services: &Services) -> CliResult<DestinationProbe> {
    let paths = services.paths()?;
    let config = crate::product_config::ProductConfig::load(paths)
        .map_err(CliFailure::new)
        .and_then(|config| crate::build_features::admit_config(&config));
    let policy = crate::product_config::SessionPolicy::load(paths).map_err(CliFailure::new);
    let export_versions = super::IMPORTABLE_VERSIONS.to_vec();
    #[cfg(feature = "test-hooks")]
    let export_versions =
        if std::env::var("PLAK_TEST_HOST_UPGRADE_IMPORT_REFUSAL").as_deref() == Ok("1") {
            Vec::new()
        } else {
            export_versions
        };
    Ok(DestinationProbe {
        protocol: PROTOCOL,
        binary_id: crate::build_identity::binary_id().to_owned(),
        state_key: crate::home::state::state_key(),
        export_versions,
        host_available: crate::host::AVAILABLE,
        runner_protocol: crate::commands::runner::version().protocol,
        config_error: config
            .and(policy.map(|_| ()))
            .err()
            .map(|error| error.message().to_owned()),
    })
}

pub(crate) fn require_capacity(capacity: &super::CarryCapacityProbe) -> CliResult<()> {
    match capacity.admitted_input {
        None => Err(CliFailure::new(
            "host_upgrade_capacity_unknown: destination input capacity is unknown; source remains authoritative",
        )),
        Some(input) if capacity.estimated_tokens > input => Err(CliFailure::new(format!(
            "host_upgrade_capacity_exceeded: estimated history {} exceeds destination input capacity {input}; source remains authoritative",
            capacity.estimated_tokens,
        ))),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_upgrade_refuses_unknown_or_smaller_capacity_instead_of_carrying_a_warning() {
        let mut capacity = crate::carry::CarryCapacityProbe {
            route: "fixture".to_owned(),
            total_window: Some(8192),
            admitted_input: None,
            estimated_tokens: 1024,
            messages: 2,
        };
        assert!(require_capacity(&capacity).is_err());
        capacity.admitted_input = Some(1023);
        assert!(require_capacity(&capacity).is_err());
        capacity.admitted_input = Some(1024);
        assert!(require_capacity(&capacity).is_ok());
    }
}
