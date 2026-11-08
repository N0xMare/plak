//! Home-local daemon binding for the serving host.

use super::{CliResult, RunnerClient, Services, finish_stops, install_reporter, start_probes};

pub(crate) fn prepare(
    services: &Services,
    environment: &super::super::environment::HostEnvironment,
    runtime: &tokio::runtime::Runtime,
) -> CliResult<Services> {
    let ownership = std::sync::Arc::new(super::super::ownership::RunnerOwnership::host(
        services.paths()?,
    )?);
    let services = services.with_runner_ownership(ownership);
    let prepared = crate::commands::runner::registry::prepare(&services)?;
    let services = prepared.services;
    services.runners().require_connection("local", true);
    let environment = environment.clone();
    start_probes(
        &services,
        prepared.local,
        Some(std::sync::Arc::new(move |services, catalog, runtime| {
            let environment = environment.clone();
            Box::pin(async move { bind_local(&services, catalog, &environment, &runtime).await })
        })),
        runtime.handle(),
    );
    Ok(services)
}

async fn bind_local(
    services: &Services,
    catalog: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: &super::super::environment::HostEnvironment,
    runtime: &tokio::runtime::Handle,
) -> Result<(RunnerClient, bool), plakous::runner::PlacementFailure> {
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
    let (readopted, failures) =
        plakous_hosted::runner_adoption::readopt_lines(&client, &sessions, "plak:host:readopt")
            .await
            .map_err(|_| unavailable(ProbePhase::Reconcile))?;
    if failures.iter().any(|failure| !failure.held_elsewhere) {
        return Err(unavailable(ProbePhase::Reconcile));
    }
    let _ = client.recover().await;
    let pending = finish_stops(services, &client, &sessions, &readopted)
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile))?;
    client
        .reconcile_settled_foreground()
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile))?;
    let registry = services.runners().clone();
    let now = services.now_unix_seconds();
    tokio::task::spawn_blocking(move || registry.ready_local(catalog, now))
        .await
        .map_err(|_| unavailable(ProbePhase::Discovery))??;
    Ok((client, pending))
}
