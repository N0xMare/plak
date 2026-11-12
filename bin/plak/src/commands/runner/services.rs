//! Startup recovery of stopped roots and their daemon-held child lines.

use std::collections::BTreeMap;

use plakous::model::LineId;
use plakous::runner::RunnerClient;
use plakous::sessions::AgentSessions;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::sessions::projector::RootInterruptCause;

use crate::shell::services::Services;

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "owned stop-reconciliation fixtures")]
mod tests;

pub(crate) use super::ownership::StandaloneRunnerError;

#[cfg(feature = "web")]
pub(crate) mod host;

type LocalDial = dyn Fn(
        Services,
        std::sync::Arc<plakous::runner::WorkspaceCatalog>,
        tokio::runtime::Handle,
    ) -> std::pin::Pin<
        Box<
            dyn Future<
                    Output = Result<(RunnerClient, Vec<LineId>), plakous::runner::PlacementFailure>,
                > + Send,
        >,
    > + Send
    + Sync;

type RemoteDial = dyn Fn(
        Services,
        super::registry::Remote,
        tokio::runtime::Handle,
        Option<RunnerClient>,
    ) -> std::pin::Pin<Box<dyn Future<Output = RemoteBind> + Send>>
    + Send
    + Sync;
type RemoteBind = Result<(super::dial::Dialed, Vec<LineId>), plakous::runner::PlacementFailure>;

pub(crate) fn standalone(
    services: &Services,
    executor: &tokio::runtime::Handle,
) -> Result<Services, StandaloneRunnerError> {
    if let Some(owner) = services.runner_ownership() {
        return if owner.is_standalone() {
            Ok(services.clone())
        } else {
            Err(StandaloneRunnerError::ImportThroughRunningHost)
        };
    }
    let ownership = super::ownership::RunnerOwnership::standalone(
        services.paths().map_err(StandaloneRunnerError::Setup)?,
    )?;
    let services = services.with_runner_ownership(std::sync::Arc::new(ownership));
    let startup = services
        .paths()
        .map_err(StandaloneRunnerError::Setup)?
        .workspace()
        .map_err(|message| StandaloneRunnerError::Setup(CliFailure::new(message)))?;
    let prepared =
        super::registry::prepare(&services, Some(startup)).map_err(StandaloneRunnerError::Setup)?;
    start_probes(&prepared.services, prepared.local, None, executor);
    Ok(prepared.services)
}

fn start_probes(
    services: &Services,
    local: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: Option<std::sync::Arc<LocalDial>>,
    runtime: &tokio::runtime::Handle,
) {
    start_probes_with_dial(
        services,
        local,
        environment,
        runtime,
        std::sync::Arc::new(|services, remote, handle, client| {
            Box::pin(async move { bind_remote(&services, &remote, &handle, client.as_ref()).await })
        }),
    );
}

fn start_probes_with_dial(
    services: &Services,
    local: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: Option<std::sync::Arc<LocalDial>>,
    runtime: &tokio::runtime::Handle,
    dial: std::sync::Arc<RemoteDial>,
) -> Retries {
    services.runners().run_probes_on(runtime.clone());
    let transports = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::<
        String,
        crate::commands::runner::dial::Dialed,
    >::new()));
    let retries: Retries = std::sync::Arc::default();
    let retry_state = retries.clone();
    let context = services.runner_probe_context();
    let handle = runtime.clone();
    services
        .runners()
        .probe_with(std::sync::Arc::new(move |registry, id| {
            let services = context.with_runner_registry(registry.clone());
            let transports = transports.clone();
            let retries = retries.clone();
            let local = local.clone();
            let environment = environment.clone();
            let handle = handle.clone();
            let dial = dial.clone();
            Box::pin(async move {
                if id == "local" {
                    return probe_local(registry, services, local, environment, handle, retries)
                        .await;
                }
                let remote = crate::commands::runner::registry::remote(&services, &id)?;
                let connected = transports
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&id)
                    .filter(|dialed| dialed.client.is_connected())
                    .map(|dialed| (dialed.client.clone(), dialed.hello.clone()));
                if let Some((client, hello)) = connected {
                    let result = refresh_existing(&services, &remote, client.clone(), hello).await;
                    match &result {
                        Ok(Some(pending)) => {
                            complete_reconciliation(
                                &services,
                                &id,
                                &client,
                                pending.clone(),
                                &retries,
                            );
                        }
                        Err(plakous::runner::PlacementFailure::RunnerUnreachable { .. }) => {
                            retry_remote(registry, id, retries);
                        }
                        _ => {}
                    }
                    return result.map(|_| ());
                }
                let client = registry.client(&id);
                match dial(services.clone(), remote, handle, client).await {
                    Ok((dialed, pending)) => {
                        let client = dialed.client.clone();
                        transports
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id.clone(), dialed);
                        complete_reconciliation(&services, &id, &client, pending, &retries);
                        watch_remote(registry, id, client, retries);
                        Ok(())
                    }
                    Err(failure) => {
                        if matches!(
                            failure,
                            plakous::runner::PlacementFailure::RunnerUnreachable { .. }
                        ) {
                            retry_remote(registry, id, retries);
                        }
                        Err(failure)
                    }
                }
            })
        }));
    let registry = services.runners().clone();
    runtime.spawn(async move {
        let _ = registry.refresh(None).await;
    });
    retry_state
}

async fn probe_local(
    registry: plakous::runner::RunnerRegistry,
    services: Services,
    local: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: Option<std::sync::Arc<LocalDial>>,
    handle: tokio::runtime::Handle,
    retries: Retries,
) -> Result<(), plakous::runner::PlacementFailure> {
    let id = "local".to_owned();
    let Some(dial) = environment else {
        return ready_local(registry, local, None, services.now_unix_seconds()).await;
    };
    let client = services.local_runner_client();
    let was_connected = client.as_ref().is_some_and(RunnerClient::is_connected);
    if was_connected
        && registry.rows().iter().any(|row| {
            row.runner.runner_id == id && row.status == plakous::runner::RunnerStatus::Ready
        })
    {
        let result =
            ready_local(registry.clone(), local, client, services.now_unix_seconds()).await;
        if result.is_err() {
            retry_remote(registry, id, retries);
        }
        return result;
    }
    let result = dial(services.clone(), local, handle).await;
    match &result {
        Ok((client, pending)) => {
            complete_reconciliation(&services, &id, client, pending.clone(), &retries);
            if !was_connected {
                watch_remote(registry, id, client.clone(), retries);
            }
        }
        Err(_) => retry_remote(registry, id, retries),
    }
    result.map(|_| ())
}

async fn ready_local(
    registry: plakous::runner::RunnerRegistry,
    catalog: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    client: Option<RunnerClient>,
    now: u64,
) -> Result<(), plakous::runner::PlacementFailure> {
    let connection = client.as_ref().map(RunnerClient::connection_revision);
    tokio::task::spawn_blocking(move || match connection {
        Some(connection) => registry.ready_local_daemon(catalog, connection, now),
        None => registry.ready_local(catalog, now),
    })
    .await
    .map_err(|_| plakous::runner::PlacementFailure::RunnerUnreachable {
        phase: plakous::runner::ProbePhase::Discovery,
        timed_out: false,
    })?
}

#[derive(Clone)]
struct StopContext {
    services: Services,
    client: RunnerClient,
}

#[derive(Default)]
struct Retry {
    delay: u64,
    scheduled: bool,
    pending_stops: Vec<LineId>,
    context: Option<StopContext>,
    generation: u64,
}
type Retries = std::sync::Arc<std::sync::Mutex<BTreeMap<String, Retry>>>;

fn complete_reconciliation(
    services: &Services,
    id: &str,
    client: &RunnerClient,
    mut pending: Vec<LineId>,
    retries: &Retries,
) {
    let registry = services.runners();
    pending.sort();
    pending.dedup();
    let deferred = !pending.is_empty();
    registry.require_connection(id, deferred || id == "local");
    {
        let mut state = retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retry = state.entry(id.into()).or_default();
        retry.generation = retry.generation.wrapping_add(1);
        retry.pending_stops = pending;
        retry.context = deferred.then(|| StopContext {
            services: services.runner_probe_context(),
            client: client.clone(),
        });
        if !deferred {
            retry.delay = 0;
        }
    }
    if deferred {
        retry_remote(registry.clone(), id.into(), retries.clone());
    }
}

fn retry_remote(registry: plakous::runner::RunnerRegistry, id: String, retries: Retries) {
    if !registry.needs_connection(&id) {
        return;
    }
    let delay = {
        let mut state = retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retry = state.entry(id.clone()).or_default();
        if retry.scheduled {
            return;
        }
        retry.delay = match retry.delay {
            0 => 1,
            1 => 2,
            2 => 5,
            _ => 15,
        };
        retry.scheduled = true;
        retry.delay
    };
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        let ready = registry.rows().iter().any(|row| {
            row.runner.runner_id == id && row.status == plakous::runner::RunnerStatus::Ready
        });
        if ready {
            retry_stops(registry, id, retries).await;
        } else {
            if let Some(retry) = retries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_mut(&id)
            {
                retry.scheduled = false;
            }
            if registry.needs_connection(&id) {
                // Only reconnect/fresh-debt recovery closes runner admission.
                registry.reconcile_when_available(&id);
                let _ = registry.refresh(Some(&[id])).await;
            }
        }
    });
}

async fn retry_stops(registry: plakous::runner::RunnerRegistry, id: String, retries: Retries) {
    let snapshot = {
        let state = retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.get(&id).map(|retry| {
            (
                retry.generation,
                retry.pending_stops.clone(),
                retry.context.clone(),
            )
        })
    };
    let Some((generation, pending, context)) = snapshot else {
        return;
    };
    let remaining = if let Some(context) = context {
        let services = context.services.with_runner_registry(registry.clone());
        if let Ok(remaining) = stop_only_followup(&services, &context.client, &pending).await {
            remaining
        } else {
            tracing::warn!(target: "plak.runner", runner_id = %id, "recorded_stop_retry_failed");
            pending
        }
    } else {
        Vec::new()
    };
    let (same_pass, deferred) = {
        let mut state = retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(retry) = state.get_mut(&id) else {
            return;
        };
        let same_pass = retry.generation == generation;
        if same_pass {
            retry.pending_stops = remaining;
            if retry.pending_stops.is_empty() {
                retry.context = None;
                retry.delay = 0;
            }
        }
        retry.scheduled = false;
        (same_pass, !retry.pending_stops.is_empty())
    };
    if same_pass {
        registry.require_connection(&id, deferred || id == "local");
    }
    registry.progress(&id, plakous::runner::ProbePhase::Reconcile);
    if deferred {
        retry_remote(registry, id, retries);
    }
}

async fn stop_only_followup(
    services: &Services,
    client: &RunnerClient,
    pending: &[LineId],
) -> CliResult<Vec<LineId>> {
    use plakous::sessions::{AgentSessionsErrorKind, SessionOperationId};
    let roots = services.state_roots()?;
    let sessions = roots.runtime_sessions()?;
    let mut remaining = Vec::new();
    for &line in pending {
        let Some(_permit) = plakous_hosted::runner_adoption::try_lease_only(&roots, line)? else {
            remaining.push(line);
            continue;
        };
        let operation = SessionOperationId::new(format!("plak:host:stop-retry:{line}"))
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let head = match sessions.open_lease_only(&operation, line).await {
            Ok(head) => head,
            Err(error) if error.kind() == AgentSessionsErrorKind::NotFound => continue,
            Err(_) => {
                remaining.push(line);
                continue;
            }
        };
        // The later-submission and head checks run inside this line's lease.
        // No driver, model, runner-wide sweep or readiness mutation is admitted.
        let budget = std::time::Duration::from_secs(sessions.lease_period_seconds()) / 2;
        let stopped = tokio::time::timeout(
            budget,
            stop_current_head(services, client, &sessions, &head),
        )
        .await;
        let released = head.release().await;
        if !matches!(stopped, Ok(Ok(()))) || released.is_err() {
            remaining.push(line);
        }
    }
    Ok(remaining)
}

async fn stop_current_head(
    services: &Services,
    client: &RunnerClient,
    sessions: &AgentSessions,
    head: &plakous::sessions::LeaseOnlyHead,
) -> CliResult<()> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut stops = BTreeMap::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        collect_stops(&mut product, paths, sessions, &id, &mut stops).await?;
    }
    if let Some(intent) = stops.get(&head.line()) {
        plakous_hosted::runner_adoption::finish_recorded_stop_held(client, head, intent)
            .await
            .map_err(CliFailure::new)?;
    }
    Ok(())
}

fn watch_remote(
    registry: plakous::runner::RunnerRegistry,
    id: String,
    client: RunnerClient,
    retries: Retries,
) {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = client.closed() => break,
                () = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                    if !registry.needs_connection(&id) { registry.idle(&id); return; }
                }
            }
        }
        registry.blocked(
            &id,
            plakous::runner::PlacementFailure::RunnerUnreachable {
                phase: plakous::runner::ProbePhase::LinkLost,
                timed_out: false,
            },
            unix_now(),
        );
        retry_remote(registry, id, retries);
    });
}

async fn refresh_existing(
    services: &Services,
    remote: &crate::commands::runner::registry::Remote,
    client: RunnerClient,
    hello: plakous::runner::protocol::Hello,
) -> Result<Option<Vec<LineId>>, plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let connection = client.connection_revision();
    let unavailable = |phase| PlacementFailure::RunnerUnreachable {
        phase,
        timed_out: false,
    };
    services.runners().progress(&remote.id, ProbePhase::Options);
    let options = client
        .workspace_options()
        .await
        .map_err(|_| unavailable(ProbePhase::Options))?;
    crate::commands::runner::registry::confirm(services, remote, &hello, &options)?;
    services
        .runners()
        .progress(&remote.id, ProbePhase::Discovery);
    let discovery = client
        .workspace_discover()
        .await
        .map_err(|_| unavailable(ProbePhase::Discovery))?;
    let runner = services.runners().reconciliation_services(&remote.id)?;
    let pending = if services.runners().rows().iter().any(|row| {
        row.runner.runner_id == remote.id && row.status == plakous::runner::RunnerStatus::Ready
    }) {
        None
    } else {
        Some(reconcile_remote(services, &remote.id, &client).await?)
    };
    crate::test_hooks::runner_before_ready(&client).await?;
    let ready = services.runners().ready(
        &remote.id,
        connection,
        runner,
        options,
        discovery,
        services.now_unix_seconds(),
    );
    crate::test_hooks::runner_ready_result(&ready);
    ready?;
    Ok(pending)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn install_reporter(client: &RunnerClient) {
    client.report_foreground_with(|line, process| {
        tracing::info!(target: "plak.runner", line, call_key = %process.call_key, pid = process.pid, ended = %process.ended, "stopped_foreground");
    });
}

async fn bind_remote(
    services: &Services,
    remote: &crate::commands::runner::registry::Remote,
    runtime: &tokio::runtime::Handle,
    client: Option<&RunnerClient>,
) -> Result<(crate::commands::runner::dial::Dialed, Vec<LineId>), plakous::runner::PlacementFailure>
{
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase, timed_out| PlacementFailure::RunnerUnreachable { phase, timed_out };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    services.runners().progress(&remote.id, ProbePhase::Ssh);
    check_ssh(&remote.config.ssh).await?;
    services.runners().progress(&remote.id, ProbePhase::Daemon);
    let dialed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        match client {
            Some(client) => {
                crate::commands::runner::dial::reconnect_placed(
                    client,
                    &remote.config.ssh,
                    &remote.config.command,
                )
                .await
            }
            None => crate::commands::runner::registry::dial(services, remote).await,
        }
    })
    .await
    .map_err(|_| unavailable(ProbePhase::Daemon, true))??;
    finish_remote_bind(services, remote, runtime, deadline, dialed).await
}

async fn reconcile_remote(
    services: &Services,
    id: &str,
    client: &RunnerClient,
) -> Result<Vec<LineId>, plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase, timed_out| PlacementFailure::RunnerUnreachable { phase, timed_out };
    services
        .runners()
        .begin_reconcile(id, services.now_unix_seconds());
    let roots = services
        .state_roots()
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?
        .with_runner_readiness(services.runners().clone(), id);
    let sessions = roots
        .runtime_sessions()
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    let (readopted, failures) =
        plakous_hosted::runner_adoption::readopt_lines(client, &roots, "plak:host:readopt")
            .await
            .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    if let Some(failure) = failures.iter().find(|failure| !failure.held_elsewhere) {
        tracing::warn!(target: "plak.runner", phase = "sweep", line = %failure.line, error = %failure.reason, "runner_recovery_failed");
        return Err(unavailable(ProbePhase::Reconcile, false));
    }
    let _ = client.recover().await;
    let pending = finish_stops(services, client, &sessions, &readopted)
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    client
        .reconcile_settled_foreground()
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    #[cfg(test)]
    tests::production_fence::after_cleanup(client).await;
    Ok(pending)
}

async fn check_ssh(destination: &str) -> Result<(), plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |timed_out| PlacementFailure::RunnerUnreachable {
        phase: ProbePhase::Ssh,
        timed_out,
    };
    // The remote login shell resolves `true`; `/bin/true` is absent on macOS.
    let mut transport = crate::commands::runner::dial::ssh_command(destination, &["true"]);
    let transport = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        transport
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| unavailable(true))?
    .map_err(|_| unavailable(false))?;
    if !transport.success() {
        return Err(unavailable(false));
    }
    Ok(())
}

async fn finish_remote_bind(
    services: &Services,
    remote: &super::registry::Remote,
    runtime: &tokio::runtime::Handle,
    deadline: tokio::time::Instant,
    dialed: super::dial::Dialed,
) -> Result<(super::dial::Dialed, Vec<LineId>), plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let connection = dialed.client.connection_revision();
    let unavailable = |phase, timed_out| PlacementFailure::RunnerUnreachable { phase, timed_out };
    install_reporter(&dialed.client);
    services.runners().progress(&remote.id, ProbePhase::Bind);
    tokio::time::timeout_at(deadline, dialed.client.lines())
        .await
        .map_err(|_| unavailable(ProbePhase::Bind, true))?
        .map_err(|_| {
            if matches!(
                dialed.client.refusal(),
                Some(plakous::runner::protocol::Refusal::NotPaired { .. })
            ) {
                PlacementFailure::RunnerNotPaired
            } else {
                unavailable(ProbePhase::Bind, false)
            }
        })?;
    services.runners().progress(&remote.id, ProbePhase::Options);
    let options = tokio::time::timeout_at(deadline, dialed.client.workspace_options())
        .await
        .map_err(|_| unavailable(ProbePhase::Options, true))?
        .map_err(|_| unavailable(ProbePhase::Options, false))?;
    crate::commands::runner::registry::confirm(services, remote, &dialed.hello, &options)?;
    services
        .runners()
        .progress(&remote.id, ProbePhase::Discovery);
    let discovery = tokio::time::timeout_at(deadline, dialed.client.workspace_discover())
        .await
        .map_err(|_| unavailable(ProbePhase::Discovery, true))?
        .map_err(|_| unavailable(ProbePhase::Discovery, false))?;
    let pending = reconcile_remote(services, &remote.id, &dialed.client).await?;
    let backend = plakous::tools::runner::RunnerBackend::new(
        dialed.hello.descriptor.clone(),
        crate::test_hooks::runner_link(dialed.client.clone()),
    )
    .map_err(|_| unavailable(ProbePhase::Bind, false))?;
    crate::test_hooks::runner_before_ready(&dialed.client).await?;
    let ready = services.runners().ready(
        &remote.id,
        connection,
        plakous::runner::RunnerServices::new(std::sync::Arc::new(backend), runtime.clone()),
        options,
        discovery,
        services.now_unix_seconds(),
    );
    crate::test_hooks::runner_ready_result(&ready);
    ready?;
    tracing::info!(target: "plak.runner", runner_id = %remote.id, "runner_reconnected");
    Ok((dialed, pending))
}

/// A stop is durable before the host can die; finish it under a new head lease.
async fn finish_stops(
    services: &Services,
    client: &RunnerClient,
    sessions: &AgentSessions,
    readopted: &[plakous_hosted::runner_adoption::Readopted],
) -> CliResult<Vec<LineId>> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut stops = BTreeMap::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        collect_stops(&mut product, paths, sessions, &id, &mut stops).await?;
    }
    let held = client.lines().await.map_err(|error| {
        CliFailure::new(format!(
            "the local runner's lines could not be read: {error:?}"
        ))
    })?;
    let mut pending = Vec::new();
    for status in held {
        let line = match status.line.parse() {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(target: "plak.runner", phase = "finish_stops", line = %status.line, error = %error, "runner_recovery_failed");
                continue;
            }
        };
        if let Some(intent) = stops.get(&line) {
            if !readopted.iter().any(|adopted| adopted.line == line) {
                pending.push(line);
                continue;
            }
            plakous_hosted::runner_adoption::finish_recorded_stop(
                client,
                &services.state_roots()?,
                line,
                intent,
            )
            .await
            .map_err(|error| {
                CliFailure::new(format!("recorded stop on {line} remains pending: {error}"))
            })?;
        }
    }
    Ok(pending)
}

async fn collect_stops(
    product: &mut crate::store::ProductStore,
    paths: &crate::home::paths::PlakPaths,
    sessions: &AgentSessions,
    id: &str,
    stops: &mut BTreeMap<LineId, String>,
) -> CliResult<()> {
    let record = product.load(id).map_err(CliFailure::new)?;
    if let Some(tree) =
        crate::store::tree::load_tree(paths, &record.node.tree_id).map_err(CliFailure::new)?
    {
        for (locator, intent) in tree.recorded_process_stops() {
            match child_stop_line(sessions, &locator).await {
                Ok(line) => {
                    stops.insert(line, intent);
                }
                Err(error) => {
                    tracing::warn!(target: "plak.runner", phase = "finish_stops", session = %id, error = %error, "runner_recovery_failed");
                }
            }
        }
    }
    let intents = product.load_intents(id).map_err(CliFailure::new)?;
    let views = product.load_views(id).map_err(CliFailure::new)?;
    let Some(stop) = intents
        .root_interrupts
        .iter()
        .max_by_key(|intent| (intent.anchor, intent.ordinal))
        .filter(|intent| intent.value.cause == RootInterruptCause::Stopped)
        .filter(|stop| {
            !intents.submissions.iter().any(|submission| {
                intents
                    .submission_anchors
                    .get(&submission.command_id)
                    .is_some_and(|anchor| *anchor >= stop.anchor)
                    && views.as_ref().is_some_and(|views| {
                        matches!(
                            views.state.deliveries.get(&submission.command_id),
                            Some(plakous_hosted::sessions::projector::Delivery::Accepted)
                        )
                    })
            })
        })
    else {
        return Ok(());
    };
    let intent = format!("plak:stop:{id}:{}", stop.ordinal);
    if let Some(locator) = record.plakous_locator {
        if admitted_since(sessions, locator.session_id(), stop.anchor).await? {
            return Ok(());
        }
        let head = sessions
            .line_head_of(locator)
            .await
            .map_err(|error| CliFailure::new(error.to_string()))?;
        if head.session_id() != locator.session_id() || head.branch_id() != locator.branch_id() {
            return Ok(());
        }
        stops.insert(head.line(), intent);
    }
    Ok(())
}

async fn child_stop_line(sessions: &AgentSessions, locator: &str) -> CliResult<LineId> {
    let locator = locator
        .parse::<plakous::sessions::SessionLocator>()
        .map_err(|error| CliFailure::new(error.to_string()))?;
    sessions
        .line_head_of(locator)
        .await
        .map(|head| head.line())
        .map_err(|error| CliFailure::new(error.to_string()))
}

async fn admitted_since(
    sessions: &AgentSessions,
    session: plakous::model::SessionId,
    anchor: u64,
) -> CliResult<bool> {
    use plakous::sessions::feed::{
        FeedCommandKind, FeedCursor, FeedFact, FeedPage, FeedPageRequest, FeedSeq,
    };
    let mut cursor = FeedSeq::new(anchor);
    loop {
        let page = sessions
            .feed_page(FeedPageRequest::after(FeedCursor::new(session, cursor)))
            .await
            .map_err(|error| CliFailure::new(error.to_string()))?;
        let FeedPage::Records { records, .. } = page else {
            // Retired history cannot authorize stopping a later owner's jobs.
            return Ok(true);
        };
        if records.is_empty() {
            return Ok(false);
        }
        for record in records {
            if record.facts().iter().any(|fact| {
                matches!(
                    fact,
                    FeedFact::CommandAccepted {
                        kind: FeedCommandKind::Root { .. },
                        ..
                    }
                )
            }) {
                return Ok(true);
            }
            cursor = record.header().seq;
        }
    }
}
