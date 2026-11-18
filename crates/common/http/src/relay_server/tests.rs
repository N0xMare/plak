//! The relay client and server core end to end over an in-memory gateway,
//! with a loopback fixture upstream behind the test-only dial override.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use plakous_gateway_protocol::envelope::RelayEnvelope;
use plakous_gateway_protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, WireIdentity,
};
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_gateway_protocol::{Endpoint, GatewayAccount, HexDigest, UuidText};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::{GatewayService, Injection, Peer, RelayServer, ServiceFuture};
use crate::relay::{
    GatewayEndpoint, InMemoryGateway, RelayCallError, RelayClient, custodied_placeholder,
};
use crate::{
    Client, ErrorKind, ExchangeControl, HeaderMap, HeaderName, HeaderValue, Limits, Method, Origin,
    Phase, Request, SensitiveBytes, TransmissionFact, sensitive_header_value,
};

const LOGICAL: &str = "https://api.example.test";
const ACCOUNT: &str = "example/work";
const SECRET: &[u8] = b"Bearer synthetic-secret";

fn binding() -> HexDigest {
    HexDigest::from_bytes(&[0xab; 32])
}

/// A fixture upstream: records every raw request and answers each with the
/// next scripted reply, then closes the connection.
struct Upstream {
    origin: Origin,
    requests: Arc<Mutex<Vec<Vec<u8>>>>,
    accepted: Arc<AtomicUsize>,
    closed: Arc<tokio::sync::Notify>,
}

#[derive(Clone)]
enum Script {
    /// Write these bytes, then close.
    Reply(Vec<u8>),
    /// Write a head and part of a body, then hang until the caller leaves.
    HangAfter(Vec<u8>),
}

async fn upstream(script: Script) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let accepted = Arc::new(AtomicUsize::new(0));
    let closed = Arc::new(tokio::sync::Notify::new());
    let (recorded, counter, notify) = (requests.clone(), accepted.clone(), closed.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let (recorded, notify, script) = (recorded.clone(), notify.clone(), script.clone());
            tokio::spawn(async move {
                let request = read_request(&mut socket).await;
                recorded.lock().unwrap().push(request);
                match script {
                    Script::Reply(reply) => {
                        let _ = socket.write_all(&reply).await;
                        let _ = socket.shutdown().await;
                    }
                    Script::HangAfter(prefix) => {
                        let _ = socket.write_all(&prefix).await;
                        let mut rest = [0_u8; 16];
                        while matches!(socket.read(&mut rest).await, Ok(read) if read > 0) {}
                        notify.notify_one();
                    }
                }
            });
        }
    });
    Upstream {
        origin: Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), port).unwrap(),
        requests,
        accepted,
        closed,
    }
}

/// Whether `bytes` hold one whole HTTP/1.1 message with a content length
/// (or none, for a bodiless one).
fn complete_message(bytes: &[u8]) -> bool {
    let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let length = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .map_or(0, |value| value.trim().parse::<usize>().unwrap());
    bytes.len() >= end + 4 + length
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length: "))
                .map_or(0, |value| value.trim().parse::<usize>().unwrap());
            if request.len() >= end + 4 + length {
                return request;
            }
        }
        match socket.read(&mut buffer).await {
            Ok(0) | Err(_) => return request,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }
}

fn ok_reply(content_type: &str, body: &[u8]) -> Vec<u8> {
    [
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\nx-upstream: kept\r\n\r\n",
            body.len()
        )
        .as_bytes(),
        body,
    ]
    .concat()
}

/// The test gateway's policy: one pinned origin and path prefix, one
/// custodied `authorization`, and a fence whose generation a test can move.
struct Service {
    generation: AtomicU64,
    moves_per_inject: AtomicU64,
    held: Arc<AtomicUsize>,
}

/// Counts itself held until dropped, as a store's Ready lease would be.
struct Hold(Arc<AtomicUsize>);

impl Drop for Hold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Service {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(7),
            moves_per_inject: AtomicU64::new(0),
            held: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn facts(&self) -> AccountFacts {
        AccountFacts {
            account: GatewayAccount::new(ACCOUNT).unwrap(),
            instance: UuidText::new("0b9e5f4c-7d3a-4e21-9f80-1a2b3c4d5e6f").unwrap(),
            observed_at_unix_ms: 1,
            credential: CredentialFacts {
                status: CredentialStatus::Ready,
                generation: Some(self.generation.load(Ordering::SeqCst)),
                access_expires_at_unix_ms: None,
                selection_epoch_digest: None,
            },
            identity: Some(plakous_gateway_protocol::facts::IdentityFacts {
                account_binding: binding(),
                credential_authority: binding(),
                email: None,
                plan: None,
            }),
            wire_identity: WireIdentity::default(),
            profile: None,
            key: None,
        }
    }
}

impl GatewayService for Service {
    type Principal = ();

    fn authenticate(&self, peer: &Peer, _key: Option<&[u8]>) -> Result<(), GatewayRefusal> {
        match peer {
            Peer::InProcess => Ok(()),
            _ => Err(GatewayRefusal::Unauthenticated {}),
        }
    }

    fn inject<'a>(
        &'a self,
        (): &'a (),
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection> {
        Box::pin(async move {
            if envelope.method != "POST" || !envelope.url.starts_with(&format!("{LOGICAL}/v1/")) {
                return Err(GatewayRefusal::UpstreamNotPinned {});
            }
            if envelope.custodied_names().collect::<Vec<_>>() != ["authorization"] {
                return Err(GatewayRefusal::CustodiedHeadersMismatch {});
            }
            let moves = self.moves_per_inject.load(Ordering::SeqCst);
            if moves > 0 {
                self.moves_per_inject.store(moves - 1, Ordering::SeqCst);
                self.generation.fetch_add(1, Ordering::SeqCst);
            }
            if envelope.expected.account_binding != binding() {
                return Err(GatewayRefusal::AccountChanged {});
            }
            if envelope.expected.generation != self.generation.load(Ordering::SeqCst) {
                return Err(GatewayRefusal::FenceMoved {
                    facts: Box::new(self.facts()),
                });
            }
            self.held.fetch_add(1, Ordering::SeqCst);
            Ok(Injection::new(vec![SensitiveBytes::new(SECRET.to_vec())])
                .holding(Box::new(Hold(self.held.clone()))))
        })
    }

    fn call<'a>(
        &'a self,
        (): &'a (),
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>> {
        Box::pin(async move {
            match endpoint {
                Endpoint::Facts => Ok(body.to_vec()),
                _ => Err(GatewayRefusal::ScopeDenied {}),
            }
        })
    }
}

fn gateway(service: Arc<Service>, upstream: &Upstream, peer: Peer) -> GatewayEndpoint {
    gateway_with(
        service,
        upstream,
        peer,
        Client::new_http1(Limits::default()).unwrap(),
    )
}

fn gateway_with(
    service: Arc<Service>,
    upstream: &Upstream,
    peer: Peer,
    client: Client,
) -> GatewayEndpoint {
    let server = RelayServer::new(service, client, super::new_instance().unwrap())
        .with_dial_override(LOGICAL, upstream.origin.clone());
    GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
        let server = server.clone();
        tokio::spawn(async move {
            let _ = server.serve(io, peer).await;
        });
    }))
}

fn relayed(endpoint: GatewayEndpoint) -> Client {
    Client::new_http1(Limits::default())
        .unwrap()
        .with_relay(Arc::new(RelayClient::new(endpoint, None).unwrap()))
}

fn headers(authorization: HeaderValue) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );
    headers.insert(HeaderName::from_static("authorization"), authorization);
    headers.append(
        HeaderName::from_static("x-order"),
        HeaderValue::from_static("one"),
    );
    headers.append(
        HeaderName::from_static("x-order"),
        HeaderValue::from_static("two"),
    );
    headers.insert(
        HeaderName::from_static("anthropic-version"),
        HeaderValue::from_static("2023-06-01"),
    );
    headers
}

fn placeholder(generation: u64) -> HeaderValue {
    sensitive_header_value(custodied_placeholder(
        ACCOUNT,
        binding().as_str(),
        generation,
    ))
    .unwrap()
}

fn request(origin: Origin, path: &str, authorization: HeaderValue) -> Request {
    Request::new(
        origin,
        Method::POST,
        path,
        headers(authorization),
        crate::Bytes::from_static(b"{\"model\":\"m\"}"),
        &Limits::default(),
    )
    .unwrap()
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(10))
}

async fn body_chunks(response: crate::Response) -> Result<Vec<Vec<u8>>, crate::Failure> {
    let mut body = response.into_body();
    let mut chunks = Vec::new();
    while let Some(chunk) = body.next_chunk().await {
        chunks.push(chunk?.to_vec());
    }
    Ok(chunks)
}

#[tokio::test]
async fn relayed_requests_reach_upstream_byte_identical_to_direct_ones() {
    let reply = ok_reply("application/json", b"{\"ok\":true}");
    let direct_upstream = upstream(Script::Reply(reply.clone())).await;
    let direct = Client::new_http1(Limits::default()).unwrap();
    let response = direct
        .execute(
            request(
                direct_upstream.origin.clone(),
                "/v1/messages?beta=true",
                sensitive_header_value(SECRET.to_vec()).unwrap(),
            ),
            control(),
        )
        .await
        .unwrap();
    let direct_body = body_chunks(response).await.unwrap().concat();

    let relay_upstream = upstream(Script::Reply(reply)).await;
    let client = relayed(gateway(Service::new(), &relay_upstream, Peer::InProcess));
    let response = client
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/messages?beta=true",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        response
            .headers()
            .get(&HeaderName::from_static("x-upstream"))
            .is_some()
    );
    assert!(
        response
            .headers()
            .get(&HeaderName::from_static("connection"))
            .is_none()
    );
    let relayed_body = body_chunks(response).await.unwrap().concat();
    assert_eq!(relayed_body, direct_body);

    // The direct request's Host names its loopback origin; the relayed one
    // names the override's, which is the same fixture authority shape.
    let normalize = |request: &[u8], origin: &Origin| {
        String::from_utf8(request.to_vec())
            .unwrap()
            .replace(origin.binding_authority(), "<host>")
    };
    let direct_request = normalize(
        &direct_upstream.requests.lock().unwrap()[0],
        &direct_upstream.origin,
    );
    let relayed_request = normalize(
        &relay_upstream.requests.lock().unwrap()[0],
        &relay_upstream.origin,
    );
    assert_eq!(relayed_request, direct_request);
    assert!(relayed_request.contains("authorization: Bearer synthetic-secret\r\n"));
    let order: Vec<&str> = relayed_request
        .lines()
        .skip(1)
        .map(|line| line.split(':').next().unwrap())
        .take_while(|name| !name.is_empty())
        .collect();
    assert_eq!(
        order,
        [
            "content-type",
            "authorization",
            "x-order",
            "x-order",
            "anthropic-version",
            "host",
            "content-length"
        ]
    );
}

#[tokio::test]
async fn a_direct_exchange_never_sends_a_custodied_placeholder() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    let failure = Client::new_http1(Limits::default())
        .unwrap()
        .execute(
            request(fixture.origin.clone(), "/v1/x", placeholder(7)),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(failure.kind(), ErrorKind::Internal);
    assert_eq!(failure.cause(), Some("custodied_placeholder"));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn event_streams_arrive_one_frame_per_event() {
    let events: &[u8] = b"event: a\ndata: 1\n\nevent: b\ndata: 2\n\n";
    let fixture = upstream(Script::Reply(ok_reply("text/event-stream", events))).await;
    let client = relayed(gateway(Service::new(), &fixture, Peer::InProcess));
    let response = client
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/stream",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    assert_eq!(
        body_chunks(response).await.unwrap(),
        vec![
            b"event: a\ndata: 1\n\n".to_vec(),
            b"event: b\ndata: 2\n\n".to_vec()
        ]
    );
}

#[tokio::test]
async fn refusals_are_unsent_and_keep_their_class() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    let logical = || Origin::parse(&format!("{LOGICAL}/")).unwrap();

    let unauthenticated = relayed(gateway(
        Service::new(),
        &fixture,
        Peer::Tcp("100.64.0.1:1".parse().unwrap()),
    ))
    .execute(request(logical(), "/v1/x", placeholder(7)), control())
    .await
    .unwrap_err();
    assert_eq!(unauthenticated.transmission(), TransmissionFact::NotStarted);

    let client = relayed(gateway(Service::new(), &fixture, Peer::InProcess));
    let not_pinned = client
        .execute(
            request(logical(), "/v2/elsewhere", placeholder(7)),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(not_pinned.transmission(), TransmissionFact::NotStarted);
    assert_eq!(not_pinned.kind(), ErrorKind::InvalidResponse);
    assert_eq!(not_pinned.cause(), Some("gateway_upstream_not_pinned"));

    let other = Origin::parse("https://elsewhere.example.test/").unwrap();
    let off_table = client
        .execute(request(other, "/v1/x", placeholder(7)), control())
        .await
        .unwrap_err();
    assert_eq!(off_table.cause(), Some("gateway_upstream_not_pinned"));

    let wrong_account = client
        .execute(
            request(
                logical(),
                "/v1/x",
                sensitive_header_value(custodied_placeholder(ACCOUNT, &"cd".repeat(32), 7))
                    .unwrap(),
            ),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(wrong_account.cause(), Some("gateway_account_changed"));

    let mut extra = headers(placeholder(7));
    extra.insert(HeaderName::from_static("x-api-key"), placeholder(7));
    let mismatch = client
        .execute(
            Request::new(
                logical(),
                Method::POST,
                "/v1/x",
                extra,
                crate::Bytes::new(),
                &Limits::default(),
            )
            .unwrap(),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(mismatch.cause(), Some("gateway_custodied_headers_mismatch"));

    let unsent = client
        .execute(
            request(
                logical(),
                "/v1/x",
                sensitive_header_value(SECRET.to_vec()).unwrap(),
            ),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(unsent.cause(), Some("relay_without_custody"));

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        fixture.accepted.load(Ordering::SeqCst),
        0,
        "no refusal dials upstream"
    );
}

#[tokio::test]
async fn a_moved_fence_is_prepared_again_at_most_twice() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"ok"))).await;
    let service = Service::new();
    service.moves_per_inject.store(2, Ordering::SeqCst);
    let client = relayed(gateway(service.clone(), &fixture, Peer::InProcess));
    let logical = || Origin::parse(&format!("{LOGICAL}/")).unwrap();
    let response = client
        .execute(request(logical(), "/v1/x", placeholder(7)), control())
        .await
        .unwrap();
    assert_eq!(body_chunks(response).await.unwrap().concat(), b"ok");
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 1);

    service.moves_per_inject.store(3, Ordering::SeqCst);
    let generation = service.generation.load(Ordering::SeqCst);
    let failure = client
        .execute(
            request(logical(), "/v1/x", placeholder(generation)),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(failure.cause(), Some("gateway_fence_moved"));
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn upstream_facts_before_the_head_arrive_verbatim() {
    // A closed port: the gateway's upstream connect fails unsent.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let fixture = Upstream {
        origin: Origin::loopback_http(IpAddr::V4(Ipv4Addr::LOCALHOST), closed).unwrap(),
        requests: Arc::default(),
        accepted: Arc::default(),
        closed: Arc::default(),
    };
    let failure = relayed(gateway(Service::new(), &fixture, Peer::InProcess))
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.phase(), Phase::Connect);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(failure.cause(), Some("gateway_upstream"));
}

#[tokio::test]
async fn a_break_after_the_head_may_have_transmitted_and_leaving_cancels_upstream() {
    let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: 1\n\n".to_vec();
    let fixture = upstream(Script::HangAfter(head)).await;
    let client = relayed(gateway(Service::new(), &fixture, Peer::InProcess));
    let response = client
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/stream",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    let mut body = response.into_body();
    assert_eq!(
        body.next_chunk().await.unwrap().unwrap().as_ref(),
        b"data: 1\n\n"
    );
    drop(body);
    tokio::time::timeout(Duration::from_secs(1), fixture.closed.notified())
        .await
        .expect("dropping the relayed body cancels the upstream exchange within 1 s");

    let truncated = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 100\r\n\r\n{\"partial\"".to_vec();
    let fixture = upstream(Script::Reply(truncated)).await;
    let client = relayed(gateway(Service::new(), &fixture, Peer::InProcess));
    let response = client
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    let failure = body_chunks(response).await.unwrap_err();
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
}

#[tokio::test]
async fn the_custodian_hold_lasts_until_the_upstream_head() {
    let head = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: 1\n\n".to_vec();
    let fixture = upstream(Script::HangAfter(head)).await;
    let service = Service::new();
    let holds = service.held.clone();
    let client = relayed(gateway(service, &fixture, Peer::InProcess));
    let response = client
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/stream",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    // The head has arrived while the body is still streaming: the gateway
    // released the hold at the head, not at the end of the body.
    assert_eq!(holds.load(Ordering::SeqCst), 0);
    let mut body = response.into_body();
    assert_eq!(
        body.next_chunk().await.unwrap().unwrap().as_ref(),
        b"data: 1\n\n"
    );
}

#[tokio::test]
async fn redirects_are_relayed_not_followed() {
    let redirect = b"HTTP/1.1 302 Found\r\nlocation: https://elsewhere.example.test/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".to_vec();
    let fixture = upstream(Script::Reply(redirect)).await;
    let response = relayed(gateway(Service::new(), &fixture, Peer::InProcess))
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 302);
    assert_eq!(body_chunks(response).await.unwrap(), Vec::<Vec<u8>>::new());
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn nothing_is_dialed_before_the_whole_request_is_read() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    let GatewayEndpoint::InMemory(gateway) = gateway(Service::new(), &fixture, Peer::InProcess)
    else {
        unreachable!()
    };
    let mut io = gateway.open();
    io.write_all(
        b"POST /v1/relay HTTP/1.1\r\nhost: gateway.invalid\r\ncontent-type: application/vnd.plak.relay\r\ncontent-length: 4096\r\n\r\n\x00\x00\x01\x00{",
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(io);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_lost_hop_maps_by_how_much_of_the_relay_request_the_gateway_holds() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    // The gateway end closes at once: the relay request was not sent whole.
    let refuse = GatewayEndpoint::InMemory(InMemoryGateway::new(drop));
    let failure = relayed(refuse)
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);

    // A gateway that answers `/versions`, then reads a relay request whole
    // and drops the connection: the upstream may have been sent.
    let lost = GatewayEndpoint::InMemory(InMemoryGateway::new(
        |mut io: tokio::io::DuplexStream| {
            tokio::spawn(async move {
                let mut whole = Vec::new();
                let mut buffer = [0_u8; 4096];
                loop {
                    match io.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(read) => whole.extend_from_slice(&buffer[..read]),
                    }
                    if !complete_message(&whole) {
                        continue;
                    }
                    if whole.starts_with(b"GET /versions") {
                        let body = br#"{"versions":[1],"instance":"0b9e5f4c-7d3a-4e21-9f80-1a2b3c4d5e6f"}"#;
                        let reply = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                            body.len()
                        );
                        let _ = io.write_all(reply.as_bytes()).await;
                        let _ = io.write_all(body).await;
                        whole.clear();
                    } else {
                        return;
                    }
                }
            });
        },
    ));
    let failure = relayed(lost)
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(fixture.accepted.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn calls_and_versions_are_path_versioned() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    let relay = RelayClient::new(gateway(Service::new(), &fixture, Peer::InProcess), None).unwrap();
    let echoed = relay
        .call(Endpoint::Facts, b"{\"accounts\":[]}".to_vec(), &control())
        .await
        .unwrap();
    assert_eq!(echoed, b"{\"accounts\":[]}");
    assert_eq!(
        relay
            .call(Endpoint::Admin, b"{}".to_vec(), &control())
            .await,
        Err(RelayCallError::Refused(GatewayRefusal::ScopeDenied {}))
    );

    let GatewayEndpoint::InMemory(gateway) = gateway(Service::new(), &fixture, Peer::InProcess)
    else {
        unreachable!()
    };
    let mut io = gateway.open();
    io.write_all(b"POST /v9/facts HTTP/1.1\r\nhost: g\r\ncontent-length: 2\r\n\r\n{}")
        .await
        .unwrap();
    let mut reply = Vec::new();
    let mut buffer = [0_u8; 4096];
    while !complete_message(&reply) {
        let read = io.read(&mut buffer).await.unwrap();
        assert!(read > 0, "the gateway answers before closing");
        reply.extend_from_slice(&buffer[..read]);
    }
    let reply = String::from_utf8_lossy(&reply).to_string();
    assert!(reply.starts_with("HTTP/1.1 426"), "{reply}");
    assert!(
        reply.contains("plak-gateway-error: version_unsupported"),
        "{reply}"
    );
    assert!(reply.contains("\"supported\":[1]"), "{reply}");
}

#[tokio::test]
async fn upstream_faults_apply_to_the_gateway_upstream_hop_only() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"x"))).await;
    // The plan names the dialed fixture host, as the gateway's own upstream
    // client sees it; the relaying client applies no plan of its own.
    let faults = crate::FaultPlan::parse("host=127.0.0.1,fault=429,retry-after=5").unwrap();
    let upstream_client = Client::new_http1(Limits::default())
        .unwrap()
        .with_faults(Some(faults));
    let relaying = Client::new_http1(Limits::default())
        .unwrap()
        .with_faults(Some(crate::FaultPlan::parse("fault=connect").unwrap()))
        .with_relay(Arc::new(
            RelayClient::new(
                gateway_with(Service::new(), &fixture, Peer::InProcess, upstream_client),
                None,
            )
            .unwrap(),
        ));
    let response = relaying
        .execute(
            request(
                Origin::parse(&format!("{LOGICAL}/")).unwrap(),
                "/v1/x",
                placeholder(7),
            ),
            control(),
        )
        .await
        .unwrap();
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(
        response
            .headers()
            .get(&HeaderName::from_static("retry-after"))
            .map(HeaderValue::as_bytes),
        Some(b"5".as_slice())
    );
    assert_eq!(
        fixture.accepted.load(Ordering::SeqCst),
        0,
        "the injected status never dials"
    );
}
