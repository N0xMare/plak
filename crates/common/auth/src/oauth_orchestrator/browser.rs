//! Browser loopback accept/read/write orchestrator for subscription OAuth.
//!
//! Ladder: bind → accept → loopback peer → read HTTP/1.1 head → dialect parse →
//! optional preflight → complete callback → write outcome. Dialects own redirect
//! grammar, CORS preflight, crypto, token exchange, and response copy bytes.

use std::future::Future;
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use tokio::net::TcpListener;
#[cfg(test)]
use tokio::net::TcpStream;
use zeroize::{Zeroize as _, Zeroizing};

/// One accepted loopback connection's classified request.
#[derive(Debug)]
#[non_exhaustive]
pub enum LoopbackConnectionEvent<C, P> {
    /// Final authorization callback (success or provider-error query).
    Callback(C),
    /// Dialect-owned preflight (e.g. CORS OPTIONS); session continues.
    Preflight(P),
}

/// Dialect hooks for the shared browser loopback accept session.
pub trait BrowserLoopbackHooks {
    /// Redaction-safe dialect error.
    type Error;
    /// Parsed callback payload (code/state or provider error).
    type Callback;
    /// Parsed preflight payload; unused dialects never produce it.
    type Preflight;
    /// Successful login output.
    type Output;

    /// Absolute deadline for the whole accept session.
    fn deadline(&self) -> Instant;
    /// Maximum connections before fail-closed (1 Codex; 4 xAI).
    fn max_connections(&self) -> usize {
        1
    }
    /// Hard bound on request-head bytes (including terminator).
    fn max_request_bytes(&self) -> usize {
        8 * 1024
    }
    /// Accept/read deadline expiry (`LoginExpired` vs `Callback` is dialect branding).
    fn deadline_error(&self) -> Self::Error;
    /// Peer/grammar/oversized/truncated loopback failure.
    fn callback_error(&self) -> Self::Error;
    /// Parse a complete request head (`local_port` for host:port pin checks).
    ///
    /// # Errors
    ///
    /// Dialect callback/grammar error for any invalid request.
    fn parse_request(
        &mut self,
        request: &[u8],
        local_port: u16,
    ) -> Result<LoopbackConnectionEvent<Self::Callback, Self::Preflight>, Self::Error>;
    /// HTTP response bytes for a preflight (orchestrator writes them).
    fn preflight_response_bytes(&self, preflight: &Self::Preflight) -> Vec<u8>;
    /// HTTP response bytes for callback success/failure (best-effort write).
    fn callback_response_bytes(&self, complete: Result<&Self::Output, &Self::Error>) -> Vec<u8>;
    /// Token exchange / identity / durable install for one callback.
    ///
    /// # Errors
    ///
    /// Correlation, token, identity, or store failure.
    fn complete_callback(
        &mut self,
        callback: Self::Callback,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}

/// Bind the first free `127.0.0.1` port from `ports`.
///
/// # Errors
///
/// The last bind failure when every candidate fails to bind, or
/// [`std::io::ErrorKind::AddrNotAvailable`] when `ports` is empty.
pub async fn bind_loopback_listener(ports: &[u16]) -> std::io::Result<(TcpListener, u16)> {
    let mut last = std::io::Error::from(std::io::ErrorKind::AddrNotAvailable);
    for port in ports {
        match TcpListener::bind(("127.0.0.1", *port)).await {
            Ok(listener) => {
                let bound = listener.local_addr()?.port();
                return Ok((listener, bound));
            }
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// Failure while reading a loopback request head.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReadRequestHeadError {
    /// Absolute session deadline elapsed.
    Deadline,
    /// Truncated stream, oversize, body after headers, or missing terminator.
    Invalid,
}

/// Read until HTTP/1.1 header terminator with a hard byte bound and deadline.
///
/// Returns only the header block (through `\r\n\r\n`). Extra body bytes fail.
///
/// # Errors
///
/// [`ReadRequestHeadError::Deadline`] or [`ReadRequestHeadError::Invalid`].
pub async fn read_http11_request_head(
    stream: &mut (impl AsyncRead + Unpin),
    max_bytes: usize,
    deadline: Instant,
) -> Result<Zeroizing<Vec<u8>>, ReadRequestHeadError> {
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024.min(max_bytes)));
    loop {
        if bytes.len() >= max_bytes {
            return Err(ReadRequestHeadError::Invalid);
        }
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or(ReadRequestHeadError::Deadline)?;
        let mut chunk = [0_u8; 512];
        let read_limit = (max_bytes - bytes.len()).min(chunk.len());
        let read = tokio::time::timeout(remaining, stream.read(&mut chunk[..read_limit]))
            .await
            .map_err(|_| ReadRequestHeadError::Deadline)?
            .map_err(|_| ReadRequestHeadError::Invalid)?;
        if read == 0 {
            return Err(ReadRequestHeadError::Invalid);
        }
        bytes.extend_from_slice(&chunk[..read]);
        chunk[..read].zeroize();
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            if bytes.len() != header_end {
                return Err(ReadRequestHeadError::Invalid);
            }
            return Ok(bytes);
        }
    }
}

/// Write all bytes before `deadline`, then shut the write half when possible.
///
/// # Errors
///
/// [`std::io::ErrorKind::TimedOut`] on deadline, otherwise the transport
/// failure.
pub async fn write_all_before_deadline(
    stream: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let timed_out = || std::io::Error::from(std::io::ErrorKind::TimedOut);
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(timed_out)?;
    tokio::time::timeout(remaining, async {
        stream.write_all(bytes).await?;
        stream.shutdown().await
    })
    .await
    .map_err(|_| timed_out())?
}

/// Run the shared loopback accept session until one callback completes.
///
/// # Errors
///
/// Propagates dialect errors. Transport/deadline map via hooks.
pub async fn run_browser_loopback_session<H>(
    listener: TcpListener,
    hooks: &mut H,
) -> Result<H::Output, H::Error>
where
    H: BrowserLoopbackHooks,
{
    let local_port = listener
        .local_addr()
        .map_err(|_| hooks.callback_error())?
        .port();
    let max_connections = hooks.max_connections().max(1);
    for _ in 0..max_connections {
        let deadline = hooks.deadline();
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| hooks.deadline_error())?;
        let (mut stream, peer) = tokio::time::timeout(remaining, listener.accept())
            .await
            .map_err(|_| hooks.deadline_error())?
            .map_err(|_| hooks.callback_error())?;
        if !peer.ip().is_loopback() {
            return Err(hooks.callback_error());
        }
        let request = match read_http11_request_head(
            &mut stream,
            hooks.max_request_bytes(),
            hooks.deadline(),
        )
        .await
        {
            Ok(request) => request,
            Err(kind) => {
                let err = match kind {
                    ReadRequestHeadError::Deadline => hooks.deadline_error(),
                    ReadRequestHeadError::Invalid => hooks.callback_error(),
                };
                let page = hooks.callback_response_bytes(Err(&err));
                let _ = write_all_before_deadline(&mut stream, &page, hooks.deadline()).await;
                return Err(err);
            }
        };
        match hooks.parse_request(&request, local_port) {
            Ok(LoopbackConnectionEvent::Preflight(preflight)) => {
                let page = hooks.preflight_response_bytes(&preflight);
                write_all_before_deadline(&mut stream, &page, hooks.deadline())
                    .await
                    .map_err(|_| hooks.callback_error())?;
            }
            Ok(LoopbackConnectionEvent::Callback(callback)) => {
                let outcome = hooks.complete_callback(callback).await;
                let page = match &outcome {
                    Ok(output) => hooks.callback_response_bytes(Ok(output)),
                    Err(error) => hooks.callback_response_bytes(Err(error)),
                };
                let _ = write_all_before_deadline(&mut stream, &page, hooks.deadline()).await;
                return outcome;
            }
            Err(error) => {
                let page = hooks.callback_response_bytes(Err(&error));
                let _ = write_all_before_deadline(&mut stream, &page, hooks.deadline()).await;
                return Err(error);
            }
        }
    }
    Err(hooks.callback_error())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn bind_loopback_uses_first_free_port() {
        let reserved = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let reserved_port = reserved.local_addr().unwrap().port();
        let free = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let free_port = free.local_addr().unwrap().port();
        drop(free);
        let (listener, bound) = bind_loopback_listener(&[reserved_port, free_port])
            .await
            .expect("second port free");
        assert_eq!(bound, free_port);
        drop(listener);
        drop(reserved);
    }

    #[tokio::test]
    async fn read_head_rejects_body_after_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(b"GET /x HTTP/1.1\r\nHost: localhost\r\n\r\nbody")
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let result = read_http11_request_head(
            &mut stream,
            8 * 1024,
            Instant::now() + Duration::from_secs(2),
        )
        .await;
        assert_eq!(result, Err(ReadRequestHeadError::Invalid));
        let _ = client.await;
    }

    #[tokio::test]
    async fn read_head_accepts_exact_headers() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            stream
                .write_all(b"GET /callback?code=c&state=s HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = read_http11_request_head(
            &mut stream,
            8 * 1024,
            Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect("headers");
        assert!(head.ends_with(b"\r\n\r\n"));
        assert!(
            std::str::from_utf8(&head)
                .unwrap()
                .contains("GET /callback")
        );
        let _ = client.await;
    }
}
