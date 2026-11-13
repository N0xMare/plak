//! Read-only destination admission for host-owned upgrades, separate from client reload.

use serde::{Deserialize, Serialize};

use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

#[cfg(feature = "web")]
pub(crate) mod process;
pub(crate) mod remote;

pub(crate) const PROTOCOL: u32 = 3;

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
    crate::test_hooks::upgrade_crash_cut(name);
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
    crate::host::preflight_upgrade_service(services, &command)?;
    for placement in probe.placements {
        let _qualified = remote::Placement::prepare(services, &placement.id, &placement.workspace)?;
    }
    Ok(())
}

#[cfg(feature = "web")]
pub(crate) mod importing;

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
    pub(crate) receipt_version: u32,
    pub(crate) binary_id: String,
    pub(crate) state_key: String,
    pub(crate) export_versions: Vec<u32>,
    pub(crate) host_available: bool,
    pub(crate) runner_protocol: u32,
    pub(crate) runner_version: plakous::runner::protocol::VersionTriple,
    pub(crate) config_error: Option<String>,
}

pub(crate) fn destination_probe(services: &Services) -> CliResult<DestinationProbe> {
    let paths = services.paths()?;
    let config = crate::product_config::ProductConfig::load(paths)
        .map_err(CliFailure::new)
        .and_then(|config| crate::build_features::admit_config(&config));
    let policy = crate::product_config::SessionPolicy::load(paths).map_err(CliFailure::new);
    let export_versions = super::IMPORTABLE_VERSIONS.to_vec();
    let export_versions = if crate::test_hooks::upgrade_import_refused() {
        Vec::new()
    } else {
        export_versions
    };
    let mut runner_version = crate::commands::runner::version();
    if let Some(revision) = crate::test_hooks::upgrade_runner_revision() {
        runner_version.source_revision = revision;
    }
    Ok(DestinationProbe {
        protocol: PROTOCOL,
        receipt_version: receipt::VERSION,
        binary_id: crate::build_identity::binary_id().to_owned(),
        state_key: crate::home::state::state_key(),
        export_versions,
        host_available: crate::host::AVAILABLE,
        runner_protocol: crate::commands::runner::version().protocol,
        runner_version,
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
