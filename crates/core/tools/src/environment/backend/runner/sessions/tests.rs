use std::cell::Cell;
use std::future::{Future, poll_fn};
use std::pin::pin;
use std::task::Poll;

use super::*;

fn snapshot(status: JoinedProcessStatus) -> JoinedProcessCursorPollResult {
    JoinedProcessCursorPollResult {
        records: Arc::from([]),
        status,
        next_cursor: JoinedProcessOutputCursor::new(0),
        more_output: false,
        skipped_bytes: 0,
    }
}

async fn pending<T>(mut future: std::pin::Pin<&mut impl Future<Output = T>>) {
    poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_silent_poll_reads_once_then_waits_for_its_deadline() {
    let (_changes, rx) = watch::channel(0);
    let control = OperationContext::new(CancellationToken::new());
    let reads = Cell::new(0);
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut result = pin!(poll_on_change(rx, deadline, &control, || {
        reads.set(reads.get() + 1);
        Ok(snapshot(JoinedProcessStatus::Running))
    }));
    pending(result.as_mut()).await;
    for _ in 0..3 {
        tokio::time::advance(Duration::from_millis(250)).await;
        pending(result.as_mut()).await;
        assert_eq!(reads.get(), 1);
    }
    assert_eq!(result.await.unwrap().status(), JoinedProcessStatus::Running);
    assert_eq!(Instant::now(), deadline);
    assert_eq!(reads.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn terminal_notification_during_the_first_read_is_not_lost() {
    let (changes, rx) = watch::channel(0);
    let control = OperationContext::new(CancellationToken::new());
    let now = Instant::now();
    let mut first = true;
    let result = poll_on_change(rx, now + Duration::from_secs(30), &control, || {
        if std::mem::take(&mut first) {
            changes.send_replace(1);
            Ok(snapshot(JoinedProcessStatus::Running))
        } else {
            Ok(snapshot(JoinedProcessStatus::Exited(Some(0))))
        }
    })
    .await
    .unwrap();
    assert_eq!(result.status(), JoinedProcessStatus::Exited(Some(0)));
    assert_eq!(Instant::now(), now);
}

#[tokio::test(start_paused = true)]
async fn a_waiting_poll_answers_notice_and_cancellation_without_a_timer() {
    for notice in [true, false] {
        let (_changes, rx) = watch::channel(0);
        let (preempt, signal) = watch::channel(ToolPreempt::None);
        let control = OperationContext::new(CancellationToken::new()).with_preempt(signal);
        let now = Instant::now();
        let mut result = pin!(poll_on_change(
            rx,
            now + Duration::from_secs(30),
            &control,
            || Ok(snapshot(JoinedProcessStatus::Running)),
        ));
        pending(result.as_mut()).await;
        if notice {
            preempt.send_replace(ToolPreempt::Notice(true));
        } else {
            control.cancellation().cancel();
        }
        assert_eq!(result.await.unwrap().status(), JoinedProcessStatus::Running);
        assert_eq!(Instant::now(), now);
    }
}
