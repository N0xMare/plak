use super::{
    Answer, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection, HostFrame,
    Outcome, ProtocolExtension, RequestRefusal, Serialize, Write, write_frame,
};
use plakous_host_protocol::wire::RequestId;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Instant;
use tokio::sync::{Notify, OwnedSemaphorePermit};

/// Bounded host-owned work whose result is private to one request id.
pub type HostReadTask<P> = std::pin::Pin<
    Box<dyn Future<Output = Result<<P as ProtocolExtension>::HostResponse, RequestRefusal>> + Send>,
>;

/// Identity minted by the authenticated transport, never client input.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum HostPrincipal {
    /// Peer credentials verified against the socket owner's uid. Console
    /// access is trusted independently of the web capability switches.
    Socket(u32),
    /// Login admitted by the web authentication gate, subject to the web
    /// capability switches even though authentication already succeeded.
    Web(String),
}

/// Authority and ownership of one attachment-free product request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostRequestContext {
    /// Transport-verified identity for aggregate budgets and capabilities.
    pub principal: HostPrincipal,
    /// Host-unique connection lifetime, not a wire attachment id.
    pub connection_id: u64,
    /// Process generation, invalidating transient reads across restart.
    pub host_generation: u64,
    /// Wire response address, used only for bounded host audit evidence.
    pub request_id: RequestId,
}

#[derive(Default)]
pub(super) struct ReadyReads {
    pub(super) notify: Notify,
    ids: Mutex<BTreeSet<RequestId>>,
}
impl ReadyReads {
    fn mark(&self, id: RequestId) {
        self.ids
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id);
        self.notify.notify_one();
    }
}
struct ReadWake {
    ready: Arc<ReadyReads>,
    id: RequestId,
}
impl Wake for ReadWake {
    fn wake(self: Arc<Self>) {
        self.ready.mark(self.id);
    }
}

pub(super) struct PendingRead<P: ProtocolExtension> {
    timeout: Result<P::HostResponse, RequestRefusal>,
    task: HostReadTask<P>,
    pub(super) deadline: Instant,
    waker: Waker,
    _principal: OwnedSemaphorePermit,
    _host: OwnedSemaphorePermit,
}

#[derive(serde::Deserialize)]
struct Peek {
    frame: String,
    #[serde(default)]
    op: Option<PeekOp>,
    #[serde(default)]
    id: Option<RequestId>,
}
#[derive(serde::Deserialize)]
struct PeekOp {
    op: String,
}
fn peek(line: &str) -> Option<RequestId> {
    let peek: Peek = serde_json::from_str(line).ok()?;
    (peek.frame == "request" && peek.op.as_ref().map(|op| op.op.as_str()) == Some("host"))
        .then_some(peek.id)
        .flatten()
}

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn host_context(&self) -> HostRequestContext {
        HostRequestContext {
            principal: self.principal.clone(),
            connection_id: self.connection_id,
            host_generation: self.host.generation,
            request_id: 0,
        }
    }

    /// Peeks at the envelope without allocating the product payload. Web
    /// transports apply the legacy close bucket only to non-console frames.
    #[must_use]
    pub fn is_host_request(line: &str) -> bool {
        peek(line).is_some()
    }

    pub(super) fn preflight_host_read(&mut self, line: &str) -> CliResult<Option<bool>> {
        let Some(id) = peek(line) else {
            return Ok(None);
        };
        let oversized = line.len() > self.host.admission.limits.request_bytes
            || !P::host_request_size_allowed(line);
        let refused = if oversized {
            Some(RequestRefusal::Malformed {
                detail: "host request exceeds its byte limit".into(),
            })
        } else {
            self.principal_budget.rate(self.host.admission.limits).err()
        };
        if let Some(refusal) = refused {
            self.audit_host_refusal(id, &refusal);
            write_frame(
                &mut self.out,
                &HostFrame::<P>::Response {
                    id,
                    outcome: Outcome::Refused(refusal),
                },
            )?;
            if oversized {
                self.leave();
            }
            return Ok(Some(!oversized));
        }
        Ok(None)
    }
    pub(super) fn start_host_read(
        &mut self,
        request_id: RequestId,
        request: P::HostRequest,
    ) -> Forwarded<P> {
        let refused = |refusal| {
            self.audit_host_refusal(request_id, &refusal);
            Forwarded::Answered(Outcome::Refused(refusal))
        };
        if self.host_reads.contains_key(&request_id) {
            return refused(RequestRefusal::Malformed {
                detail: "host request id is already in flight".to_owned(),
            });
        }
        let limits = self.host.admission.limits;
        let busy = || RequestRefusal::RateLimited {
            retry_after_ms: 1_000_u64.div_ceil(u64::from(limits.messages_per_second).max(1)),
        };
        if self.host_reads.len() >= limits.per_connection {
            return refused(busy());
        }
        let Ok(principal_permit) = self.principal_budget.active.clone().try_acquire_owned() else {
            return refused(busy());
        };
        let Ok(host_permit) = self.host.admission.host.clone().try_acquire_owned() else {
            return refused(busy());
        };
        let context = HostRequestContext {
            principal: self.principal.clone(),
            connection_id: self.connection_id,
            host_generation: self.host.generation,
            request_id,
        };
        let started = Instant::now();
        let deadline = started.checked_add(limits.deadline).unwrap_or(started);
        let timeout = self.host.sessions.host_request_timeout(&request);
        self.host_reads.insert(
            request_id,
            PendingRead {
                timeout,
                task: self.host.sessions.host_request(context, request),
                deadline,
                waker: Waker::from(Arc::new(ReadWake {
                    ready: self.read_ready.clone(),
                    id: request_id,
                })),
                _principal: principal_permit,
                _host: host_permit,
            },
        );
        self.read_ready.mark(request_id);
        Forwarded::Reading(true)
    }

    pub(super) fn flush_host_reads(&mut self) -> CliResult<()> {
        let mut ready = std::mem::take(
            &mut *self
                .read_ready
                .ids
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let now = Instant::now();
        ready.extend(
            self.host_reads
                .iter()
                .filter_map(|(id, read)| (now >= read.deadline).then_some(*id)),
        );
        let mut completed = Vec::new();
        for id in ready {
            let Some(read) = self.host_reads.get_mut(&id) else {
                continue;
            };
            let result = if now >= read.deadline {
                Poll::Ready(read.timeout.clone())
            } else {
                read.task
                    .as_mut()
                    .poll(&mut Context::from_waker(&read.waker))
            };
            let result = if Instant::now() >= read.deadline {
                Poll::Ready(read.timeout.clone())
            } else {
                result
            };
            if let Poll::Ready(result) = result {
                completed.push((id, result));
            }
        }
        for (id, result) in completed {
            // Drop work and permits before answering; a late wake cannot
            // restore this entry or publish a second answer.
            self.host_reads.remove(&id);
            let outcome = match result {
                Ok(response) => Outcome::Ok(Answer::Host { response }),
                Err(refusal) => {
                    self.audit_host_refusal(id, &refusal);
                    Outcome::Refused(refusal)
                }
            };
            let frame = HostFrame::<P>::Response { id, outcome };
            write_frame(&mut self.out, &frame)?;
            if let HostFrame::Response {
                outcome: Outcome::Ok(Answer::Host { response }),
                ..
            } = frame
            {
                let mut context = self.host_context();
                context.request_id = id;
                self.host.sessions.host_response_sent(&context, &response);
            }
        }
        Ok(())
    }

    fn audit_host_refusal(&self, request_id: RequestId, refusal: &RequestRefusal) {
        self.host.sessions.host_request_refused(
            HostRequestContext {
                principal: self.principal.clone(),
                connection_id: self.connection_id,
                host_generation: self.host.generation,
                request_id,
            },
            refusal,
        );
    }
}
