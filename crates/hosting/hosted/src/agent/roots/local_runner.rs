//! An embedded attachment refuses live daemon work anywhere in its tree.

use std::collections::BTreeSet;
use std::path::Path;

use super::ProductStateRoots;
use crate::error::{CliFailure, CliResult};
use plakous::runner::protocol::{Bind, PROTOCOL_VERSION, VersionTriple};
use plakous::runner::{ClientWindows, ConnectError, RunnerClient};
use plakous::tools::runner::RunnerFailure;

#[cfg(test)]
#[path = "local_runner_tests.rs"]
mod inspection_tests;

impl ProductStateRoots {
    pub(crate) async fn refuse_daemon_tree(
        &self,
        locator: plakous::sessions::SessionLocator,
        tree_id: &str,
    ) -> CliResult<()> {
        // This exemption is home-local custody, not the job-release lifetime.
        if self.local_daemon.is_some() {
            return Ok(());
        }
        let Some(lines) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            inspect_home(&self.product),
        )
        .await
        .map_err(|_| {
            CliFailure::retryable_open("the local runner did not answer the embedded inspection")
        })??
        else {
            return Ok(());
        };
        // The transport budget does not include storage work. Line identity
        // reads use branch metadata, not the materialized conversation graph.
        let sessions = self.runtime_sessions()?;
        let head = sessions
            .line_head_of(locator)
            .await
            .map_err(|error| CliFailure::inspection_read(tree_id, &locator.to_string(), error))?;
        let mut tree_lines = BTreeSet::from([head.line().to_string()]);
        if let Some(tree) = self
            .tree_store()?
            .load(tree_id)
            .map_err(|error| CliFailure::new(error.to_string()))?
        {
            for (agent, node) in tree.rows().nodes() {
                if let Some(recorded) = &node.session_locator {
                    let locator = recorded
                        .parse::<plakous::sessions::SessionLocator>()
                        .map_err(|error| {
                            CliFailure::local_runner_inspection(agent, recorded, error)
                        })?;
                    let head = sessions
                        .line_head_of(locator)
                        .await
                        .map_err(|error| CliFailure::inspection_read(agent, recorded, error))?;
                    tree_lines.insert(head.line().to_string());
                }
            }
        }
        if lines
            .iter()
            .any(|line| tree_lines.contains(&line.line) && has_processes(line))
        {
            return Err(CliFailure::local_runner_processes());
        }
        Ok(())
    }
}

async fn inspect_home(
    product: &Path,
) -> CliResult<Option<Vec<plakous::runner::protocol::LineStatus>>> {
    let builds = match std::fs::read_dir(product.join("host/runner")) {
        Ok(builds) => builds,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(CliFailure::new(error.to_string())),
    };
    let protocol = format!("v{PROTOCOL_VERSION}");
    let mut lines = None::<Vec<_>>;
    for build in builds {
        let build = build.map_err(|error| CliFailure::new(error.to_string()))?;
        let name = build.file_name();
        if !name.to_str().is_some_and(|name| {
            name == protocol
                || name
                    .strip_prefix(&protocol)
                    .and_then(|suffix| suffix.strip_prefix('-'))
                    .is_some_and(|hash| {
                        hash.len() == 12 && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    })
        }) || !build
            .file_type()
            .map_err(|error| CliFailure::new(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let socket = build.path().join(if name == protocol.as_str() {
            "run/runner.sock"
        } else {
            "run/r.sock"
        });
        let stream = match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                continue;
            }
            Err(error) => return Err(CliFailure::new(error.to_string())),
        };
        let read = async {
            let bind = Bind {
                host_id: std::fs::read_to_string(product.join("host-id"))
                    .map_err(|error| CliFailure::new(error.to_string()))?
                    .trim()
                    .to_owned(),
                host_instance: "embedded-inspection".into(),
                version: VersionTriple::current("inspection", "inspection"),
                inspect: true,
            };
            let (client, _) = match RunnerClient::connect_home_local(
                stream,
                bind,
                ClientWindows::default(),
                None,
            )
            .await
            {
                Ok(connected) => connected,
                Err(ConnectError::Closed) => {
                    return Err(CliFailure::retryable_open(
                        "the local runner closed the embedded inspection",
                    ));
                }
                Err(error) => return Err(CliFailure::new(error.to_string())),
            };
            let lines = client.lines().await;
            client.disconnect();
            let lines = lines.map_err(|error| {
                let message = format!("the local runner inspection failed: {error:?}");
                match error {
                    RunnerFailure::NotSent(_) | RunnerFailure::ReadInterrupted(_) => {
                        CliFailure::retryable_open(message)
                    }
                    _ => CliFailure::new(message),
                }
            })?;
            Ok(lines)
        }
        .await?;
        lines.get_or_insert_with(Vec::new).extend(read);
    }
    Ok(lines)
}

fn has_processes(line: &plakous::runner::protocol::LineStatus) -> bool {
    line.foreground
        || line.jobs["jobs"]
            .as_array()
            .is_some_and(|jobs| jobs.iter().any(|job| job["status"]["status"] == "running"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_live_work_refuses_an_embedded_open() {
        let mut line = plakous::runner::protocol::LineStatus {
            host_id: "host".into(),
            line: "line".into(),
            owner: None,
            adopted: true,
            jobs: serde_json::json!({"jobs": []}),
            foreground: false,
        };
        assert!(!super::has_processes(&line));
        for status in ["exited", "stopped", "released", "outcome_unknown"] {
            line.jobs = serde_json::json!({"jobs": [{"status": {"status": status}}]});
            assert!(!super::has_processes(&line), "{status}");
        }
        line.jobs = serde_json::json!({"jobs": [{"status": {"status": "running"}}]});
        assert!(super::has_processes(&line));
        line.jobs = serde_json::json!({"jobs": []});
        line.foreground = true;
        assert!(super::has_processes(&line));
    }
}
