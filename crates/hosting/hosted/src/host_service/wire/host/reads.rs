use super::{
    Answer, CliResult, ClientFrame, DeserializeOwned, Forwarded, HostConnection, HostFrame,
    Outcome, ProtocolExtension, RequestRefusal, Serialize, Write, write_frame,
};
use plakous_host_protocol::wire::RequestId;

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
}

impl<P: ProtocolExtension, W: Write + Clone> HostConnection<P, W>
where
    HostFrame<P>: Serialize,
    ClientFrame<P>: DeserializeOwned,
{
    pub(super) fn start_host_read(
        &mut self,
        request_id: RequestId,
        request: P::HostRequest,
    ) -> Forwarded<P> {
        let refused = |refusal| Forwarded::Answered(Outcome::Refused(refusal));
        let Some(principal) = self.principal.clone() else {
            return refused(RequestRefusal::RefusedForRole {
                row: "authenticated_console".to_owned(),
            });
        };
        if self.host_reads.contains_key(&request_id) {
            return refused(RequestRefusal::Malformed {
                detail: "host request id is already in flight".to_owned(),
            });
        }
        if self.host_reads.len() >= 2 {
            return refused(RequestRefusal::RateLimited { retry_after_ms: 50 });
        }
        let context = HostRequestContext {
            principal,
            connection_id: self.connection_id,
            host_generation: self.host.generation,
        };
        self.host_reads.insert(
            request_id,
            self.host.sessions.host_request(context, request),
        );
        Forwarded::Reading(true)
    }

    pub(super) fn flush_host_reads(&mut self) -> CliResult<()> {
        let mut completed = Vec::new();
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        for (id, task) in &mut self.host_reads {
            if let std::task::Poll::Ready(result) = task.as_mut().poll(&mut context) {
                completed.push((*id, result));
            }
        }
        for (id, result) in completed {
            self.host_reads.remove(&id);
            let outcome = match result {
                Ok(response) => Outcome::Ok(Answer::Host { response }),
                Err(refusal) => Outcome::Refused(refusal),
            };
            write_frame(&mut self.out, &HostFrame::<P>::Response { id, outcome })?;
        }
        Ok(())
    }
}
