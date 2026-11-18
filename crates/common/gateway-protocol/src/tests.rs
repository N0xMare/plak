use serde_json::json;

use crate::admin::{AdminRequest, AdminResponse};
use crate::call::{self, CallError};
use crate::envelope::{
    EnvelopeError, ExpectedFence, MAX_ENVELOPE_BYTES, RelayEnvelope, RelayLimits, decode_request,
    encode_request, split_upstream_url,
};
use crate::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, MAX_WIRE_IDENTITY_ENTRIES, WireIdentity,
};
use crate::frame::{
    Data, Frame, FrameDecoder, FrameError, Head, MAX_BUFFERED_BYTES, MAX_DATA_BYTES, RelayEnd,
    Trailers,
};
use crate::header::{HeaderError, RequestHeader, WireHeader};
use crate::refusal::{GatewayRefusal, RefusalBody};
use crate::scope::{CallerId, CallerKey, Capability, ScopeSet, SourceNet};
use crate::{
    BoundedText, Endpoint, GatewayAccount, HexDigest, SUPPORTED_VERSIONS, SecretText, UuidText,
    negotiate,
};

const OPERATION: &str = "0b9e5f4c-7d3a-4e21-9f80-1a2b3c4d5e6f";

fn digest(byte: u8) -> HexDigest {
    HexDigest::from_bytes(&[byte; 32])
}

fn account(text: &str) -> GatewayAccount {
    GatewayAccount::new(text).unwrap()
}

fn header(name: &str, value: &str) -> WireHeader {
    WireHeader::new(name, value.as_bytes().to_vec()).unwrap()
}

fn value(name: &str, value: &str) -> RequestHeader {
    RequestHeader::Value(header(name, value))
}

fn custodied(name: &str) -> RequestHeader {
    RequestHeader::custodied(name).unwrap()
}

fn envelope(body_len: u64) -> RelayEnvelope {
    RelayEnvelope {
        operation: UuidText::new(OPERATION).unwrap(),
        attempt: 1,
        account: account("claude/work"),
        expected: ExpectedFence {
            account_binding: digest(0xab),
            generation: 7,
        },
        method: "POST".into(),
        url: "https://api.anthropic.com/v1/messages?beta=true".into(),
        headers: vec![
            value("content-type", "application/json"),
            custodied("authorization"),
            value("anthropic-version", "2023-06-01"),
        ],
        body_len,
        limits: RelayLimits {
            head_timeout_ms: 180_000,
            body_idle_timeout_ms: 180_000,
            max_response_bytes: 32 * 1024 * 1024,
            max_chunk_bytes: 1024 * 1024,
        },
        deadline_unix_ms: None,
    }
}

fn refused(mutate: impl FnOnce(&mut RelayEnvelope)) -> EnvelopeError {
    let mut value = envelope(2);
    mutate(&mut value);
    encode_request(&value, b"{}").unwrap_err()
}

#[test]
fn accounts_follow_the_reference_grammar() {
    let named = account("xai-api/team-2");
    assert_eq!((named.kind(), named.name()), ("xai-api", Some("team-2")));
    let default = account("codex");
    assert_eq!((default.kind(), default.name()), ("codex", None));
    for bad in [
        "",
        "Codex",
        "codex/",
        "/work",
        "codex/default",
        "codex/work/extra",
        "1codex",
        "codex/Work",
        "codex/w_rk",
        &"a".repeat(33),
    ] {
        assert!(GatewayAccount::new(bad).is_err(), "{bad:?} must be refused");
    }
    assert!(serde_json::from_value::<GatewayAccount>(json!("codex/default")).is_err());
}

#[test]
fn shaped_text_is_checked_on_decode() {
    assert!(serde_json::from_value::<HexDigest>(json!("AB".repeat(32))).is_err());
    assert!(serde_json::from_value::<HexDigest>(json!("ab".repeat(31))).is_err());
    assert_eq!(digest(0x0f).as_str(), "0f".repeat(32));
    assert!(serde_json::from_value::<UuidText>(json!(OPERATION.to_uppercase())).is_err());
    assert!(serde_json::from_value::<UuidText>(json!(OPERATION.replace('-', "_"))).is_err());
    assert!(BoundedText::<4>::new("abcde").is_err());
    assert!(BoundedText::<16>::new("line\nbreak").is_err());
    assert!(serde_json::from_value::<BoundedText<16>>(json!("tab\there")).is_err());
}

#[test]
fn secrets_are_bounded_and_never_printed() {
    let secret: SecretText = serde_json::from_value(json!("sk-not-a-real-key")).unwrap();
    assert_eq!(secret.expose(), "sk-not-a-real-key");
    assert_eq!(format!("{secret:?}"), "SecretText(<redacted>)");
    let oversized = "x".repeat(SecretText::MAX_BYTES + 1);
    assert!(serde_json::from_value::<SecretText>(json!(oversized)).is_err());
    let request = AdminRequest::KeySet {
        account: account("openai"),
        secret,
    };
    assert!(!format!("{request:?}").contains("sk-not"));
}

#[test]
fn header_fields_keep_exact_bytes_and_refuse_injection() {
    let value = WireHeader::new("x-trace", vec![b'a', 0x80, b'\t', b'z']).unwrap();
    let encoded = serde_json::to_value(&value).unwrap();
    assert_eq!(encoded, json!(["x-trace", "YYAJeg=="]));
    assert_eq!(
        serde_json::from_value::<WireHeader>(encoded).unwrap(),
        value
    );
    assert!(!format!("{value:?}").contains("YYAJeg"));
    assert_eq!(
        WireHeader::new("X-Trace", "a").unwrap_err(),
        HeaderError::Name
    );
    assert_eq!(
        WireHeader::new("x trace", "a").unwrap_err(),
        HeaderError::Name
    );
    assert_eq!(WireHeader::new("", "a").unwrap_err(), HeaderError::Name);
    assert_eq!(
        WireHeader::new("x", "a\r\nb").unwrap_err(),
        HeaderError::Value
    );
    assert_eq!(WireHeader::new("x", "a\0").unwrap_err(), HeaderError::Value);
    assert!(serde_json::from_value::<WireHeader>(json!(["x", "not base64!"])).is_err());
    assert!(serde_json::from_value::<WireHeader>(json!(["x", "YQ"])).is_err());
    assert!(serde_json::from_value::<WireHeader>(json!(["x", "DQo="])).is_err());
}

#[test]
fn a_relay_request_round_trips_its_envelope_and_exact_body() {
    let body = b"{\"model\":\"m\"}";
    let value = envelope(u64::try_from(body.len()).unwrap());
    let request = encode_request(&value, body).unwrap();
    let (decoded, decoded_body) = decode_request(&request).unwrap();
    assert_eq!(decoded, value);
    assert_eq!(decoded_body, body);
}

#[test]
fn a_relay_request_that_is_not_whole_is_refused() {
    let request = encode_request(&envelope(2), b"{}").unwrap();
    assert_eq!(
        decode_request(&request[..3]).unwrap_err(),
        EnvelopeError::Truncated
    );
    assert_eq!(
        decode_request(&request[..10]).unwrap_err(),
        EnvelopeError::Truncated
    );
    assert_eq!(
        decode_request(&request[..request.len() - 1]).unwrap_err(),
        EnvelopeError::BodyLength
    );
    let mut longer = request;
    longer.push(b' ');
    assert_eq!(
        decode_request(&longer).unwrap_err(),
        EnvelopeError::BodyLength
    );
    let oversized = u32::try_from(MAX_ENVELOPE_BYTES + 1).unwrap().to_be_bytes();
    assert_eq!(
        decode_request(&oversized).unwrap_err(),
        EnvelopeError::EnvelopeTooLarge
    );
    assert_eq!(
        encode_request(&envelope(3), b"{}").unwrap_err(),
        EnvelopeError::BodyLength
    );
}

#[test]
fn a_relay_envelope_with_unknown_fields_is_refused() {
    let mut value = serde_json::to_value(envelope(0)).unwrap();
    value["redirect"] = json!(true);
    let json = serde_json::to_vec(&value).unwrap();
    let mut request = u32::try_from(json.len()).unwrap().to_be_bytes().to_vec();
    request.extend_from_slice(&json);
    assert_eq!(decode_request(&request).unwrap_err(), EnvelopeError::Json);
}

#[test]
fn upstream_urls_must_be_exact_https_urls() {
    assert_eq!(
        split_upstream_url("https://chatgpt.com/backend-api/codex/models?client_version=1")
            .unwrap(),
        (
            "https://chatgpt.com",
            "/backend-api/codex/models?client_version=1"
        )
    );
    for bad in [
        "http://api.openai.com/v1/responses",
        "https://api.openai.com",
        "https://user@api.openai.com/v1/responses",
        "https://API.openai.com/v1/responses",
        "https://api.openai.com/v1/responses#frag",
        "https://api.openai.com/v1/re sponses",
        "https:///v1/responses",
        "HTTPS://api.openai.com/v1/responses",
    ] {
        assert_eq!(
            refused(|envelope| envelope.url = bad.into()),
            EnvelopeError::Url,
            "{bad}"
        );
    }
}

#[test]
fn callers_may_not_send_credential_framing_or_gateway_headers() {
    for name in [
        "authorization",
        "host",
        "content-length",
        "transfer-encoding",
        "connection",
        "te",
        "upgrade",
        "proxy-authorization",
        "plak-gateway-key",
    ] {
        assert_eq!(
            refused(|envelope| envelope.headers.push(value(name, "x"))),
            EnvelopeError::ReservedHeader,
            "{name}"
        );
    }
    let without_custodied = vec![value("content-type", "application/json")];
    for headers in [
        without_custodied,
        vec![custodied("authorization"), custodied("authorization")],
        vec![custodied("host")],
        ["a", "b", "c", "d", "e"].map(custodied).to_vec(),
    ] {
        assert_eq!(
            refused(|envelope| envelope.headers = headers.clone()),
            EnvelopeError::CustodiedHeaders,
            "{headers:?}"
        );
    }
    assert_eq!(
        RequestHeader::custodied("Authorization").unwrap_err(),
        HeaderError::Name
    );
}

#[test]
fn custodied_entries_keep_their_place_in_the_header_order() {
    let value = envelope(0);
    let names: Vec<&str> = value.headers.iter().map(RequestHeader::name).collect();
    assert_eq!(
        names,
        ["content-type", "authorization", "anthropic-version"]
    );
    assert_eq!(
        value.custodied_names().collect::<Vec<_>>(),
        ["authorization"]
    );
    let encoded = serde_json::to_value(&value.headers).unwrap();
    assert_eq!(encoded[1], json!(["authorization", null]));
    let decoded: Vec<RequestHeader> = serde_json::from_value(encoded).unwrap();
    assert_eq!(decoded, value.headers);
    assert!(serde_json::from_value::<RequestHeader>(json!(["Authorization", null])).is_err());
}

#[test]
fn envelope_method_headers_and_limits_are_bounded() {
    assert_eq!(
        refused(|envelope| envelope.method = "post".into()),
        EnvelopeError::Method
    );
    assert_eq!(
        refused(|envelope| envelope.method = String::new()),
        EnvelopeError::Method
    );
    assert_eq!(
        refused(|envelope| envelope.headers.extend(vec![value("x-a", "b"); 62])),
        EnvelopeError::HeaderLimits
    );
    let mut fits = envelope(2);
    fits.headers.extend(vec![value("x-a", "b"); 61]);
    assert!(encode_request(&fits, b"{}").is_ok());
    assert_eq!(
        refused(|envelope| envelope.limits.head_timeout_ms = 0),
        EnvelopeError::Limits
    );
    assert_eq!(
        refused(
            |envelope| envelope.limits.max_chunk_bytes = u32::try_from(MAX_DATA_BYTES + 1).unwrap()
        ),
        EnvelopeError::Limits
    );
}

fn stream() -> Vec<Frame> {
    vec![
        Frame::Head(Head {
            status: 200,
            headers: vec![header("content-type", "text/event-stream")],
        }),
        Frame::Data(Data {
            seq: 0,
            offset: 0,
            bytes: b"event: a\n\n".to_vec(),
        }),
        Frame::Data(Data {
            seq: 1,
            offset: 10,
            bytes: b"event: b\n\n".to_vec(),
        }),
        Frame::Trailers(Trailers { headers: vec![] }),
        Frame::End(RelayEnd::Complete),
    ]
}

fn encoded(frames: &[Frame]) -> Vec<u8> {
    frames
        .iter()
        .flat_map(|frame| frame.encode().unwrap())
        .collect()
}

fn decode_all(bytes: &[u8], max: u64) -> Result<Vec<Frame>, FrameError> {
    let mut decoder = FrameDecoder::new(max);
    let mut frames = Vec::new();
    decoder.push(bytes)?;
    while let Some(frame) = decoder.next_frame()? {
        frames.push(frame);
    }
    decoder.finish()?;
    Ok(frames)
}

#[test]
fn a_frame_stream_decodes_byte_by_byte() {
    let frames = stream();
    let bytes = encoded(&frames);
    let mut decoder = FrameDecoder::new(1024);
    let mut received = Vec::new();
    for byte in &bytes {
        decoder.push(std::slice::from_ref(byte)).unwrap();
        while let Some(frame) = decoder.next_frame().unwrap() {
            received.push(frame);
        }
    }
    decoder.finish().unwrap();
    assert!(decoder.is_ended());
    assert_eq!(received, frames);
}

#[test]
fn an_upstream_failure_before_its_head_ends_the_stream() {
    let frames = vec![Frame::End(RelayEnd::UpstreamFailed {
        phase: crate::frame::WirePhase::Connect,
        transmission: crate::frame::WireTransmission::NotStarted,
        kind: crate::frame::WireFailureKind::Io,
        timeout: false,
    })];
    assert_eq!(decode_all(&encoded(&frames), 0).unwrap(), frames);
}

#[test]
fn frames_out_of_order_are_refused() {
    let all = stream();
    for order in [
        vec![1, 4],
        vec![0, 0],
        vec![0, 3, 1],
        vec![0, 3, 3],
        vec![3],
    ] {
        let frames: Vec<Frame> = order.iter().map(|index| all[*index].clone()).collect();
        assert_eq!(
            decode_all(&encoded(&frames), 1024).unwrap_err(),
            FrameError::Order,
            "{order:?}"
        );
    }
}

#[test]
fn data_frames_must_continue_the_stream() {
    let mut frames = stream();
    let Frame::Data(data) = &mut frames[2] else {
        unreachable!("sample order")
    };
    data.seq = 2;
    assert_eq!(
        decode_all(&encoded(&frames), 1024).unwrap_err(),
        FrameError::Sequence
    );
    let mut frames = stream();
    let Frame::Data(data) = &mut frames[2] else {
        unreachable!("sample order")
    };
    data.offset = 11;
    assert_eq!(
        decode_all(&encoded(&frames), 1024).unwrap_err(),
        FrameError::Sequence
    );
    assert_eq!(
        decode_all(&encoded(&stream()), 19).unwrap_err(),
        FrameError::ResponseTooLarge
    );
    assert_eq!(decode_all(&encoded(&stream()), 20).unwrap().len(), 5);
}

#[test]
fn frame_sizes_and_types_are_refused_before_their_payload() {
    let empty = Frame::Data(Data {
        seq: 0,
        offset: 0,
        bytes: Vec::new(),
    });
    assert_eq!(empty.encode().unwrap_err(), FrameError::Size);
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(&[0x02, 0, 0, 0, 16]).unwrap();
    assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Size);
    assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Failed);
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(&[0x01, 0x00, 0x04, 0x00, 0x01]).unwrap();
    assert_eq!(decoder.next_frame().unwrap_err(), FrameError::Size);
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(&[0x09, 0, 0, 0, 0]).unwrap();
    assert_eq!(decoder.next_frame().unwrap_err(), FrameError::UnknownType);
    let mut decoder = FrameDecoder::new(1024);
    assert_eq!(
        decoder.push(&vec![0; MAX_BUFFERED_BYTES + 1]).unwrap_err(),
        FrameError::BufferFull
    );
}

#[test]
fn a_stream_must_end_with_end_and_nothing_after_it() {
    let bytes = encoded(&stream());
    assert_eq!(
        decode_all(&bytes[..bytes.len() - 1], 1024).unwrap_err(),
        FrameError::Truncated
    );
    let mut longer = bytes;
    longer.push(0x04);
    assert_eq!(
        decode_all(&longer, 1024).unwrap_err(),
        FrameError::Truncated
    );
}

#[test]
fn a_head_or_end_payload_with_unknown_fields_or_bad_values_is_refused() {
    for payload in [
        json!({"status": 200, "headers": [], "extra": 1}),
        json!({"status": 99, "headers": []}),
        json!({"status": 200, "headers": [["Bad", ""]]}),
    ] {
        let payload = serde_json::to_vec(&payload).unwrap();
        let mut bytes = vec![0x01];
        bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        bytes.extend_from_slice(&payload);
        assert_eq!(decode_all(&bytes, 1024).unwrap_err(), FrameError::Json);
    }
}

#[test]
fn refusal_codes_statuses_and_bodies_agree() {
    let body = RefusalBody {
        gateway_error: GatewayRefusal::Busy {
            retry_after_ms: 250,
        },
    };
    let encoded = serde_json::to_value(&body).unwrap();
    assert_eq!(
        encoded,
        json!({"gateway_error": {"code": "busy", "retry_after_ms": 250}})
    );
    assert_eq!(body.gateway_error.code(), "busy");
    assert_eq!(body.gateway_error.http_status(), 503);
    assert_eq!(GatewayRefusal::Unauthenticated.http_status(), 401);
    assert_eq!(GatewayRefusal::UpstreamNotPinned.http_status(), 422);
    for sample in crate::pinned_tests::refusal_samples() {
        let value = serde_json::to_value(&sample).unwrap();
        assert_eq!(value["code"], json!(sample.code()));
        assert!(matches!(sample.http_status(), 401..=599));
    }
}

#[test]
fn facts_carry_the_fence_a_request_is_prepared_against() {
    let facts = crate::pinned_tests::facts_sample();
    assert_eq!(
        facts.expected_fence(),
        Some(ExpectedFence {
            account_binding: digest(0xab),
            generation: 7
        })
    );
    let signed_out = AccountFacts {
        identity: None,
        credential: CredentialFacts {
            status: CredentialStatus::SignedOut,
            generation: None,
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
        },
        ..facts.clone()
    };
    assert_eq!(signed_out.expected_fence(), None);
    assert!(!format!("{facts:?}").contains("person@example.com"));
}

#[test]
fn wire_identity_is_bounded() {
    let value = BoundedText::new("v").unwrap();
    let too_many = (0..=MAX_WIRE_IDENTITY_ENTRIES)
        .map(|index| (format!("k{index}"), value.clone()))
        .collect();
    assert!(WireIdentity::new(too_many).is_err());
    assert!(serde_json::from_value::<WireIdentity>(json!({"Bad-Key": "v"})).is_err());
    let ok: WireIdentity = serde_json::from_value(json!({"claude_account_uuid": "u"})).unwrap();
    assert_eq!(ok.get("claude_account_uuid"), Some("u"));
}

#[test]
fn call_bodies_are_bounded_and_strict() {
    let request = AdminRequest::LoginPoll {
        attempt: UuidText::new(OPERATION).unwrap(),
    };
    let body = call::encode(&request).unwrap();
    assert_eq!(call::decode::<AdminRequest>(&body).unwrap(), request);
    assert_eq!(
        call::decode::<AdminRequest>(br#"{"request":"login_poll","attempt":"x"}"#).unwrap_err(),
        CallError::Json
    );
    assert_eq!(
        call::decode::<AdminRequest>(br#"{"request":"list","extra":1}"#).unwrap_err(),
        CallError::Json
    );
    assert_eq!(
        call::decode::<AdminResponse>(&vec![b' '; call::MAX_CALL_BYTES + 1]).unwrap_err(),
        CallError::TooLarge
    );
}

#[test]
fn scopes_parse_canonically_and_match_accounts() {
    let scopes = ScopeSet::parse("admin:codex/*,relay:*,admin:claude/work").unwrap();
    assert_eq!(
        scopes.to_string(),
        "relay:*,admin:codex/*,admin:claude/work"
    );
    assert!(scopes.allows(Capability::Relay, &account("grok")));
    assert!(scopes.allows(Capability::Admin, &account("codex")));
    assert!(scopes.allows(Capability::Admin, &account("codex/work")));
    assert!(scopes.allows(Capability::Admin, &account("claude/work")));
    assert!(!scopes.allows(Capability::Admin, &account("claude")));
    assert!(!scopes.allows(Capability::Admin, &account("openai")));
    let exact = ScopeSet::parse("relay:openai").unwrap();
    assert!(exact.allows(Capability::Relay, &account("openai")));
    assert!(!exact.allows(Capability::Relay, &account("openai/team")));
    for bad in [
        "",
        "relay",
        "write:*",
        "allowance:*",
        "relay:Codex",
        "relay:codex/default",
        "relay:*,relay:*",
        "relay:*/*",
        "relay:* ",
    ] {
        assert!(ScopeSet::parse(bad).is_err(), "{bad:?}");
    }
    let many = (0..17)
        .map(|index| format!("relay:k{index}"))
        .collect::<Vec<_>>();
    assert!(ScopeSet::parse(&many.join(",")).is_err());
}

#[test]
fn caller_keys_round_trip_and_never_print_their_secret() {
    let key = CallerKey::from_secret(CallerId::new("host-a").unwrap(), [7; 32]);
    let text = key.expose();
    assert_eq!(
        text.as_str(),
        "plakgw1.host-a.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"
    );
    let parsed = CallerKey::parse(&text).unwrap();
    assert_eq!(parsed.id().as_str(), "host-a");
    assert_eq!(parsed.secret(), &[7; 32]);
    assert!(!format!("{parsed:?}").contains("BwcH"));
    for bad in [
        "plakgw2.host-a.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
        "plakgw1.Host.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
        "plakgw1.host-a.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
        "plakgw1.host-a.BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwd",
        "plakgw1.host-a.BwcH",
        "plakgw1.host-a",
    ] {
        assert!(CallerKey::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn versions_negotiate_to_the_highest_common_one() {
    assert_eq!(negotiate(SUPPORTED_VERSIONS, &[1]), Some(1));
    assert_eq!(negotiate(&[3, 4], &[2, 3]), Some(3));
    assert_eq!(negotiate(&[3, 4], &[4, 5]), Some(4));
    assert_eq!(negotiate(&[3, 4], &[5, 6]), None);
}

#[test]
fn endpoint_paths_carry_the_version() {
    for endpoint in Endpoint::ALL {
        assert_eq!(Endpoint::parse(&endpoint.path(12)), Some((12, endpoint)));
    }
    assert_eq!(Endpoint::path(Endpoint::Relay, 1), "/v1/relay");
    for bad in [
        "/v01/relay",
        "/v/relay",
        "/1/relay",
        "/v1/relay/",
        "/v1/settle",
        "/vx/relay",
    ] {
        assert_eq!(Endpoint::parse(bad), None, "{bad}");
    }
}

#[test]
fn source_networks_parse_canonically_and_match_their_family() {
    let tailnet = SourceNet::parse("100.64.0.0/10").unwrap();
    assert!(tailnet.contains("100.101.102.103".parse().unwrap()));
    assert!(!tailnet.contains("100.128.0.1".parse().unwrap()));
    assert!(!tailnet.contains("::ffff:100.101.102.103".parse().unwrap()));
    let host = SourceNet::parse("100.101.102.103").unwrap();
    assert_eq!(host.to_string(), "100.101.102.103");
    assert!(host.contains("100.101.102.103".parse().unwrap()));
    assert!(!host.contains("100.101.102.104".parse().unwrap()));
    let v6 = SourceNet::parse("fd7a:115c:a1e0:0::/48").unwrap();
    assert_eq!(v6.to_string(), "fd7a:115c:a1e0::/48");
    assert!(v6.contains("fd7a:115c:a1e0:ab12::1".parse().unwrap()));
    assert!(
        SourceNet::parse("0.0.0.0/0")
            .unwrap()
            .contains("192.0.2.1".parse().unwrap())
    );
    for bad in [
        "100.64.0.1/10",
        "100.64.0.0/33",
        "100.64.0.0/010",
        "100.64.0.0/",
        "host",
        "::/129",
    ] {
        assert!(SourceNet::parse(bad).is_err(), "{bad}");
    }
    assert_eq!(
        serde_json::to_value(tailnet).unwrap(),
        json!("100.64.0.0/10")
    );
}

#[test]
fn an_issued_caller_key_never_prints() {
    let response = AdminResponse::CallerKeyIssued {
        caller: CallerId::new("host-a").unwrap(),
        key: SecretText::new(zeroize::Zeroizing::new("plakgw1.host-a.secret".to_owned())).unwrap(),
    };
    assert!(!format!("{response:?}").contains("plakgw1"));
}
