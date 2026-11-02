//! Executor-neutral runtime interfaces: spawning, sleeping, monotonic time and
//! one HTTP exchange.
//!
//! Wall-clock time is [`Clock`](crate::Clock). The `plakous` facade provides
//! the native implementations; a host on another executor or target supplies
//! its own.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures_core::Stream;

/// `Send` on native targets, where runtimes move tasks between threads; no
/// bound on `wasm32`, whose single-threaded hosts hold `!Send` handles.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSend: Send {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Send + ?Sized> MaybeSend for T {}
/// `Send` on native targets, where runtimes move tasks between threads; no
/// bound on `wasm32`, whose single-threaded hosts hold `!Send` handles.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSend {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSend for T {}

/// `Sync` on native targets; no bound on `wasm32`.
#[cfg(not(target_arch = "wasm32"))]
pub trait MaybeSync: Sync {}
#[cfg(not(target_arch = "wasm32"))]
impl<T: Sync + ?Sized> MaybeSync for T {}
/// `Sync` on native targets; no bound on `wasm32`.
#[cfg(target_arch = "wasm32")]
pub trait MaybeSync {}
#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> MaybeSync for T {}

/// A boxed future that is `Send` exactly where [`MaybeSend`] requires it.
#[cfg(not(target_arch = "wasm32"))]
pub type MaybeSendFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;
/// A boxed future that is `Send` exactly where [`MaybeSend`] requires it.
#[cfg(target_arch = "wasm32")]
pub type MaybeSendFuture<T> = Pin<Box<dyn Future<Output = T> + 'static>>;

/// A task a [`Runtime`] drives to completion.
pub type RuntimeTask = MaybeSendFuture<()>;

/// A future that completes when a [`Runtime::sleep`] has elapsed.
pub type RuntimeSleep = MaybeSendFuture<()>;

/// Control over one task a [`Runtime`] spawned. Dropping it detaches the task.
pub trait SpawnedTask: MaybeSend + MaybeSync {
    /// Requests that the task stop. A task that has not finished is dropped
    /// at its next suspension point.
    fn abort(&self);

    /// Whether the task has finished, by completing or by being aborted.
    fn is_finished(&self) -> bool;
}

/// How Plakous code starts tasks, waits and reads monotonic time without
/// naming an executor.
pub trait Runtime: MaybeSend + MaybeSync + 'static {
    /// Starts `task` concurrently with the caller.
    fn spawn(&self, task: RuntimeTask) -> Box<dyn SpawnedTask>;

    /// A future that completes once `duration` has elapsed.
    fn sleep(&self, duration: Duration) -> RuntimeSleep;

    /// Monotonic time elapsed since this runtime's origin. It never decreases
    /// and has no relation to wall-clock time.
    fn now(&self) -> Duration;
}

/// One outgoing HTTP request for an [`HttpTransport`].
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    method: String,
    url: String,
    headers: Vec<(String, Vec<u8>)>,
    body: Vec<u8>,
}

impl HttpRequest {
    /// A request with no headers and an empty body. `url` is absolute.
    #[must_use]
    pub fn new(method: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            method: method.into(),
            url: url.into(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Appends one header; repeated names are sent in order.
    #[must_use]
    pub fn header(mut self, name: impl Into<String>, value: impl Into<Vec<u8>>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replaces the body.
    #[must_use]
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// The request method, for example `GET`.
    #[must_use]
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The absolute request URL.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The headers, in the order they were added.
    #[must_use]
    pub fn headers(&self) -> &[(String, Vec<u8>)] {
        &self.headers
    }

    /// The request body.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }
}

impl fmt::Debug for HttpRequest {
    // Header values and bodies can carry credentials; only their shape prints.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("headers", &self.headers.len())
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// A pull-driven response body: chunks in order, then the end of the stream.
/// Dropping it before the end abandons the rest of the exchange.
///
/// Chunks are owned `Vec<u8>`; a shared-buffer type would save a copy per
/// chunk and is left until a transport needs it.
#[cfg(not(target_arch = "wasm32"))]
pub type HttpBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>> + Send>>;
/// A pull-driven response body: chunks in order, then the end of the stream.
/// Dropping it before the end abandons the rest of the exchange.
#[cfg(target_arch = "wasm32")]
pub type HttpBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpTransportError>>>>;

/// A response head and its pull-driven body.
pub struct HttpResponse {
    status: u16,
    headers: Vec<(String, Vec<u8>)>,
    body: HttpBody,
}

impl HttpResponse {
    /// A response from its parts.
    #[must_use]
    pub fn new(status: u16, headers: Vec<(String, Vec<u8>)>, body: HttpBody) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// The wire status code.
    #[must_use]
    pub const fn status(&self) -> u16 {
        self.status
    }

    /// The response headers, in wire order.
    #[must_use]
    pub fn headers(&self) -> &[(String, Vec<u8>)] {
        &self.headers
    }

    /// Consumes the response and returns its body.
    pub fn into_body(self) -> HttpBody {
        self.body
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers.len())
            .finish_non_exhaustive()
    }
}

/// Where an HTTP exchange failed.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HttpTransportErrorKind {
    /// The request was refused before anything was sent.
    InvalidRequest,
    /// The exchange failed before a response head arrived.
    Exchange,
    /// The response body failed after its head arrived.
    Body,
}

/// A redaction-safe HTTP transport failure.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct HttpTransportError {
    kind: HttpTransportErrorKind,
    message: String,
}

impl HttpTransportError {
    /// A failure of `kind`. `message` must carry no request content.
    #[must_use]
    pub fn new(kind: HttpTransportErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Where the exchange failed.
    #[must_use]
    pub const fn kind(&self) -> HttpTransportErrorKind {
        self.kind
    }
}

/// The response future of [`HttpTransport::send`].
pub type HttpResponseFuture = MaybeSendFuture<Result<HttpResponse, HttpTransportError>>;

/// Sends one HTTP request and returns its response head and body.
///
/// A transport has no redirect, retry or authentication semantics of its own.
pub trait HttpTransport: MaybeSend + MaybeSync + 'static {
    /// Starts one exchange. Dropping the returned future cancels it: a
    /// request not yet sent is never sent, and one in flight is abandoned
    /// with no response delivered; whether the server acted on it is unknown.
    fn send(&self, request: HttpRequest) -> HttpResponseFuture;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_debug_never_prints_header_values_or_body() {
        let request = HttpRequest::new("POST", "https://example.test/v1")
            .header("authorization", "Bearer secret-token")
            .body("secret-body");
        let printed = format!("{request:?}");
        assert!(!printed.contains("secret"));
        assert!(printed.contains("POST"));
        assert_eq!(request.headers().len(), 1);
        assert_eq!(request.body_bytes(), b"secret-body");
        assert_eq!(request.url(), "https://example.test/v1");
    }
}
