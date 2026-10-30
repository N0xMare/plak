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
             http2_io: false, connection: None, idle_age: None, connect_attempts: None }"
    );
}
