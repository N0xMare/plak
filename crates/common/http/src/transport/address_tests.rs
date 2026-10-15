//! Connect-path tests for [`AddressPolicy`] with a scripted resolver: the
//! policy decides the exact address that is connected, on every connection.

use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;

use super::{Client, Resolver};
use crate::{
    AddressPolicy, Bytes, ErrorKind, ExchangeControl, HeaderMap, Limits, Method, Origin, Phase,
    Request, TransmissionFact,
};

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// A TCP peer that counts accepted connections and drops them, so a TLS
/// handshake against it fails after the connect.
async fn counting_peer() -> TestResult<(SocketAddr, Arc<AtomicUsize>)> {
    let listener = TcpListener::bind((LOOPBACK, 0)).await?;
    let address = listener.local_addr()?;
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            drop(stream);
        }
    });
    Ok((address, accepted))
}

/// Answers the n-th lookup with `answers[n]` (the last answer repeats) and
/// counts lookups.
fn scripted(answers: Vec<Vec<SocketAddr>>) -> (Resolver, Arc<AtomicUsize>) {
    let lookups = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&lookups);
    let resolver = Resolver::Scripted(Arc::new(move |_host: &str, _port: u16| {
        let call = counter.fetch_add(1, Ordering::SeqCst);
        answers
            .get(call)
            .or_else(|| answers.last())
            .cloned()
            .unwrap_or_default()
    }));
    (resolver, lookups)
}

fn control() -> ExchangeControl {
    ExchangeControl::new(Instant::now() + Duration::from_secs(5))
}

fn request(origin: &Origin) -> TestResult<Request> {
    Ok(Request::new(
        origin.clone(),
        Method::GET,
        "/",
        HeaderMap::new(),
        Bytes::new(),
        &Limits::default(),
    )?)
}

fn client(resolver: Resolver, policy: Option<AddressPolicy>) -> TestResult<Client> {
    Ok(Client::new(Limits::default())?
        .with_resolver(resolver)
        .with_address_policy(policy))
}

fn private(port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, 1], port))
}

fn assert_refused(failure: crate::Failure) {
    assert_eq!(failure.phase(), Phase::Resolution);
    assert_eq!(failure.kind(), ErrorKind::AddressRefused);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
}

#[tokio::test]
async fn public_only_refuses_a_private_resolution_without_connecting() -> TestResult {
    let (peer, accepted) = counting_peer().await?;
    let (resolver, _) = scripted(vec![vec![peer, private(peer.port())]]);
    let client = client(resolver, Some(AddressPolicy::public_only()))?;
    let origin = Origin::parse(&format!("https://service.example:{}", peer.port()))?;
    let failure = client
        .execute(request(&origin)?, control())
        .await
        .err()
        .ok_or("a private-only resolution must be refused")?;
    assert_refused(failure);
    let failure = client
        .probe(&origin, control())
        .await
        .err()
        .ok_or("the probe must be refused too")?;
    assert_refused(failure);
    assert_eq!(accepted.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn the_first_admitted_address_is_the_one_connected() -> TestResult {
    let (peer, accepted) = counting_peer().await?;
    let (resolver, _) = scripted(vec![vec![private(peer.port()), peer]]);
    let client = client(resolver, Some(AddressPolicy::only(LOOPBACK)))?;
    let origin = Origin::parse(&format!("https://service.example:{}", peer.port()))?;
    let failure = client
        .probe(&origin, control())
        .await
        .err()
        .ok_or("the counting peer does not speak TLS")?;
    assert_eq!(failure.phase(), Phase::Tls);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn rebinding_to_a_refused_address_is_refused_on_the_next_connection() -> TestResult {
    let (peer, accepted) = counting_peer().await?;
    // The first lookup answers the admitted address; the second rebinds the
    // same name to a refused one.
    let (resolver, lookups) = scripted(vec![vec![peer], vec![private(peer.port())]]);
    let client = client(resolver, Some(AddressPolicy::only(LOOPBACK)))?;
    let origin = Origin::parse(&format!("https://rebind.example:{}", peer.port()))?;
    let first = client
        .execute(request(&origin)?, control())
        .await
        .err()
        .ok_or("the counting peer does not speak TLS")?;
    assert_eq!(first.phase(), Phase::Tls);
    let second = client
        .execute(request(&origin)?, control())
        .await
        .err()
        .ok_or("the rebound resolution must be refused")?;
    assert_refused(second);
    assert_eq!(lookups.load(Ordering::SeqCst), 2);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn refused_host_names_are_never_resolved() -> TestResult {
    let (resolver, lookups) = scripted(vec![vec![SocketAddr::from(([8, 8, 8, 8], 443))]]);
    let client = client(resolver, Some(AddressPolicy::public_only()))?;
    for host in ["localhost", "api.localhost", "metadata.google.internal"] {
        let origin = Origin::parse(&format!("https://{host}"))?;
        let failure = client
            .probe(&origin, control())
            .await
            .err()
            .ok_or("a refused host name must fail")?;
        assert_refused(failure);
    }
    assert_eq!(lookups.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn without_a_policy_the_first_resolved_address_is_connected() -> TestResult {
    let (peer, accepted) = counting_peer().await?;
    let (resolver, _) = scripted(vec![vec![peer, private(peer.port())]]);
    let client = client(resolver, None)?;
    assert_eq!(client.address_policy(), None);
    let origin = Origin::parse("https://localhost")?;
    let failure = client
        .probe(&origin, control())
        .await
        .err()
        .ok_or("the counting peer does not speak TLS")?;
    assert_eq!(failure.phase(), Phase::Tls);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test]
async fn an_empty_resolution_stays_a_resolution_failure() -> TestResult {
    let (resolver, _) = scripted(vec![Vec::new()]);
    let client = client(resolver, Some(AddressPolicy::public_only()))?;
    let failure = client
        .probe(&Origin::parse("https://empty.example")?, control())
        .await
        .err()
        .ok_or("no address must fail")?;
    assert_eq!(failure.phase(), Phase::Resolution);
    assert_eq!(failure.kind(), ErrorKind::Resolution);
    Ok(())
}

#[test]
fn the_address_policy_is_part_of_the_transport_behavior_identity() -> TestResult {
    let plain = Client::new(Limits::default())?;
    let guarded = plain.with_address_policy(Some(AddressPolicy::public_only()));
    assert_ne!(plain.behavior_identity(), guarded.behavior_identity());
    assert_eq!(
        plain.behavior_identity(),
        guarded.with_address_policy(None).behavior_identity()
    );
    assert_eq!(
        guarded.pooled().behavior_identity(),
        plain
            .pooled()
            .with_address_policy(Some(AddressPolicy::public_only()))
            .behavior_identity()
    );
    Ok(())
}
