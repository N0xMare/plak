//! How a relay caller turns a hop-1 observation into the kernel's upstream fact.
//!
//! The gateway never retries. This map is the whole composition: hop 1's
//! transmission, a gateway refusal, or an `End` frame becomes one decision
//! the existing kernels already understand. It does no I/O.

use crate::{ErrorKind, Phase, TransmissionFact};

/// A hop-2 failure the gateway measured and carried on `End`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MeasuredUpstream {
    /// Where the upstream exchange failed.
    pub phase: Phase,
    /// How far the upstream request got.
    pub transmission: TransmissionFact,
    /// What failed.
    pub kind: ErrorKind,
}

/// What the caller observed on hop 1.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayObservation {
    /// No relay `Head` frame arrived.
    BeforeHead(BeforeHead),
    /// A `Head` frame arrived, then hop 1 broke.
    BreakAfterHead,
    /// `Trailers` and then `End` complete.
    TrailersThenComplete,
    /// `End` complete, with no trailers.
    Complete,
}

/// A relay that ended before `Head`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BeforeHead {
    /// `connect` or `hang`: the gateway was not reached.
    Unreachable,
    /// `write-reset`: hop 1 did not accept the whole request.
    WriteReset,
    /// The request may have reached the gateway, and no `Head` came back.
    /// `relay-lost=before-head` is this observation.
    Lost,
    /// A non-200 answer carrying `plak-gateway-error`.
    Refusal {
        /// The refusal code.
        code: String,
    },
    /// `End` `upstream_failed` before `Head`, with the gateway's measurement.
    UpstreamFailed(MeasuredUpstream),
    /// `End` `cancelled` before `Head`.
    Cancelled,
    /// `End` `gateway_failed` before `Head`.
    GatewayFailed,
}

/// The class of an unsent relay, for the kernel's retry choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsentClass {
    /// Hop 1 did not reach a gateway that could have sent upstream.
    Transport,
    /// `busy`: a synthetic connect inside the unreachable budget.
    Busy,
    /// `fence_moved`: re-prepare inside the same logical attempt.
    FenceMoved,
    /// `internal`: typed, and not retried.
    Internal,
    /// Any other gateway refusal: typed, and not retried.
    Refusal,
}

/// An unsent decision. The incomplete-write allowance is never used: that
/// ride-out does not apply to hop 1.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Unsent {
    /// Always [`TransmissionFact::NotStarted`].
    pub transmission: TransmissionFact,
    /// Refunded like an unsent provider attempt.
    pub refund: bool,
    /// Counts against the unreachable budget.
    pub unreachable_budget: bool,
    /// Re-prepare inside the same logical attempt, at most twice.
    pub reprepare: bool,
    /// The sixteen-request incomplete-write allowance. Always false on hop 1.
    pub incomplete_write_allowance: bool,
    /// Which refusal or transport class this is.
    pub class: UnsentClass,
}

/// What the kernel does with one relay observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RelayKernel {
    /// Upstream `NotStarted`.
    Unsent(Unsent),
    /// The request may have reached the upstream peer.
    MayHaveTransmitted,
    /// Use the gateway's hop-2 measurement as the failure, unchanged.
    Measured(MeasuredUpstream),
    /// A relay `Head`: the upstream status and headers.
    Response,
    /// Any break after `Head`: `Failure(ResponseBody, MayHaveTransmitted)`.
    MidStream,
    /// A clean end of the upstream body.
    Clean {
        /// The body carried trailers.
        trailers: bool,
    },
}

/// Maps one hop-1 observation onto the kernel decision.
#[must_use]
pub fn map_relay(observation: &RelayObservation) -> RelayKernel {
    match observation {
        RelayObservation::BeforeHead(before) => map_before_head(before),
        RelayObservation::BreakAfterHead => RelayKernel::MidStream,
        RelayObservation::TrailersThenComplete => RelayKernel::Clean { trailers: true },
        RelayObservation::Complete => RelayKernel::Clean { trailers: false },
    }
}

fn map_before_head(before: &BeforeHead) -> RelayKernel {
    match before {
        BeforeHead::Unreachable => RelayKernel::Unsent(unsent(UnsentClass::Transport, true, true)),
        BeforeHead::WriteReset => RelayKernel::Unsent(unsent(UnsentClass::Transport, false, true)),
        BeforeHead::Lost | BeforeHead::Cancelled | BeforeHead::GatewayFailed => {
            RelayKernel::MayHaveTransmitted
        }
        BeforeHead::UpstreamFailed(measured) => RelayKernel::Measured(*measured),
        BeforeHead::Refusal { code } => RelayKernel::Unsent(map_refusal(code)),
    }
}

fn map_refusal(code: &str) -> Unsent {
    match code {
        "busy" => unsent(UnsentClass::Busy, true, true),
        "fence_moved" => Unsent {
            reprepare: true,
            ..unsent(UnsentClass::FenceMoved, false, true)
        },
        "internal" => unsent(UnsentClass::Internal, false, false),
        _ => unsent(UnsentClass::Refusal, false, false),
    }
}

fn unsent(class: UnsentClass, unreachable_budget: bool, refund: bool) -> Unsent {
    Unsent {
        transmission: TransmissionFact::NotStarted,
        refund,
        unreachable_budget,
        reprepare: false,
        incomplete_write_allowance: false,
        class,
    }
}
