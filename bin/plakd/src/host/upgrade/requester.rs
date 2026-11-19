//! Requester loss withdraws preparation; a Ready transfer no longer belongs to its socket.

use std::io::Read as _;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{
    CliFailure, CliResult, DeployRunnerInspection, DeployRunnerVerdict, Identity, PROBE_BOUND,
    PlakPaths, UnixStream, io_failure, process_safety, read, socket, write,
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Request {
    pub(super) source_instance: String,
    pub(super) source_key: String,
    pub(super) destination: PathBuf,
    pub(super) destination_key: String,
    pub(super) expected_binary: String,
    pub(super) wait_seconds: u64,
    pub(super) drain: crate::command::HostDrain,
    pub(super) abandon: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) inspect_runners: Vec<String>,
}

pub(super) fn prove_deploy_source(
    paths: &PlakPaths,
    inspection: &DeployRunnerInspection,
) -> CliResult<()> {
    let mut stream =
        crate::wire_transport::hosts::socket_until(&socket(paths), Instant::now() + PROBE_BOUND)
            .map_err(io_failure)?;
    stream
        .set_read_timeout(Some(PROBE_BOUND * 2))
        .map_err(io_failure)?;
    stream
        .set_write_timeout(Some(PROBE_BOUND))
        .map_err(io_failure)?;
    let identity: Identity = read(&stream)?;
    if identity.instance != inspection.source_instance || identity.deploy_idle != Some(true) {
        return Err(process_safety::deploy_refusal(
            "source coordinator changed or is not idle; replan",
        ));
    }
    write(&mut stream, inspection)?;
    match read(&stream).map_err(|_| {
        process_safety::deploy_refusal("source cannot prove deploy runner inspection")
    })? {
        DeployRunnerVerdict::Settled => Ok(()),
        DeployRunnerVerdict::Blocked { detail } => Err(CliFailure::new(detail)),
    }
}

pub(super) struct Caller {
    pub(super) cancelled: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    reader: UnixStream,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Caller {
    pub(super) fn watch(stream: &UnixStream, host_cancelled: Arc<AtomicBool>) -> CliResult<Self> {
        let mut reader = stream.try_clone().map_err(io_failure)?;
        reader
            .set_read_timeout(Some(Duration::from_millis(100)))
            .map_err(io_failure)?;
        let closer = reader.try_clone().map_err(io_failure)?;
        let cancelled = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (lost, finished) = (Arc::clone(&cancelled), Arc::clone(&done));
        let worker = std::thread::spawn(move || {
            let mut byte = [0];
            while !finished.load(Ordering::Acquire) {
                if host_cancelled.load(Ordering::Acquire) {
                    lost.store(true, Ordering::Release);
                    break;
                }
                match reader.read(&mut byte) {
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock
                                | std::io::ErrorKind::TimedOut
                                | std::io::ErrorKind::Interrupted
                        ) => {}
                    _ => {
                        if !finished.load(Ordering::Acquire) {
                            lost.store(true, Ordering::Release);
                        }
                        break;
                    }
                }
            }
        });
        Ok(Self {
            cancelled,
            done,
            reader: closer,
            worker: Some(worker),
        })
    }

    pub(super) fn ready(&self) {
        self.done.store(true, Ordering::Release);
        let _ = self.reader.shutdown(std::net::Shutdown::Read);
    }
}

impl Drop for Caller {
    fn drop(&mut self) {
        self.ready();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::services::Services;

    #[test]
    fn deploy_inspection_keeps_legacy_request_bytes_and_uses_the_gate_for_both_triples() {
        let exact = r#"{"source_instance":"source","source_key":"key","destination":"/example/plakd","destination_key":"key","expected_binary":"0123456789ab","wait_seconds":0,"drain":{"mode":"idle"},"abandon":null}"#;
        let mut request: Request = serde_json::from_str(exact).unwrap();
        assert_eq!(serde_json::to_string(&request).unwrap(), exact);
        request.inspect_runners.push("desk".into());
        let wire = serde_json::to_string(&request).unwrap();
        assert_eq!(
            wire,
            format!(
                "{},\"inspect_runners\":[\"desk\"]}}",
                exact.trim_end_matches('}')
            )
        );
        let (_temp, paths) = PlakPaths::isolated_test_home();
        let services = Services::for_home(paths);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let same = crate::commands::runner::version();
        let mut changed = same.clone();
        changed.source_revision = "different".into();
        for triple in [&same, &changed] {
            let error = request
                .check_processes(&services, triple, runtime.handle())
                .unwrap_err();
            assert!(
                error
                    .message()
                    .contains("native process inspection is unavailable for runner desk")
            );
        }
    }
}
