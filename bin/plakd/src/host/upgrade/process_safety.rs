//! Remove this admission gate once host upgrade carries jobs across runner triples.

use super::{CliFailure, CliResult, Duration, Services};
use crate::deploy::{DeployRunnerInspection, DeployRunnerVerdict, RUNNER_INSPECTION_REFUSAL};
use plakous::runner::protocol::{LineStatus, VersionTriple};
use std::collections::BTreeMap;

impl super::Request {
    pub(super) fn check_processes(
        &self,
        services: &Services,
        destination: &VersionTriple,
        runtime: &tokio::runtime::Handle,
    ) -> CliResult<()> {
        if self.inspect_runners.is_empty() {
            check(services, destination, runtime)
        } else {
            inspect_deploy(services, &self.inspect_runners, runtime)
        }
    }
}

pub(super) fn check(
    services: &Services,
    destination: &VersionTriple,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    if destination == &crate::commands::runner::version() {
        return Ok(());
    }
    inspect(services, &[], runtime)
}

pub(super) fn inspect_deploy(
    services: &Services,
    runners: &[String],
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    validate_ids(runners)?;
    inspect(services, runners, runtime).map_err(|error| {
        if error.message().starts_with(RUNNER_INSPECTION_REFUSAL) {
            error
        } else {
            deploy_refusal(error.message())
        }
    })
}

fn validate_ids(runners: &[String]) -> CliResult<()> {
    if runners.is_empty()
        || runners.len() > 8
        || runners
            .iter()
            .any(|id| crate::product_config::validate_runner_name(id).is_err())
        || runners
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != runners.len()
    {
        return Err(deploy_refusal("invalid deploy runner inspection ids"));
    }
    Ok(())
}

pub(super) fn deploy_refusal(detail: &str) -> CliFailure {
    CliFailure::new(format!("{RUNNER_INSPECTION_REFUSAL} {detail}"))
}

pub(super) fn deploy_verdict(
    request: &DeployRunnerInspection,
    instance: &str,
    idle: bool,
    inspect: impl FnOnce(&[String]) -> CliResult<()>,
) -> DeployRunnerVerdict {
    let result = if request.source_instance != instance {
        Err(deploy_refusal(
            "source coordinator instance changed; replan",
        ))
    } else if !idle {
        Err(deploy_refusal("source coordinator is not idle; replan"))
    } else {
        inspect(&request.deploy_inspect_runners)
    };
    match result {
        Ok(()) => DeployRunnerVerdict::Settled,
        Err(error) => DeployRunnerVerdict::Blocked {
            detail: error.message().to_owned(),
        },
    }
}

fn enrolled_clients<T>(
    runners: &[String],
    client: impl Fn(&str) -> Option<T>,
) -> CliResult<BTreeMap<String, T>> {
    runners
        .iter()
        .map(|id| Ok((id.clone(), client(id).ok_or_else(|| unavailable(id))?)))
        .collect()
}

fn inspect(
    services: &Services,
    runners: &[String],
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    let mut clients = enrolled_clients(runners, |id| services.runners().client(id))?;
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
            .map_err(|_| inspection_timeout(&id))?
            .map_err(|_| {
                if client.is_connected() {
                    inspection_incomplete(&id)
                } else {
                    unavailable(&id)
                }
            })?;
        for line in lines.iter().filter(|line| {
            line.host_id == plakous_recursive_subagents::tree::custody_host().host_id
        }) {
            if !runners.is_empty() {
                check_unknown_jobs(&id, line)?;
            }
            if live(line)? {
                return Err(refused());
            }
        }
    }
    Ok(())
}

fn check_unknown_jobs(runner: &str, line: &LineStatus) -> CliResult<()> {
    let jobs = line.jobs["jobs"].as_array().ok_or_else(refused)?;
    if let Some(unknown) = jobs
        .iter()
        .find(|job| job["status"]["status"] == "outcome_unknown")
    {
        let number = unknown["number"]
            .as_u64()
            .filter(|n| u32::try_from(*n).is_ok())
            .ok_or_else(refused)?;
        return Err(deploy_refusal(&format!(
            "runner {runner} job {number} on line {} is outcome_unknown; use jobs.ack or jobs.disown on the old build before retrying; no source surrendered",
            line.line
        )));
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
        "host_upgrade_process_retention_unavailable: runner triple changes while foreground/background processes are live or their state is unknown; settle and drain them using the old build, then retry; runner retention v1 exists, but host upgrade does not carry jobs across changed runner triples",
    )
}

fn unavailable(runner: &str) -> CliFailure {
    if runner == "local" {
        return CliFailure::new(
            "host_upgrade_process_retention_unavailable: native process inspection is unavailable for the local runner; restore the old host's home-local runner connection, inspect and settle its processes using the old build, then retry; no paired dedicated link change is needed; no source surrendered",
        );
    }
    CliFailure::new(format!(
        "host_upgrade_process_retention_unavailable: native process inspection is unavailable for runner {runner}; restore its source-compatible dedicated link, reconnect a settled source root on that runner read-only and keep it attached until upgrade completes, then advance the destination runner link and retry; a picker Ready row alone does not retain the source connection; no source surrendered"
    ))
}

fn inspection_timeout(runner: &str) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_process_retention_unavailable: process inspection for runner {runner} did not answer within 5 seconds; wait for the source runner to respond, check its reachability and load, then retry; a slow response does not require changing its link or attaching a root; no source surrendered"
    ))
}

fn inspection_incomplete(runner: &str) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_process_retention_unavailable: runner {runner} is connected but its native inspection did not complete; check its response, reachability and load, then retry; a failed read alone does not require changing its link or attaching a root; no source surrendered"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deploy_inspection_rejects_wrong_instance_busy_source_and_unavailable_placements() {
        let (_temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let services = Services::for_home(paths);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut request = DeployRunnerInspection {
            source_instance: "100-0000000000000001".into(),
            deploy_inspect_runners: vec!["desk".into()],
        };
        let check = |ids: &[String]| inspect_deploy(&services, ids, runtime.handle());
        for (instance, idle, expected) in [
            ("101-0000000000000002", true, "coordinator instance changed"),
            ("100-0000000000000001", false, "coordinator is not idle"),
            (
                "100-0000000000000001",
                true,
                "native process inspection is unavailable for runner desk",
            ),
        ] {
            let DeployRunnerVerdict::Blocked { detail } =
                deploy_verdict(&request, instance, idle, check)
            else {
                panic!("inspection must refuse")
            };
            assert!(detail.contains(expected), "{detail}");
        }
        request.deploy_inspect_runners[0] = "local".into();
        let DeployRunnerVerdict::Blocked { detail } =
            deploy_verdict(&request, &request.source_instance, true, check)
        else {
            panic!("invalid runner ID must refuse")
        };
        assert!(detail.contains("invalid deploy runner inspection ids"));
    }

    #[test]
    fn deploy_inspection_blocks_an_unknown_job_without_changing_manual_gate() {
        let line = LineStatus {
            host_id: "host".into(),
            line: "line".into(),
            owner: None,
            adopted: true,
            jobs: serde_json::json!({"jobs":[{"number":17,"status":{"status":"outcome_unknown"}}]}),
            foreground: false,
        };
        let refusal = check_unknown_jobs("desk", &line).unwrap_err();
        for expected in ["runner desk job 17", "jobs.ack", "jobs.disown"] {
            assert!(refusal.message().contains(expected));
        }
        assert!(!live(&line).unwrap());
    }

    #[test]
    fn host_upgrade_process_inspection_local_and_timeout_remedies_are_distinct() {
        let local = unavailable("local");
        assert!(local.message().contains("home-local runner connection"));
        assert!(!local.message().contains("reconnect a settled source root"));
        let timeout = inspection_timeout("paired");
        assert!(timeout.message().contains("within 5 seconds"));
        assert!(timeout.message().contains("reachability and load"));
        assert!(!timeout.message().contains("restore its source-compatible"));
        let incomplete = inspection_incomplete("paired");
        assert!(incomplete.message().contains("connected"));
        assert!(
            !incomplete
                .message()
                .contains("restore its source-compatible")
        );
        for failure in [local, timeout, incomplete] {
            assert!(
                failure
                    .message()
                    .starts_with("host_upgrade_process_retention_unavailable:")
            );
            assert!(failure.message().contains("no source surrendered"));
        }
    }

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
        let failure = refused();
        assert!(
            failure
                .message()
                .starts_with("host_upgrade_process_retention_unavailable:")
        );
        assert!(failure.message().contains("retention v1 exists"));
        assert!(
            failure
                .message()
                .contains("host upgrade does not carry jobs across changed runner triples")
        );
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
            let super::super::Answer::Progress {
                detail,
                qualification: None,
            } = super::super::read_frame(&mut reader).unwrap()
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
