use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::transport::{Resolver, TaskGuard, resolve, wait_task};
use crate::{ConnectAttempts, ExchangeControl, Origin};

#[tokio::test(start_paused = true)]
async fn refused_addresses_never_reach_the_racing_connector() -> TestResult {
    let admitted = SocketAddr::from(([8, 8, 8, 8], 443));
    let answers = vec![
        SocketAddr::from(([10, 0, 0, 1], 443)),
        admitted,
        "[fc00::1]:443".parse()?,
    ];
    let resolver = Resolver::Scripted(Arc::new(move |_, _| answers.clone()));
    let targets = resolve(
        resolver,
        &Origin::parse("https://service.example")?,
        Some(crate::AddressPolicy::public_only()),
    )
    .await?;
    assert_eq!(targets, [admitted]);
    let mut connected = Vec::new();
    let failure = race(targets, |address| {
        connected.push(address);
        async { Err::<(), _>(std::io::Error::from(std::io::ErrorKind::ConnectionRefused)) }
    })
    .await;
    assert!(failure.is_err());
    assert_eq!(connected, [admitted]);
    Ok(())
}

#[tokio::test]
async fn duplicate_resolution_answers_preserve_unique_family_order() -> TestResult {
    let v4a = SocketAddr::from(([192, 0, 2, 1], 443));
    let v4b = SocketAddr::from(([192, 0, 2, 2], 443));
    let v6a: SocketAddr = "[2001:db8::1]:443".parse()?;
    let v6b: SocketAddr = "[2001:db8::2]:443".parse()?;
    let resolver = Resolver::Scripted(Arc::new(move |_, _| vec![v4a, v4b, v6a, v4a, v6b, v6a]));
    assert_eq!(
        resolve(resolver, &Origin::parse("https://service.example")?, None).await?,
        [v4a, v6a, v4b, v6b]
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn client_connect_deadline_bounds_all_blackholed_addresses() -> TestResult {
    let limits = crate::Limits {
        connect_timeout: Duration::from_secs(2),
        ..crate::Limits::default()
    };
    let mut client = crate::Client::new(limits.clone())?
        .with_resolver(Resolver::Scripted(Arc::new(|_, _| addresses())));
    let dropped = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&dropped);
    Arc::get_mut(&mut client.inner)
        .ok_or("unique client")?
        .connector = Connector::Scripted(Arc::new(move |_| {
        let guard = Dropped(Arc::clone(&counter));
        Box::pin(async move {
            let _guard = guard;
            std::future::pending().await
        })
    }));
    let started = tokio::time::Instant::now();
    let request = crate::Request::new(
        Origin::parse("https://service.example")?,
        crate::Method::GET,
        "/",
        crate::HeaderMap::new(),
        crate::Bytes::new(),
        &limits,
    )?;
    let failure = client
        .execute(
            request,
            ExchangeControl::new((started + Duration::from_secs(300)).into_std()),
        )
        .await
        .err()
        .ok_or("blackholes time out")?;
    assert_eq!(
        (failure.phase(), failure.kind(), failure.transmission()),
        (
            Phase::Connect,
            ErrorKind::DeadlineExceeded,
            TransmissionFact::NotStarted
        )
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        limits.connect_timeout
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    Ok(())
}

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn addresses() -> Vec<SocketAddr> {
    vec![
        SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 443)),
        SocketAddr::from(([192, 0, 2, 1], 443)),
    ]
}

struct Dropped(Arc<AtomicUsize>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(start_paused = true)]
async fn blackholed_first_address_loses_at_the_stagger_and_is_cancelled() -> TestResult {
    let origin = Origin::parse("https://service.example")?;
    let resolver = Resolver::Scripted(Arc::new(|host, port| {
        assert_eq!((host, port), ("service.example", 443));
        let addresses = addresses();
        vec![addresses[0], addresses[0], addresses[1]]
    }));
    let targets = resolve(resolver, &origin, None).await?;
    assert_eq!(targets, addresses());
    let started = tokio::time::Instant::now();
    let dropped = Arc::new(AtomicUsize::new(0));
    let winner = tokio::time::timeout(
        Duration::from_secs(1),
        race(targets, |address| {
            let guard = Dropped(Arc::clone(&dropped));
            async move {
                let _guard = guard;
                if address.is_ipv6() {
                    std::future::pending::<()>().await;
                }
                Ok(address)
            }
        }),
    )
    .await??;
    assert_eq!(winner, addresses()[1]);
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_millis(250)
    );
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    assert_eq!(origin.host(), "service.example");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn all_failed_addresses_name_both_attempted_families() -> TestResult {
    let started = tokio::time::Instant::now();
    let failure = race(addresses(), |_| async {
        Err::<(), _>(std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    })
    .await
    .err()
    .ok_or("all addresses failed")?;
    assert_eq!(failure.phase(), Phase::Connect);
    assert_eq!(failure.kind(), ErrorKind::Io);
    assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
    assert_eq!(
        failure.connect_attempts(),
        Some(ConnectAttempts { ipv4: 1, ipv6: 1 })
    );
    assert!(failure.to_string().contains("IPv4=1, IPv6=1"));
    assert_eq!(
        tokio::time::Instant::now(),
        started,
        "failure accelerates the next attempt"
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn racing_keeps_the_overall_connect_deadline_and_cancels_every_attempt() -> TestResult {
    for absolute_bound in [false, true] {
        let started = tokio::time::Instant::now();
        let bound = Duration::from_secs(1);
        let control = ExchangeControl::new(
            (started + if absolute_bound { bound } else { bound * 2 }).into_std(),
        );
        let dropped = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&dropped);
        let task = TaskGuard::new(tokio::spawn(async move {
            race(addresses(), |_| {
                let guard = Dropped(Arc::clone(&counter));
                async move {
                    let _guard = guard;
                    std::future::pending::<Result<(), std::io::Error>>().await
                }
            })
            .await
        }));
        let failure = wait_task(
            task,
            &control,
            bound,
            Phase::Connect,
            TransmissionFact::NotStarted,
        )
        .await
        .err()
        .ok_or("blackholes must time out")?;
        assert_eq!(
            (failure.phase(), failure.kind()),
            (Phase::Connect, ErrorKind::DeadlineExceeded)
        );
        assert_eq!(failure.transmission(), TransmissionFact::NotStarted);
        assert_eq!(tokio::time::Instant::now() - started, bound);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }
    Ok(())
}
