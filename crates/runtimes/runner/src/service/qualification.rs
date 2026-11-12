//! Paired read-only preparation never enters host custody or the operation tables.

use super::{RunnerService, answer_frame, context, refused};
use crate::connection::FrameStream;
use plakous_model::CancellationToken;
use plakous_runner_protocol::{
    HostFrame, Refusal, Request, RequestClass, RunnerFrame, VersionTriple,
};
use plakous_tools::ToolPreempt;
use plakous_tools::internal::runner::{
    RunnerVerb, ServeFailure, environment_failure, requested_workspace, serve, serve_probe,
};
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::sync::{mpsc, watch};

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "isolated qualification streams"
)]
mod tests;

impl RunnerService {
    pub(super) async fn serve_qualification<R: AsyncRead + Unpin>(
        &self,
        host: &str,
        version: &VersionTriple,
        reader: &mut FrameStream<R>,
        sender: &mpsc::UnboundedSender<RunnerFrame>,
        closer: &CancellationToken,
    ) {
        if let Some(refusal) = self.qualification_refusal(host, version) {
            let _ = sender.send(refused(None, refusal));
            return;
        }
        loop {
            let frame = tokio::select! {
                frame = tokio::time::timeout(self.inner.dead, reader.next::<HostFrame>()) => frame,
                () = closer.cancelled() => return,
            };
            match frame {
                Ok(Some(Ok(HostFrame::Ping { nonce }))) => {
                    let _ = sender.send(RunnerFrame::Pong { nonce });
                }
                Ok(Some(Ok(HostFrame::Request(request)))) => {
                    if !request_allowed(&request) {
                        let _ =
                            sender.send(refused(Some(request.request_id), Refusal::InspectOnly));
                        return;
                    }
                    let id = request.request_id.clone();
                    let answer = tokio::select! {
                        answer = tokio::time::timeout(self.inner.dead, self.qualification_read(request)) => answer,
                        () = closer.cancelled() => return,
                    };
                    match answer {
                        Ok(answer) => {
                            let _ = sender.send(answer_frame(id, answer));
                        }
                        Err(_) => return,
                    }
                }
                Ok(Some(Ok(_))) => {
                    let _ = sender.send(refused(None, Refusal::InspectOnly));
                    return;
                }
                Ok(Some(Err(_)) | None) | Err(_) => return,
            }
        }
    }

    fn qualification_refusal(&self, host: &str, version: &VersionTriple) -> Option<Refusal> {
        if *version != self.inner.hello.version {
            return Some(Refusal::VersionMismatch {
                runner: self.inner.hello.version.clone(),
            });
        }
        if self.inner.home_local.is_some() {
            return Some(Refusal::Unsupported {
                what: "home-local qualification".into(),
            });
        }
        let paired = self
            .inner
            .hosts_allowed
            .as_ref()
            .is_some_and(|allowed| allowed(host));
        (!paired).then(|| Refusal::NotPaired {
            host_id: host.to_owned(),
        })
    }

    async fn qualification_read(&self, request: Request) -> Result<Value, ServeFailure> {
        let verb = RunnerVerb::parse(&request.operation.verb).ok_or(ServeFailure::Unsupported)?;
        let cancellation = CancellationToken::new();
        let (_, yields) = watch::channel(ToolPreempt::None);
        let control =
            context(&request, cancellation, yields).map_err(|_| ServeFailure::Unsupported)?;
        let payload = request.operation.payload;
        match verb {
            RunnerVerb::WorkspaceOptions | RunnerVerb::WorkspaceValidate => {
                crate::workspace::serve(self.inner.workspace_catalog.clone(), verb, payload).await
            }
            RunnerVerb::ProbeWorkspace => match &self.inner.probe_paths {
                Some(admits) => serve_probe(payload, admits.as_ref()).await,
                None => Err(ServeFailure::Unsupported),
            },
            RunnerVerb::ReadInstructions => match &self.inner.extension {
                Some(extension) => extension.serve(verb, payload).await,
                None => Err(ServeFailure::Unsupported),
            },
            RunnerVerb::ReadWorkspace => {
                let environment = match requested_workspace(&payload) {
                    Some((root, cwd)) => match &self.inner.workspaces {
                        Some(workspaces) => {
                            workspaces(&root, &cwd).map_err(|error| environment_failure(&error))?
                        }
                        None => return Err(ServeFailure::Unsupported),
                    },
                    None => return Err(ServeFailure::Unsupported),
                };
                serve(&environment, verb.as_str(), payload, control).await
            }
            _ => Err(ServeFailure::Unsupported),
        }
    }
}

pub(crate) fn request_allowed(request: &Request) -> bool {
    request.class == RequestClass::BuildRead
        && request.owner.is_none()
        && !request.replay
        && request.age_ms.is_none()
        && request.digest == crate::client::digest(&request.operation)
        && RunnerVerb::parse(&request.operation.verb).is_some_and(|verb| {
            matches!(
                verb,
                RunnerVerb::WorkspaceOptions
                    | RunnerVerb::WorkspaceValidate
                    | RunnerVerb::ProbeWorkspace
                    | RunnerVerb::ReadWorkspace
                    | RunnerVerb::ReadInstructions
            )
        })
}
