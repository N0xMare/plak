use plakd_wire_types::{lifecycle as legacy, v13};

use super::{LifecycleState, write_frame};
use crate::surface_protocol::PlakExtension;

/// Sends an advisory after Welcome, with a countdown sampled by the connection.
///
/// # Errors
///
/// The frame could not be encoded or written.
pub fn write_lifecycle(
    out: &mut dyn std::io::Write,
    state: LifecycleState<PlakExtension>,
    seconds_remaining: u64,
) -> plakous_hosted::CliResult<()> {
    use v13::{HostLifecyclePhase as Phase, HostLifecycleReason as Reason};
    let (host_generation, phase, reason, target, retry_after_ms) = match state {
        LifecycleState::Starting { host_generation } => (
            host_generation,
            Phase::Starting,
            Reason::PairedUpgrade,
            None,
            Some(250),
        ),
        LifecycleState::Started { host_generation } => (
            host_generation,
            Phase::Running,
            Reason::PairedUpgrade,
            None,
            None,
        ),
        LifecycleState::Transition(notice) => (
            notice.host_generation,
            match notice.phase {
                legacy::HostLifecyclePhase::Draining => Phase::Draining,
                legacy::HostLifecyclePhase::Restarting => Phase::Restarting,
                legacy::HostLifecyclePhase::Stopping => Phase::Stopping,
                legacy::HostLifecyclePhase::Running => Phase::Running,
            },
            match notice.reason {
                legacy::HostLifecycleReason::PairedUpgrade => Reason::PairedUpgrade,
                legacy::HostLifecycleReason::CrossKeyUpgrade => Reason::CrossKeyUpgrade,
                legacy::HostLifecycleReason::Shutdown => Reason::Shutdown,
            },
            notice.target,
            notice.retry_after_ms,
        ),
    };
    write_frame(
        out,
        v13::HostFrameV13::HostLifecycle {
            host_generation,
            phase,
            reason,
            target,
            seconds_remaining,
            deadline_at: None,
            retry_after_ms,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successor_starting_is_an_advisory_not_a_handshake_refusal() {
        let mut out = Vec::new();
        write_lifecycle(
            &mut out,
            LifecycleState::Starting {
                host_generation: 43,
            },
            3,
        )
        .unwrap();
        let frame = serde_json::from_slice::<serde_json::Value>(&out).unwrap();
        assert_eq!(
            frame,
            serde_json::json!({
                "frame": "host_lifecycle",
                "host_generation": 43,
                "phase": "starting",
                "reason": "paired_upgrade",
                "seconds_remaining": 3,
                "retry_after_ms": 250
            })
        );
    }
}
