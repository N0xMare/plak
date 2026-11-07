//! Startup recovery of stopped roots and their daemon-held child lines.

use std::collections::BTreeMap;

use plakous::model::LineId;
use plakous::runner::RunnerClient;
use plakous::sessions::AgentSessions;
use plakous_hosted::error::{CliFailure, CliResult};
use plakous_hosted::sessions::projector::RootInterruptCause;

use crate::shell::services::Services;

pub(super) fn prepare(
    services: &Services,
    environment: &super::environment::HostEnvironment,
    runtime: &tokio::runtime::Runtime,
) -> CliResult<Services> {
    let prepared = crate::commands::runner::registry::prepare(services)?;
    let services = prepared.services;
    services.runners().require_connection("local", true);
    start_remotes(
        &services,
        prepared.remote,
        prepared.local,
        environment.clone(),
        runtime.handle(),
    );
    Ok(services)
}

fn start_remotes(
    services: &Services,
    remote: Vec<crate::commands::runner::registry::Remote>,
    local: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: super::environment::HostEnvironment,
    runtime: &tokio::runtime::Handle,
) {
    let remote = std::sync::Arc::new(
        remote
            .into_iter()
            .map(|remote| (remote.id.clone(), remote))
            .collect::<BTreeMap<_, _>>(),
    );
    let transports = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::<
        String,
        crate::commands::runner::dial::Dialed,
    >::new()));
    let retries = std::sync::Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let context = services.runner_probe_context();
    let handle = runtime.clone();
    services
        .runners()
        .probe_with(std::sync::Arc::new(move |registry, id| {
            let services = context.with_runner_registry(registry.clone());
            let remote = remote.clone();
            let transports = transports.clone();
            let retries = retries.clone();
            let local = local.clone();
            let environment = environment.clone();
            let handle = handle.clone();
            Box::pin(async move {
                if id == "local" {
                    if services
                        .local_runner_client()
                        .is_some_and(|client| client.is_connected())
                        && registry.rows().iter().any(|row| {
                            row.runner.runner_id == "local"
                                && row.status == plakous::runner::RunnerStatus::Ready
                        })
                    {
                        return tokio::task::spawn_blocking(move || {
                            registry.ready_local(local, services.now_unix_seconds())
                        })
                        .await
                        .map_err(|_| {
                            plakous::runner::PlacementFailure::RunnerUnreachable {
                                phase: plakous::runner::ProbePhase::Discovery,
                                timed_out: false,
                            }
                        })?;
                    }
                    let result = bind_local(&services, local, &environment, &handle).await;
                    match &result {
                        Ok(client) => watch_remote(registry, id, client.clone(), retries),
                        Err(_) => retry_remote(registry, id, retries),
                    }
                    return result.map(|_| ());
                }
                let remote = remote.get(&id).ok_or_else(|| {
                    registry
                        .rows()
                        .into_iter()
                        .find_map(|row| {
                            if row.runner.runner_id == id
                                && let plakous::runner::RunnerStatus::Blocked(failure) = row.status
                            {
                                Some(failure)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(plakous::runner::PlacementFailure::RunnerNotPaired)
                })?;
                let connected = transports
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .get(&id)
                    .filter(|dialed| dialed.client.is_connected())
                    .map(|dialed| (dialed.client.clone(), dialed.hello.clone()));
                if let Some((client, hello)) = connected {
                    return refresh_existing(&services, remote, client, hello).await;
                }
                let client = registry.client(&id);
                match bind_remote(&services, remote, &handle, client.as_ref()).await {
                    Ok(dialed) => {
                        let client = dialed.client.clone();
                        transports
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(id.clone(), dialed);
                        retries
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .entry(id.clone())
                            .or_default()
                            .delay = 0;
                        registry.require_connection(&id, false);
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
}

#[derive(Default)]
struct Retry {
    delay: u64,
    scheduled: bool,
}
type Retries = std::sync::Arc<std::sync::Mutex<BTreeMap<String, Retry>>>;

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
        if let Some(retry) = retries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_mut(&id)
        {
            retry.scheduled = false;
        }
        if registry.needs_connection(&id) {
            let _ = registry.refresh(Some(&[id])).await;
        }
    });
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
) -> Result<(), plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
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
    services.runners().ready(
        &remote.id,
        client,
        runner,
        options,
        discovery,
        services.now_unix_seconds(),
    )
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

async fn bind_local(
    services: &Services,
    catalog: std::sync::Arc<plakous::runner::WorkspaceCatalog>,
    environment: &super::environment::HostEnvironment,
    runtime: &tokio::runtime::Handle,
) -> Result<RunnerClient, plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase| PlacementFailure::RunnerUnreachable {
        phase,
        timed_out: false,
    };
    let paths = services
        .paths()
        .map_err(|_| unavailable(ProbePhase::Daemon))?;
    services.runners().progress("local", ProbePhase::Daemon);
    let client = if let Some(client) = services.local_runner_client() {
        if !client.is_connected() {
            crate::commands::runner::local::reconnect(paths, &client)
                .await
                .map_err(|_| unavailable(ProbePhase::Daemon))?;
        }
        client
    } else {
        crate::commands::runner::local::dial(paths)
            .await
            .map_err(|_| unavailable(ProbePhase::Daemon))?
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
    if !failures.is_empty() {
        return Err(unavailable(ProbePhase::Reconcile));
    }
    let _ = client.recover().await;
    finish_stops(services, &client, &sessions, &readopted)
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
    Ok(client)
}

async fn bind_remote(
    services: &Services,
    remote: &crate::commands::runner::registry::Remote,
    runtime: &tokio::runtime::Handle,
    client: Option<&RunnerClient>,
) -> Result<crate::commands::runner::dial::Dialed, plakous::runner::PlacementFailure> {
    use plakous::runner::{PlacementFailure, ProbePhase};
    let unavailable = |phase, timed_out| PlacementFailure::RunnerUnreachable { phase, timed_out };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
    services.runners().progress(&remote.id, ProbePhase::Ssh);
    let mut transport =
        crate::commands::runner::dial::ssh_command(&remote.config.ssh, &["/bin/true"]);
    let transport = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        transport
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status(),
    )
    .await
    .map_err(|_| unavailable(ProbePhase::Ssh, true))?
    .map_err(|_| unavailable(ProbePhase::Ssh, false))?;
    if !transport.success() {
        return Err(unavailable(ProbePhase::Ssh, false));
    }
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
    services.runners().blocked(
        &remote.id,
        unavailable(ProbePhase::Reconcile, false),
        services.now_unix_seconds(),
    );
    let roots = services
        .state_roots()
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    let sessions = roots
        .runtime_sessions()
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    let (readopted, failures) = plakous_hosted::runner_adoption::readopt_lines(
        &dialed.client,
        &sessions,
        "plak:host:readopt",
    )
    .await
    .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    for failure in failures {
        tracing::warn!(target: "plak.runner", phase = "sweep", line = %failure.line, error = %failure.reason, "runner_recovery_failed");
        return Err(unavailable(ProbePhase::Reconcile, false));
    }
    let _ = dialed.client.recover().await;
    finish_stops(services, &dialed.client, &sessions, &readopted)
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    dialed
        .client
        .reconcile_settled_foreground()
        .await
        .map_err(|_| unavailable(ProbePhase::Reconcile, false))?;
    let backend = plakous::tools::runner::RunnerBackend::new(
        dialed.hello.descriptor.clone(),
        std::sync::Arc::new(dialed.client.clone()),
    )
    .map_err(|_| unavailable(ProbePhase::Bind, false))?;
    services.runners().ready(
        &remote.id,
        dialed.client.clone(),
        plakous::runner::RunnerServices::new(std::sync::Arc::new(backend), runtime.clone()),
        options,
        discovery,
        services.now_unix_seconds(),
    )?;
    Ok(dialed)
}

/// A stop is durable before the host can die; finish it under a new head lease.
pub(super) async fn finish_stops(
    services: &Services,
    client: &RunnerClient,
    sessions: &AgentSessions,
    readopted: &[plakous_hosted::runner_adoption::Readopted],
) -> CliResult<()> {
    let paths = services.paths()?;
    let mut product = crate::store::ProductStore::open(paths)
        .map_err(|error| CliFailure::new(error.to_string()))?;
    let mut stops = BTreeMap::<LineId, String>::new();
    for id in product.session_ids().map_err(CliFailure::new)? {
        collect_stops(&mut product, paths, sessions, &id, &mut stops).await?;
    }
    let held = client.lines().await.map_err(|error| {
        CliFailure::new(format!(
            "the local runner's lines could not be read: {error:?}"
        ))
    })?;
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
                continue;
            }
            plakous_hosted::runner_adoption::finish_recorded_stop(client, sessions, line, intent)
                .await
                .map_err(|error| {
                    CliFailure::new(format!("recorded stop on {line} remains pending: {error}"))
                })?;
        }
    }
    Ok(())
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
    let Some(stop) = intents
        .root_interrupts
        .iter()
        .max_by_key(|intent| (intent.anchor, intent.ordinal))
        .filter(|intent| intent.value.cause == RootInterruptCause::Stopped)
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
        stops.insert(head.line(), intent.clone());
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
