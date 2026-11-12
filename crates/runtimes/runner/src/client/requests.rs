//! Request classification, response decoding and loss windows.

use super::{
    Duration, Refusal, Request, RequestClass, RunnerFailure, RunnerFrame, RunnerVerb, Value,
};

/// The request a runner frame answers, its answer, and whether the runner
/// keeps that answer until it is acknowledged. `InProgress` (the answer
/// still comes), `Pong` and frames without a request id answer nothing.
pub(super) fn request_answer(
    frame: RunnerFrame,
) -> Option<(String, Result<Value, RunnerFailure>, bool)> {
    Some(match frame {
        RunnerFrame::Result {
            request_id,
            payload,
        } => (request_id, Ok(payload), true),
        RunnerFrame::Error { request_id, error } => {
            (request_id, Err(RunnerFailure::Backend(error.error)), true)
        }
        RunnerFrame::NotSeen { request_id } => (
            request_id,
            Err(RunnerFailure::Unavailable(
                "the runner never saw the call; nothing ran".into(),
            )),
            false,
        ),
        RunnerFrame::Spent { request_id } => (request_id, Err(RunnerFailure::Spent), false),
        RunnerFrame::Expired { request_id } => (
            request_id,
            Err(RunnerFailure::OutcomeUnknown(
                "the call is older than the runner keeps answers, so whether it ran is unknown"
                    .into(),
            )),
            false,
        ),
        RunnerFrame::StaleRequest { request_id } => (
            request_id,
            Err(RunnerFailure::Refused(
                "the runner holds another request under this call's key".into(),
            )),
            false,
        ),
        RunnerFrame::StaleOwner {
            request_id: Some(request_id),
            ..
        } => (request_id, Err(RunnerFailure::StaleOwner), false),
        RunnerFrame::Refused {
            request_id: Some(request_id),
            refusal,
        } => (
            request_id,
            Err(match refusal {
                Refusal::Unsupported { .. } => RunnerFailure::Unsupported,
                Refusal::OwnerNotTransferred => RunnerFailure::NotHolder,
                other => RunnerFailure::Refused(format!("{other:?}")),
            }),
            false,
        ),
        _ => return None,
    })
}

/// Whether the runner refuses the connection itself with `refusal`, before
/// any request of it ran.
pub(super) const fn ends_connection(refusal: &Refusal) -> bool {
    matches!(
        refusal,
        Refusal::Unbound
            | Refusal::VersionMismatch { .. }
            | Refusal::NotPaired { .. }
            | Refusal::InspectOnly
    )
}

/// How often a model read checks loss, or a lost model effect checks rebinding.
pub(super) const LOST_LINK_RECHECK: Duration = Duration::from_secs(1);

/// How a sent call's answer window ends.
#[derive(Clone, Copy)]
pub(super) enum Expiring {
    /// Unknown outcome at the window's end, whatever the link did.
    Window,
    /// A model tool call's effect: its window pauses while the link is lost.
    ModelEffect,
    /// A model read: rechecks link loss without shortening its effect window.
    ModelRead,
    /// A host read or observation: checks link loss only at its window's end.
    Read,
}

impl Expiring {
    pub(super) fn recheck(self, window: Duration) -> Duration {
        match self {
            Self::ModelRead => window.min(LOST_LINK_RECHECK),
            Self::Window | Self::Read | Self::ModelEffect => window,
        }
    }

    pub(super) fn lost(self, detail: &str) -> RunnerFailure {
        match self {
            Self::Read | Self::ModelRead => RunnerFailure::ReadInterrupted(detail.into()),
            Self::Window | Self::ModelEffect => RunnerFailure::OutcomeUnknown(detail.into()),
        }
    }

    pub(super) fn of(request: &Request) -> Self {
        match request.class {
            RequestClass::EffectCall { .. } if !keyed(request) => Self::ModelRead,
            RequestClass::EffectCall { .. } => Self::ModelEffect,
            _ if !keyed(request) && replayable(request) => Self::Read,
            _ => Self::Window,
        }
    }
}

/// How a replay or recovery says the link was lost before an answer came.
pub(super) const LINK_LOST: &str = "the link was lost before the replay was answered";

/// Whether a request runs at most once per key: an effect verb under a
/// model call, an owner control or a tree step. The journal records these.
pub(super) fn keyed(request: &Request) -> bool {
    match request.class {
        RequestClass::EffectCall { .. } => {
            !RunnerVerb::parse(&request.operation.verb).is_some_and(RunnerVerb::is_read)
        }
        RequestClass::OwnerControl { .. } | RequestClass::TreeOperation { .. } => true,
        _ => false,
    }
}

/// Whether a request outstanding at a lost link may be sent again: a keyed
/// call or control, an observation, or a read.
pub(super) fn replayable(request: &Request) -> bool {
    matches!(
        request.class,
        RequestClass::EffectCall { .. }
            | RequestClass::OwnerControl { .. }
            | RequestClass::TreeOperation { .. }
            | RequestClass::ProcessObservation { .. }
    ) || RunnerVerb::parse(&request.operation.verb).is_some_and(RunnerVerb::is_read)
}
