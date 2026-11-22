//! Source-side inventory checks. Only the successor can transfer retained jobs.

use super::{CliFailure, CliResult, Duration, Request, Services, progress};
use crate::deploy::{
    DeployRunnerInspection, DeployRunnerVerdict, DeployRunnerWarning, RUNNER_INSPECTION_REFUSAL,
};
use plakous::runner::protocol::{LineStatus, VersionTriple};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum Check {
    BeforeDrain,
    Held,
    CrossKey,
}

pub(super) fn before_drain(
    request: &Request,
    services: &Services,
    destination: &VersionTriple,
    runtime: &tokio::runtime::Handle,
    report: &progress::Report<'_>,
) -> CliResult<bool> {
    let cross_key = request.source_key != request.destination_key;
    if cross_key && !request.inspect_runners.is_empty() {
        return Err(deploy_refusal(
            "deploy runner inspection requires a same-key upgrade",
        ));
    }
    let needed =
        destination != &crate::commands::runner::version() || !request.inspect_runners.is_empty();
    if needed {
        let check = if cross_key {
            Check::CrossKey
        } else {
            Check::BeforeDrain
        };
        progress::timed(report, progress::Phase::RunnerProcess, || {
            inspect(services, &request.inspect_runners, check, runtime).map(drop)
        })?;
    }
    Ok(needed)
}

pub(super) fn held(
    request: &Request,
    services: &Services,
    runtime: &tokio::runtime::Handle,
) -> CliResult<()> {
    for warning in inspect(services, &request.inspect_runners, Check::Held, runtime)? {
        tracing::warn!(target: "plak.runner", %warning);
    }
    Ok(())
}

pub(super) fn inspect(
    services: &Services,
    enrolled: &[String],
    check: Check,
    runtime: &tokio::runtime::Handle,
) -> CliResult<Vec<DeployRunnerWarning>> {
    if enrolled.len() > 8
        || enrolled
            .iter()
            .any(|id| crate::product_config::validate_runner_name(id).is_err())
        || enrolled.iter().collect::<BTreeSet<_>>().len() != enrolled.len()
    {
        return Err(deploy_refusal("invalid deploy runner inspection ids"));
    }
    let paths = services.paths()?;
    let mut runners = BTreeMap::<String, (bool, Vec<String>)>::new();
    for name in enrolled {
        let id = crate::commands::runner::registry::read_pin(paths, name)
            .map_or_else(|| name.clone(), |pin| pin.runner_id);
        runners.entry(id).or_default().1.push(name.clone());
    }
    runners.entry("local".into()).or_default();
    for record in
        crate::store::ProductStore::inspect_upgrade_roots(paths).map_err(CliFailure::new)?
    {
        let id = record.runner.as_ref().map_or("local", |pin| &pin.runner_id);
        runners.entry(id.into()).or_default().0 = true;
        let tree = plakous_recursive_subagents_store::SqliteTreeStore::inspect(
            &paths.subagents_database(),
            &record.node.tree_id,
        )
        .map_err(|err| CliFailure::new(err.message()))?;
        if let plakous_recursive_subagents_store::TreeInspection::Tree(tree) = tree {
            for node in tree.rows().nodes().values() {
                // Child placement is independently pinned.
                let id = node.placement.runner_id.as_deref().unwrap_or("local");
                runners.entry(id.into()).or_default().0 = true;
            }
        }
    }

    let host = &plakous_recursive_subagents::tree::custody_host().host_id;
    let mut warnings = Vec::new();
    for (id, (referenced, aliases)) in runners {
        let client = if id == "local" {
            services.local_runner_client()
        } else {
            services.runners().client(&id)
        };
        let result = client
            .as_ref()
            .filter(|client| client.is_connected())
            .map(|client| {
                runtime.block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), client.lines()).await
                })
            });
        let lines =
            match result {
                Some(Ok(Ok(lines))) => lines,
                _ if id != "local" && !referenced && check != Check::CrossKey => {
                    warnings.extend(aliases.into_iter().map(|runner| {
                        DeployRunnerWarning::UnreferencedRunnerUnavailable { runner }
                    }));
                    continue;
                }
                None if id == "local" && !referenced => continue,
                None => return Err(unavailable(&id)),
                Some(Err(_)) => return Err(inspection_timeout(&id)),
                Some(Ok(Err(_))) => return Err(inspection_incomplete(&id)),
            };
        for line in lines.iter().filter(|line| &line.host_id == host) {
            check_line(&id, line, check)?;
        }
    }
    Ok(warnings)
}

fn check_line(id: &str, line: &LineStatus, check: Check) -> CliResult<()> {
    if line.foreground && check != Check::BeforeDrain {
        return Err(blocked(&format!(
            "runner {id} line {} still has unpromoted foreground work; let it finish or promote it using the source build, then retry",
            line.line
        )));
    }
    let jobs = line.jobs["jobs"]
        .as_array()
        .ok_or_else(|| inspection_incomplete(id))?;
    for job in jobs {
        match job["status"]["status"].as_str() {
            Some("running") if check == Check::CrossKey => {
                return Err(blocked(&format!(
                    "runner {id} line {} has a running job; cross-key carry requires settled roots; settle and drain using the source build",
                    line.line
                )));
            }
            Some("running" | "exited" | "stopped" | "released" | "outcome_unknown") => {}
            _ => return Err(inspection_incomplete(id)),
        }
    }
    Ok(())
}

fn blocked(detail: &str) -> CliFailure {
    CliFailure::new(format!(
        "host_upgrade_runner_carry_unavailable: {detail}; no source surrendered"
    ))
}

fn unavailable(runner: &str) -> CliFailure {
    if runner == "local" {
        return blocked(
            "native process inspection is unavailable for the local runner; restore the source host's home-local runner connection and retry",
        );
    }
    blocked(&format!(
        "native process inspection is unavailable for referenced runner {runner}; restore its source-compatible dedicated link if necessary, reconnect a source root on that runner read-only and keep it attached, then advance the link to the destination and retry; a picker Ready row does not retain the source connection"
    ))
}

fn inspection_timeout(runner: &str) -> CliFailure {
    blocked(&format!(
        "process inspection for runner {runner} did not answer within 5 seconds; check reachability and load, then retry"
    ))
}

fn inspection_incomplete(runner: &str) -> CliFailure {
    blocked(&format!(
        "runner {runner} did not complete its native inspection; check its response, reachability and load, then retry"
    ))
}

pub(super) fn deploy_refusal(detail: &str) -> CliFailure {
    CliFailure::new(format!("{RUNNER_INSPECTION_REFUSAL} {detail}"))
}

pub(super) fn deploy_verdict(
    request: &DeployRunnerInspection,
    instance: &str,
    idle: bool,
    inspect: impl FnOnce(&[String]) -> CliResult<Vec<DeployRunnerWarning>>,
) -> DeployRunnerVerdict {
    let result = if request.source_instance != instance {
        Err(deploy_refusal(
            "source coordinator instance changed; replan",
        ))
    } else if !idle {
        Err(deploy_refusal(
            "source coordinator is already preparing a handoff; replan",
        ))
    } else if request.deploy_inspect_runners.is_empty() {
        Err(deploy_refusal("invalid deploy runner inspection ids"))
    } else {
        inspect(&request.deploy_inspect_runners)
    };
    match result {
        Ok(warnings) => DeployRunnerVerdict::Settled { warnings },
        Err(err) => DeployRunnerVerdict::Blocked {
            detail: if err.message().starts_with(RUNNER_INSPECTION_REFUSAL) {
                err.message().to_owned()
            } else {
                deploy_refusal(err.message()).message().to_owned()
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carry_accepts_joined_jobs_but_not_held_foreground_or_cross_key_jobs() {
        let mut line = LineStatus {
            host_id: "host".into(),
            line: "line".into(),
            owner: None,
            adopted: true,
            jobs: serde_json::json!({"jobs":[{"status":{"status":"running"}}]}),
            foreground: true,
        };
        assert!(check_line("local", &line, Check::BeforeDrain).is_ok());
        assert!(check_line("local", &line, Check::Held).is_err());
        line.foreground = false;
        assert!(check_line("local", &line, Check::Held).is_ok());
        assert!(check_line("local", &line, Check::CrossKey).is_err());
        line.jobs = serde_json::json!({"jobs":[{"status":{"status":"outcome_unknown"}}]});
        assert!(check_line("local", &line, Check::Held).is_ok());
    }

    #[test]
    fn unused_paired_runner_is_a_typed_warning() {
        let (_temp, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        let services = Services::for_home(paths);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = DeployRunnerInspection {
            source_instance: "100-0000000000000001".into(),
            deploy_inspect_runners: vec!["desk".into()],
        };
        let DeployRunnerVerdict::Settled { warnings } =
            deploy_verdict(&request, &request.source_instance, true, |runners| {
                inspect(&services, runners, Check::BeforeDrain, runtime.handle())
            })
        else {
            panic!("unreferenced runner must not block");
        };
        assert_eq!(
            warnings,
            [DeployRunnerWarning::UnreferencedRunnerUnavailable {
                runner: "desk".into()
            }]
        );
    }
}
