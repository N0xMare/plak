//! The runner end: answers one host connection at a time from a local
//! execution environment, with the owner and op tables of this instance.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use plakous_model::{AttemptId, BranchId, CallId, CancellationToken, LineId, Timestamp};
use plakous_runner_protocol::frame::FrameError;
use plakous_runner_protocol::{
    Hello, HostFrame, LineOwner, Refusal, Request, RequestClass, RunnerFrame, VersionTriple,
    WireError, capabilities,
};
use plakous_tools::internal::runner::{RunnerVerb, ServeFailure, runner_descriptor, serve};
use plakous_tools::{
    ExecutionEnvironment, ExecutionEnvironmentError, OperationClass, OperationContext, RunnerId,
};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::connection::{FrameStream, fits, write_frames};
use crate::state::{Admission, OpKey, Tables};

/// One runner process: its identity, its environment and the tables every
/// connection to it shares. A new service is a new instance.
#[derive(Clone)]
pub struct RunnerService {
    inner: Arc<Inner>,
}

struct Inner {
    environment: ExecutionEnvironment,
    hello: Hello,
    tables: Mutex<Tables>,
    /// Each host's current connection: answers go there, so an answer that
    /// outlives its connection reaches the host's reconnection.
    hosts: Mutex<HashMap<String, mpsc::UnboundedSender<RunnerFrame>>>,
    /// Cancellation of each running request, by its op key when it is
    /// keyed, so a replay that moved the key's request id still reaches it.
    running: Mutex<HashMap<(String, RunKey), CancellationToken>>,
}

/// What a runner says about itself in its hello.
#[derive(Clone, Debug)]
pub struct RunnerIdentity {
    /// The runner's stable identity; never `local`.
    pub runner_id: RunnerId,
    /// The version triple it binds.
    pub version: VersionTriple,
    /// The machine boot it runs in.
    pub boot_id: String,
    /// Its build's binary id, as evidence.
    pub binary_id: Option<String>,
}

impl RunnerService {
    /// A new runner instance answering from `environment`.
    ///
    /// # Errors
    ///
    /// Refuses the reserved `local` runner id.
    pub fn new(
        environment: ExecutionEnvironment,
        identity: RunnerIdentity,
    ) -> Result<Self, ExecutionEnvironmentError> {
        let descriptor = runner_descriptor(&environment, identity.runner_id.clone())?;
        let hello = Hello {
            version: identity.version,
            binary_id: identity.binary_id,
            runner_id: identity.runner_id.as_str().to_owned(),
            instance: uuid::Uuid::new_v4().to_string(),
            boot_id: identity.boot_id,
            descriptor,
            capabilities: vec![capabilities::WORKSPACE_VERBS.to_owned()],
            path: None,
            home: None,
            job_bound: 0,
        };
        Ok(Self {
            inner: Arc::new(Inner {
                environment,
                hello,
                tables: Mutex::default(),
                hosts: Mutex::default(),
                running: Mutex::default(),
            }),
        })
    }

    /// What this instance says first on every connection.
    #[must_use]
    pub fn hello(&self) -> &Hello {
        &self.inner.hello
    }

    /// Answers one connection until it ends. Requests it admitted keep
    /// running after that, and answer on the host's next connection.
    pub async fn serve_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (sender, frames) = mpsc::unbounded_channel();
        tokio::spawn(write_frames(writer, frames));
        let mut reader = FrameStream::new(reader);
        let _ = sender.send(RunnerFrame::Hello(self.inner.hello.clone()));
        let Some(Ok(HostFrame::Bind(bind))) = reader.next::<HostFrame>().await else {
            let _ = sender.send(refused(None, Refusal::Unbound));
            return;
        };
        if bind.version != self.inner.hello.version {
            let _ = sender.send(refused(
                None,
                Refusal::VersionMismatch {
                    runner: self.inner.hello.version.clone(),
                },
            ));
            return;
        }
        let host = bind.host_id;
        lock(&self.inner.hosts).insert(host.clone(), sender.clone());
        while let Some(frame) = reader.next::<HostFrame>().await {
            let frame = match frame {
                Ok(frame) => frame,
                Err(error) => {
                    let _ = sender.send(refused(
                        None,
                        Refusal::Malformed {
                            detail: error.to_string(),
                        },
                    ));
                    // An oversized prefix leaves the stream unframed: the
                    // connection ends.
                    if error == FrameError::TooLarge {
                        break;
                    }
                    continue;
                }
            };
            match frame {
                HostFrame::Request(request) => self.admit(&host, request, &sender),
                HostFrame::Cancel { request_id } => {
                    let run = self.run_key(&host, request_id);
                    if let Some(token) = lock(&self.inner.running).get(&(host.clone(), run)) {
                        token.cancel();
                    }
                }
                // No operation of this version reads a yield.
                HostFrame::Preempt { .. } => {}
                HostFrame::Ack { request_id } => {
                    lock(&self.inner.tables).acknowledge(&host, &request_id);
                }
                HostFrame::TransferOwner(transfer) => {
                    let answer =
                        lock(&self.inner.tables).transfer(&host, transfer.control_id, transfer.to);
                    let _ = sender.send(answer);
                }
                HostFrame::Ping { nonce } => {
                    let _ = sender.send(RunnerFrame::Pong { nonce });
                }
                HostFrame::Bind(_) => {
                    let _ = sender.send(refused(
                        None,
                        Refusal::Malformed {
                            detail: "the connection is already bound".into(),
                        },
                    ));
                }
                HostFrame::Stream(_) => {
                    let _ = sender.send(refused(
                        None,
                        Refusal::Unsupported {
                            what: "stream".into(),
                        },
                    ));
                }
            }
        }
    }

    fn admit(&self, host: &str, request: Request, sender: &mpsc::UnboundedSender<RunnerFrame>) {
        let id = request.request_id.clone();
        if !matches!(
            request.class,
            RequestClass::EffectCall { .. } | RequestClass::BuildRead
        ) {
            let _ = sender.send(refused(
                Some(id),
                Refusal::Unsupported {
                    what: "request class".into(),
                },
            ));
            return;
        }
        if RunnerVerb::parse(&request.operation.verb).is_none() {
            let _ = sender.send(refused(
                Some(id),
                Refusal::Unsupported {
                    what: request.operation.verb,
                },
            ));
            return;
        }
        let token = CancellationToken::new();
        let control = match context(&request, token.clone()) {
            Ok(control) => control,
            Err(detail) => {
                let _ = sender.send(refused(Some(id), Refusal::Malformed { detail }));
                return;
            }
        };
        let key = match lock(&self.inner.tables).admit(host, &request, Instant::now()) {
            Admission::Answer(frame) => {
                let _ = sender.send(frame);
                return;
            }
            Admission::Run(key) => key,
        };
        let run = key
            .clone()
            .map_or_else(|| RunKey::Request(id.clone()), RunKey::Op);
        lock(&self.inner.running).insert((host.to_owned(), run), token);
        let service = self.clone();
        let host = host.to_owned();
        let served = serve(
            &self.inner.environment,
            &request.operation.verb,
            request.operation.payload,
            control,
        );
        tokio::spawn(async move {
            let answer = answer_frame(id.clone(), served.await);
            service.finish(&host, &id, key.as_ref(), answer);
        });
    }

    fn finish(&self, host: &str, request_id: &str, key: Option<&OpKey>, answer: RunnerFrame) {
        let run = key
            .cloned()
            .map_or_else(|| RunKey::Request(request_id.to_owned()), RunKey::Op);
        lock(&self.inner.running).remove(&(host.to_owned(), run));
        let answer = match key {
            Some(key) => lock(&self.inner.tables).answer(host, key, answer, Instant::now()),
            None => answer,
        };
        if let Some(sender) = lock(&self.inner.hosts).get(host) {
            let _ = sender.send(answer);
        }
    }
}

/// How a running request is found again for cancellation.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum RunKey {
    Op(OpKey),
    Request(String),
}

impl RunnerService {
    fn run_key(&self, host: &str, request_id: String) -> RunKey {
        lock(&self.inner.tables)
            .key_for(host, &request_id)
            .map_or(RunKey::Request(request_id), RunKey::Op)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

const fn refused(request_id: Option<String>, refusal: Refusal) -> RunnerFrame {
    RunnerFrame::Refused {
        request_id,
        refusal,
    }
}

fn answer_frame(request_id: String, served: Result<Value, ServeFailure>) -> RunnerFrame {
    let frame = match served {
        Ok(payload) => RunnerFrame::Result {
            request_id: request_id.clone(),
            payload,
        },
        Err(ServeFailure::Unsupported) => refused(
            Some(request_id.clone()),
            Refusal::Unsupported {
                what: "verb".into(),
            },
        ),
        Err(ServeFailure::Backend {
            error,
            proven_no_effect,
        }) => RunnerFrame::Error {
            request_id: request_id.clone(),
            error: WireError {
                error,
                proven_no_effect,
            },
        },
    };
    if fits(&frame) {
        return frame;
    }
    RunnerFrame::Error {
        request_id,
        error: WireError {
            error: plakous_tools::internal::runner::answer_too_large(),
            proven_no_effect: false,
        },
    }
}

/// The operation context the request names, as the runner's own clock and
/// cancellation carry it.
fn context(request: &Request, token: CancellationToken) -> Result<OperationContext, String> {
    let mut control = OperationContext::new(token);
    if let Some(owner) = &request.owner {
        control = control.with_owner(line_owner(owner)?);
    }
    if let RequestClass::EffectCall {
        call_id,
        attempt_id,
    } = &request.class
    {
        control = control.with_class(OperationClass::EffectCall {
            call_id: CallId::from_str(call_id).map_err(|error| error.to_string())?,
            attempt_id: AttemptId::from_str(attempt_id).map_err(|error| error.to_string())?,
        });
    }
    if let Some(remaining) = request.deadline_ms {
        let deadline = plakous_model::now_unix_millis().saturating_add(remaining);
        control = control.with_budget(
            Timestamp::new(
                deadline / 1000,
                u32::try_from((deadline % 1000) * 1_000_000).unwrap_or(0),
            )
            .map_err(|error| error.to_string())?,
        );
    }
    Ok(control)
}

pub(crate) fn line_owner(owner: &LineOwner) -> Result<plakous_tools::LineOwner, String> {
    Ok(plakous_tools::LineOwner::new(
        LineId::from_str(&owner.line).map_err(|error| error.to_string())?,
        owner.generation,
        BranchId::from_str(&owner.branch).map_err(|error| error.to_string())?,
        owner.fence,
    ))
}
