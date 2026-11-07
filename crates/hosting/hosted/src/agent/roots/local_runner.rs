//! An embedded attachment never opens a process line still held by the daemon.

use super::ProductStateRoots;
use crate::error::{CliFailure, CliResult};
use plakous::runner::protocol::{Bind, PROTOCOL_VERSION, VersionTriple};
use plakous::runner::{ClientWindows, ConnectError, RunnerClient};

impl ProductStateRoots {
    pub(crate) async fn refuse_daemon_line(
        &self,
        locator: plakous::sessions::SessionLocator,
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
            let head = self
                .runtime_sessions()?
                .line_head_of(locator)
                .await
                .map_err(|error| CliFailure::new(error.to_string()))?;
            if lines
                .iter()
                .any(|line| line.line == head.line().to_string() && has_processes(line))
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
