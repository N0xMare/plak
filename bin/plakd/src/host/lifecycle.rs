//! Product reasons for admitted planned transitions, never admission authority.

use plakd_wire_types::lifecycle::{HostLifecycle, HostLifecyclePhase, HostLifecycleReason};
use plakous_hosted::host_service::wire::SessionHost;

use crate::surface_protocol::PlakExtension;

fn notice(
    host: &SessionHost<PlakExtension>,
    phase: HostLifecyclePhase,
    reason: HostLifecycleReason,
) -> HostLifecycle {
    HostLifecycle {
        host_generation: host.generation(),
        phase,
        reason,
        target: None,
        retry_after_ms: None,
    }
}

/// Seeds every connection admitted before the startup inventories complete.
pub(crate) fn starting(host: &SessionHost<PlakExtension>) {
    host.publish_lifecycle(notice(
        host,
        HostLifecyclePhase::Starting,
        HostLifecycleReason::Startup,
    ));
}

/// Tells the connections seeded with `starting` that it ended. A later
/// connection gets no startup notice.
pub(crate) fn running(host: &SessionHost<PlakExtension>) {
    host.settle_lifecycle(notice(
        host,
        HostLifecyclePhase::Running,
        HostLifecycleReason::Startup,
    ));
}

pub(crate) fn stopping(host: &SessionHost<PlakExtension>) {
    if host.lifecycle().is_some_and(|notice| {
        matches!(
            notice.phase,
            HostLifecyclePhase::Stopping | HostLifecyclePhase::Restarting
        )
    }) {
        return;
    }
    host.publish_lifecycle(notice(
        host,
        HostLifecyclePhase::Stopping,
        HostLifecycleReason::Shutdown,
    ));
}
