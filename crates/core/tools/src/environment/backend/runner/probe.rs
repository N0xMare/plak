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
    Canonicalize { path: String },
    IsFile { path: String },
    TreeBytes { path: String },
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
    let (ProbeWire::Canonicalize { path }
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
