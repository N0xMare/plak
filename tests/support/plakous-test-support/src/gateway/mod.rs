//! A scripted credential gateway over the shared relay server core.
//!
//! [`plakous_http::relay_server::RelayServer`] is the only protocol
//! implementation. This module supplies a
//! [`GatewayService`](plakous_http::relay_server::GatewayService) and a
//! loopback upstream reached through
//! [`RelayServer::with_dial_override`](plakous_http::relay_server::RelayServer::with_dial_override)
//! (`fault-injection`, repository-only). Pin checks still see the logical URL.

mod service;
mod upstream;

#[cfg(test)]
mod durability;
#[cfg(test)]
mod tests;

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plakous_gateway_protocol::UuidText;
use plakous_gateway_protocol::envelope::split_upstream_url;
use plakous_gateway_protocol::facts::AccountFacts;
use plakous_gateway_protocol::header::{RequestHeader, WireHeader};
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_http::relay::{GatewayEndpoint, InMemoryGateway, RelayClient};
use plakous_http::relay_server::{Peer, RelayOutcome, RelayServer};
use plakous_http::{Client, ConfigError, Limits, Origin};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;

use service::Scripted;

/// Why the fixture could not be built or served.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// Binding the loopback upstream or serving a connection failed.
    #[error("fixture gateway connection failed")]
    Io(#[source] std::io::Error),
    /// A custodied entry named a value the fixture was not given.
    #[error("custodied header has no scripted value")]
    MissingCredential,
    /// A URL or header could not be rendered.
    #[error("fixture gateway request is not valid HTTP")]
    Http,
    /// The upstream client or relay client could not be built.
    #[error("fixture gateway upstream client could not be built")]
    Upstream(#[source] ConfigError),
    /// A scripted instance id was not a canonical UUID.
    #[error("scripted gateway instance is not a UUID")]
    Instance,
}

/// What one fully read relay should do. The default, when nothing is queued,
/// substitutes the scripted API key and answers `200`.
#[derive(Clone, Debug)]
pub enum GatewayScript {
    /// Dial the loopback upstream and relay its reply.
    Relay(UpstreamReply),
    /// A gateway refusal. Nothing is dialed.
    Refuse(GatewayRefusal),
    /// Close after the full read, with no response and no dial.
    DropBeforeResponse,
    /// Dial, relay a head, and leave the upstream open until the caller leaves.
    DropAfterHead,
    /// Wait, still without dialing, then relay. A dropped caller ends the wait.
    Stall(std::time::Duration),
}

/// The upstream response a [`GatewayScript::Relay`] turns into frames.
#[derive(Clone, Debug)]
pub struct UpstreamReply {
    /// Upstream status, `100..=599`.
    pub status: u16,
    /// End-to-end headers, in order.
    pub headers: Vec<WireHeader>,
    /// Upstream body. Empty sends no data frame.
    pub body: Vec<u8>,
    /// Trailers, when the upstream sent them.
    pub trailers: Vec<WireHeader>,
}

impl Default for UpstreamReply {
    fn default() -> Self {
        Self {
            status: 200,
            headers: Vec::new(),
            body: b"ok".to_vec(),
            trailers: Vec::new(),
        }
    }
}

/// One upstream request recorded at the loopback peer, after the core's dial.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedUpstream {
    /// The raw HTTP/1.1 bytes the upstream accepted.
    pub http: Vec<u8>,
}

/// How one connection ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServeReport {
    /// The loopback upstream accepted a connection.
    pub dialed: bool,
    /// The caller left after the core had started the upstream exchange.
    pub caller_cancelled: bool,
}

struct Inner {
    service: Arc<Scripted>,
    client: Client,
    dial: Origin,
    generation: AtomicU64,
    server: Mutex<RelayServer<Scripted>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    start: AsyncMutex<Start>,
}

struct Start {
    listener: Option<std::net::TcpListener>,
    listening: bool,
}

/// An in-process gateway. Clones share the script, the instance and the dial
/// counter.
#[derive(Clone)]
pub struct FixtureGateway {
    inner: Arc<Inner>,
}

impl FixtureGateway {
    /// A gateway that substitutes `credentials` for custodied entries of the
    /// same name. `key` is the required `plak-gateway-key`, or none for an
    /// in-process or same-UID caller.
    ///
    /// The scripted API-key fence is generation 1 with the fixed test binding.
    /// Subscription refresh is not scripted here.
    ///
    /// # Errors
    /// Refuses an upstream client that cannot be built, or a listener that
    /// cannot bind. The initial instance id is fixed, so it is not the
    /// failure.
    pub fn new(
        credentials: Vec<(String, Vec<u8>)>,
        key: Option<String>,
    ) -> Result<Self, FixtureError> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(FixtureError::Io)?;
        let port = listener.local_addr().map_err(FixtureError::Io)?.port();
        let dial = Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
            .map_err(|_| FixtureError::Http)?;
        let service = Scripted::new(credentials, key);
        let client = Client::new_http1(Limits::default()).map_err(FixtureError::Upstream)?;
        let server = build_server(&service, &client, &dial, instance_at(1)?);
        Ok(Self {
            inner: Arc::new(Inner {
                service,
                client,
                dial,
                generation: AtomicU64::new(1),
                server: Mutex::new(server),
                tasks: Mutex::new(Vec::new()),
                start: AsyncMutex::new(Start {
                    listener: Some(listener),
                    listening: false,
                }),
            }),
        })
    }

    /// Queues one action. Relays consume the queue in order.
    pub fn push(&self, action: GatewayScript) {
        self.inner.service.push(action);
    }

    /// On the n-th decoded relay, answer `fence_moved` and do not dial.
    pub fn script_fence_moved(&self, nth: u64, facts: AccountFacts) {
        self.inner.service.script_fence_moved(nth, facts);
    }

    /// Replaces the instance id. A restart does not clear the dial counter
    /// and does not drop a connection that is already being served.
    ///
    /// # Errors
    /// Refuses an id that is not a canonical UUID.
    pub fn restart(&self) -> Result<UuidText, FixtureError> {
        let next = self.inner.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let id = instance_at(next)?;
        *lock(&self.inner.server) = build_server(
            &self.inner.service,
            &self.inner.client,
            &self.inner.dial,
            id.clone(),
        );
        Ok(id)
    }

    /// The instance id a new connection's `/versions` reports.
    #[must_use]
    pub fn instance(&self) -> UuidText {
        lock(&self.inner.server).instance().clone()
    }

    /// Upstream TCP accepts. A refusal, a drop before the response, a stall
    /// that has not finished and an incomplete body do not increment it.
    #[must_use]
    pub fn dials(&self) -> u64 {
        self.inner.service.upstream.accepted.load(Ordering::SeqCst)
    }

    /// Raw upstream requests, in accept order.
    #[must_use]
    pub fn captured(&self) -> Vec<CapturedUpstream> {
        lock(&self.inner.service.upstream.requests)
            .iter()
            .cloned()
            .map(|http| CapturedUpstream { http })
            .collect()
    }

    /// The loopback origin the dial override uses.
    #[must_use]
    pub fn upstream_origin(&self) -> Origin {
        self.inner.dial.clone()
    }

    /// A host client that relays through this gateway.
    ///
    /// # Errors
    /// Returns the client construction failure.
    pub async fn relayed(&self) -> Result<Client, FixtureError> {
        self.ensure_upstream().await?;
        let relay = RelayClient::new(self.endpoint(), None).map_err(FixtureError::Upstream)?;
        let client = Client::new_http1(Limits::default()).map_err(FixtureError::Upstream)?;
        Ok(client.with_relay(Arc::new(relay)))
    }

    /// Serves one connection on the current instance until the caller closes
    /// it, or until a scripted drop closes it first.
    ///
    /// # Errors
    /// Returns a listener failure. A caller that hangs up is a finished
    /// report, not an error.
    pub async fn serve_one<IO>(&self, io: IO) -> Result<ServeReport, FixtureError>
    where
        IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        self.ensure_upstream().await?;
        let before = self.dials();
        let gate = Arc::new(Notify::new());
        self.inner.service.arm_drop(Arc::clone(&gate));
        let server = lock(&self.inner.server).clone();
        tokio::select! {
            biased;
            () = gate.notified() => {}
            result = server.serve(io, Peer::InProcess) => {
                let _ = result;
            }
        }
        self.inner.service.clear_drop();
        let caller_cancelled = matches!(
            self.inner.service.last_outcome(),
            Some(RelayOutcome::Abandoned)
        );
        Ok(ServeReport {
            dialed: self.dials() > before,
            caller_cancelled,
        })
    }

    /// Stops connections this gateway spawned. The caller's own tasks are
    /// left alone.
    pub fn kill(&self) {
        let tasks = std::mem::take(&mut *lock(&self.inner.tasks));
        for task in tasks {
            task.abort();
        }
    }

    /// Resolves when the next hung upstream connection is dropped.
    pub async fn wait_for_upstream_close(&self) {
        self.inner.service.upstream.wait_for_close().await;
    }

    fn endpoint(&self) -> GatewayEndpoint {
        let gateway = self.clone();
        GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
            let running = gateway.clone();
            let handle = tokio::spawn(async move {
                let _ = running.serve_one(io).await;
            });
            lock(&gateway.inner.tasks).push(handle);
        }))
    }

    async fn ensure_upstream(&self) -> Result<(), FixtureError> {
        let mut start = self.inner.start.lock().await;
        if start.listening {
            return Ok(());
        }
        let listener = start.listener.take().ok_or_else(|| {
            FixtureError::Io(std::io::Error::other("fixture upstream listener is gone"))
        })?;
        listener.set_nonblocking(true).map_err(FixtureError::Io)?;
        let listener = tokio::net::TcpListener::from_std(listener).map_err(FixtureError::Io)?;
        let upstream = Arc::clone(&self.inner.service.upstream);
        tokio::spawn(async move {
            upstream::serve(listener, upstream).await;
        });
        start.listening = true;
        Ok(())
    }
}

fn build_server(
    service: &Arc<Scripted>,
    client: &Client,
    dial: &Origin,
    instance: UuidText,
) -> RelayServer<Scripted> {
    RelayServer::new(Arc::clone(service), client.clone(), instance)
        .with_dial_override(service::LOGICAL, dial.clone())
}

fn instance_at(n: u64) -> Result<UuidText, FixtureError> {
    UuidText::new(format!("00000000-0000-4000-8000-{n:012x}")).map_err(|_| FixtureError::Instance)
}

/// Fills custodied entries in place. Other entries keep their bytes and
/// their positions.
///
/// # Errors
/// Refuses a custodied name with no scripted value, and a value HTTP cannot
/// carry.
pub fn substitute(
    headers: &[RequestHeader],
    credentials: &[(String, Vec<u8>)],
) -> Result<Vec<WireHeader>, FixtureError> {
    let mut rendered = Vec::with_capacity(headers.len());
    for header in headers {
        match header {
            RequestHeader::Value(value) => rendered.push(value.clone()),
            RequestHeader::Custodied { name } => {
                let Some((_, value)) = credentials.iter().find(|(candidate, _)| candidate == name)
                else {
                    return Err(FixtureError::MissingCredential);
                };
                rendered.push(
                    WireHeader::new(name.clone(), value.clone())
                        .map_err(|_| FixtureError::MissingCredential)?,
                );
            }
        }
    }
    Ok(rendered)
}

/// The HTTP/1.1 request a direct rendering of the same headers produces.
///
/// `host` comes from the URL, then the headers in the order given, then
/// `content-length`. Bytes a real client writes are measured at the loopback
/// upstream and compared with a direct exchange, not with this rendering.
///
/// # Errors
/// Refuses a URL the protocol would refuse.
pub fn render_upstream(
    method: &str,
    url: &str,
    headers: &[WireHeader],
    body: &[u8],
) -> Result<Vec<u8>, FixtureError> {
    let (origin, path) = split_upstream_url(url).map_err(|_| FixtureError::Http)?;
    let authority = origin.strip_prefix("https://").ok_or(FixtureError::Http)?;
    let mut out = format!("{method} {path} HTTP/1.1\r\nhost: {authority}\r\n").into_bytes();
    for header in headers {
        out.extend_from_slice(header.name().as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(header.value());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("content-length: {}\r\n\r\n", body.len()).as_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
