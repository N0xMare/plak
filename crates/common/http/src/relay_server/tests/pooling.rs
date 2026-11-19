use super::{
    ACCOUNT, LOGICAL, Peer, RelayClient, RelayServer, SECOND_ORIGIN, Service, binding, body_chunks,
    control, endpoint, placeholder, read_request, relayed, request,
};
use crate::relay::custodied_placeholder;
use crate::transport::test_tls::{Endpoint, endpoint_with_versions};
use crate::{
    Client, ErrorKind, Failure, FaultPlan, HeaderValue, Limits, Origin, Phase, Response,
    TransmissionFact, sensitive_header_value,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::Notify;

const EVENT: &[u8] = b"data: hello\n\n";

struct TlsPeer {
    origin: Origin,
    config: rustls::ClientConfig,
    accepts: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    aborted: Arc<Notify>,
}

impl TlsPeer {
    async fn start() -> Self {
        let Endpoint {
            listener,
            acceptor,
            client: config,
            origin,
        } = endpoint_with_versions(b"http/1.1", &[&rustls::version::TLS13])
            .await
            .unwrap();
        let accepts = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let aborted = Arc::new(Notify::new());
        let (accepted, seen, closed) = (accepts.clone(), requests.clone(), aborted.clone());
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                let (acceptor, seen, closed) = (acceptor.clone(), seen.clone(), closed.clone());
                tokio::spawn(async move {
                    let Ok(mut socket) = acceptor.accept(socket).await else {
                        return;
                    };
                    loop {
                        let request = read_request(&mut socket).await;
                        if !super::complete_message(&request) {
                            return;
                        }
                        seen.fetch_add(1, Ordering::SeqCst);
                        if request.starts_with(b"POST /v1/drop ") {
                            return;
                        }
                        let hangs = request.starts_with(b"POST /v1/hang ");
                        let truncated = request.starts_with(b"POST /v1/truncated ");
                        let length = if hangs || truncated {
                            1024
                        } else {
                            EVENT.len()
                        };
                        let connection = if request.starts_with(b"POST /v1/close ") {
                            "connection: close\r\n"
                        } else {
                            ""
                        };
                        let mut reply = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {length}\r\n{connection}\r\n"
                        )
                        .into_bytes();
                        reply.extend_from_slice(EVENT);
                        if socket.write_all(&reply).await.is_err() {
                            return;
                        }
                        if truncated {
                            let _ = socket.shutdown().await;
                            return;
                        }
                        if hangs {
                            assert!(!matches!(socket.read(&mut [0_u8; 1]).await, Ok(1)));
                            closed.notify_one();
                            return;
                        }
                    }
                });
            }
        });
        Self {
            origin,
            config,
            accepts,
            requests,
            aborted,
        }
    }

    fn upstream(&self) -> Client {
        Client::with_tls_config(Limits::default(), self.config.clone()).unwrap()
    }

    fn relay(&self, service: Arc<Service>, upstream: Client) -> RelayServer<Service> {
        RelayServer::new(service, upstream, super::super::new_instance().unwrap())
            .with_dial_override(LOGICAL, self.origin.clone())
            .with_dial_override(SECOND_ORIGIN, self.origin.clone())
    }

    fn counts(&self) -> (usize, usize) {
        (
            self.accepts.load(Ordering::SeqCst),
            self.requests.load(Ordering::SeqCst),
        )
    }
}

fn via(server: &RelayServer<Service>) -> Client {
    relayed(endpoint(server.clone(), Peer::InProcess))
}

async fn send(
    client: &Client,
    origin: &str,
    path: &str,
    auth: HeaderValue,
) -> Result<Response, Failure> {
    let origin = Origin::parse(origin).unwrap();
    client.execute(request(origin, path, auth), control()).await
}

async fn drain(client: &Client, origin: &str, path: &str, auth: HeaderValue) {
    let response = send(client, origin, path, auth).await.unwrap();
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(body_chunks(response).await.unwrap(), [EVENT]);
}

#[tokio::test]
async fn drained_relays_reuse_one_tls_handshake_but_unmarked_relays_do_not() {
    let peer = TlsPeer::start().await;
    let reusable = peer.relay(Service::new(), peer.upstream());
    drain(&via(&reusable), LOGICAL, "/v1/x", placeholder(7)).await;
    drain(&via(&reusable), LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (1, 2), "two relays, one upstream handshake");

    let one_shot = peer.relay(Service::new(), peer.upstream().pooled());
    for _ in 0..2 {
        drain(&via(&one_shot), LOGICAL, "/v1/one-shot", placeholder(7)).await;
    }
    assert_eq!(
        peer.counts(),
        (3, 4),
        "unmarked relays do not use either pool"
    );
}

#[tokio::test]
async fn an_aborted_stream_on_a_reused_socket_is_discarded() {
    let peer = TlsPeer::start().await;
    let client = via(&peer.relay(Service::new(), peer.upstream()));
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    let mut body = send(&client, LOGICAL, "/v1/hang", placeholder(7))
        .await
        .unwrap()
        .into_body();
    assert_eq!(body.next_chunk().await.unwrap().unwrap().as_ref(), EVENT);
    assert_eq!(peer.counts(), (1, 2));
    drop(body);
    tokio::time::timeout(Duration::from_secs(5), peer.aborted.notified())
        .await
        .unwrap();

    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (2, 3));
}

#[tokio::test]
async fn clean_close_and_truncated_responses_never_park_a_socket() {
    let peer = TlsPeer::start().await;
    let server = peer.relay(Service::new(), peer.upstream().with_faults(None));
    let client = via(&server);
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(server.inner.upstream_pool.idle_count(), 1);

    let mut body = send(&client, LOGICAL, "/v1/truncated", placeholder(7))
        .await
        .unwrap()
        .into_body();
    assert_eq!(body.next_chunk().await.unwrap().unwrap().as_ref(), EVENT);
    let failure = body.next_chunk().await.unwrap().unwrap_err();
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(peer.counts(), (1, 2));
    assert_eq!(server.inner.upstream_pool.idle_count(), 0);

    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    drain(&client, LOGICAL, "/v1/close", placeholder(7)).await;
    assert_eq!(peer.counts(), (2, 4));
    assert_eq!(server.inner.upstream_pool.idle_count(), 0);
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (3, 5));
}

#[tokio::test]
async fn upstream_origin_and_tls_configuration_each_partition_the_pool() {
    let peer = TlsPeer::start().await;
    let server = peer.relay(Service::new(), peer.upstream());
    let client = via(&server);
    for origin in [LOGICAL, SECOND_ORIGIN, LOGICAL] {
        drain(&client, origin, "/v1/x", placeholder(7)).await;
    }
    assert_eq!(
        peer.counts(),
        (2, 3),
        "distinct logical origins, even on the same test dial target"
    );

    // Keep the pool and both TLS configurations alive. Production never
    // substitutes one gateway's upstream client; this exercises the key itself.
    let other_config = server
        .clone()
        .rebuild(|inner| inner.upstream = peer.upstream());
    drain(&via(&other_config), LOGICAL, "/v1/x", placeholder(7)).await;
    let untrusted = server.clone().rebuild(|inner| {
        inner.upstream = Client::new_http1(Limits::default()).unwrap();
    });
    let failure = send(&via(&untrusted), LOGICAL, "/v1/x", placeholder(7))
        .await
        .unwrap_err();
    assert_eq!(failure.phase(), Phase::Tls);
    assert_eq!(failure.kind(), ErrorKind::Tls);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (4, 5));
}

#[tokio::test]
async fn a_gateway_socket_is_not_reused_at_the_five_second_idle_bound() {
    let peer = TlsPeer::start().await;
    let server = peer.relay(Service::new(), peer.upstream());
    let client = via(&server);
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    server.inner.upstream_pool.age(Duration::from_secs(5));
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (2, 2));
}

#[tokio::test]
async fn a_gateway_does_not_retry_an_injected_or_peer_failure_on_reuse() {
    let peer = TlsPeer::start().await;
    let plan = FaultPlan::parse("nth=2,fault=reset,after-events=1").unwrap();
    let client = via(&peer.relay(Service::new(), peer.upstream().with_faults(Some(plan))));
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    let response = send(&client, LOGICAL, "/v1/x", placeholder(7))
        .await
        .unwrap();
    let failure = body_chunks(response).await.unwrap_err();
    assert_eq!(failure.phase(), Phase::ResponseBody);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(
        peer.counts(),
        (1, 2),
        "there was no replay after the injected reset"
    );

    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(peer.counts(), (2, 3), "the faulted socket was discarded");
    let failure = send(&client, LOGICAL, "/v1/drop", placeholder(7))
        .await
        .unwrap_err();
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(
        peer.counts(),
        (2, 4),
        "the peer read the whole request before dropping the socket"
    );
}

#[tokio::test]
async fn warm_sockets_do_not_bypass_permits_or_cross_authority_fences() {
    let peer = TlsPeer::start().await;
    let service = Service::new();
    let server = peer.relay(service.clone(), peer.upstream());
    let client = via(&server);
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;

    let denied = relayed(endpoint(server, Peer::Tcp("127.0.0.1:1".parse().unwrap())));
    let failure = send(&denied, LOGICAL, "/v1/x", placeholder(7))
        .await
        .unwrap_err();
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    service.permit.store(false, Ordering::SeqCst);
    let failure = send(&client, LOGICAL, "/v1/x", placeholder(7))
        .await
        .unwrap_err();
    assert_eq!(failure.cause(), Some("gateway_scope_denied"));
    service.permit.store(true, Ordering::SeqCst);

    let wrong =
        sensitive_header_value(custodied_placeholder(ACCOUNT, &"cd".repeat(32), 7)).unwrap();
    let failure = send(&client, LOGICAL, "/v1/x", wrong).await.unwrap_err();
    assert_eq!(failure.cause(), Some("gateway_account_changed"));
    let failure = send(&client, LOGICAL, "/outside-pin", placeholder(7))
        .await
        .unwrap_err();
    assert_eq!(failure.cause(), Some("gateway_upstream_not_pinned"));
    assert_eq!(peer.counts(), (1, 1), "refusals never reach the peer");

    let other = sensitive_header_value(custodied_placeholder(
        "example/other",
        binding().as_str(),
        7,
    ))
    .unwrap();
    drain(&client, LOGICAL, "/v1/x", other).await;
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;
    assert_eq!(
        peer.counts(),
        (2, 3),
        "different slots with the same fence do not share"
    );

    service.generation.store(8, Ordering::SeqCst);
    drain(&client, LOGICAL, "/v1/x", placeholder(8)).await;
    assert_eq!(
        peer.counts(),
        (3, 4),
        "the same slot's new generation does not reuse"
    );
}

#[tokio::test]
async fn a_new_account_binding_in_the_same_slot_does_not_reuse_the_socket() {
    let peer = TlsPeer::start().await;
    let service = Service::new();
    let client = via(&peer.relay(service.clone(), peer.upstream()));
    drain(&client, LOGICAL, "/v1/x", placeholder(7)).await;

    let new_binding = plakous_gateway_protocol::HexDigest::from_bytes(&[0xcd; 32]);
    *service.binding.lock().unwrap() = new_binding.clone();
    let authorization =
        sensitive_header_value(custodied_placeholder(ACCOUNT, new_binding.as_str(), 7)).unwrap();
    drain(&client, LOGICAL, "/v1/x", authorization.clone()).await;
    drain(&client, LOGICAL, "/v1/x", authorization).await;
    assert_eq!(peer.counts(), (2, 3));
}

#[tokio::test]
async fn each_gateway_protocol_version_reuses_only_its_own_socket() {
    use plakous_gateway_protocol::{PREVIOUS_VERSION, PROTOCOL_VERSION};

    let peer = TlsPeer::start().await;
    let server = peer.relay(Service::new(), peer.upstream());
    let for_version = |version| {
        let gateway = endpoint(server.clone(), Peer::InProcess);
        let relay = RelayClient::new(gateway, None)
            .unwrap()
            .with_test_version(version);
        Client::new_http1(Limits::default())
            .unwrap()
            .with_relay(Arc::new(relay))
    };
    let previous = for_version(PREVIOUS_VERSION);
    let current = for_version(PROTOCOL_VERSION);
    for client in [&previous, &current, &previous, &current] {
        drain(client, LOGICAL, "/v1/x", placeholder(7)).await;
    }
    assert_eq!(peer.counts(), (2, 4));
}
