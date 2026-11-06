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
    let client = runtime.block_on(crate::commands::runner::local::dial(services.paths()?))?;
    let services = services.with_local_daemon(
        environment.backend(client.clone(), runtime.handle().clone()),
        client.clone(),
    );
    let roots = services.state_roots()?;
    let runtime_sessions = roots.runtime_sessions()?;
    let obsolete = runtime.block_on(crate::commands::runner::local::older(services.paths()?))?;
    runtime
        .block_on(plakous_hosted::runner_adoption::report_obsolete_lines(
            &runtime_sessions,
            obsolete,
        ))
        .map_err(CliFailure::new)?;
    let (readopted, failed) = runtime
        .block_on(plakous_hosted::runner_adoption::readopt_lines(
            &client,
            &runtime_sessions,
            "plak:host:readopt",
        ))
        .map_err(|error| CliFailure::new(format!("the local runner sweep failed: {error:?}")))?;
    for failure in failed {
        eprintln!(
            "plak host: line {} was not re-adopted: {}",
            failure.line, failure.reason
        );
    }
    let _ = runtime.block_on(client.recover());
    runtime.block_on(finish_stops(
        &services,
        &client,
        &runtime_sessions,
        &readopted,
    ))?;
    Ok(services)
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
        let record = product.load(&id).map_err(CliFailure::new)?;
        if let Some(tree) =
            crate::store::tree::load_tree(paths, &record.node.tree_id).map_err(CliFailure::new)?
        {
            for (locator, intent) in tree.recorded_process_stops() {
                let locator = locator
                    .parse::<plakous::sessions::SessionLocator>()
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                let head = sessions
                    .line_head_of(locator)
                    .await
                    .map_err(|error| CliFailure::new(error.to_string()))?;
                stops.insert(head.line(), intent);
            }
        }
        let intents = product.load_intents(&id).map_err(CliFailure::new)?;
        let Some(stop) = intents
            .root_interrupts
            .iter()
            .max_by_key(|intent| (intent.anchor, intent.ordinal))
            .filter(|intent| intent.value.cause == RootInterruptCause::Stopped)
        else {
            continue;
        };
        let intent = format!("plak:stop:{id}:{}", stop.ordinal);
        if let Some(locator) = record.plakous_locator {
            if admitted_since(sessions, locator.session_id(), stop.anchor).await? {
                continue;
            }
            let head = sessions
                .line_head_of(locator)
                .await
                .map_err(|error| CliFailure::new(error.to_string()))?;
            if head.session_id() != locator.session_id() || head.branch_id() != locator.branch_id()
            {
                continue;
            }
            stops.insert(head.line(), intent.clone());
        }
    }
    // Only lines this process re-adopts are stopped. Unknown lines are not
    // this state key's, and a live embedded holder refuses the head lease.
    let held = client.lines().await.map_err(|error| {
        CliFailure::new(format!(
            "the local runner's lines could not be read: {error:?}"
        ))
    })?;
    for status in held {
        let Ok(line) = status.line.parse() else {
            continue;
        };
        if let Some(intent) = stops.get(&line) {
            if !readopted.iter().any(|adopted| adopted.line == line) {
                continue;
            }
            plakous_hosted::runner_adoption::finish_recorded_stop(client, sessions, line, intent)
                .await
                .map_err(CliFailure::new)?;
        }
    }
    Ok(())
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
