//! Scoped qualification and host-owned seeding never publish runner Ready.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::runner::{dial, registry};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

pub(crate) struct Placement {
    pub(crate) services: Services,
    // The client and its executor outlive configuration construction and seeding.
    _dialed: Option<dial::Dialed>,
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Placement {
    pub(crate) fn prepare(services: &Services, id: &str, workspace: &Path) -> CliResult<Self> {
        Self::qualify(services, id, workspace)
    }

    #[cfg(feature = "web")]
    pub(super) fn for_import(services: &Services, id: &str, workspace: &Path) -> CliResult<Self> {
        if services
            .runner_ownership()
            .is_none_or(crate::commands::runner::ownership::RunnerOwnership::is_standalone)
        {
            return Err(CliFailure::new(
                "passive runner binding requires destination host custody",
            ));
        }
        if id == "local" {
            return Ok(Self {
                services: services.clone(),
                _dialed: None,
                _runtime: None,
            });
        }
        let runner = services
            .runners()
            .services(id)
            .map_err(refused)?
            .ok_or_else(|| CliFailure::new("remote import requires a Ready registry connection"))?;
        let client = services
            .runners()
            .client(id)
            .ok_or_else(|| CliFailure::new("remote import requires the registry owned client"))?;
        let path = workspace
            .to_str()
            .ok_or_else(|| CliFailure::new("runner workspace is not UTF-8"))?;
        runner.runtime().block_on(async {
            tokio::time::timeout(
                Duration::from_secs(25),
                client.workspace_validate(path, true),
            )
            .await
            .map_err(|_| CliFailure::new("remote import workspace validation timed out"))?
            .map_err(|error| CliFailure::new(format!("runner validation failed: {error:?}")))?
            .map_err(|error| CliFailure::new(format!("runner workspace refused: {error:?}")))
        })?;
        Ok(Self {
            services: services.with_upgrade_runner(id, workspace, runner, Some(client)),
            _dialed: None,
            _runtime: None,
        })
    }

    fn qualify(services: &Services, id: &str, workspace: &Path) -> CliResult<Self> {
        if id == "local" {
            return Ok(Self {
                services: services.clone(),
                _dialed: None,
                _runtime: None,
            });
        }
        let remote = registry::paired_remote(services.paths()?, id).map_err(refused)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let dialed = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(25), async {
                let dialed = dial::qualify(services.paths()?, &remote.config.ssh, &remote.config.command).await?;
                if dialed.hello.runner_id != remote.id || dialed.hello.machine_id.as_deref() != Some(remote.pin.machine_id.as_str()) {
                    return Err(CliFailure::new("host_upgrade_runner_identity: re-pair the configured runner using the retained build"));
                }
                let options = dialed.client.workspace_options().await.map_err(|failure| CliFailure::new(format!("runner qualification refused: {failure:?}")))?;
                if options.roots.as_ref() != Some(&remote.pin.roots) {
                    return Err(CliFailure::new("host_upgrade_runner_roots: effective roots differ from the pairing; re-pair before upgrade"));
                }
                let workspace = workspace.to_str().ok_or_else(|| CliFailure::new("runner workspace is not UTF-8"))?;
                dialed.client.workspace_validate(workspace, true).await
                    .map_err(|failure| CliFailure::new(format!("runner qualification failed: {failure:?}")))?
                    .map_err(|failure| CliFailure::new(format!("runner workspace refused: {failure:?}")))?;
                Ok::<_, CliFailure>(dialed)
            }).await.map_err(|_| CliFailure::new("host_upgrade_runner_connection_timeout: the paired runner did not finish preparation"))?
        })?;
        let backend = plakous::tools::runner::RunnerBackend::new(
            dialed.hello.descriptor.clone(),
            crate::test_hooks::runner_link(dialed.client.clone()),
        )
        .map_err(|error| CliFailure::new(error.to_string()))?;
        if backend.runner_id().as_str() != id {
            return Err(CliFailure::new(
                "runner descriptor identity differs from its paired hello",
            ));
        }
        let runner =
            plakous::runner::RunnerServices::new(Arc::new(backend), runtime.handle().clone());
        Ok(Self {
            services: services.with_upgrade_runner(id, workspace, runner, None),
            _dialed: Some(dialed),
            _runtime: Some(runtime),
        })
    }
}

fn refused(failure: plakous::runner::PlacementFailure) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_runner_qualification: {failure:?}; advance the runner dedicated link to the matching snapshot first; retain the old build to inspect and drain old workers"
    ))
}
