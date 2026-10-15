//! The owned, single-dispatch prepared model call.

use std::fmt;
use std::marker::PhantomData;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;
use thiserror::Error;

use super::authority::RuntimeModelAuthorityError;
use super::recovery::{ModelCallRecoveryError, PreparedCallRecord};
use super::{
    ModelCallContext, ModelControlStream, ModelError, ModelErrorKind, ModelStream, ModelStreamItem,
    StartModelCall, StartModelCallWithControl,
};
use futures_util::StreamExt;

type StartPreparedModelCall =
    Box<dyn FnOnce(ModelCallContext) -> StartModelCallWithControl + Send + 'static>;

/// One non-cloneable, non-serializable, one-shot prepared model call.
///
/// Dropping this value drops the only start capability and performs no provider
/// work. Starting consumes the exact captured lowering; attempt controls may
/// cancel or bound it but cannot replace target, body, cache, or token facts.
pub struct PreparedModelCall {
    record: PreparedCallRecord,
    start: Option<StartPreparedModelCall>,
}

impl PreparedModelCall {
    /// Couples one inert record to its sole private start capability.
    #[must_use]
    pub fn new<F>(record: PreparedCallRecord, start: F) -> Self
    where
        F: FnOnce(ModelCallContext) -> StartModelCall + Send + 'static,
    {
        Self::new_with_control(record, move |context| {
            semantic_start_from_events(start(context))
        })
    }

    /// Couples an inert record to a locator-aware one-shot start capability.
    #[must_use]
    pub fn new_with_control<F>(record: PreparedCallRecord, start: F) -> Self
    where
        F: FnOnce(ModelCallContext) -> StartModelCallWithControl + Send + 'static,
    {
        Self {
            record,
            start: Some(Box::new(start)),
        }
    }

    /// Returns the bounded serializable preparation record.
    #[must_use]
    pub const fn record(&self) -> &PreparedCallRecord {
        &self.record
    }

    /// Wraps the opaque start future without exposing or rebuilding the call.
    #[must_use]
    pub fn map_start<F>(self, map: F) -> Self
    where
        F: FnOnce(StartModelCall) -> StartModelCall + Send + 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => Self::new(record, move |context| {
                map(events_start_from_semantic(start(context)))
            }),
            None => Self::new(record, move |_| missing_prepared_start()),
        }
    }

    /// Wraps the locator-aware start future without projecting control items.
    #[must_use]
    pub fn map_start_with_control<F>(self, map: F) -> Self
    where
        F: FnOnce(StartModelCallWithControl) -> StartModelCallWithControl + Send + 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => Self::new_with_control(record, move |context| map(start(context))),
            None => Self::new_with_control(record, move |_| missing_prepared_control_start()),
        }
    }

    /// Runs prepared-call construction, start, and stream polls inside one
    /// immutable caller-supplied scope.
    ///
    /// The guard is never retained across a yield; it is recreated for each
    /// poll. Responses dialects that await credential refresh inside preflight
    /// can complete `start` on the first poll and acquire inference on a later
    /// stream poll — those stream polls must re-enter the same scope.
    #[must_use]
    pub fn scope_start<F, G>(self, enter: F) -> Self
    where
        F: Fn() -> G + Send + Sync + 'static,
        G: 'static,
    {
        let Self { record, mut start } = self;
        match start.take() {
            Some(start) => {
                let enter = Arc::new(enter);
                Self::new_with_control(record, move |context| {
                    let enter = Arc::clone(&enter);
                    let mut call = {
                        let _guard = enter();
                        start(context)
                    };
                    Box::pin(async move {
                        let stream = std::future::poll_fn(|cx| {
                            let _guard = enter();
                            call.as_mut().poll(cx)
                        })
                        .await?;
                        Ok(Box::pin(ScopedControlStream {
                            inner: stream,
                            enter,
                            _guard: PhantomData,
                        }) as ModelControlStream)
                    })
                })
            }
            None => Self::new_with_control(record, move |_| missing_prepared_control_start()),
        }
    }

    /// Consumes the sole start capability under attempt-only controls.
    pub fn start(self, context: ModelCallContext) -> StartModelCall {
        events_start_from_semantic(self.start_with_control(context))
    }

    /// Consumes the sole start capability while preserving locator controls.
    pub fn start_with_control(mut self, context: ModelCallContext) -> StartModelCallWithControl {
        if context.attempt_id() != self.record.projection.attempt_id {
            return Box::pin(async {
                Err(ModelError::new(
                    ModelErrorKind::InvalidRequest,
                    "model call context belongs to another prepared attempt",
                ))
            });
        }
        let Some(start) = self.start.take() else {
            return missing_prepared_control_start();
        };
        let prepared_call = self.record.identity();
        let attempt_id = context.attempt_id();
        let call = start(context);
        Box::pin(async move {
            let stream = call.await.map_err(|mut error| {
                error.bind_dispatch_rejection(prepared_call, attempt_id);
                error
            })?;
            Ok(Box::pin(stream.map(move |item| {
                item.map_err(|mut error| {
                    error.bind_dispatch_rejection(prepared_call, attempt_id);
                    error
                })
            })) as ModelControlStream)
        })
    }
}

fn semantic_start_from_events(start: StartModelCall) -> StartModelCallWithControl {
    Box::pin(async move {
        let stream = start.await?;
        Ok(Box::pin(stream.map(|item| item.map(ModelStreamItem::Event))) as ModelControlStream)
    })
}

fn events_start_from_semantic(start: StartModelCallWithControl) -> StartModelCall {
    Box::pin(async move {
        let stream = start.await?;
        Ok(Box::pin(stream.map(|item| match item {
            Ok(ModelStreamItem::Event(event)) => Ok(event),
            Ok(ModelStreamItem::RecoveryLocatorBound(_)) => Err(ModelError::new(
                ModelErrorKind::InvalidEvent,
                "locator-aware stream requires PreparedModelCall::start_with_control",
            )),
            Err(error) => Err(error),
        })) as ModelStream)
    })
}

fn missing_prepared_start() -> StartModelCall {
    Box::pin(async {
        Err(ModelError::new(
            ModelErrorKind::Protocol,
            "prepared call lost its sole start capability",
        ))
    })
}

fn missing_prepared_control_start() -> StartModelCallWithControl {
    Box::pin(async {
        Err(ModelError::new(
            ModelErrorKind::Protocol,
            "prepared call lost its sole start capability",
        ))
    })
}

struct ScopedControlStream<F, G> {
    inner: ModelControlStream,
    enter: Arc<F>,
    _guard: PhantomData<fn() -> G>,
}

impl<F, G> Stream for ScopedControlStream<F, G>
where
    F: Fn() -> G + Send + Sync + 'static,
    G: 'static,
{
    type Item = Result<ModelStreamItem, ModelError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let _guard = (this.enter)();
        this.inner.as_mut().poll_next(cx)
    }
}

impl fmt::Debug for PreparedModelCall {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedModelCall")
            .field("record", &self.record)
            .field("start", &"<one-shot opaque capability>")
            .finish()
    }
}

/// Typed construction failure for preparation evidence and inert records.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum PreparedCallError {
    /// One exact descriptor or plan/projection dimension drifted.
    #[error("prepared call scope drifted at {field}")]
    ScopeDrift {
        /// Stable non-secret name of the mismatched scope axis.
        field: &'static str,
    },
    /// A digest did not use the required domain, algorithm, or revision.
    #[error("prepared call contains an invalid digest")]
    InvalidDigest,
    /// A behavior-bearing label was empty, oversized, or non-portable.
    #[error("prepared call contains an invalid revision or field label")]
    InvalidLabel,
    /// The exact credential-free body was absent or exceeded its bound.
    #[error("prepared call wire size is invalid")]
    InvalidWireSize,
    /// The serializable recovery record exceeded its strict bound.
    #[error("prepared call record exceeds its retention bound")]
    RecordTooLarge,
    /// Serialization needed for an integrity identity failed.
    #[error("prepared call identity serialization failed")]
    Serialization,
    /// Serialized facts disagreed with their integrity identity.
    #[error("prepared call integrity mismatch")]
    IntegrityMismatch,
    /// A provider recovery locator was absent, malformed, or foreign.
    #[error(transparent)]
    CallRecovery(#[from] ModelCallRecoveryError),
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] crate::DigestError),
    /// Fresh model authority was invalid or stale.
    #[error(transparent)]
    RuntimeAuthority(#[from] RuntimeModelAuthorityError),
}
