//! Agent-owned controls at the exact executor dispatch fence.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use plakous_model::{CancellationToken, Clock, Timestamp};
use plakous_tools::{ProgressSink, ToolRuntimeError};

/// Boxed acknowledgement at the exact pre-executor dispatch boundary.
pub type ToolDispatchObserverFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ToolDispatchPermit<'a>, ToolRuntimeError>> + Send + 'a>>;

/// One-shot final synchronous admission, retaining any guard acquired while
/// waiting. The runtime consumes it after mutable binding and control checks,
/// immediately before executor construction. Dropping it refuses the start.
pub struct ToolDispatchPermit<'a> {
    start: Box<dyn FnOnce() -> Result<(), ToolRuntimeError> + Send + 'a>,
}

impl<'a> ToolDispatchPermit<'a> {
    /// Binds the final check/observation and its owned guard to this start.
    #[must_use]
    pub fn new(start: impl FnOnce() -> Result<(), ToolRuntimeError> + Send + 'a) -> Self {
        Self {
            start: Box::new(start),
        }
    }

    pub(super) fn start(self) -> Result<(), ToolRuntimeError> {
        (self.start)()
    }
}

/// Optional admission owner gated immediately before an executor may start.
///
/// Implementations must remain content-free. Returning an error prevents the
/// executor from being invoked and preserves the runtime's `NotStarted` fact.
pub trait ToolDispatchObserver: Send + Sync + 'static {
    /// Waits after scheduler admission, retaining authority in a one-shot
    /// permit. Final mutable revalidation happens after this wait; consuming
    /// the permit checks current authority and observes the actual start.
    fn dispatch_started(&self) -> ToolDispatchObserverFuture<'_>;
}

/// Caller-owned cooperative controls for one already authorized dispatch.
///
/// This value carries no authority and cannot be used to bypass policy. The
/// runtime combines it with an opaque authorized invocation to construct the
/// executor-visible [`ToolContext`](plakous_tools::ToolContext).
#[derive(Clone)]
pub struct ToolExecutionControl {
    cancellation: CancellationToken,
    deadline: Option<Timestamp>,
    clock: Arc<dyn Clock>,
    progress: ProgressSink,
    dispatch_observer: Option<Arc<dyn ToolDispatchObserver>>,
    preempt: tokio::sync::watch::Receiver<plakous_tools::ToolPreempt>,
}

impl ToolExecutionControl {
    /// Creates explicit cancellation, time, and bounded-progress controls.
    #[must_use]
    pub fn new(
        cancellation: CancellationToken,
        deadline: Option<Timestamp>,
        clock: Arc<dyn Clock>,
        progress: ProgressSink,
    ) -> Self {
        Self {
            cancellation,
            deadline,
            clock,
            progress,
            dispatch_observer: None,
            preempt: tokio::sync::watch::channel(plakous_tools::ToolPreempt::None).1,
        }
    }

    /// Installs one exact pre-executor dispatch observer.
    #[must_use]
    pub fn with_dispatch_observer(mut self, observer: Arc<dyn ToolDispatchObserver>) -> Self {
        self.dispatch_observer = Some(observer);
        self
    }

    /// Supplies a cooperative yield request, separate from cancellation.
    #[must_use]
    pub(crate) fn with_preempt(
        mut self,
        preempt: tokio::sync::watch::Receiver<plakous_tools::ToolPreempt>,
    ) -> Self {
        self.preempt = preempt;
        self
    }

    pub(super) const fn preempt(
        &self,
    ) -> &tokio::sync::watch::Receiver<plakous_tools::ToolPreempt> {
        &self.preempt
    }

    pub(super) const fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(super) const fn deadline(&self) -> Option<Timestamp> {
        self.deadline
    }

    pub(super) const fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    pub(super) const fn progress(&self) -> &ProgressSink {
        &self.progress
    }

    pub(super) fn dispatch_observer(&self) -> Option<&Arc<dyn ToolDispatchObserver>> {
        self.dispatch_observer.as_ref()
    }
}

impl fmt::Debug for ToolExecutionControl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolExecutionControl")
            .field("cancellation", &self.cancellation)
            .field("deadline", &self.deadline)
            .field("progress", &self.progress)
            .field(
                "dispatch_observer",
                &self.dispatch_observer.as_ref().map(|_| "<bound>"),
            )
            .finish_non_exhaustive()
    }
}
