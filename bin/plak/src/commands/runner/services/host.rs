//! Home-local daemon binding for the serving host.

use super::{CliResult, RunnerClient, Services, install_reporter, start_probes};

pub(crate) fn prepare(
    services: &Services,
    startup: Option<&std::path::Path>,
) -> CliResult<(Services, std::sync::Arc<plakous::runner::WorkspaceCatalog>)> {
    let services = custody(services)?;
    let prepared = crate::commands::runner::registry::prepare(&services, startup)?;
    let services = prepared.services;
    services.runners().require_connection("local", true);
    Ok((services, prepared.local))
}

pub(crate) fn custody(services: &Services) -> CliResult<Services> {
    if let Some(owner) = services.runner_ownership() {
        if owner.is_standalone() {
            return Err(crate::CliFailure::new(
                "a standalone owner cannot become a serving host",
            ));
        }
        return Ok(services.clone());
    }
    let ownership = std::sync::Arc::new(super::super::ownership::RunnerOwnership::host(
        services.paths()?,
    )?);
    Ok(services.with_runner_ownership(ownership))
}

/// Starts daemon probes only after every host endpoint has bound successfully.
pub(crate) fn start(
    services: &Services,
    local: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: &super::super::environment::HostEnvironment,
    runtime: &tokio::runtime::Runtime,
) {
    let environment = environment.clone();
    start_probes(
        services,
        local,
        Some(std::sync::Arc::new(move |services, catalog, runtime| {
            let environment = environment.clone();
            Box::pin(async move { bind_local(&services, catalog, &environment, &runtime).await })
        })),
        runtime.handle(),
    );
}

async fn bind_local(
    services: &Services,
    catalog: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: &super::super::environment::HostEnvironment,
    runtime: &tokio::runtime::Handle,
) -> Result<(RunnerClient, Vec<super::LineId>), plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase| PlacementFailure::RunnerUnreachable {
        phase,
        timed_out: false,
    };
    let paths = services
        .paths()
        .map_err(|_| unavailable(ProbePhase::Daemon))?;
    services
        .runners()
        .begin_reconcile("local", services.now_unix_seconds());
    services.runners().progress("local", ProbePhase::Daemon);
    let client = if let Some(client) = services.local_runner_client() {
        if !client.is_connected() {
            crate::commands::runner::local::reconnect(paths, &client)
                .await
                .map_err(|error| {
                    tracing::warn!(target: "plak.runner", runner_id = "local", phase = "daemon", error = %error, "runner_recovery_failed");
                    unavailable(ProbePhase::Daemon)
                })?;
        }
        client
    } else {
        crate::commands::runner::local::dial(paths)
            .await
            .map_err(|error| {
                tracing::warn!(target: "plak.runner", runner_id = "local", phase = "daemon", error = %error, "runner_recovery_failed");
                unavailable(ProbePhase::Daemon)
            })?
    };
    install_reporter(&client);
    let _ = services.with_local_daemon(
        environment.backend(client.clone(), runtime.clone()),
        client.clone(),
    );
    services.runners().progress("local", ProbePhase::Reconcile);
    let sessions = services
        .state_roots()
        .and_then(|roots| roots.runtime_sessions())
        .map_err(|_| unavailable(ProbePhase::Reconcile))?;
    let obsolete = crate::commands::runner::local::older(paths)
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile))?;
    plakous_hosted::runner_adoption::report_obsolete_lines(&sessions, obsolete).await;
    finish_local_bind(services, catalog, client).await
}

pub(super) async fn finish_local_bind(
    services: &Services,
    catalog: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    client: RunnerClient,
) -> Result<(RunnerClient, Vec<super::LineId>), plakous::runner::PlacementFailure> {
    let connection = client.connection_revision();
    let pending = super::reconcile_remote(services, "local", &client).await?;
    let registry = services.runners().clone();
    let now = services.now_unix_seconds();
    tokio::task::spawn_blocking(move || registry.ready_local_daemon(catalog, connection, now))
        .await
        .map_err(|_| plakous::runner::PlacementFailure::RunnerUnreachable {
            phase: plakous::runner::ProbePhase::Discovery,
            timed_out: false,
        })??;
    Ok((client, pending))
}
