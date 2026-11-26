//! Product reasons for admitted planned transitions, never admission authority.

use plakd_wire_types::lifecycle::{HostLifecycle, HostLifecyclePhase, HostLifecycleReason};
use plakous_hosted::host_service::wire::SessionHost;

use crate::surface_protocol::PlakExtension;

pub(crate) fn stopping(host: &SessionHost<PlakExtension>) {
    if host.lifecycle().is_some_and(|notice| {
        matches!(
            notice.phase,
            HostLifecyclePhase::Stopping | HostLifecyclePhase::Restarting
        )
    }) {
        return;
    }
    host.publish_lifecycle(HostLifecycle {
        host_generation: host.generation(),
        phase: HostLifecyclePhase::Stopping,
        reason: HostLifecycleReason::Shutdown,
        target: None,
        retry_after_ms: None,
    });
}
