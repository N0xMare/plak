#[tokio::test(start_paused = true)]
async fn steady_h1_and_h2_progress_outlives_whole_write_budget() {
    use http_body::Body;
    use std::{
        pin::Pin,
        sync::Arc,
        task::{Context, Waker},
    };
    for http2 in [false, true] {
        let limits = Limits {
            request_write_idle_timeout: Duration::from_millis(100),
            ..Limits::default()
        };
        let progress = Arc::new(super::RequestProgress::new(false, http2, 64 * 1024));
        let mut body = super::RequestBody::new(vec![b'x'; 64 * 1024].into(), Arc::clone(&progress));
        let start = tokio::time::Instant::now();
        let absolute = (start + Duration::from_secs(1)).into_std();
        progress.start_write_clock(limits.request_write_idle_timeout);
        for _ in 0..4 {
            tokio::time::advance(Duration::from_millis(60)).await;
            if http2 {
                let mut cx = Context::from_waker(Waker::noop());
                assert!(Pin::new(&mut body).poll_frame(&mut cx).is_ready());
            } else {
                progress.record_accepted(16 * 1024);
            }
            let (deadline, _) = progress.response_deadline(absolute, &limits);
            assert!(deadline > tokio::time::Instant::now());
        }
        assert!(tokio::time::Instant::now() - start > limits.request_write_idle_timeout);
        if !http2 {
            progress.mark_consumed();
            progress.mark_complete();
        }
        let (deadline, phase) = progress.response_deadline(absolute, &limits);
        assert_eq!(deadline.into_std(), absolute);
        assert_eq!(
            phase,
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::ResponseHead
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn late_progress_and_flush_cannot_revive_idle_write() {
    for http2 in [false, true] {
        let limits = Limits {
            request_write_idle_timeout: Duration::from_millis(100),
            ..Limits::default()
        };
        let progress = super::RequestProgress::new(false, http2, 100);
        let start = tokio::time::Instant::now();
        let absolute = (start + Duration::from_secs(5)).into_std();
        progress.start_write_clock(limits.request_write_idle_timeout);
        tokio::time::advance(Duration::from_millis(60)).await;
        if http2 {
            progress.record_write_progress();
        } else {
            progress.record_accepted(10);
        }
        let mut waiter_limits = limits.clone();
        waiter_limits.request_write_idle_timeout = Duration::from_secs(5);
        let expected = progress.response_deadline(absolute, &waiter_limits);
        assert_eq!(expected.0, start + Duration::from_millis(160));
        tokio::time::advance(Duration::from_millis(101)).await;
        if http2 {
            progress.record_write_progress();
        } else {
            progress.record_accepted(10);
        }
        progress.mark_consumed();
        progress.mark_complete();
        assert_eq!(
            progress.response_deadline(absolute, &waiter_limits),
            expected
        );
        assert_eq!(
            expected.1,
            if http2 {
                Phase::RequestWriteOrResponseHead
            } else {
                Phase::RequestWrite
            }
        );
    }
}

#[tokio::test(start_paused = true)]
async fn absolute_deadline_caps_steady_h1_and_h2_write() {
    for http2 in [false, true] {
        let limits = Limits {
            request_write_idle_timeout: Duration::from_millis(100),
            ..Limits::default()
        };
        let progress = super::RequestProgress::new(false, http2, 100);
        let start = tokio::time::Instant::now();
        let absolute = (start + Duration::from_millis(180)).into_std();
        progress.start_write_clock(limits.request_write_idle_timeout);
        for _ in 0..3 {
            tokio::time::advance(Duration::from_millis(60)).await;
            if http2 {
                progress.record_write_progress();
            } else {
                progress.record_accepted(10);
            }
        }
        assert_eq!(
            progress.response_deadline(absolute, &limits).0.into_std(),
            absolute
        );
    }
}

#[tokio::test(start_paused = true)]
async fn stalled_and_absolute_write_failures_keep_phase_and_transmission() {
    use std::sync::Arc;
    for http2 in [false, true] {
        for absolute_wins in [false, true] {
            let limits = Limits {
                request_write_idle_timeout: Duration::from_millis(100),
                ..Limits::default()
            };
            let progress = Arc::new(super::RequestProgress::new(false, http2, 100));
            let writer = Arc::clone(&progress);
            let start = tokio::time::Instant::now();
            let absolute = start
                + if absolute_wins {
                    Duration::from_millis(180)
                } else {
                    Duration::from_secs(5)
                };
            let waiting = tokio::spawn(async move {
                let connection = ConnectionTask::new(tokio::spawn(std::future::pending()), None);
                super::wait_response(
                    std::future::pending(),
                    connection,
                    progress,
                    &ExchangeControl::new(absolute.into_std()),
                    &limits,
                )
                .await
            });
            tokio::task::yield_now().await;
            for _ in 0..2 {
                tokio::time::advance(Duration::from_millis(60)).await;
                if http2 {
                    writer.record_write_progress();
                } else {
                    writer.record_accepted(10);
                }
            }
            let failure = waiting.await.unwrap().err().unwrap();
            assert_eq!(
                tokio::time::Instant::now(),
                start + Duration::from_millis(if absolute_wins { 180 } else { 220 })
            );
            assert_eq!(
                failure.phase(),
                if http2 {
                    Phase::RequestWriteOrResponseHead
                } else {
                    Phase::RequestWrite
                }
            );
            assert_eq!(failure.kind(), ErrorKind::DeadlineExceeded);
            assert_eq!(failure.transmission(), TransmissionFact::MayHaveTransmitted);
        }
    }
}

use super::response_wait_deadline;

fn expired_response_deadline(
    last_write_progress: Instant,
    completed: Option<Instant>,
    http2: bool,
    absolute: Instant,
    limits: &Limits,
    now: Instant,
) -> Option<Phase> {
    let (deadline, phase) = response_wait_deadline(
        last_write_progress,
        completed,
        http2,
        absolute,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
    );
    (now >= deadline.into_std()).then_some(phase)
}

#[test]
fn ready_response_cannot_revive_expired_write_or_head_budget() {
    let start = Instant::now();
    let limits = Limits {
        request_write_idle_timeout: Duration::from_millis(150),
        response_head_timeout: Duration::from_secs(5),
        ..Limits::default()
    };
    let absolute = start + Duration::from_secs(10);
    let late = Some(start + Duration::from_millis(160));
    let timely = Some(start + Duration::from_millis(140));
    let ready = start + Duration::from_millis(180);
    assert_eq!(
        expired_response_deadline(start, late, false, absolute, &limits, ready),
        Some(Phase::RequestWrite)
    );
    assert_eq!(
        expired_response_deadline(start, timely, false, absolute, &limits, ready),
        None
    );
    assert_eq!(
        expired_response_deadline(
            start,
            timely,
            false,
            absolute,
            &limits,
            start + Duration::from_millis(5140)
        ),
        Some(Phase::ResponseHead)
    );
    assert_eq!(
        expired_response_deadline(
            start,
            timely,
            false,
            start + Duration::from_millis(170),
            &limits,
            ready
        ),
        Some(Phase::ResponseHead)
    );
}

#[test]
fn recorded_flush_time_owns_write_to_head_deadline_transition() {
    let start = Instant::now();
    let limits = Limits {
        request_write_idle_timeout: Duration::from_millis(150),
        response_head_timeout: Duration::from_secs(5),
        ..Limits::default()
    };
    let absolute = start + Duration::from_secs(10);
    let before = response_wait_deadline(
        start,
        None,
        false,
        absolute,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
    );
    assert_eq!(
        before,
        (
            (start + Duration::from_millis(150)).into(),
            Phase::RequestWrite
        )
    );
    // Even if the waiter resumes after its old timer, a flush at 140ms
    // owns a new head budget; notification/timer polling order is irrelevant.
    let timely = response_wait_deadline(
        start,
        Some(start + Duration::from_millis(140)),
        false,
        absolute,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
    );
    assert_eq!(
        timely,
        (
            (start + Duration::from_millis(5140)).into(),
            Phase::ResponseHead
        )
    );
    let late = response_wait_deadline(
        start,
        Some(start + Duration::from_millis(160)),
        false,
        absolute,
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
    );
    assert_eq!(
        late, before,
        "late flush must not revive the expired write budget"
    );
    let capped = response_wait_deadline(
        start,
        Some(start + Duration::from_millis(140)),
        false,
        start + Duration::from_millis(200),
        limits.request_write_idle_timeout,
        limits.response_head_timeout,
    );
    assert_eq!(
        capped,
        (
            (start + Duration::from_millis(200)).into(),
            Phase::ResponseHead
        )
    );
}

use super::{Client, ConnectionTask, Limits, TransportPolicy};
use crate::{ErrorKind, ExchangeControl, Phase, TransmissionFact};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

async fn panicking_driver(ready: oneshot::Sender<()>) -> Result<(), hyper::Error> {
    let _ = ready.send(());
    tokio::task::yield_now().await;
    panic!("injected connection-driver panic");
}

#[tokio::test]
async fn completed_http2_dispatcher_does_not_abort_active_connection_task() {
    let registry = super::TaskRegistry::new();
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel::<()>();
    registry.spawn(async move {
        let _ = started_tx.send(());
        let _ = release_rx.await;
    });
    started_rx.await.expect("child started");
    let handle = tokio::spawn(async { Ok::<_, hyper::Error>(()) });
    while !handle.is_finished() {
        tokio::task::yield_now().await;
    }
    let stop = crate::CancellationToken::new();
    let supervisor = super::supervise_connection(
        super::DirectConnection {
            handle: Some(handle),
            registry: Some(registry),
        },
        ExchangeControl::new(Instant::now() + Duration::from_secs(5)),
        stop.clone(),
        crate::CancellationToken::new(),
    );
    tokio::pin!(supervisor);
    std::future::poll_fn(|cx| {
        use std::future::Future as _;
        assert!(supervisor.as_mut().poll(cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    // Process an already-completed dispatcher, then prove its body task was
    // not aborted while the response owner still requires it.
    tokio::task::yield_now().await;
    assert!(!release_tx.is_closed(), "active H2 connection was aborted");
    stop.cancel();
    supervisor.await.expect("owned cleanup");
    assert!(
        release_tx.is_closed(),
        "child must be joined after owner close"
    );
}

#[tokio::test]
async fn clean_eof_surfaces_owned_task_panic_as_cleanup_failure() {
    let (ready_tx, ready_rx) = oneshot::channel();
    let handle = tokio::spawn(panicking_driver(ready_tx));
    assert!(ready_rx.await.is_ok());
    let mut connection = ConnectionTask::new(handle, None).into_supervised(ExchangeControl::new(
        Instant::now() + Duration::from_secs(1),
    ));
    let result = connection.abort_and_join().await;
    assert!(matches!(
        result,
        Err(failure)
            if failure.phase() == Phase::Cleanup
                && failure.kind() == ErrorKind::Internal
                && failure.transmission() == TransmissionFact::MayHaveTransmitted
    ));
}

#[tokio::test]
async fn a_panicked_stream_task_pruned_early_still_fails_cleanup() {
    let registry = super::TaskRegistry::new();
    registry.spawn(async { panic!("stream task panic") });
    for _ in 0..1_000 {
        let finished = registry
            .handles
            .lock()
            .unwrap()
            .iter()
            .all(tokio::task::JoinHandle::is_finished);
        if finished {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    // Spawning more stream work prunes the finished task.
    registry.spawn(async {});
    assert!(registry.panicked());
    assert_eq!(registry.handles.lock().unwrap().len(), 1);
    let result = super::abort_direct_connection(None, Some(registry)).await;
    assert!(matches!(
        result,
        Err(failure) if failure.phase() == Phase::Cleanup && failure.kind() == ErrorKind::Internal
    ));
}

#[test]
fn one_shot_http1_client_pins_alpn_without_fallback() -> Result<(), super::ConfigError> {
    let client = Client::new_http1(Limits::default())?;
    assert_eq!(
        client.inner.tls_config.alpn_protocols,
        [b"http/1.1".to_vec()]
    );
    assert_eq!(client.transport_policy(), TransportPolicy::WebPkiHttp1Only);

    let default = Client::new(Limits::default())?;
    assert_eq!(
        default.inner.tls_config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert_eq!(
        default.transport_policy(),
        TransportPolicy::WebPkiHttp2Preferred
    );
    Ok(())
}

#[test]
fn streaming_inference_limits_wait_the_idle_bound_not_the_rest_stall() {
    let limits = Limits::for_streaming_inference();
    let defaults = Limits::default();
    assert_eq!(limits.response_head_timeout, super::STREAMING_IDLE_TIMEOUT);
    assert_eq!(limits.response_body_timeout, super::STREAMING_IDLE_TIMEOUT);
    assert_eq!(super::STREAMING_IDLE_TIMEOUT, Duration::from_secs(180));
    assert_eq!(limits.connect_timeout, defaults.connect_timeout);
    assert_eq!(limits.tls_timeout, defaults.tls_timeout);
    assert_eq!(
        limits.request_write_idle_timeout,
        defaults.request_write_idle_timeout
    );
    assert_eq!(defaults.response_body_timeout, Duration::from_secs(30));
}

#[test]
fn with_limits_keeps_tls_and_policy_and_recomputes_identity() -> Result<(), super::ConfigError> {
    let defaults = Limits::default();
    let client = Client::new_http1(defaults.clone())?;
    let rebuilt = client.with_limits(Limits::for_streaming_inference())?;
    assert_eq!(rebuilt.transport_policy(), client.transport_policy());
    assert_eq!(
        rebuilt.inner.tls_config.alpn_protocols,
        client.inner.tls_config.alpn_protocols
    );
    assert_eq!(
        rebuilt.limits().response_body_timeout,
        super::STREAMING_IDLE_TIMEOUT
    );
    assert_ne!(rebuilt.behavior_identity(), client.behavior_identity());
    let same = client.with_limits(defaults)?;
    assert_eq!(same.behavior_identity(), client.behavior_identity());
    Ok(())
}
