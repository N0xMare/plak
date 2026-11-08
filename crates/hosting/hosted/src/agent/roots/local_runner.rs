//! An embedded attachment refuses live daemon work anywhere in its tree.

use std::collections::BTreeSet;

use super::ProductStateRoots;
use crate::error::{CliFailure, CliResult};
use plakous::runner::protocol::{Bind, PROTOCOL_VERSION, VersionTriple};
use plakous::runner::{ClientWindows, ConnectError, RunnerClient};

#[cfg(test)]
#[path = "local_runner_tests.rs"]
mod inspection_tests;

impl ProductStateRoots {
    pub(crate) async fn refuse_daemon_tree(
        &self,
        locator: plakous::sessions::SessionLocator,
        tree_id: &str,
    ) -> CliResult<()> {
        if self.daemon_processes() {
            return Ok(());
        }
        let socket = self
            .product
            .join(format!("host/runner/v{PROTOCOL_VERSION}/run/runner.sock"));
        let stream = match tokio::net::UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(CliFailure::new(error.to_string())),
        };
        let inspection = async {
            let bind = Bind {
                host_id: std::fs::read_to_string(self.product.join("host-id"))
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
                Err(ConnectError::Closed) => return Ok(()),
                Err(error) => return Err(CliFailure::new(error.to_string())),
            };
            let lines = client.lines().await;
            client.disconnect();
            let lines = lines.map_err(|error| {
                CliFailure::new(format!("the local runner inspection failed: {error:?}"))
            })?;
            let sessions = self.runtime_sessions()?;
            let head = sessions.line_head_of(locator).await.map_err(|error| {
                CliFailure::local_runner_inspection(tree_id, &locator.to_string(), error)
            })?;
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
                        let head = sessions.line_head_of(locator).await.map_err(|error| {
                            CliFailure::local_runner_inspection(agent, recorded, error)
                        })?;
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
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), inspection)
            .await
            .map_err(|_| {
                CliFailure::new("the local runner did not answer the embedded inspection")
            })?
    }
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
