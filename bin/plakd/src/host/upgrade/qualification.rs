//! Pre-pause capability and same-key qualification, and the held-head recheck.

use super::{
    PROBE_BOUND, Refusal, Request, bindings, check_cancel, io_failure, process, progress, receipt,
    reopen,
};
use crate::{
    CliFailure, CliResult, carry::upgrade::DestinationProbe, home::paths::PlakPaths,
    shell::services::Services,
};

pub(super) fn qualify_destination(
    services: &Services,
    request: &Request,
    arguments: &[String],
    report: &progress::Report<'_>,
) -> Result<
    (
        DestinationProbe,
        process::VerifiedImage,
        Option<reopen::Preflight>,
    ),
    Refusal,
> {
    let paths = services.paths()?;
    let identity = plakous_recursive_subagents::tree::custody_host();
    if request.source_instance != identity.instance.to_string()
        || request.source_key != crate::home::state::state_key()
        || !request.destination.is_absolute()
        || !crate::carry::upgrade::state_key_valid(&request.destination_key)
        || request.wait_seconds > 3600
        || !request.drain.valid()
    {
        return Err(CliFailure::new(
            "host_upgrade_stale_source: source instance or state key changed",
        )
        .into());
    }
    let image = progress::timed(report, progress::Phase::Image, || {
        process::verify_image(
            &std::env::current_exe().map_err(io_failure)?,
            crate::build_identity::binary_id(),
        )?;
        process::verify_image(&request.destination, &request.expected_binary)
    })?;
    let scratch = Workspace::new(paths, request)?;
    let (probe, before_rehearsal) = progress::timed(report, progress::Phase::Capability, || {
        // Rehearsal copies product and tree stores during the capability call.
        // A reusable verdict for those copies must not be stamped by newer live rows.
        let before_rehearsal = (request.source_key == request.destination_key)
            .then(|| reopen::snapshot(paths))
            .transpose()?;
        let probe = service_probe(paths, request, arguments, &scratch)?;
        Ok((probe, before_rehearsal))
    })?;
    if request.source_key == request.destination_key && !probe.accepts_reexec {
        return Err(Refusal::missing_descriptor_handoff());
    }
    let reopen = before_rehearsal
        .map(|roots| {
            let stores = scratch.stores();
            let stores = stores.is_dir().then_some(stores.as_path());
            bindings::preflight(
                paths,
                &request.destination,
                &request.destination_key,
                stores,
                report,
            )?;
            progress::timed(report, progress::Phase::ExactReopen, || {
                reopen::preflight(paths, request, stores, roots)
            })
        })
        .transpose()?;
    Ok((probe, image, reopen))
}

pub(super) fn recheck_held(
    services: &Services,
    request: &Request,
    arguments: &[String],
    preflight: &reopen::Preflight,
    cancelled: &std::sync::atomic::AtomicBool,
) -> CliResult<()> {
    check_cancel(cancelled)?;
    let paths = services.paths()?;
    let changed = preflight.changed(paths)?;
    if changed.is_empty() {
        return Ok(());
    }
    let scratch = preflight
        .rehearsed
        .then(|| {
            // The old copy does not contain product/tree commits made while
            // admissions were open. Rehearse on a new copy of the held state.
            let scratch = Workspace::new(paths, request)?;
            service_probe(paths, request, arguments, &scratch)?;
            if !scratch.stores().is_dir() {
                return Err(CliFailure::new(
                    "host upgrade store rehearsal did not produce held-state copies",
                ));
            }
            Ok(scratch)
        })
        .transpose()?;
    let stores = scratch.as_ref().map(Workspace::stores);
    reopen::verify_held(paths, request, stores.as_deref(), &changed)
}

struct Workspace {
    directory: tempfile::TempDir,
    _rehearsal: Option<std::fs::File>,
}

impl Workspace {
    fn new(paths: &PlakPaths, request: &Request) -> CliResult<Self> {
        let directory = tempfile::tempdir_in(paths.home()).map_err(io_failure)?;
        let rehearsal = (request.source_key == request.destination_key)
            .then(|| super::super::store_revisions::request_rehearsal(directory.path()))
            .transpose()?;
        Ok(Self {
            directory,
            _rehearsal: rehearsal,
        })
    }

    fn stores(&self) -> std::path::PathBuf {
        self.directory.path().join("stores")
    }
}

fn service_probe(
    paths: &PlakPaths,
    request: &Request,
    arguments: &[String],
    scratch: &Workspace,
) -> CliResult<DestinationProbe> {
    let service_file = scratch.directory.path().join("service.json");
    let probe_arguments = arguments
        .iter()
        .enumerate()
        .filter_map(|(index, argument)| {
            (!(index == 1 && argument == "__serve-auto")).then_some(argument)
        })
        .collect::<Vec<_>>();
    std::fs::write(
        &service_file,
        serde_json::to_vec(&crate::carry::upgrade::ServiceProbe {
            arguments: probe_arguments.into_iter().cloned().collect(),
            placements: probe_placements(paths)?,
        })
        .map_err(|error| CliFailure::new(error.to_string()))?,
    )
    .map_err(io_failure)?;
    let mut probe_command =
        process::command(&request.destination, paths.home(), &request.destination_key);
    probe_command
        .args(["version", "--host-upgrade-probe", "--host-upgrade-service"])
        .arg(service_file);
    if request.source_key == request.destination_key {
        probe_command.env(crate::environment::REEXEC_PROBE, "1");
    }
    let probe: DestinationProbe = serde_json::from_slice(
        &process::run(probe_command, PROBE_BOUND)?.stdout,
    )
    .map_err(|_| CliFailure::new("host upgrade destination did not answer its capability probe"))?;
    if probe.protocol != crate::carry::upgrade::PROTOCOL
        || probe.runner_protocol != probe.runner_version.protocol
        || probe.receipt_version != receipt::VERSION
        || !probe.host_available
        || probe.binary_id != request.expected_binary
        || probe.state_key != request.destination_key
        || probe.config_error.is_some()
        || probe.state_key != request.source_key
            && !probe.export_versions.contains(&crate::carry::VERSION)
    {
        return Err(CliFailure::new(format!(
            "host_upgrade_destination_unsupported: {}",
            probe
                .config_error
                .as_deref()
                .unwrap_or("identity, import version or host capability does not match")
        )));
    }
    if request.source_key == request.destination_key
        && probe.runner_protocol != crate::commands::runner::version().protocol
    {
        return Err(CliFailure::new(
            "host_upgrade_runner_protocol: settle and drain the old build's daemon before changing runner protocols",
        ));
    }
    Ok(probe)
}

fn probe_placements(paths: &PlakPaths) -> CliResult<Vec<crate::carry::upgrade::PlacementProbe>> {
    let withheld = receipt::withheld_targets(paths)?;
    let mut seen = std::collections::BTreeSet::new();
    let records =
        crate::store::ProductStore::inspect_upgrade_roots(paths).map_err(CliFailure::new)?;
    for record in records
        .iter()
        .filter(|record| !record.archived && !withheld.contains(&record.id))
    {
        if let Some(runner) = &record.runner {
            seen.insert((runner.runner_id.clone(), record.workspace_root.clone()));
        }
        if let Some((tree, _)) =
            plakous_recursive_subagents_store::SqliteTreeStore::inspect_tree_with_custody(
                &paths.subagents_database(),
                &record.node.tree_id,
            )
            .map_err(|error| CliFailure::new(error.message()))?
        {
            for node in tree.rows().nodes().values().filter(|node| node.active_turn) {
                if let Some(runner) = &node.placement.runner_id {
                    seen.insert((runner.clone(), node.path.clone()));
                }
            }
        }
    }
    Ok(seen
        .into_iter()
        .map(|(id, workspace)| crate::carry::upgrade::PlacementProbe { id, workspace })
        .collect())
}
