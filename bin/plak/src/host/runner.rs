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
    Ok((services, client))
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
