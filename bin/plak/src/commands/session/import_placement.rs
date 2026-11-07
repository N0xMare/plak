//! Standalone import custody and its executor outlive the imported interactive face.

use crate::shell::services::Services;
use plakous_hosted::error::{CliFailure, CliFailureKind, CliResult};
use std::path::Path;

pub(crate) struct ImportPlacement {
    pub(crate) services: Services,
    // Dropped after the services: its runner tasks remain available through the face.
    _runtime: Option<tokio::runtime::Runtime>,
}

fn standalone_refusal(
    error: crate::commands::runner::services::StandaloneRunnerError,
) -> CliFailure {
    use crate::commands::runner::services::StandaloneRunnerError;
    match error {
        StandaloneRunnerError::ImportThroughRunningHost => CliFailure::classified(
            CliFailureKind::ImportThroughRunningHost,
            "a running host owns this home; stop the host, or import through a client attached to it",
        ),
        StandaloneRunnerError::HomeBusy => CliFailure::classified(
            CliFailureKind::Usage,
            "another standalone runner operation owns this home; retry after it finishes",
        ),
        StandaloneRunnerError::Setup(failure) => failure,
    }
}

impl ImportPlacement {
    pub(crate) fn prepare(services: &Services, id: &str, workspace: &Path) -> CliResult<Self> {
        if id == "local" {
            return Ok(Self {
                services: services.clone(),
                _runtime: None,
            });
        }
        let (services, runtime) = if services.runner_ownership().is_some() {
            (services.clone(), None)
        } else {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|error| {
                    CliFailure::new(format!("import runner executor failed: {error}"))
                })?;
            let selected = services
                .with_standalone_runners(runtime.handle())
                .map_err(standalone_refusal)?;
            (selected, Some(runtime))
        };
        let handle = if let Some(runtime) = &runtime {
            runtime.handle().clone()
        } else {
            services
                .runners()
                .reconciliation_services(id)
                .map_err(|failure| {
                    CliFailure::new(format!("runner placement refused: {failure:?}"))
                })?
                .runtime()
                .clone()
        };
        let workspace = workspace.to_str().ok_or_else(|| {
            CliFailure::classified(CliFailureKind::Usage, "runner workspace is not UTF-8")
        })?;
        handle
            .block_on(async {
                let registry = services.runners();
                registry.refresh(Some(&[id.to_owned()])).await?;
                registry.validate_workspace(id, workspace, true).await
            })
            .map_err(|failure| {
                CliFailure::classified(
                    CliFailureKind::Usage,
                    format!("runner placement refused before import: {failure:?}"),
                )
            })?;
        Ok(Self {
            services,
            _runtime: runtime,
        })
    }
}
