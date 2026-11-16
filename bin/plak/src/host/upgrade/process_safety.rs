//! Remove this one admission gate once cross-build job retention is available.

use super::{CliFailure, CliResult, Duration, Services};
use plakous::runner::protocol::{LineStatus, VersionTriple};

pub(super) fn check(
    services: &Services,
    destination: &VersionTriple,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    if destination == &crate::commands::runner::version() {
        return Ok(());
    }
    let mut clients = std::collections::BTreeMap::new();
    if let Some(client) = services.local_runner_client() {
        clients.insert("local".to_owned(), client);
    }
    for record in crate::store::ProductStore::inspect_upgrade_roots(services.paths()?)
        .map_err(CliFailure::new)?
    {
        if let Some(runner) = record.runner {
            let client = services
                .runners()
                .client(&runner.runner_id)
                .ok_or_else(|| unavailable(&runner.runner_id))?;
            clients.insert(runner.runner_id, client);
        }
        let tree = plakous_recursive_subagents_store::SqliteTreeStore::inspect(
            &services.paths()?.subagents_database(),
            &record.node.tree_id,
        )
        .map_err(|_| refused())?;
        if let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = tree {
            // Child placement is independently pinned, not inherited at this read.
            for node in tree.rows().nodes().values() {
                if let Some(id) = &node.placement.runner_id {
                    let client = services
                        .runners()
                        .client(id)
                        .ok_or_else(|| unavailable(id))?;
                    clients.insert(id.clone(), client);
                }
            }
        }
    }
    for (id, client) in clients {
        let lines = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), client.lines()).await })
            .map_err(|_| unavailable(&id))?
            .map_err(|_| unavailable(&id))?;
        for line in lines.iter().filter(|line| {
            line.host_id == plakous_recursive_subagents::tree::custody_host().host_id
        }) {
            if live(line)? {
                return Err(refused());
            }
        }
    }
    Ok(())
}

fn live(line: &LineStatus) -> CliResult<bool> {
    if line.foreground {
        return Ok(true);
    }
    let jobs = line.jobs["jobs"].as_array().ok_or_else(refused)?;
    for job in jobs {
        match job["status"]["status"].as_str() {
            Some("running") => return Ok(true),
            Some("exited" | "stopped" | "released" | "outcome_unknown") => {}
            _ => return Err(refused()),
        }
    }
    Ok(false)
}

fn refused() -> CliFailure {
    CliFailure::new(
        "host_upgrade_process_retention_unavailable: runner triple changes while foreground/background processes are live or their state is unknown; settle and drain them using the old build, then retry; cross-build job retention is not available yet",
    )
}

fn unavailable(runner: &str) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_process_retention_unavailable: native process inspection is unavailable for runner {runner}; restore its source-compatible dedicated link, reconnect a settled source root on that runner read-only and keep it attached until upgrade completes, then advance the destination runner link and retry; a picker Ready row alone does not retain the source connection; no source surrendered"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_upgrade_missing_process_inspection_names_the_source_connection_remedy() {
        let failure = unavailable("paired");
        for expected in [
            "runner paired",
            "source-compatible",
            "read-only",
            "keep it attached",
            "then advance",
            "picker Ready",
            "no source surrendered",
        ] {
            assert!(
                failure.message().contains(expected),
                "missing remedy: {expected}"
            );
        }
    }

    #[test]
    fn host_upgrade_changed_triple_process_gate_is_fail_closed() {
        let mut line = LineStatus {
            host_id: "host".into(),
            line: "line".into(),
            owner: None,
            adopted: true,
            jobs: serde_json::json!({"jobs":[]}),
            foreground: true,
        };
        assert!(live(&line).unwrap());
        line.foreground = false;
        assert!(!live(&line).unwrap());
        for status in [
            "running",
            "exited",
            "stopped",
            "released",
            "outcome_unknown",
        ] {
            line.jobs = serde_json::json!({"jobs":[{"status":{"status":status}}]});
            assert_eq!(live(&line).unwrap(), status == "running");
        }
        line.jobs = serde_json::json!({"jobs":[{"status":{"status":"unknown-new-state"}}]});
        assert!(live(&line).is_err());
        line.jobs = serde_json::json!({});
        assert!(live(&line).is_err());
    }

    #[test]
    fn host_upgrade_progress_frames_preserve_buffered_following_replies() {
        let bytes = b"{\"status\":\"progress\",\"detail\":\"waiting 1s: model=1\"}\n{\"status\":\"progress\",\"detail\":\"waiting 2s: paid_compaction=1\"}\n{\"status\":\"ready\",\"receipt\":null}\n";
        let mut reader = std::io::BufReader::new(bytes.as_slice());
        for expected in ["model=1", "paid_compaction=1"] {
            let super::super::Answer::Progress { detail } =
                super::super::read_frame(&mut reader).unwrap()
            else {
                panic!("missing progress");
            };
            assert!(detail.contains(expected));
        }
        assert!(matches!(
            super::super::read_frame(&mut reader).unwrap(),
            super::super::Answer::Ready { receipt: None }
        ));
    }
}
