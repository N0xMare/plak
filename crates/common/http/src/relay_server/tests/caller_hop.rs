//! Caller connection lifecycle through the real relay client and server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use plakous_gateway_protocol::Endpoint;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader, DuplexStream};

use super::{
    Client, Duration, ErrorKind, GatewayEndpoint, InMemoryGateway, LOGICAL, Method, Origin, Peer,
    Phase, RelayClient, RelayServer, Script, Service, TransmissionFact, body_chunks, control,
    headers, ok_reply, placeholder, relayed, request, upstream,
};
use crate::Request;
use crate::relay_server::RelayOutcome;

/// Pass through the real gateway's messages until its outer relay response
/// head, then drop both ends while the upstream is still waiting.
async fn cut_before_first_frame(caller: DuplexStream, gateway: DuplexStream) {
    let (mut caller_read, mut caller_write) = tokio::io::split(caller);
    let (gateway_read, mut gateway_write) = tokio::io::split(gateway);
    let up = tokio::io::copy(&mut caller_read, &mut gateway_write);
    let down = async {
        let mut reader = BufReader::new(gateway_read);
        let mut is_relay = false;
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).await? == 0 {
                return Ok::<_, std::io::Error>(());
            }
            caller_write.write_all(&line).await?;
            is_relay |= line.strip_prefix(b"content-type: ").is_some_and(|value| {
                value.starts_with(plakous_gateway_protocol::RELAY_FRAMES_CONTENT_TYPE.as_bytes())
            });
            if is_relay && line == b"\r\n" {
                return Ok(());
            }
        }
    };
    tokio::select! {
        result = up => { result.unwrap(); }
        result = down => result.unwrap(),
    }
}

/// Only the selected connection is closing; other connections reach a
/// healthy server. Negotiation still goes through the real relay server.
async fn closing_gateway(cut_connection: usize) -> (Client, super::Upstream, Arc<Service>) {
    let idle_upstream = upstream(Script::HangAfter(Vec::new())).await;
    let healthy = upstream(Script::Reply(ok_reply("application/json", b"{}"))).await;
    let service = Service::new();
    let server = |origin: Origin| {
        RelayServer::new(
            service.clone(),
            Client::new_http1(crate::Limits::default()).unwrap(),
            super::super::new_instance().unwrap(),
        )
        .with_dial_override(LOGICAL, origin)
    };
    let closing = server(idle_upstream.origin);
    let serving = server(healthy.origin.clone());
    let next = AtomicUsize::new(0);
    let client = relayed(GatewayEndpoint::InMemory(InMemoryGateway::new(move |io| {
        if next.fetch_add(1, Ordering::SeqCst) == cut_connection {
            let closing = closing.clone();
            let (bridge, socket) = tokio::io::duplex(InMemoryGateway::BUFFER_BYTES);
            tokio::spawn(async move {
                let _ = closing.serve(socket, Peer::InProcess).await;
            });
            tokio::spawn(cut_before_first_frame(io, bridge));
        } else {
            let serving = serving.clone();
            tokio::spawn(async move {
                let _ = serving.serve(io, Peer::InProcess).await;
            });
        }
    })));
    (client, healthy, service)
}

#[tokio::test]
async fn each_gateway_call_and_relay_opens_one_connection() {
    let fixture = upstream(Script::Reply(ok_reply("text/plain", b"ok"))).await;
    let server = RelayServer::new(
        Service::new(),
        Client::new_http1(crate::Limits::default()).unwrap(),
        super::super::new_instance().unwrap(),
    )
    .with_dial_override(LOGICAL, fixture.origin.clone());
    let accepted = Arc::new(AtomicUsize::new(0));
    let endpoint = GatewayEndpoint::InMemory(InMemoryGateway::new({
        let accepted = Arc::clone(&accepted);
        move |io| {
            accepted.fetch_add(1, Ordering::Relaxed);
            let server = server.clone();
            tokio::spawn(async move {
                let _ = server.serve(io, Peer::InProcess).await;
            });
        }
    }));
    let relay = Arc::new(RelayClient::new(endpoint, None).unwrap());
    let client = Client::new_http1(crate::Limits::default())
        .unwrap()
        .with_relay(Arc::clone(&relay));
    for _ in 0..2 {
        let reply = relay
            .call(Endpoint::Facts, b"{}".to_vec(), &control())
            .await
            .unwrap();
        assert_eq!(reply, b"{}");
        let request = request(Origin::parse(LOGICAL).unwrap(), "/v1/x", placeholder(7));
        let response = client.execute(request, control()).await.unwrap();
        assert_eq!(body_chunks(response).await.unwrap().concat(), b"ok");
    }
    assert_eq!(
        accepted.load(Ordering::Relaxed),
        5,
        "versions, two facts calls, two relays"
    );
}

#[tokio::test]
async fn catalog_never_reuses_the_connection_left_idle_by_negotiation() {
    let (client, upstream, service) = closing_gateway(0).await;
    let request = Request::new(
        Origin::parse(LOGICAL).unwrap(),
        Method::GET,
        "/v1/catalog",
        headers(placeholder(7)),
        bytes::Bytes::new(),
        client.limits(),
    )
    .unwrap();
    let response = client.execute(request, control()).await.unwrap();
    assert_eq!(body_chunks(response).await.unwrap().concat(), b"{}");
    assert_eq!(upstream.requests.lock().unwrap().len(), 1);
    assert_eq!(*service.audited.lock().unwrap(), [RelayOutcome::Complete]);
}

#[tokio::test]
async fn an_admitted_post_losing_its_relay_head_is_never_resent() {
    let (client, healthy, service) = closing_gateway(1).await;
    let request = request(Origin::parse(LOGICAL).unwrap(), "/v1/x", placeholder(7));
    let failure = client.execute(request, control()).await.unwrap_err();
    assert_eq!(failure.phase(), Phase::ResponseHead);
    assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
    assert_eq!(failure.kind(), ErrorKind::Protocol);
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !service.audited.lock().unwrap().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(*service.audited.lock().unwrap(), [RelayOutcome::Abandoned]);
    assert_eq!(healthy.accepted.load(Ordering::SeqCst), 0);
}
