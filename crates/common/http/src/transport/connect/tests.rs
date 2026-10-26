use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::transport::{Resolver, TaskGuard, resolve, wait_task};
use crate::{ConnectAttempts, ExchangeControl, Origin};

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
    assert_eq!(
        targets,
        vec![addresses()[0], addresses()[1], addresses()[0]]
    );
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
    assert_eq!(tokio::time::Instant::now() - started, ATTEMPT_DELAY);
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
