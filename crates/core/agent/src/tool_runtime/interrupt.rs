use super::{
    Arc, Duration, EffectiveDeadline, Either, Instant, Interrupt, Timestamp, ToolDispatchError,
    ToolRuntimeErrorKind, ToolTimer, select,
};

pub(super) async fn wait_for_interrupt(
    cancellation: plakous_model::CancellationToken,
    deadline: Instant,
    timer: Arc<dyn ToolTimer>,
) -> Interrupt {
    if cancellation.is_cancelled() {
        return Interrupt::Cancelled;
    }
    match select(cancellation.cancelled(), timer.sleep_until(deadline)).await {
        Either::Left(((), _)) => Interrupt::Cancelled,
        Either::Right(((), _)) => Interrupt::Deadline,
    }
}

pub(super) async fn wait_for_dispatch_interrupt(
    caller_cancellation: plakous_model::CancellationToken,
    execution_cancellation: plakous_model::CancellationToken,
    deadline: Instant,
    timer: Arc<dyn ToolTimer>,
) -> Interrupt {
    if caller_cancellation.is_cancelled() || execution_cancellation.is_cancelled() {
        return Interrupt::Cancelled;
    }
    let either_cancelled = select(
        caller_cancellation.cancelled(),
        execution_cancellation.cancelled(),
    );
    match select(Box::pin(either_cancelled), timer.sleep_until(deadline)).await {
        Either::Left(_) => Interrupt::Cancelled,
        Either::Right(_) => Interrupt::Deadline,
    }
}

pub(super) fn effective_deadline(
    now: Timestamp,
    caller: Option<Timestamp>,
    execution_duration: Duration,
) -> EffectiveDeadline {
    let environment = timestamp_add(now, execution_duration);
    let wall = caller.map_or(environment, |caller| caller.min(environment));
    let remaining = timestamp_duration_since(wall, now);
    EffectiveDeadline {
        wall,
        monotonic: Instant::now() + remaining,
    }
}

fn timestamp_add(timestamp: Timestamp, duration: Duration) -> Timestamp {
    let nanos = u64::from(timestamp.nanoseconds()) + u64::from(duration.subsec_nanos());
    let carry = nanos / 1_000_000_000;
    let seconds = timestamp
        .seconds()
        .saturating_add(duration.as_secs())
        .saturating_add(carry);
    let nanoseconds = if seconds == u64::MAX {
        999_999_999
    } else {
        (nanos % 1_000_000_000) as u32
    };
    match Timestamp::new(seconds, nanoseconds) {
        Ok(timestamp) => timestamp,
        Err(_) => timestamp,
    }
}

fn timestamp_duration_since(later: Timestamp, earlier: Timestamp) -> Duration {
    if later <= earlier {
        return Duration::ZERO;
    }
    let mut seconds = later.seconds() - earlier.seconds();
    let nanoseconds = if later.nanoseconds() >= earlier.nanoseconds() {
        later.nanoseconds() - earlier.nanoseconds()
    } else {
        seconds -= 1;
        1_000_000_000 + later.nanoseconds() - earlier.nanoseconds()
    };
    Duration::new(seconds, nanoseconds)
}

pub(super) fn cancelled_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Cancelled,
        "tool execution was cancelled",
    )
}

pub(super) fn execution_timeout_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::ExecutionTimeout,
        "tool execution deadline elapsed before dispatch completed",
    )
}

pub(super) fn execution_panic_error() -> ToolDispatchError {
    ToolDispatchError::new(
        ToolRuntimeErrorKind::Execution,
        "tool executor panicked during dispatch",
    )
}
