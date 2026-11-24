//! Product projections for the unserved protocol 13 boundary.

mod usage;
pub use usage::{accounts_usage, usage_limits};

use plakd_wire_types::{console, v13};
use plakous_host_protocol::wire::RequestId;

fn host_response(id: RequestId, response: v13::HostResponseV13) -> v13::HostFrame {
    v13::HostFrame::Draft(Box::new(v13::HostFrameV13::Response {
        id,
        outcome: v13::Outcome::Ok(Box::new(v13::Answer::Draft(v13::AnswerV13::Host {
            response: Box::new(v13::HostResponse::Draft(response)),
        }))),
    }))
}

/// Projects a known Changes version mismatch, never an untyped VCS refusal.
#[must_use]
pub fn changes_refusal(id: RequestId, response: console::HostResponse) -> Option<v13::HostFrame> {
    let console::HostResponse::Refused {
        reason:
            console::ReadRefusal::WorkspaceReadFailed {
                version: Some(reason),
                ..
            },
    } = response
    else {
        return None;
    };
    Some(host_response(
        id,
        v13::HostResponseV13::Refused {
            reason: v13::ReadRefusal::Draft(reason),
        },
    ))
}
