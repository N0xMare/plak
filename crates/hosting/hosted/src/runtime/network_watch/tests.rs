use super::*;
use plakous::model::{ModelNetworkFailure, ModelNetworkTransmission, ModelRetryPhase};
use std::sync::atomic::{AtomicUsize, Ordering};

fn fake(
    route: &str,
    calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
    ready_at: usize,
) -> ReachabilityProbe {
    ReachabilityProbe::new(RouteId::new(route).unwrap(), move |_, _| {
        let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
        entered.notify_one();
        Box::pin(async move {
            if n >= ready_at {
                Ok(())
            } else {
                Err(ModelNetworkFailure {
                    phase: ModelRetryPhase::Tls,
                    sent: ModelNetworkTransmission::NotSent,
                    timed_out: false,
                })
            }
        })
    })
}

#[tokio::test(start_paused = true)]
async fn network_watch_shares_route_and_uses_injected_time() {
    let service = Arc::new(NetworkWatch::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    let probe = fake("test:route", calls.clone(), entered.clone(), 3);
    let cancel_one = CancellationToken::new();
    let first = {
        let service = service.clone();
        let probe = probe.clone();
        let cancel = cancel_one.clone();
        tokio::spawn(async move {
            service
                .wait(Some(probe), Duration::from_secs(30), &cancel)
                .await
        })
    };
    entered.notified().await;
    let second = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .wait(
                    Some(probe),
                    Duration::from_secs(30),
                    &CancellationToken::new(),
                )
                .await
        })
    };
    tokio::task::yield_now().await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    cancel_one.cancel();
    assert!(!first.await.unwrap());
    tokio::time::advance(Duration::from_secs(5)).await;
    entered.notified().await;
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    tokio::time::advance(Duration::from_secs(10)).await;
    assert!(second.await.unwrap());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(start_paused = true)]
async fn network_watch_cancels_last_probe_and_falls_back_without_probe() {
    let service = Arc::new(NetworkWatch::default());
    let started = Arc::new(tokio::sync::Notify::new());
    let (sent, received) = tokio::sync::oneshot::channel();
    let sent = Arc::new(Mutex::new(Some(sent)));
    let probe = ReachabilityProbe::new(RouteId::new("test:cancel").unwrap(), move |cancel, _| {
        if let Some(sent) = sent.lock().unwrap().take() {
            sent.send(cancel).unwrap();
        }
        started.notify_one();
        Box::pin(std::future::pending())
    });
    let cancellation = CancellationToken::new();
    let waiter = {
        let service = service.clone();
        let cancel = cancellation.clone();
        tokio::spawn(async move {
            service
                .wait(Some(probe), Duration::from_secs(30), &cancel)
                .await
        })
    };
    let probe_cancel = received.await.unwrap();
    cancellation.cancel();
    assert!(!waiter.await.unwrap());
    assert!(probe_cancel.is_cancelled());
    let fallback = tokio::spawn(async move {
        service
            .wait(None, Duration::from_secs(30), &CancellationToken::new())
            .await
    });
    tokio::task::yield_now().await;
    tokio::time::advance(Duration::from_secs(29)).await;
    assert!(!fallback.is_finished());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(fallback.await.unwrap());
}
