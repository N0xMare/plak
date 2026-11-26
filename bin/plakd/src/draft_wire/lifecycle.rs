use plakd_wire_types::{lifecycle as legacy, v13};

use super::write_frame;

/// Projects a hosted advisory onto the protocol 13 connection.
///
/// # Errors
/// The frame could not be encoded or written.
pub fn write_lifecycle(
    out: &mut dyn std::io::Write,
    notice: legacy::HostLifecycle,
) -> plakous_hosted::CliResult<()> {
    use v13::{HostLifecyclePhase as Phase, HostLifecycleReason as Reason};
    let phase = match notice.phase {
        legacy::HostLifecyclePhase::Draining => Phase::Draining,
        legacy::HostLifecyclePhase::Restarting => Phase::Restarting,
        legacy::HostLifecyclePhase::Stopping => Phase::Stopping,
        legacy::HostLifecyclePhase::Running => Phase::Running,
    };
    let reason = match notice.reason {
        legacy::HostLifecycleReason::PairedUpgrade => Reason::PairedUpgrade,
        legacy::HostLifecycleReason::CrossKeyUpgrade => Reason::CrossKeyUpgrade,
        legacy::HostLifecycleReason::Shutdown => Reason::Shutdown,
    };
    write_frame(
        out,
        v13::HostFrameV13::HostLifecycle {
            host_generation: notice.host_generation,
            phase,
            reason,
            target: notice.target,
            seconds_remaining: 0,
            deadline_at: None,
            retry_after_ms: notice.retry_after_ms,
        },
    )
}
