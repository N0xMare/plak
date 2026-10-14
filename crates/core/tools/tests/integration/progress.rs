//! Bounded asynchronous progress transport contracts.

use std::error::Error;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use plakous_model::{CallId, Clock, FixedClock, Timestamp};
use plakous_tools::{ProgressReceiver, ProgressSink, ToolProgress, ToolRuntimeErrorKind};

fn progress_channel(capacity: usize) -> Result<(ProgressSink, ProgressReceiver), Box<dyn Error>> {
    let call_id = CallId::from_str("00000000-0000-0000-0000-000000000004")?;
    let clock: Arc<dyn Clock> = Arc::new(FixedClock::new(Timestamp::new(42, 7)?));
    Ok(ProgressSink::channel(capacity, call_id, clock)?)
}

async fn recv(receiver: &ProgressReceiver) -> Result<Option<ToolProgress>, Box<dyn Error>> {
    Ok(tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await?)
}

#[tokio::test]
async fn async_receipt_preserves_report_order() -> Result<(), Box<dyn Error>> {
    let (sink, receiver) = progress_channel(3)?;
    sink.report("first", Some(1), Some(3))?;
    sink.report("second", Some(2), Some(3))?;
    sink.report("third", Some(3), Some(3))?;

    let first = recv(&receiver).await?.ok_or("first progress missing")?;
    let second = recv(&receiver).await?.ok_or("second progress missing")?;
    let third = recv(&receiver).await?.ok_or("third progress missing")?;

    assert_eq!((first.sequence(), first.message()), (0, "first"));
    assert_eq!((second.sequence(), second.message()), (1, "second"));
    assert_eq!((third.sequence(), third.message()), (2, "third"));
    assert_eq!(first.timestamp(), Timestamp::new(42, 7)?);
    Ok(())
}

#[tokio::test]
async fn saturation_drops_newest_without_blocking_later_reports() -> Result<(), Box<dyn Error>> {
    let (sink, receiver) = progress_channel(1)?;
    sink.report("retained", None, None)?;
    sink.report("coalesced", None, None)?;

    let retained = receiver.try_recv().ok_or("retained progress missing")?;
    assert_eq!((retained.sequence(), retained.message()), (0, "retained"));
    assert!(receiver.try_recv().is_none());

    sink.report("after room", None, None)?;
    let after_room = recv(&receiver)
        .await?
        .ok_or("progress after saturation missing")?;
    assert_eq!(
        (after_room.sequence(), after_room.message()),
        (2, "after room")
    );
    Ok(())
}

#[tokio::test]
async fn close_drains_buffered_progress_before_ending_receipt() -> Result<(), Box<dyn Error>> {
    let (sink, receiver) = progress_channel(2)?;
    sink.report("first", None, None)?;
    sink.report("second", None, None)?;
    sink.close();

    assert_eq!(
        recv(&receiver).await?.map(|progress| progress.sequence()),
        Some(0)
    );
    assert_eq!(
        recv(&receiver).await?.map(|progress| progress.sequence()),
        Some(1)
    );
    assert!(recv(&receiver).await?.is_none());
    Ok(())
}

#[tokio::test]
async fn reports_after_close_are_suppressed_for_every_sink_clone() -> Result<(), Box<dyn Error>> {
    let (sink, receiver) = progress_channel(1)?;
    let clone = sink.clone();
    sink.close();

    sink.report("suppressed from original", None, None)?;
    clone.report("suppressed from clone", None, None)?;

    assert!(recv(&receiver).await?.is_none());
    assert!(receiver.try_recv().is_none());
    Ok(())
}

#[test]
fn progress_bounds_and_redacted_debug_are_preserved() -> Result<(), Box<dyn Error>> {
    fn assert_send_sync<T: Send + Sync>() {}
    fn assert_send<T: Send>(_: T) {}

    assert_send_sync::<ProgressSink>();
    assert_send_sync::<ProgressReceiver>();

    let zero = ProgressSink::channel(
        0,
        CallId::from_str("00000000-0000-0000-0000-000000000004")?,
        Arc::new(FixedClock::new(Timestamp::new(42, 7)?)),
    );
    assert_eq!(
        zero.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    let excessive = ProgressSink::channel(
        usize::MAX,
        CallId::from_str("00000000-0000-0000-0000-000000000004")?,
        Arc::new(FixedClock::new(Timestamp::new(42, 7)?)),
    );
    assert_eq!(
        excessive.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );

    let (sink, receiver) = progress_channel(2)?;
    assert_send(receiver.recv());
    sink.report("secret progress", None, None)?;
    assert!(!format!("{receiver:?}").contains("secret progress"));
    let progress = receiver.try_recv().ok_or("progress missing")?;
    assert!(!format!("{progress:?}").contains("secret progress"));

    sink.report("x".repeat(4_096), None, None)?;
    let oversized = sink.report("x".repeat(4_097), None, None);
    assert_eq!(
        oversized.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::ResourceLimit)
    );
    let inconsistent = sink.report("invalid units", Some(2), Some(1));
    assert_eq!(
        inconsistent.err().map(|error| error.kind()),
        Some(ToolRuntimeErrorKind::Execution)
    );
    Ok(())
}
