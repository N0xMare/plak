//! A protocol-exact credential gateway for tests.
//!
//! It speaks HTTP/1.1 over whatever duplex the test supplies and uses
//! `plakous-gateway-protocol` for every envelope, frame and refusal. The
//! shared relay server core is not in this tree yet, so this module is the
//! scripted authority and the carrier that core will own. It does not
//! reimplement framing. A scripted upstream is recorded only after the whole
//! request body has been read.

mod serve;

#[cfg(test)]
mod durability;
#[cfg(test)]
mod tests;

pub use serve::{ServeReport, serve_one};

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plakous_gateway_protocol::UuidText;
use plakous_gateway_protocol::envelope::{RelayEnvelope, split_upstream_url};
use plakous_gateway_protocol::facts::AccountFacts;
use plakous_gateway_protocol::header::{RequestHeader, WireHeader};
use plakous_gateway_protocol::refusal::GatewayRefusal;

/// Why the fixture refused to finish an exchange. It never echoes a header
/// value or a request body.
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// The duplex read or write failed.
    #[error("fixture gateway connection failed")]
    Io(#[source] std::io::Error),
    /// The caller closed the request before `content-length` bytes arrived.
    #[error("fixture gateway request ended before the body")]
    Incomplete,
    /// The bytes were not one HTTP/1.1 request the fixture can carry.
    #[error("fixture gateway request is not valid HTTP")]
    Http,
    /// A custodied entry named a value the fixture was not given.
    #[error("custodied header has no scripted value")]
    MissingCredential,
    /// A scripted upstream reply could not be encoded as relay frames.
    #[error("scripted upstream reply could not be framed")]
    Frame,
    /// A scripted instance id was not a canonical UUID.
    #[error("scripted gateway instance is not a UUID")]
    Instance,
}

/// What one fully read request should do. The default, when nothing is
/// queued, relays a small `200` and dials once.
#[derive(Clone, Debug)]
pub enum GatewayScript {
    /// Substitute custodied headers, record the upstream request, and answer
    /// with relay frames. A facts, refresh or admin call answers in JSON and
    /// does not dial.
    Relay(UpstreamReply),
    /// A gateway refusal. Nothing is dialed.
    Refuse(GatewayRefusal),
    /// Close after the full read, with no response and no dial.
    DropBeforeResponse,
    /// Dial, write a relay `Head`, then drop the connection.
    DropAfterHead,
    /// Wait, still without dialing. A dropped caller ends the wait.
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

/// One upstream request the fixture recorded, after custodied substitution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedUpstream {
    /// The upstream method.
    pub method: String,
    /// The absolute upstream URL.
    pub url: String,
    /// Headers in envelope order, with credential bytes filled in.
    pub headers: Vec<WireHeader>,
    /// The upstream body.
    pub body: Vec<u8>,
    /// The request [`render_upstream`] produces.
    pub http: Vec<u8>,
}

struct Instance {
    id: UuidText,
    n: u64,
}

struct Inner {
    instance: Mutex<Instance>,
    dials: AtomicU64,
    relay_ordinal: AtomicU64,
    credentials: Vec<(String, Vec<u8>)>,
    key: Option<String>,
    script: Mutex<VecDeque<GatewayScript>>,
    fence: Mutex<Option<(u64, AccountFacts)>>,
    captured: Mutex<Vec<CapturedUpstream>>,
    facts: Mutex<Option<AccountFacts>>,
}

/// An in-process gateway. Clones share the script, the instance and the dial
/// counter.
#[derive(Clone)]
pub struct FixtureGateway {
    inner: Arc<Inner>,
}

impl FixtureGateway {
    /// A gateway with `credentials` substituted for custodied entries of the
    /// same name. `key` is the required `plak-gateway-key`, or none for a
    /// same-UID local administrator.
    ///
    /// # Errors
    /// Refuses an instance id that is not a canonical UUID. The initial id is
    /// fixed, so this does not fail for the built-in value.
    pub fn new(
        credentials: Vec<(String, Vec<u8>)>,
        key: Option<String>,
    ) -> Result<Self, FixtureError> {
        Ok(Self {
            inner: Arc::new(Inner {
                instance: Mutex::new(Instance {
                    id: instance_at(1)?,
                    n: 1,
                }),
                dials: AtomicU64::new(0),
                relay_ordinal: AtomicU64::new(0),
                credentials,
                key,
                script: Mutex::new(VecDeque::new()),
                fence: Mutex::new(None),
                captured: Mutex::new(Vec::new()),
                facts: Mutex::new(None),
            }),
        })
    }

    /// Queues one action. Requests consume the queue in order.
    pub fn push(&self, action: GatewayScript) {
        Self::lock(&self.inner.script).push_back(action);
    }

    /// On the n-th decoded relay, answer `fence_moved` and do not dial.
    pub fn script_fence_moved(&self, nth: u64, facts: AccountFacts) {
        *Self::lock(&self.inner.fence) = Some((nth, facts));
    }

    /// Facts returned by `/facts` and `/refresh`.
    pub fn set_facts(&self, facts: Option<AccountFacts>) {
        *Self::lock(&self.inner.facts) = facts;
    }

    /// Replaces the instance id. A restart does not clear the dial counter.
    ///
    /// # Errors
    /// Refuses an id that is not a canonical UUID.
    pub fn restart(&self) -> Result<UuidText, FixtureError> {
        let mut instance = Self::lock(&self.inner.instance);
        instance.n += 1;
        instance.id = instance_at(instance.n)?;
        Ok(instance.id.clone())
    }

    /// The instance id `/versions` currently reports.
    #[must_use]
    pub fn instance(&self) -> UuidText {
        Self::lock(&self.inner.instance).id.clone()
    }

    /// Upstream dials. A refusal, a drop before the response, a stall and an
    /// incomplete body do not increment it.
    #[must_use]
    pub fn dials(&self) -> u64 {
        self.inner.dials.load(Ordering::Relaxed)
    }

    /// Upstream requests recorded at dial, in order.
    #[must_use]
    pub fn captured(&self) -> Vec<CapturedUpstream> {
        Self::lock(&self.inner.captured).clone()
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
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

/// The HTTP/1.1 request a fixture upstream observes.
///
/// `host` comes from the URL, then the headers in the order given, then
/// `content-length`. That is the order both the relay path and a direct
/// local-custody rendering must share.
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

pub(crate) fn dial(
    gateway: &FixtureGateway,
    envelope: &RelayEnvelope,
    body: &[u8],
) -> Result<(), FixtureError> {
    let headers = substitute(&envelope.headers, &gateway.inner.credentials)?;
    let http = render_upstream(&envelope.method, &envelope.url, &headers, body)?;
    gateway.inner.dials.fetch_add(1, Ordering::Relaxed);
    FixtureGateway::lock(&gateway.inner.captured).push(CapturedUpstream {
        method: envelope.method.clone(),
        url: envelope.url.clone(),
        headers,
        body: body.to_vec(),
        http,
    });
    Ok(())
}
