//! Product projections for the unserved protocol 13 boundary.

mod usage;
pub use usage::{accounts_usage, usage_limits};

use plakd_wire_types::{console, v13};
use plakous_host_protocol::wire::{BlobRejection, BlobRejectionReason, RequestId};
use plakous_hosted::host_service::wire::LifecycleState;

mod lifecycle;
pub use lifecycle::write_lifecycle;

/// Writes a draft rejection only for the named blob, without an intent verdict.
///
/// # Errors
///
/// The outcome could not be encoded or written.
pub fn write_blob_refusal(
    out: &mut dyn std::io::Write,
    BlobRejection { id, reason }: BlobRejection,
) -> plakous_hosted::CliResult<()> {
    let reason = match reason {
        BlobRejectionReason::Malformed => v13::BlobRefusal::Malformed,
        BlobRejectionReason::TooLarge => v13::BlobRefusal::TooLarge,
        BlobRejectionReason::PendingLimit => v13::BlobRefusal::PendingLimit,
        BlobRejectionReason::Discarded => v13::BlobRefusal::Discarded,
    };
    write_frame(out, v13::HostFrameV13::BlobRefused { id, reason })
}

fn write_frame(
    out: &mut dyn std::io::Write,
    frame: v13::HostFrameV13,
) -> plakous_hosted::CliResult<()> {
    let frame = v13::HostFrame::Draft(Box::new(frame));
    let line = plakous_host_protocol::wire::encode_line(&frame)
        .map_err(|error| plakous_hosted::CliFailure::new(error.to_string()))?;
    out.write_all(line.as_bytes())
        .and_then(|()| out.flush())
        .map_err(|_| plakous_hosted::CliFailure::new("stdout failed"))
}

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
