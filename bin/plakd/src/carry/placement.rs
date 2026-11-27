//! Scoped read-only qualification never publishes runner Ready.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::commands::runner::{dial, registry};
use crate::shell::services::Services;
use crate::{CliFailure, CliResult};

/// Runner executables a qualification dials instead of a paired runner's
/// configured command, by configured runner name: the candidate's own image
/// on the runner machine, so the probe reaches the candidate's already running
/// exact-triple daemon while the command link still names the incumbent.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RunnerCommands(Vec<(String, String)>);

impl RunnerCommands {
    /// Parses repeated `NAME=PATH` values: an absolute path of plain
    /// characters, since the runner machine's shell reads the remote command.
    pub(crate) fn parse(values: &[String]) -> Result<Self, String> {
        let mut commands = Vec::with_capacity(values.len());
        for value in values {
            let refused = || {
                format!(
                    "--runner-command {value}: expected NAME=PATH with an absolute path of letters, digits and / . _ + -"
                )
            };
            let (name, path) = value.split_once('=').ok_or_else(refused)?;
            let plain = |text: &str| {
                text.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/._+-".contains(&byte))
            };
            if name.is_empty() || !plain(name) || !path.starts_with('/') || !plain(path) {
                return Err(refused());
            }
            if commands.iter().any(|(known, _)| known == name) {
                return Err(format!("--runner-command names runner {name} twice"));
            }
            commands.push((name.to_owned(), path.to_owned()));
        }
        Ok(Self(commands))
    }

    /// Refuses a name no `[runners.<name>]` configures, so a misspelt
    /// runner cannot silently leave its roots on the configured command.
    pub(crate) fn refuse_unconfigured(
        &self,
        paths: &crate::home::paths::PlakPaths,
    ) -> Result<(), String> {
        if self.0.is_empty() {
            return Ok(());
        }
        let configured = crate::product_config::ProductConfig::load(paths)?.runners;
        match self
            .0
            .iter()
            .find(|(name, _)| !configured.contains_key(name))
        {
            Some((name, _)) => Err(format!(
                "check_reopen_runner_command: --runner-command names {name}, which no [runners.{name}] configures"
            )),
            None => Ok(()),
        }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(known, _)| known == name)
            .map(|(_, path)| path.as_str())
    }
}

pub(crate) struct Placement {
    pub(crate) services: Services,
    // The client and its executor outlive configuration construction.
    _dialed: Option<dial::Dialed>,
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Placement {
    pub(crate) fn prepare(
        services: &Services,
        id: &str,
        workspace: &Path,
        commands: &RunnerCommands,
    ) -> CliResult<Self> {
        if id == "local" {
            return Ok(Self {
                services: services.clone(),
                _dialed: None,
                _runtime: None,
            });
        }
        let remote = registry::paired_remote(services.paths()?, id).map_err(refused)?;
        let command = commands.get(&remote.name).unwrap_or(&remote.config.command);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let dialed = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(25), async {
                let dialed = dial::qualify(services.paths()?, &remote.config.ssh, command).await?;
                if dialed.hello.runner_id != remote.id || dialed.hello.machine_id.as_deref() != Some(remote.pin.machine_id.as_str()) {
                    return Err(CliFailure::new("check_reopen_runner_identity: re-pair the configured runner using the retained build"));
                }
                let options = dialed.client.workspace_options().await.map_err(|failure| CliFailure::new(format!("runner qualification refused: {failure:?}")))?;
                if options.roots.as_ref() != Some(&remote.pin.roots) {
                    return Err(CliFailure::new("check_reopen_runner_roots: effective roots differ from the pairing; re-pair before checking reopen"));
                }
                let workspace = workspace.to_str().ok_or_else(|| CliFailure::new("runner workspace is not UTF-8"))?;
                dialed.client.workspace_validate(workspace, true).await
                    .map_err(|failure| CliFailure::new(format!("runner qualification failed: {failure:?}")))?
                    .map_err(|failure| CliFailure::new(format!("runner workspace refused: {failure:?}")))?;
                Ok::<_, CliFailure>(dialed)
            }).await.map_err(|_| CliFailure::new("check_reopen_runner_connection_timeout: the paired runner did not finish qualification"))?
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
            services: services.with_upgrade_runner(id, workspace, runner),
            _dialed: Some(dialed),
            _runtime: Some(runtime),
        })
    }
}

fn refused(failure: plakous::runner::PlacementFailure) -> CliFailure {
    CliFailure::new(format!(
        "check_reopen_runner_qualification: {failure:?}; point the runner link at the matching snapshot and start its daemon before checking reopen"
    ))
}
