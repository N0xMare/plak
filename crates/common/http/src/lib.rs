//! Provider-blind, bounded HTTP transport and SSE framing.
//!
//! Each [`Client::execute`] call submits exactly one HTTP request to an HTTPS
//! origin or a plain-HTTP loopback origin ([`Origin::loopback_http`]), on a fresh
//! DNS/TCP/TLS connection or, for a [`Client::pooled`] client, on an idle one
//! a clean earlier exchange left. The crate deliberately has no redirect,
//! proxy, decompression, retry, authentication, provider, model, or JSON
//! semantics; the one exception is that a reused connection that fails before
//! accepting any request byte is replaced once by a fresh one. An optional
//! [`AddressPolicy`] limits which resolved addresses a client connects to. [`sse`]
//! provides HTML5 Server-Sent Events **framing** only
//! (byte stream → complete messages); it does not interpret provider event
//! types, sequences, or `[DONE]` markers.

#![forbid(unsafe_code)]

mod address;
mod control;
mod error;
mod observation;
mod request;
mod sensitive;
pub mod sse;
mod transport;

/// Whether provider advice forbids replay; `true` never grants permission.
#[must_use]
pub fn retry_disabled(headers: &Headers) -> bool {
    headers.iter().any(|(name, value)| {
        name.as_str() == "x-should-retry"
            && value
                .to_str()
                .is_ok_and(|value| value.trim().eq_ignore_ascii_case("false"))
    })
}

pub use address::AddressPolicy;
pub use bytes::Bytes;
pub use control::{CancellationToken, ExchangeControl};
pub use error::{
    ConfigError, ConnectionUse, ErrorKind, Failure, LimitKind, Phase, RequestError, TlsCause,
    TransmissionFact, WireProtocol,
};
pub use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Version};
pub use observation::{ExchangeObservation, ExchangeObserver, ExchangeTerminal};
pub use request::{Headers, Origin, Request};
pub use sensitive::{SensitiveBytes, sensitive_header_value};
pub use sse::{SseDecoder, SseError, SseLimits, SseMessage};
#[cfg(feature = "fault-injection")]
pub use transport::fault::{FAULTS_ENV, FaultPlan, FaultSpecError};
pub use transport::{
    Client, Limits, Response, ResponseBody, STREAMING_IDLE_TIMEOUT, TlsConfigIdentity,
    TransportBehaviorIdentity, TransportPolicy,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_retry_advice_only_tightens() -> Result<(), http::header::InvalidHeaderValue> {
        for (value, disabled) in [
            ("false", true),
            (" FALSE ", true),
            ("true", false),
            ("unknown", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                HeaderName::from_static("x-should-retry"),
                HeaderValue::from_str(value)?,
            );
            assert_eq!(retry_disabled(&Headers::new(headers)), disabled);
        }
        Ok(())
    }
}
