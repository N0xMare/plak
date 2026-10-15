use std::fmt;

use thiserror::Error;

/// The transport phase in which an exchange failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Phase {
    /// Hostname resolution.
    Resolution,
    /// TCP connection establishment.
    Connect,
    /// TLS or negotiated HTTP connection establishment.
    Tls,
    /// Request head or body transmission.
    RequestWrite,
    /// Request transmission or response-head wait when the exact boundary is
    /// not observable.
    RequestWriteOrResponseHead,
    /// Waiting for or parsing the response head.
    ResponseHead,
    /// Pulling or validating response body bytes.
    ResponseBody,
    /// Stopping and joining connection work.
    Cleanup,
}

/// Whether request bytes could have reached the peer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransmissionFact {
    /// No HTTP request bytes were submitted to the connection.
    NotStarted,
    /// Request bytes may have reached the peer, but the peer cannot hold the
    /// complete request: bytes remained local, or an HTTP/1.1 peer rejected a
    /// TLS record with `bad_record_mac` before any response byte. This is as replay-safe as
    /// [`TransmissionFact::NotStarted`].
    RequestIncomplete,
    /// The whole request may have reached the peer.
    MayHaveTransmitted,
}

impl TransmissionFact {
    /// Whether the peer cannot have received a complete request, so a replay
    /// cannot duplicate any effect or charge.
    #[must_use]
    pub const fn peer_cannot_have_acted(self) -> bool {
        matches!(self, Self::NotStarted | Self::RequestIncomplete)
    }
}

/// The configured bound that was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LimitKind {
    /// Number of request headers.
    RequestHeaderCount,
    /// Encoded request header bytes.
    RequestHeaderBytes,
    /// Request body bytes.
    RequestBodyBytes,
    /// Number of response headers or trailers.
    ResponseHeaderCount,
    /// Encoded response header or trailer bytes.
    ResponseHeaderBytes,
    /// One response data frame.
    ResponseChunkBytes,
    /// Total response body bytes.
    ResponseBodyBytes,
}

/// A stable, non-sensitive classification of a transport failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// Caller cancellation was observed.
    Cancelled,
    /// An absolute or phase deadline elapsed.
    DeadlineExceeded,
    /// A configured resource limit was exceeded.
    LimitExceeded(LimitKind),
    /// DNS resolution failed or returned no address.
    Resolution,
    /// The client's [`AddressPolicy`](crate::AddressPolicy) refused the origin
    /// host name or every address it resolved to; nothing was connected.
    AddressRefused,
    /// TCP I/O failed.
    Io,
    /// TLS negotiation or peer verification failed.
    Tls,
    /// HTTP framing or protocol processing failed.
    Protocol,
    /// A peer response was structurally unsupported.
    InvalidResponse,
    /// An owned transport task failed unexpectedly.
    Internal,
}

/// The TLS-layer error behind an I/O failure, when Rustls reported one.
///
/// A closed, non-secret category: never key material, certificate bytes,
/// peer text or the library's message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TlsCause {
    /// The peer sent a fatal alert; the numeric TLS alert description.
    AlertReceived(u8),
    /// A record failed to decrypt or authenticate.
    DecryptError,
    /// A record or handshake message could not be parsed.
    InvalidMessage,
    /// A message arrived that the protocol state did not permit.
    InappropriateMessage,
    /// The peer violated the protocol in a way Rustls names.
    PeerMisbehaved,
    /// The peer lacks a capability this client requires.
    PeerIncompatible,
    /// The peer certificate was rejected.
    InvalidCertificate,
    /// The peer sent a record larger than TLS permits.
    PeerSentOversizedRecord,
    /// Any other Rustls error.
    Other,
}

impl TlsCause {
    fn from_rustls(error: &rustls::Error) -> Self {
        match error {
            rustls::Error::AlertReceived(alert) => Self::AlertReceived(u8::from(*alert)),
            rustls::Error::DecryptError => Self::DecryptError,
            rustls::Error::InvalidMessage(_) => Self::InvalidMessage,
            rustls::Error::InappropriateMessage { .. }
            | rustls::Error::InappropriateHandshakeMessage { .. } => Self::InappropriateMessage,
            rustls::Error::PeerMisbehaved(_) => Self::PeerMisbehaved,
            rustls::Error::PeerIncompatible(_) => Self::PeerIncompatible,
            rustls::Error::InvalidCertificate(_) => Self::InvalidCertificate,
            rustls::Error::PeerSentOversizedRecord => Self::PeerSentOversizedRecord,
            _ => Self::Other,
        }
    }

    /// A stable label for logs.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::AlertReceived(_) => "tls_alert_received",
            Self::DecryptError => "tls_decrypt_error",
            Self::InvalidMessage => "tls_invalid_message",
            Self::InappropriateMessage => "tls_inappropriate_message",
            Self::PeerMisbehaved => "tls_peer_misbehaved",
            Self::PeerIncompatible => "tls_peer_incompatible",
            Self::InvalidCertificate => "tls_invalid_certificate",
            Self::PeerSentOversizedRecord => "tls_peer_oversized_record",
            Self::Other => "tls_other",
        }
    }

    /// The received alert description code, for [`Self::AlertReceived`].
    #[must_use]
    pub const fn alert(self) -> Option<u8> {
        match self {
            Self::AlertReceived(code) => Some(code),
            _ => None,
        }
    }
}

/// The application protocol ALPN selected for a failed exchange's connection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WireProtocol {
    /// HTTP/1.1, negotiated or the default without ALPN.
    Http1,
    /// HTTP/2.
    Http2,
}

impl WireProtocol {
    /// The ALPN protocol id.
    #[must_use]
    pub const fn alpn(self) -> &'static str {
        match self {
            Self::Http1 => "http/1.1",
            Self::Http2 => "h2",
        }
    }
}

/// Whether an exchange ran on a connection opened for it or on one a pooled
/// client kept from an earlier exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionUse {
    /// A connection opened for this exchange.
    Fresh,
    /// An idle connection kept from an earlier, cleanly finished exchange.
    Reused,
    /// A connection opened for this exchange after a reused one failed
    /// with its TLS stream having accepted no byte of the request, so the
    /// peer can hold at most an incomplete prefix of it.
    ReplacedStale,
}

/// A redacted physical-exchange failure.
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct Failure {
    phase: Phase,
    transmission: TransmissionFact,
    kind: ErrorKind,
    cause: Option<&'static str>,
    http2_reason: Option<u32>,
    os_error: Option<i32>,
    tls: Option<TlsCause>,
    protocol: Option<WireProtocol>,
    http2_io: bool,
    connection: Option<ConnectionUse>,
    idle_age: Option<std::time::Duration>,
}

impl Failure {
    pub(crate) const fn new(phase: Phase, transmission: TransmissionFact, kind: ErrorKind) -> Self {
        Self {
            phase,
            transmission,
            kind,
            cause: None,
            http2_reason: None,
            os_error: None,
            tls: None,
            protocol: None,
            http2_io: false,
            connection: None,
            idle_age: None,
        }
    }

    pub(crate) fn with_protocol_cause(mut self, error: &hyper::Error) -> Self {
        use std::error::Error as _;
        let mut source = error.source();
        while let Some(value) = source {
            if let Some(io) = value.downcast_ref::<std::io::Error>() {
                return self.with_io_cause(io);
            }
            if let Some(h2) = value.downcast_ref::<h2::Error>() {
                if let Some(io) = h2.get_io() {
                    self.http2_io = true;
                    return self.with_io_cause(io);
                }
                self.cause = Some(match (h2.is_remote(), h2.is_reset(), h2.is_go_away()) {
                    (true, true, _) => "http2_remote_reset",
                    (true, _, true) => "http2_remote_goaway",
                    (false, true, _) => "http2_local_reset",
                    (false, _, true) => "http2_local_goaway",
                    _ => "http2_protocol",
                });
                self.http2_reason = h2.reason().map(u32::from);
                return self;
            }
            source = value.source();
        }
        self.cause = Some(hyper_cause(error));
        self
    }

    /// Records the I/O kind, OS error number and any Rustls error behind `io`.
    pub(crate) fn with_io_cause(mut self, io: &std::io::Error) -> Self {
        self.cause = Some(io_cause(io.kind()));
        self.os_error = io.raw_os_error();
        self.tls = io
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
            .map(TlsCause::from_rustls);
        self
    }

    pub(crate) const fn with_peer_rejected_record(mut self) -> Self {
        self.transmission = TransmissionFact::RequestIncomplete;
        self.cause = Some("peer_rejected_record");
        self.tls = Some(TlsCause::AlertReceived(20));
        self
    }

    pub(crate) const fn with_wire_protocol(mut self, protocol: WireProtocol) -> Self {
        self.protocol = Some(protocol);
        self
    }

    /// Names the connection the failure happened on; `None` leaves it unset.
    pub(crate) fn with_connection_use(
        mut self,
        connection: impl Into<Option<ConnectionUse>>,
    ) -> Self {
        if let Some(connection) = connection.into() {
            self.connection = Some(connection);
        }
        self
    }

    pub(crate) const fn with_idle_age(mut self, age: Option<std::time::Duration>) -> Self {
        self.idle_age = age;
        self
    }

    /// Age of the pooled connection at checkout, including a stale replacement.
    #[must_use]
    #[cfg(test)]
    pub(crate) const fn idle_age(self) -> Option<std::time::Duration> {
        self.idle_age
    }

    /// A closed, non-sensitive transport-library cause category, when known.
    /// This never includes peer text, headers, paths, or the raw source error.
    #[must_use]
    pub const fn cause(self) -> Option<&'static str> {
        self.cause
    }

    /// The operating system's error number behind an I/O cause, when there was
    /// one. A number only; never the platform's message text.
    #[must_use]
    pub const fn os_error(self) -> Option<i32> {
        self.os_error
    }

    /// Numeric HTTP/2 reason, when the library retained one; never GOAWAY debug data.
    #[must_use]
    pub const fn http2_reason(self) -> Option<u32> {
        self.http2_reason
    }

    /// The Rustls error behind an I/O cause, when TLS reported one.
    #[must_use]
    pub const fn tls_cause(self) -> Option<TlsCause> {
        self.tls
    }

    /// The protocol ALPN selected for the failed exchange's connection, when
    /// the failure came after negotiation.
    #[must_use]
    pub const fn wire_protocol(self) -> Option<WireProtocol> {
        self.protocol
    }

    /// Whether the I/O cause was reached through the HTTP/2 connection's own
    /// I/O error rather than a stream or frame error.
    #[must_use]
    pub const fn via_http2_io(self) -> bool {
        self.http2_io
    }

    /// Whether the failed exchange ran on a fresh, reused or stale-replaced
    /// connection, when the failure came after a connection was chosen.
    #[must_use]
    pub const fn connection_use(self) -> Option<ConnectionUse> {
        self.connection
    }

    /// Returns the phase in which the failure occurred.
    #[must_use]
    pub const fn phase(self) -> Phase {
        self.phase
    }

    /// Returns the conservative transmission fact.
    #[must_use]
    pub const fn transmission(self) -> TransmissionFact {
        self.transmission
    }

    /// Returns the stable failure classification.
    #[must_use]
    pub const fn kind(self) -> ErrorKind {
        self.kind
    }

    /// One failure with exactly these facts, for classifier tests.
    ///
    /// A provider adapter decides what a transport failure means, and that
    /// decision is worth a test of its own. Producing a real one needs a peer
    /// that misbehaves in a specific way, which is a physical fixture per case;
    /// this lets a classifier be asked the question directly. It builds no
    /// exchange and reads nothing, so it cannot stand in for a physical test of
    /// the transport itself.
    #[doc(hidden)]
    #[must_use]
    pub const fn for_classifier_test(
        phase: Phase,
        transmission: TransmissionFact,
        kind: ErrorKind,
    ) -> Self {
        Self::new(phase, transmission, kind)
    }
}

impl fmt::Debug for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Failure")
            .field("phase", &self.phase)
            .field("transmission", &self.transmission)
            .field("kind", &self.kind)
            .field("cause", &self.cause)
            .field("http2_reason", &self.http2_reason)
            .field("os_error", &self.os_error)
            .field("tls", &self.tls.map(TlsCause::label))
            .field("tls_alert", &self.tls.and_then(TlsCause::alert))
            .field("alpn", &self.protocol.map(WireProtocol::alpn))
            .field("http2_io", &self.http2_io)
            .field("connection", &self.connection)
            .field("idle_age", &self.idle_age)
            .finish()
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "HTTP transport failed in {:?} ({:?}, {:?})",
            self.phase, self.transmission, self.kind
        )?;
        if let Some(age) = self.idle_age {
            write!(formatter, " [idle at checkout: {} ms]", age.as_millis())?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

fn hyper_cause(error: &hyper::Error) -> &'static str {
    if error.is_parse() {
        "http_parse"
    } else if error.is_incomplete_message() {
        "incomplete_message"
    } else if error.is_closed() {
        "channel_closed"
    } else if error.is_canceled() {
        "request_canceled"
    } else if error.is_timeout() {
        "library_timeout"
    } else if error.is_user() {
        "local_body_or_service"
    } else {
        "http_protocol"
    }
}

fn io_cause(kind: std::io::ErrorKind) -> &'static str {
    match kind {
        std::io::ErrorKind::ConnectionReset => "connection_reset",
        std::io::ErrorKind::ConnectionAborted => "connection_aborted",
        std::io::ErrorKind::BrokenPipe => "broken_pipe",
        std::io::ErrorKind::UnexpectedEof => "unexpected_eof",
        std::io::ErrorKind::TimedOut => "io_timeout",
        std::io::ErrorKind::NotConnected => "not_connected",
        // rustls reports TLS alerts and record errors this way.
        std::io::ErrorKind::InvalidData => "invalid_data",
        _ => "io_other",
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorKind, Failure, Phase, TlsCause, TransmissionFact, WireProtocol};

    #[test]
    fn io_causes_distinguish_reset_from_eof_without_error_text() {
        assert_eq!(
            super::io_cause(std::io::ErrorKind::ConnectionReset),
            "connection_reset"
        );
        assert_eq!(
            super::io_cause(std::io::ErrorKind::UnexpectedEof),
            "unexpected_eof"
        );
        assert_eq!(super::io_cause(std::io::ErrorKind::Other), "io_other");
    }

    fn io_failure(error: &std::io::Error) -> Failure {
        Failure::new(
            Phase::RequestWriteOrResponseHead,
            TransmissionFact::MayHaveTransmitted,
            ErrorKind::Protocol,
        )
        .with_io_cause(error)
    }

    #[test]
    fn rustls_errors_inside_invalid_data_map_to_closed_tls_causes() {
        use rustls::{AlertDescription, PeerMisbehaved};
        let cases = [
            (
                rustls::Error::AlertReceived(AlertDescription::BadRecordMac),
                TlsCause::AlertReceived(20),
                "tls_alert_received",
            ),
            (
                rustls::Error::DecryptError,
                TlsCause::DecryptError,
                "tls_decrypt_error",
            ),
            (
                rustls::Error::InvalidMessage(rustls::InvalidMessage::InvalidContentType),
                TlsCause::InvalidMessage,
                "tls_invalid_message",
            ),
            (
                rustls::Error::PeerMisbehaved(PeerMisbehaved::IllegalMiddleboxChangeCipherSpec),
                TlsCause::PeerMisbehaved,
                "tls_peer_misbehaved",
            ),
            (
                rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOfferedOrEnabled),
                TlsCause::PeerIncompatible,
                "tls_peer_incompatible",
            ),
            (
                rustls::Error::General("peer text must not leak".into()),
                TlsCause::Other,
                "tls_other",
            ),
        ];
        for (rustls_error, expected, label) in cases {
            let failure = io_failure(&std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls_error,
            ));
            assert_eq!(failure.cause(), Some("invalid_data"));
            assert_eq!(failure.tls_cause(), Some(expected));
            assert_eq!(expected.label(), label);
            assert_eq!(failure.os_error(), None);
            let debug = format!("{failure:?}");
            assert!(debug.contains(label), "{debug}");
            assert!(!debug.contains("peer text"), "{debug}");
        }
        assert_eq!(TlsCause::AlertReceived(20).alert(), Some(20));
        assert_eq!(TlsCause::DecryptError.alert(), None);
    }

    #[test]
    fn plain_io_errors_carry_no_tls_cause_and_debug_names_every_fact() {
        let failure = io_failure(&std::io::Error::from(std::io::ErrorKind::ConnectionReset));
        assert_eq!(failure.cause(), Some("connection_reset"));
        assert_eq!(failure.tls_cause(), None);
        let opaque = io_failure(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "not a rustls error",
        ));
        assert_eq!(opaque.tls_cause(), None);

        let alert = io_failure(&std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::InternalError),
        ))
        .with_wire_protocol(WireProtocol::Http2);
        assert_eq!(alert.wire_protocol(), Some(WireProtocol::Http2));
        assert!(!alert.via_http2_io());
        assert_eq!(
            format!("{alert:?}"),
            "Failure { phase: RequestWriteOrResponseHead, transmission: MayHaveTransmitted, \
             kind: Protocol, cause: Some(\"invalid_data\"), http2_reason: None, os_error: None, \
             tls: Some(\"tls_alert_received\"), tls_alert: Some(80), alpn: Some(\"h2\"), \
             http2_io: false, connection: None, idle_age: None }"
        );
    }
}

/// Invalid strict transport configuration.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A count or duration limit that must be nonzero was zero.
    #[error("transport count and duration limits must be nonzero")]
    ZeroLimit,
    /// The HTTP/1 response buffer is below Hyper's safe parser minimum.
    #[error("max_response_header_bytes must be at least 8192")]
    ResponseHeaderBufferTooSmall,
    /// A limit cannot be represented by the HTTP/2 implementation.
    #[error("transport limit exceeds the HTTP/2 representable range")]
    Http2LimitTooLarge,
    /// A caller-supplied TLS behavior identity omitted its semantic revision.
    #[error("caller TLS behavior identity must use a nonzero revision")]
    UnversionedTlsIdentity,
}

/// An invalid immutable request.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum RequestError {
    /// The origin is not an absolute HTTPS origin without user information,
    /// path, query, or fragment, nor a loopback HTTP origin.
    #[error("origin must be an absolute HTTPS origin or a loopback HTTP origin")]
    InvalidOrigin,
    /// The request target is not a non-network origin-form path and query.
    #[error("request target must be an origin-form path and optional query")]
    InvalidPathAndQuery,
    /// CONNECT is intentionally unsupported because it changes routing semantics.
    #[error("CONNECT requests are not supported")]
    ConnectMethod,
    /// A routing, framing, proxy, or hop-by-hop header was supplied.
    #[error("request header is controlled by the transport: {0}")]
    ControlledHeader(http::HeaderName),
    /// The immutable request exceeds a configured bound.
    #[error("request exceeds configured {0:?} limit")]
    LimitExceeded(LimitKind),
}
