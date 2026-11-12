//! Workspace probes on a runner: the host's [`WorkspaceProbe`] whose answers
//! come from the runner, and the runner side that answers them from its own
//! filesystem.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::wire::{from_value, path_text, to_value};
use super::{
    RunnerCall, RunnerLink, RunnerVerb, ServeFailure, ServeFuture, backend_failure, call_blocking,
};
use crate::{LocalWorkspaceProbe, ProbeError, WorkspaceProbe};

#[derive(Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ProbeWire {
    FilesystemIdentity { path: String },
    Canonicalize { path: String },
    IsFile { path: String },
    TreeBytes { path: String },
}

#[cfg(test)]
#[test]
#[allow(clippy::unwrap_used, reason = "protocol payload pin")]
fn protocol_seven_probe_payloads_are_pinned() {
    assert_eq!(plakous_runner_protocol::PROTOCOL_VERSION, 7);
    let path = "/workspace".to_owned();
    let identity: crate::HostFilesystemObjectIdentity =
        serde_json::from_value(json!({"device": 1, "inode": 2})).unwrap();
    let actual = [
        to_value(&ProbeWire::FilesystemIdentity { path: path.clone() }).unwrap(),
        to_value(&ProbeWire::Canonicalize { path: path.clone() }).unwrap(),
        to_value(&ProbeWire::IsFile { path: path.clone() }).unwrap(),
        to_value(&ProbeWire::TreeBytes { path }).unwrap(),
        to_value(&identity).unwrap(),
        json!({"path": "/workspace"}),
        json!({"file": true}),
        json!({"bytes": 42}),
    ];
    let pinned = include_str!("probe/pinned/v7.jsonl")
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(actual.as_slice(), pinned.as_slice());
}

/// A [`WorkspaceProbe`] whose answers come from the runner behind a link.
/// Each question is one request, waited for on a thread of its own: callers
/// of this synchronous port run off the async workers (spec 01 §8.5). Any
/// link failure is [`ProbeError::Unavailable`], never an absent answer.
pub struct RunnerWorkspaceProbe {
    link: Arc<dyn RunnerLink>,
    runtime: tokio::runtime::Handle,
}

impl RunnerWorkspaceProbe {
    /// The probe of the runner behind `link`, whose calls run on `runtime`.
    #[must_use]
    pub fn new(link: Arc<dyn RunnerLink>, runtime: tokio::runtime::Handle) -> Self {
        Self { link, runtime }
    }

    fn ask(&self, question: &ProbeWire) -> Result<Value, ProbeError> {
        let payload = to_value(question).map_err(|_| ProbeError::Unavailable)?;
        call_blocking(
            self.link.as_ref(),
            &self.runtime,
            RunnerCall::unkeyed(RunnerVerb::ProbeWorkspace, payload),
        )
        .map_err(|_| ProbeError::Unavailable)
    }
}

impl std::fmt::Debug for RunnerWorkspaceProbe {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RunnerWorkspaceProbe")
            .finish_non_exhaustive()
    }
}

fn path(path: &Path) -> Result<String, ProbeError> {
    path_text(path).map_err(|_| ProbeError::Unavailable)
}

impl WorkspaceProbe for RunnerWorkspaceProbe {
    fn filesystem_identity(
        &self,
        target: &Path,
    ) -> Result<crate::HostFilesystemObjectIdentity, ProbeError> {
        serde_json::from_value(self.ask(&ProbeWire::FilesystemIdentity {
            path: path(target)?,
        })?)
        .map_err(|_| ProbeError::Unavailable)
    }
    fn canonicalize(&self, target: &Path) -> Result<Option<PathBuf>, ProbeError> {
        let answer = self.ask(&ProbeWire::Canonicalize {
            path: path(target)?,
        })?;
        match answer.get("path") {
            Some(Value::String(resolved)) => Ok(Some(PathBuf::from(resolved))),
            Some(Value::Null) => Ok(None),
            _ => Err(ProbeError::Unavailable),
        }
    }

    fn is_file(&self, target: &Path) -> Result<bool, ProbeError> {
        self.ask(&ProbeWire::IsFile {
            path: path(target)?,
        })?
        .get("file")
        .and_then(Value::as_bool)
        .ok_or(ProbeError::Unavailable)
    }

    fn tree_bytes(&self, target: &Path) -> Result<Option<u64>, ProbeError> {
        let answer = self.ask(&ProbeWire::TreeBytes {
            path: path(target)?,
        })?;
        match answer.get("bytes") {
            Some(Value::Null) => Ok(None),
            Some(bytes) => bytes.as_u64().map(Some).ok_or(ProbeError::Unavailable),
            None => Err(ProbeError::Unavailable),
        }
    }
}

/// Answers one probe question from this process's filesystem, as the runner
/// side of a link, about a path `admits` accepts; any other path is refused
/// as a workspace the runner does not serve. A resolution that is not UTF-8
/// is refused, so the host sees no answer rather than an absent one.
pub fn serve_probe(payload: Value, admits: &(dyn Fn(&Path) -> bool + Send + Sync)) -> ServeFuture {
    let question = match from_value::<ProbeWire>(payload) {
        Ok(question) => question,
        Err(error) => return Box::pin(std::future::ready(Err(backend_failure(&error)))),
    };
    let (ProbeWire::FilesystemIdentity { path }
    | ProbeWire::Canonicalize { path }
    | ProbeWire::IsFile { path }
    | ProbeWire::TreeBytes { path }) = &question;
    if !admits(Path::new(path)) {
        return Box::pin(std::future::ready(Err(super::environment_failure(
            &super::workspace_refusal(),
        ))));
    }
    Box::pin(async move {
        tokio::task::spawn_blocking(move || {
            let probe = LocalWorkspaceProbe;
            let answer = match question {
                ProbeWire::FilesystemIdentity { path } => serde_json::to_value(probe.filesystem_identity(Path::new(&path)).map_err(|_| ServeFailure::Unsupported)?).map_err(|_| ServeFailure::Unsupported)?,
                ProbeWire::Canonicalize { path } => {
                    match probe.canonicalize(Path::new(&path)).ok().flatten() {
                        None => json!({ "path": null }),
                        // A resolution the wire cannot carry is no answer,
                        // never an absent one.
                        Some(resolved) => json!({ "path": path_text(&resolved).map_err(|error| backend_failure(&error))? }),
                    }
                }
                ProbeWire::IsFile { path } => {
                    json!({ "file": probe.is_file(Path::new(&path)).unwrap_or(false) })
                }
                ProbeWire::TreeBytes { path } => {
                    json!({ "bytes": probe.tree_bytes(Path::new(&path)).ok().flatten() })
                }
            };
            Ok(answer)
        })
        .await
        .unwrap_or(Err(ServeFailure::Unsupported))
    })
}
