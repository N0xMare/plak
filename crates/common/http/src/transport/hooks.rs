//! Exchange paths other than a client's own connections: a credential
//! gateway relay takes whole exchanges, dials its hop's connections and
//! supplies relayed bodies through these hooks.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};

use http::{HeaderMap, Version};

use super::response::{content_length, validate_response_headers};
use super::{BodySource, ConnectionTask, ResponseBody};
use crate::{
    ErrorKind, ExchangeControl, Failure, Headers, LimitKind, Limits, Phase, Request, Response,
    StatusCode, TransmissionFact, WireProtocol,
};

/// A boxed hook future.
pub(crate) type HookFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Takes a client's whole exchange instead of its own connections.
pub(crate) trait Relay: Send + Sync {
    /// Performs the exchange under the client's `limits`, applying the
    /// client's gateway-hop `faults` to each relay request it sends.
    fn execute<'a>(
        &'a self,
        limits: &'a Limits,
        request: Request,
        control: ExchangeControl,
        faults: super::test_hop::HopFaults,
    ) -> HookFuture<'a, Result<Response, Failure>>;
}

/// A byte stream a [`Dialer`] opened.
pub(crate) trait DialedIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> DialedIo for T {}

/// Opens a client's fresh connections instead of resolving its origin; the
/// connection carries plain HTTP/1.1.
pub(crate) trait Dialer: Send + Sync {
    /// Opens one connection.
    fn dial(&self) -> HookFuture<'static, Result<Box<dyn DialedIo>, Failure>>;
}

/// Supplies a response body's frames.
pub(crate) trait FrameSource: Send {
    /// The next frame, or `None` after a clean end. Cancel-safe.
    fn frame(&mut self) -> HookFuture<'_, Option<Result<http_body::Frame<Bytes>, ErrorKind>>>;
}

/// A response another exchange path produced (a credential gateway relay):
/// its head, and a body read from `source`. A malformed head is a response
/// failure after possible transmission.
#[allow(
    dead_code,
    reason = "the credential gateway relay client builds its responses here"
)]
pub(crate) fn hooked_response(
    status: StatusCode,
    headers: HeaderMap,
    source: Box<dyn FrameSource>,
    control: ExchangeControl,
    limits: &Limits,
) -> Result<Response, Failure> {
    let failure = |kind| {
        Failure::new(
            Phase::ResponseHead,
            TransmissionFact::MayHaveTransmitted,
            kind,
        )
    };
    validate_response_headers(&headers, limits).map_err(failure)?;
    if status == StatusCode::SWITCHING_PROTOCOLS {
        return Err(failure(ErrorKind::InvalidResponse));
    }
    if content_length(&headers).is_some_and(|length| {
        length > u64::try_from(limits.max_response_body_bytes).unwrap_or(u64::MAX)
    }) {
        return Err(failure(ErrorKind::LimitExceeded(
            LimitKind::ResponseBodyBytes,
        )));
    }
    Ok(Response {
        status,
        version: Version::HTTP_11,
        headers: Headers::new(headers),
        connection: None,
        body: ResponseBody::fresh(
            BodySource::Hooked(source),
            ConnectionTask::closed(),
            control,
            WireProtocol::Http1,
            None,
            None,
            limits,
        ),
    })
}
