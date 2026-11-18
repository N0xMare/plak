//! Repository-only gateway-hop faults for a relaying client
//! (`fault-injection`): the hop-1 rules of its plan, one per relay request,
//! composed through the hop mapping a real hop event takes. A product build
//! compiles `test_hop_off.rs` in its place.

use super::fault::{FaultHop, FaultPlan, RelayCut, Selected, StreamFault};
use super::{Client, Response};
use crate::{ErrorKind, ExchangeControl, Failure, Limits, Phase, TransmissionFact, WireProtocol};

/// The relaying client's plan, applied on the gateway hop.
pub(crate) type HopFaults = Option<FaultPlan>;

/// The gateway-hop fault one relay request takes, if any.
pub(crate) type Selection = Option<HopFault>;

/// What one gateway-hop fault does to one relay request.
pub(crate) enum HopFault {
    /// The hop fails this way before the gateway holds the whole request.
    Fail(Failure),
    /// This synthetic answer stands in for the gateway's; nothing is sent.
    Answer(Box<Response>),
    /// The request is sent; its answer is lost before the relayed head.
    LostBeforeHead,
    /// The request is sent; the hop drops right after the relayed head.
    LostAfterHead,
    /// The relayed body takes this stream fault.
    Stream(Box<StreamFault>),
}

/// How a relay request starts under its selection.
#[allow(
    dead_code,
    reason = "the credential gateway relay client sends through it"
)]
pub(crate) enum BeforeSend {
    /// The hop fails this way; nothing is sent.
    Fail(Failure),
    /// This answer stands in for the gateway's; nothing is sent.
    Answer(Box<Response>),
    /// Send the request on the hop.
    Send,
}

/// `client`'s plan on the gateway hop, for the relay it hands its exchange.
pub(crate) fn relaying_faults(client: &Client) -> HopFaults {
    client
        .faults
        .clone()
        .map(|plan| plan.with_hop(FaultHop::Gateway))
}

/// The fault `faults` selects for one relay request to the logical upstream
/// `host` and `path_and_query`.
#[allow(
    dead_code,
    reason = "the credential gateway relay client selects its faults here"
)]
pub(crate) async fn select(
    faults: &HopFaults,
    host: &str,
    path_and_query: &str,
    control: &ExchangeControl,
    limits: &Limits,
) -> Result<Selection, Failure> {
    let Some(selected) = faults
        .as_ref()
        .and_then(|plan| plan.select(host, path_and_query))
    else {
        return Ok(None);
    };
    Ok(Some(match selected {
        Selected::Invalid => HopFault::Fail(Failure::new(
            Phase::Resolution,
            TransmissionFact::NotStarted,
            ErrorKind::Internal,
        )),
        Selected::Connect => HopFault::Fail(
            Failure::new(Phase::Connect, TransmissionFact::NotStarted, ErrorKind::Io)
                .with_io_cause(&std::io::Error::from(std::io::ErrorKind::ConnectionRefused)),
        ),
        Selected::Hang(duration) => {
            HopFault::Fail(super::fault::injected_hang(duration, control).await)
        }
        Selected::WriteReset => HopFault::Fail(Failure::new(
            Phase::RequestWriteOrResponseHead,
            TransmissionFact::RequestIncomplete,
            ErrorKind::Protocol,
        )),
        Selected::RecordRejected => HopFault::Fail(
            Failure::new(
                Phase::RequestWriteOrResponseHead,
                TransmissionFact::RequestIncomplete,
                ErrorKind::Protocol,
            )
            .with_peer_rejected_record()
            .with_wire_protocol(WireProtocol::Http1),
        ),
        Selected::Status {
            status,
            retry_after,
        } => HopFault::Answer(Box::new(
            super::fault::injected_status(status, retry_after, control.clone(), limits).await?,
        )),
        Selected::GatewayRefusal(refusal) => HopFault::Answer(Box::new(
            super::fault::injected_reply(refusal.reply(), control.clone(), limits).await?,
        )),
        Selected::RelayLost(RelayCut::BeforeHead) => HopFault::LostBeforeHead,
        Selected::RelayLost(RelayCut::AfterHead) => HopFault::LostAfterHead,
        Selected::Stream(fault) => HopFault::Stream(fault),
    }))
}

/// How the relay request under `fault` starts. A failure or answer is
/// taken out of the selection.
#[allow(
    dead_code,
    reason = "the credential gateway relay client sends through it"
)]
pub(crate) fn before_send(fault: &mut Selection) -> BeforeSend {
    match fault.take() {
        Some(HopFault::Fail(failure)) => BeforeSend::Fail(failure),
        Some(HopFault::Answer(answer)) => BeforeSend::Answer(answer),
        other => {
            *fault = other;
            BeforeSend::Send
        }
    }
}

/// Whether the sent request's answer is lost before the relayed head.
#[allow(
    dead_code,
    reason = "the credential gateway relay client reads its answer here"
)]
pub(crate) const fn lost_before_head(fault: &Selection) -> bool {
    matches!(fault, Some(HopFault::LostBeforeHead))
}

/// Whether the hop drops right after the relayed head.
#[allow(
    dead_code,
    reason = "the credential gateway relay client reads its answer here"
)]
pub(crate) const fn cut_after_head(fault: &Selection) -> bool {
    matches!(fault, Some(HopFault::LostAfterHead))
}

/// The relayed `response`, its body taking the selection's stream fault.
#[allow(
    dead_code,
    reason = "the credential gateway relay client builds its responses here"
)]
pub(crate) fn relayed(fault: Selection, mut response: Response) -> Response {
    if let Some(HopFault::Stream(stream)) = fault {
        response.body.fault = Some(stream);
    }
    response
}
