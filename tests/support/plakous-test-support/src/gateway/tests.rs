use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use plakous_gateway_protocol::call;
use plakous_gateway_protocol::envelope::{
    ExpectedFence, RelayEnvelope, RelayLimits, encode_request,
};
use plakous_gateway_protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, IdentityFacts,
};
use plakous_gateway_protocol::frame::{Frame, FrameDecoder};
use plakous_gateway_protocol::header::{RequestHeader, WireHeader};
use plakous_gateway_protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous_gateway_protocol::{Endpoint, PROTOCOL_VERSION, VersionsReply};

use super::{FixtureError, FixtureGateway, GatewayScript, render_upstream, serve_one, substitute};

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const TOKEN: &[u8] = b"secret-token";
const DIRECT: &str = "\
POST /v1/messages HTTP/1.1\r\n\
host: api.anthropic.com\r\n\
content-type: application/json\r\n\
authorization: secret-token\r\n\
anthropic-version: 2023-06-01\r\n\
content-length: 2\r\n\
\r\n\
{}";

#[tokio::test]
async fn versions_follow_the_instance_across_restart() -> TestResult {
    let gateway = gateway()?;
    let (bytes, report) = exchange(&gateway, b"GET /versions HTTP/1.1\r\nhost: g\r\n\r\n").await?;
    assert!(!report.dialed);
    let reply: VersionsReply = call::decode(body(&bytes)?)?;
    assert_eq!(reply.versions, vec![PROTOCOL_VERSION]);
    assert_eq!(reply.instance, gateway.instance());
    let restarted = gateway.restart()?;
    assert_ne!(restarted, reply.instance);
    let (bytes, _) = exchange(&gateway, b"GET /versions HTTP/1.1\r\nhost: g\r\n\r\n").await?;
    let reply: VersionsReply = call::decode(body(&bytes)?)?;
    assert_eq!(reply.instance, restarted);
    Ok(())
}

#[tokio::test]
async fn a_partial_body_never_dials() -> TestResult {
    let gateway = gateway()?;
    let (client, server) = tokio::io::duplex(8 * 1024);
    let running = gateway.clone();
    let served = tokio::spawn(async move { serve_one(server, &running).await });
    let (read, mut write) = tokio::io::split(client);
    let header = b"POST /v1/relay HTTP/1.1\r\nhost: g\r\ncontent-length: 40\r\n\r\n";
    write.write_all(header).await?;
    write.write_all(b"short").await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(gateway.dials(), 0);
    drop(write);
    drop(read);
    let report = served.await?;
    assert!(matches!(report, Err(FixtureError::Incomplete)));
    assert_eq!(gateway.dials(), 0);
    assert!(gateway.captured().is_empty());
    Ok(())
}

#[tokio::test]
async fn relay_substitutes_in_place_and_frames_round_trip() -> TestResult {
    let gateway = gateway()?;
    let (bytes, report) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(report.dialed);
    assert_eq!(gateway.dials(), 1);
    let captured = gateway.captured();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].http, DIRECT.as_bytes());
    assert_eq!(captured[0].headers[1].value(), TOKEN);
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(body(&bytes)?)?;
    assert!(matches!(decoder.next_frame()?, Some(Frame::Head(_))));
    assert!(matches!(decoder.next_frame()?, Some(Frame::Data(_))));
    assert!(matches!(decoder.next_frame()?, Some(Frame::End(_))));
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
            RequestHeader::Value(headers[0].clone()),
            RequestHeader::custodied("authorization")?,
            RequestHeader::Value(headers[2].clone()),
        ],
        &[("authorization".to_owned(), TOKEN.to_vec())],
    )?;
    let relayed = render_upstream(
        "POST",
        "https://api.anthropic.com/v1/messages",
        &relayed,
        b"{}",
    )?;
    assert_eq!(direct, DIRECT.as_bytes());
    assert_eq!(relayed, direct);
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
    let refusal: RefusalBody = call::decode(body(&bytes)?)?;
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
    assert!(bytes.is_empty());
    assert_eq!(gateway.dials(), 0);
    Ok(())
}

#[tokio::test]
async fn a_drop_after_head_dials_once_and_omits_end() -> TestResult {
    let gateway = gateway()?;
    gateway.push(GatewayScript::DropAfterHead);
    let (bytes, report) = exchange(
        &gateway,
        &http_post(&Endpoint::Relay.path(1), &relay_body()?),
    )
    .await?;
    assert!(report.dialed);
    assert_eq!(gateway.dials(), 1);
    let mut decoder = FrameDecoder::new(1024);
    decoder.push(body(&bytes)?)?;
    assert!(matches!(decoder.next_frame()?, Some(Frame::Head(_))));
    assert!(decoder.next_frame()?.is_none());
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
    let refusal: RefusalBody = call::decode(body(&bytes)?)?;
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
    let refusal: RefusalBody = call::decode(body(&bytes)?)?;
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
    let (client, server) = tokio::io::duplex(64 * 1024);
    let running = gateway.clone();
    let served = tokio::spawn(async move { serve_one(server, &running).await });
    let (mut read, mut write) = tokio::io::split(client);
    write.write_all(request).await?;
    write.shutdown().await?;
    let mut response = Vec::new();
    read.read_to_end(&mut response).await?;
    let report = served.await??;
    Ok((response, report))
}

pub(super) fn gateway() -> Result<FixtureGateway, FixtureError> {
    FixtureGateway::new(credentials(), None)
}

fn credentials() -> Vec<(String, Vec<u8>)> {
    vec![("authorization".to_owned(), TOKEN.to_vec())]
}

fn http_post(path: &str, body: &[u8]) -> Vec<u8> {
    let mut bytes = format!(
        "POST {path} HTTP/1.1\r\nhost: gateway.invalid\r\n\
         content-type: application/vnd.plak.relay\r\ncontent-length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

fn body(response: &[u8]) -> TestResult<&[u8]> {
    let index = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or("response has no head")?;
    Ok(&response[index + 4..])
}

pub(super) fn relay_body() -> TestResult<Vec<u8>> {
    let envelope = RelayEnvelope {
        operation: plakous_gateway_protocol::UuidText::new("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")?,
        attempt: 1,
        account: plakous_gateway_protocol::GatewayAccount::new("anthropic")?,
        expected: ExpectedFence {
            account_binding: digest()?,
            generation: 1,
        },
        method: "POST".to_owned(),
        url: "https://api.anthropic.com/v1/messages".to_owned(),
        headers: vec![
            RequestHeader::Value(WireHeader::new(
                "content-type",
                b"application/json".as_slice(),
            )?),
            RequestHeader::custodied("authorization")?,
            RequestHeader::Value(WireHeader::new(
                "anthropic-version",
                b"2023-06-01".as_slice(),
            )?),
        ],
        body_len: 2,
        limits: RelayLimits {
            head_timeout_ms: 1_000,
            body_idle_timeout_ms: 1_000,
            max_response_bytes: 4_096,
            max_chunk_bytes: 4_096,
        },
        deadline_unix_ms: None,
    };
    Ok(encode_request(&envelope, b"{}")?)
}

fn digest() -> TestResult<plakous_gateway_protocol::HexDigest> {
    Ok(plakous_gateway_protocol::HexDigest::new("ab".repeat(32))?)
}

fn facts() -> TestResult<AccountFacts> {
    Ok(AccountFacts {
        account: plakous_gateway_protocol::GatewayAccount::new("anthropic")?,
        instance: plakous_gateway_protocol::UuidText::new("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")?,
        facts_revision: 2,
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
