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
) -> CliResult<(Services, RunnerClient)> {
    let prepared = crate::commands::runner::registry::prepare(services)?;
    let services = &prepared.services;
    let client = runtime.block_on(crate::commands::runner::local::dial(services.paths()?))?;
    client.report_foreground_with(|line, process| {
        tracing::info!(target: "plak.runner", line, call_key = %process.call_key, pid = process.pid, ended = %process.ended, "stopped_foreground");
    });
    let services = services.with_local_daemon(
        environment.backend(client.clone(), runtime.handle().clone()),
        client.clone(),
    );
    let roots = services.state_roots()?;
    let runtime_sessions = roots.runtime_sessions()?;
    let obsolete = runtime.block_on(crate::commands::runner::local::older(services.paths()?))?;
    runtime.block_on(plakous_hosted::runner_adoption::report_obsolete_lines(
        &runtime_sessions,
        obsolete,
    ));
    let (readopted, failed) = runtime
        .block_on(plakous_hosted::runner_adoption::readopt_lines(
            &client,
            &runtime_sessions,
            "plak:host:readopt",
        ))
        .unwrap_or_else(|error| {
            tracing::warn!(target: "plak.runner", phase = "sweep", error = ?error, "runner_recovery_failed");
            (Vec::new(), Vec::new())
        });
    for failure in failed {
        tracing::warn!(target: "plak.runner", phase = "sweep", line = %failure.line, error = %failure.reason, "runner_recovery_failed");
    }
    let _ = runtime.block_on(client.recover());
    if let Err(error) = runtime.block_on(finish_stops(
        &services,
        &client,
        &runtime_sessions,
        &readopted,
    )) {
        tracing::warn!(target: "plak.runner", phase = "finish_stops", error = %error, "runner_recovery_failed");
    }
    services
        .runners()
        .ready_local(prepared.local, services.now_unix_seconds())
        .map_err(|failure| CliFailure::new(format!("local runner placement: {failure:?}")))?;
    start_remotes(&services, prepared.remote, runtime.handle());
    Ok((services, client))
}

fn start_remotes(
    services: &Services,
    remote: Vec<crate::commands::runner::registry::Remote>,
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
    let local = services.runners().local_catalog();
    let handle = runtime.clone();
    services
        .runners()
        .probe_with(std::sync::Arc::new(move |registry, id| {
            let services = context.with_runner_registry(registry.clone());
            let remote = remote.clone();
            let transports = transports.clone();
            let retries = retries.clone();
            let local = local.clone();
            let handle = handle.clone();
            Box::pin(async move {
                if id == "local" {
                    let catalog =
                        local.ok_or(plakous::runner::PlacementFailure::RunnerNotPaired)?;
                    return tokio::task::spawn_blocking(move || {
                        registry.ready_local(catalog, services.now_unix_seconds())
                    })
                    .await
                    .map_err(|_| {
                        plakous::runner::PlacementFailure::RunnerUnreachable {
                            phase: plakous::runner::ProbePhase::Discovery,
                            timed_out: false,
                        }
                    })?;
                }
                let remote = remote
                    .get(&id)
                    .ok_or(plakous::runner::PlacementFailure::RunnerNotPaired)?;
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
    }
    let _ = dialed.client.recover().await;
    finish_stops(services, &dialed.client, &sessions, &readopted)
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

pub(super) async fn supervise(
    paths: &crate::home::paths::PlakPaths,
    client: &RunnerClient,
) -> CliResult<()> {
    loop {
        client.closed().await;
        crate::commands::runner::local::reconnect(paths, client).await?;
    }
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
        if let Err(error) = collect_stops(&mut product, paths, sessions, &id, &mut stops).await {
            tracing::warn!(target: "plak.runner", phase = "finish_stops", session = %id, error = %error, "runner_recovery_failed");
        }
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
            if let Err(error) = plakous_hosted::runner_adoption::finish_recorded_stop(
                client, sessions, line, intent,
            )
            .await
            {
                tracing::warn!(target: "plak.runner", phase = "finish_stops", line = %line, error = %error, "runner_recovery_failed");
            }
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
