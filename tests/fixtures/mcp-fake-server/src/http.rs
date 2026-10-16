//! Streamable HTTP serving of the scripted server on a loopback port, behind
//! a credential gate that records every request it sees.

use std::convert::Infallible;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::FakeServer;

/// The endpoint path the fixture serves.
pub const PATH: &str = "/mcp";

/// Credentials the gate requires before a request reaches the server.
#[derive(Clone, Debug, Default)]
pub struct HttpGate {
    /// `Authorization: Bearer <token>` every request must carry.
    pub bearer: Option<String>,
    /// A header name and value every request must carry.
    pub header: Option<(String, String)>,
}

/// One request the gate saw.
#[derive(Clone, Debug)]
pub struct SeenRequest {
    /// The HTTP method.
    pub method: String,
    /// Every header, lowercase name and lossy value.
    pub headers: Vec<(String, String)>,
    /// The status the gate answered without the server, if it refused.
    pub refused: Option<u16>,
    /// The `Mcp-Session-Id` the server issued in its response, if any.
    pub issued_session: Option<String>,
}

impl SeenRequest {
    /// The value of header `name`, if present.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(found, _)| found == name)
            .map(|(_, value)| value.as_str())
    }
}

/// What the HTTP gate observed, shared with the test.
#[derive(Debug, Default)]
pub struct HttpObserved {
    requests: Mutex<Vec<SeenRequest>>,
    changed: Notify,
}

impl HttpObserved {
    /// Requests in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<SeenRequest> {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits up to `bound` until `done` holds for the observed requests.
    pub async fn wait_for(&self, bound: Duration, done: impl Fn(&[SeenRequest]) -> bool) -> bool {
        tokio::time::timeout(bound, async {
            loop {
                let notified = self.changed.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if done(&self.requests()) {
                    return;
                }
                notified.await;
            }
        })
        .await
        .is_ok()
    }

    fn record(&self, request: SeenRequest) {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        self.changed.notify_waiters();
    }
}

/// A running loopback HTTP fixture; it stops when dropped.
pub struct HttpFixture {
    address: SocketAddr,
    observed: Arc<HttpObserved>,
    revoked: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl HttpFixture {
    /// The endpoint URL.
    #[must_use]
    pub fn url(&self) -> String {
        format!("http://{}{PATH}", self.address)
    }

    /// The gate's observations.
    #[must_use]
    pub fn observed(&self) -> Arc<HttpObserved> {
        Arc::clone(&self.observed)
    }

    /// Refuses every later request with 401, as a revoked token would be.
    pub fn revoke_credentials(&self) {
        self.revoked.store(true, Ordering::Release);
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

type Body = BoxBody<Bytes, Infallible>;

/// Serves `server` over streamable HTTP on `127.0.0.1` behind `gate`.
///
/// # Errors
///
/// Returns the bind error.
pub async fn serve_http(server: FakeServer, gate: HttpGate) -> io::Result<HttpFixture> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let address = listener.local_addr()?;
    let observed = Arc::new(HttpObserved::default());
    let revoked = Arc::new(AtomicBool::new(false));
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let state = Arc::new(GateState {
        gate,
        observed: Arc::clone(&observed),
        revoked: Arc::clone(&revoked),
        service,
    });
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                let handler = service_fn(move |request| {
                    let state = Arc::clone(&state);
                    async move { Ok::<_, Infallible>(state.handle(request).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), handler)
                    .await;
            });
        }
    });
    Ok(HttpFixture {
        address,
        observed,
        revoked,
        task,
    })
}

struct GateState {
    gate: HttpGate,
    observed: Arc<HttpObserved>,
    revoked: Arc<AtomicBool>,
    service: StreamableHttpService<FakeServer, LocalSessionManager>,
}

impl GateState {
    async fn handle(&self, request: Request<Incoming>) -> Response<Body> {
        let mut seen = SeenRequest {
            method: request.method().to_string(),
            headers: request
                .headers()
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        String::from_utf8_lossy(value.as_bytes()).into_owned(),
                    )
                })
                .collect(),
            refused: None,
            issued_session: None,
        };
        let bearer_ok = self.gate.bearer.as_ref().is_none_or(|token| {
            seen.header("authorization") == Some(format!("Bearer {token}").as_str())
        });
        let header_ok = self
            .gate
            .header
            .as_ref()
            .is_none_or(|(name, value)| seen.header(name) == Some(value.as_str()));
        if self.revoked.load(Ordering::Acquire) || !bearer_ok || !header_ok {
            seen.refused = Some(401);
            self.observed.record(seen);
            let mut response =
                Response::new(Full::new(Bytes::from_static(b"unauthorized")).boxed());
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            response.headers_mut().insert(
                hyper::header::WWW_AUTHENTICATE,
                hyper::header::HeaderValue::from_static("Bearer"),
            );
            return response;
        }
        let response = self.service.handle(request).await;
        seen.issued_session = response
            .headers()
            .get("mcp-session-id")
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        self.observed.record(seen);
        response
    }
}
