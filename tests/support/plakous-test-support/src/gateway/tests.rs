use std::time::{Duration, Instant};

use plakous_gateway_protocol::call;
use plakous_gateway_protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, IdentityFacts,
};
use plakous_gateway_protocol::header::WireHeader;
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{Endpoint, PROTOCOL_VERSION, VersionsReply};
use plakous_http::relay::custodied_placeholder;
use plakous_http::{
    Bytes, Client, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Limits, Method, Origin,
    Request, StatusCode, sensitive_header_value,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::service::{ACCOUNT, BINDING, GENERATION, LOGICAL};
use super::{FixtureError, FixtureGateway, GatewayScript, render_upstream, substitute};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const TOKEN: &[u8] = b"secret-token";

#[tokio::test]
async fn versions_follow_the_instance_across_restart() -> TestResult {
    let gateway = gateway()?;
    let (bytes, report) = exchange(&gateway, b"GET /versions HTTP/1.1\r\nhost: g\r\n\r\n").await?;
    assert!(!report.dialed);
    let reply: VersionsReply = call::decode(&body(&bytes)?)?;
    assert_eq!(reply.versions, vec![PROTOCOL_VERSION]);
    assert_eq!(reply.instance, gateway.instance());
    let restarted = gateway.restart()?;
    assert_ne!(restarted, reply.instance);
    let (bytes, _) = exchange(&gateway, b"GET /versions HTTP/1.1\r\nhost: g\r\n\r\n").await?;
    let reply: VersionsReply = call::decode(&body(&bytes)?)?;
    assert_eq!(reply.instance, restarted);
    Ok(())
}

#[tokio::test]
async fn a_partial_body_never_dials() -> TestResult {
    let gateway = gateway()?;
    let (client, server) = tokio::io::duplex(8 * 1024);
    let running = gateway.clone();
    let task = tokio::spawn(async move { running.serve_one(server).await });
    let (read, mut write) = tokio::io::split(client);
    write
        .write_all(b"POST /v1/relay HTTP/1.1\r\nhost: g\r\ncontent-length: 40\r\n\r\nshort")
        .await?;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(gateway.dials(), 0);
    drop(write);
    drop(read);
    let report = task.await??;
    assert!(!report.dialed);
    assert_eq!(gateway.dials(), 0);
    assert_eq!(gateway.captured().len(), 0);
    Ok(())
}

#[tokio::test]
async fn relayed_bytes_equal_a_direct_exchange() -> TestResult {
    let gateway = gateway()?;
    // `relayed` binds the loopback accept loop. The direct exchange has to
    // follow that, or it dials a listener nobody is accepting yet.
    let relayed = gateway.relayed().await?;
    let direct = Client::new_http1(Limits::default())?;
    let direct_response = direct
        .execute(
            api_request(gateway.upstream_origin(), real_authorization()?)?,
            control(),
        )
        .await?;
    assert_eq!(direct_response.status(), StatusCode::OK);

    let relayed_response = relayed
        .execute(
            api_request(Origin::parse(&format!("{LOGICAL}/"))?, placeholder()?)?,
            control(),
        )
        .await?;
    assert_eq!(relayed_response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 2);

    let captured = gateway.captured();
    assert_eq!(captured.len(), 2);
    let host = gateway.upstream_origin().binding_authority().to_owned();
    let normalize = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&host, "<host>");
    assert_eq!(normalize(&captured[1].http), normalize(&captured[0].http));
    let text = String::from_utf8_lossy(&captured[1].http);
    assert!(text.contains("authorization: secret-token\r\n"), "{text}");
    assert!(!text.as_bytes().contains(&0xff), "{text}");
    let order: Vec<&str> = text
        .lines()
        .skip(1)
        .map(|line| line.split(':').next().unwrap_or(""))
        .take_while(|name| !name.is_empty())
        .collect();
    assert_eq!(
        order,
        [
            "content-type",
            "authorization",
            "anthropic-version",
            "host",
            "content-length"
        ]
    );
    Ok(())
}

#[test]
fn relayed_bytes_equal_direct_local_custody() -> TestResult {
    let headers = vec![
        WireHeader::new("content-type", b"application/json".as_slice())?,
        WireHeader::new("authorization", TOKEN)?,
        WireHeader::new("anthropic-version", b"2023-06-01".as_slice())?,
    ];
    let direct = render_upstream(
        "POST",
        "https://api.anthropic.com/v1/messages",
        &headers,
        b"{}",
    )?;
    let relayed = substitute(
        &[
            plakous_gateway_protocol::header::RequestHeader::Value(headers[0].clone()),
            plakous_gateway_protocol::header::RequestHeader::custodied("authorization")?,
            plakous_gateway_protocol::header::RequestHeader::Value(headers[2].clone()),
        ],
        &[("authorization".to_owned(), TOKEN.to_vec())],
    )?;
    let relayed = render_upstream(
        "POST",
        "https://api.anthropic.com/v1/messages",
        &relayed,
        b"{}",
    )?;
    assert_eq!(relayed, direct);
    assert!(direct.windows(TOKEN.len()).any(|window| window == TOKEN));
    Ok(())
}

#[tokio::test]
async fn refusals_and_drops_do_not_dial() -> TestResult {
    let gateway = gateway()?;
    gateway.push(GatewayScript::Refuse(GatewayRefusal::Unauthenticated {}));
    let (bytes, report) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(!report.dialed);
    assert_eq!(gateway.dials(), 0);
    assert!(bytes.starts_with(b"HTTP/1.1 401 "));
    let refusal: RefusalBody = call::decode(&body(&bytes)?)?;
    assert_eq!(refusal.gateway_error, GatewayRefusal::Unauthenticated {});
    assert!(
        bytes
            .windows(ERROR_MARK.len())
            .any(|window| window == ERROR_MARK)
    );

    gateway.push(GatewayScript::DropBeforeResponse);
    let (bytes, report) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(!report.dialed);
    assert_eq!(bytes.len(), 0);
    assert_eq!(gateway.dials(), 0);
    Ok(())
}

#[tokio::test]
async fn a_drop_after_head_dials_once_and_omits_end() -> TestResult {
    let gateway = gateway()?;
    gateway.push(GatewayScript::DropAfterHead);
    let client = gateway.relayed().await?;
    let response = client
        .execute(
            api_request(Origin::parse(&format!("{LOGICAL}/"))?, placeholder()?)?,
            control(),
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(gateway.dials(), 1);
    let mut body = response.into_body();
    let pending = tokio::time::timeout(Duration::from_millis(200), body.next_chunk()).await;
    assert!(pending.is_err(), "an end frame arrived before the drop");
    Ok(())
}

#[tokio::test]
async fn fence_moved_on_the_nth_relay_does_not_dial() -> TestResult {
    let gateway = gateway()?;
    let facts = facts()?;
    gateway.script_fence_moved(2, facts.clone());
    let (_, first) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(first.dialed);
    let (bytes, second) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(!second.dialed);
    assert_eq!(gateway.dials(), 1);
    let refusal: RefusalBody = call::decode(&body(&bytes)?)?;
    assert_eq!(
        refusal.gateway_error,
        GatewayRefusal::FenceMoved {
            facts: Box::new(facts)
        }
    );
    Ok(())
}

#[tokio::test]
async fn an_unserved_version_is_refused_without_a_dial() -> TestResult {
    let gateway = gateway()?;
    let (bytes, report) = exchange(&gateway, &http_post("/v2/relay", &relay_body()?)).await?;
    assert!(!report.dialed);
    assert_eq!(gateway.dials(), 0);
    let refusal: RefusalBody = call::decode(&body(&bytes)?)?;
    assert!(matches!(
        refusal.gateway_error,
        GatewayRefusal::VersionUnsupported { .. }
    ));
    Ok(())
}

#[tokio::test]
async fn a_wrong_key_is_unauthenticated_and_undialed() -> TestResult {
    let gateway = FixtureGateway::new(credentials(), Some("expected".to_owned()))?;
    let (bytes, report) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(!report.dialed);
    assert!(bytes.starts_with(b"HTTP/1.1 401 "));
    Ok(())
}

const ERROR_MARK: &[u8] = b"plak-gateway-error: unauthenticated";

async fn exchange(
    gateway: &FixtureGateway,
    request: &[u8],
) -> TestResult<(Vec<u8>, super::ServeReport)> {
    let (mut client, server) = tokio::io::duplex(64 * 1024);
    let running = gateway.clone();
    let task = tokio::spawn(async move { running.serve_one(server).await });
    client.write_all(request).await?;
    let mut response = Vec::new();
    let mut buffer = [0_u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !http_message_complete(&response) && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Ok(read) = tokio::time::timeout(remaining, client.read(&mut buffer)).await else {
            break;
        };
        let Ok(read) = read else { break };
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
    }
    drop(client);
    let report = task.await??;
    Ok((response, report))
}

fn http_message_complete(bytes: &[u8]) -> bool {
    let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    if let Some(length) = content_length(&head) {
        return bytes.len() >= end + 4 + length;
    }
    if head.contains("transfer-encoding:") && head.contains("chunked") {
        return bytes[end + 4..]
            .windows(5)
            .any(|window| window == b"0\r\n\r\n");
    }
    true
}

fn content_length(head: &str) -> Option<usize> {
    head.lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
}

fn gateway() -> Result<FixtureGateway, FixtureError> {
    FixtureGateway::new(credentials(), None)
}

fn credentials() -> Vec<(String, Vec<u8>)> {
    vec![("authorization".to_owned(), TOKEN.to_vec())]
}

fn http_post(path: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "POST {path} HTTP/1.1\r\nhost: gateway.invalid\r\ncontent-type: application/vnd.plak.relay\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn body(response: &[u8]) -> TestResult<Vec<u8>> {
    let index = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("response has no head")?;
    let head = String::from_utf8_lossy(&response[..index]).to_ascii_lowercase();
    let rest = &response[index + 4..];
    if let Some(length) = content_length(&head) {
        if rest.len() < length {
            return Err("response body is short".into());
        }
        return Ok(rest[..length].to_vec());
    }
    if head.contains("transfer-encoding:") && head.contains("chunked") {
        return decode_chunks(rest);
    }
    Ok(rest.to_vec())
}

fn decode_chunks(mut rest: &[u8]) -> TestResult<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = rest
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or("chunk size is missing")?;
        let size_text = std::str::from_utf8(&rest[..line_end])?;
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or(""), 16)?;
        rest = &rest[line_end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if rest.len() < size + 2 {
            return Err("chunk is short".into());
        }
        out.extend_from_slice(&rest[..size]);
        rest = &rest[size + 2..];
    }
}

pub(super) fn relay_body() -> TestResult<Vec<u8>> {
    let envelope = plakous_gateway_protocol::envelope::RelayEnvelope {
        operation: plakous_gateway_protocol::UuidText::new("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")?,
        attempt: 1,
        account: plakous_gateway_protocol::GatewayAccount::new(ACCOUNT)?,
        expected: plakous_gateway_protocol::envelope::ExpectedFence {
            account_binding: plakous_gateway_protocol::HexDigest::new(BINDING)?,
            generation: GENERATION,
        },
        method: "POST".to_owned(),
        url: format!("{LOGICAL}/v1/messages"),
        headers: vec![
            plakous_gateway_protocol::header::RequestHeader::Value(WireHeader::new(
                "content-type",
                b"application/json".as_slice(),
            )?),
            plakous_gateway_protocol::header::RequestHeader::custodied("authorization")?,
            plakous_gateway_protocol::header::RequestHeader::Value(WireHeader::new(
                "anthropic-version",
                b"2023-06-01".as_slice(),
            )?),
        ],
        body_len: 2,
        limits: plakous_gateway_protocol::envelope::RelayLimits {
            head_timeout_ms: 1_000,
            body_idle_timeout_ms: 1_000,
            max_response_bytes: 4_096,
            max_chunk_bytes: 4_096,
        },
        deadline_unix_ms: None,
    };
    Ok(plakous_gateway_protocol::envelope::encode_request(
        &envelope, b"{}",
    )?)
}

fn digest() -> TestResult<plakous_gateway_protocol::HexDigest> {
    Ok(plakous_gateway_protocol::HexDigest::new(BINDING)?)
}

fn facts() -> TestResult<AccountFacts> {
    Ok(AccountFacts {
        account: plakous_gateway_protocol::GatewayAccount::new(ACCOUNT)?,
        instance: plakous_gateway_protocol::UuidText::new("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")?,
        observed_at_unix_ms: 1,
        credential: CredentialFacts {
            status: CredentialStatus::Ready,
            generation: Some(2),
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
        },
        identity: Some(IdentityFacts {
            account_binding: digest()?,
            credential_authority: digest()?,
            email: None,
            plan: None,
        }),
        wire_identity: plakous_gateway_protocol::facts::WireIdentity::default(),
        profile: None,
        key: None,
    })
}

pub(super) fn api_request(origin: Origin, authorization: HeaderValue) -> TestResult<Request> {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(HeaderName::from_static("authorization"), authorization);
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    Ok(Request::new(
        origin,
        Method::POST,
        "/v1/messages",
        headers,
        Bytes::from_static(b"{}"),
        &Limits::default(),
    )?)
}

pub(super) fn placeholder() -> TestResult<HeaderValue> {
    Ok(sensitive_header_value(custodied_placeholder(
        ACCOUNT, BINDING, GENERATION,
    ))?)
}

fn real_authorization() -> TestResult<HeaderValue> {
    Ok(sensitive_header_value(TOKEN.to_vec())?)
}

pub(super) fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(5))
}

pub(super) fn open_gateway() -> Result<FixtureGateway, FixtureError> {
    gateway()
}
