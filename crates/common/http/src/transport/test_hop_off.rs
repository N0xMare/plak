//! The product build's gateway hop: no faults (`test_hop.rs` is the
//! repository-only `fault-injection` counterpart).

use super::{Client, Response};
use crate::{ExchangeControl, Failure, Limits};

/// No plan.
pub(crate) struct HopFaults;

/// No fault.
#[allow(
    dead_code,
    reason = "the credential gateway relay client selects its faults here"
)]
pub(crate) struct Selection;

/// How a relay request starts: always sent.
#[allow(
    dead_code,
    reason = "the credential gateway relay client sends through it"
)]
pub(crate) enum BeforeSend {
    /// Never built without faults.
    Fail(Failure),
    /// Never built without faults.
    Answer(Box<Response>),
    /// Send the request on the hop.
    Send,
}

/// Nothing to apply.
pub(crate) const fn relaying_faults(_client: &Client) -> HopFaults {
    HopFaults
}

/// No fault is selected.
#[allow(
    dead_code,
    clippy::unused_async,
    reason = "the credential gateway relay client selects its faults here"
)]
pub(crate) async fn select(
    _faults: &HopFaults,
    _host: &str,
    _path_and_query: &str,
    _control: &ExchangeControl,
    _limits: &Limits,
) -> Result<Selection, Failure> {
    Ok(Selection)
}

/// Every relay request is sent.
#[allow(
    dead_code,
    reason = "the credential gateway relay client sends through it"
)]
pub(crate) const fn before_send(_fault: &mut Selection) -> BeforeSend {
    BeforeSend::Send
}

/// Never lost.
#[allow(
    dead_code,
    reason = "the credential gateway relay client reads its answer here"
)]
pub(crate) const fn lost_before_head(_fault: &Selection) -> bool {
    false
}

/// Never cut.
#[allow(
    dead_code,
    reason = "the credential gateway relay client reads its answer here"
)]
pub(crate) const fn cut_after_head(_fault: &Selection) -> bool {
    false
}

/// The response unchanged.
#[allow(
    dead_code,
    reason = "the credential gateway relay client builds its responses here"
)]
pub(crate) const fn relayed(_fault: Selection, response: Response) -> Response {
    response
}
