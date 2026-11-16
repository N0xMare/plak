//! Product reasons for admitted planned transitions, never admission authority.

use plakd_wire_types::lifecycle::{
    HostLifecycle, HostLifecyclePhase, HostLifecycleReason, HostLifecycleTarget,
};
use plakous_hosted::host_service::wire::SessionHost;

use crate::surface_protocol::PlakExtension;

pub(super) fn draining(host: &SessionHost<PlakExtension>, binary: &str, key: &str) {
    let reason = if key == crate::home::state::state_key() {
        HostLifecycleReason::PairedUpgrade
    } else {
        HostLifecycleReason::CrossKeyUpgrade
    };
    host.publish_lifecycle(HostLifecycle {
        host_generation: host.generation(),
        phase: HostLifecyclePhase::Draining,
        reason,
        target: Some(HostLifecycleTarget {
            binary_id: binary.to_owned(),
            state_key: key.to_owned(),
        }),
        retry_after_ms: Some(250),
    });
}

pub(super) async fn restarting(host: &SessionHost<PlakExtension>) {
    if let Some(mut notice) = host.lifecycle() {
        notice.phase = HostLifecyclePhase::Restarting;
        host.publish_lifecycle(notice);
    }
    host.wait_lifecycle_delivery(std::time::Duration::from_secs(1))
        .await;
}

pub(super) fn running(host: &SessionHost<PlakExtension>) {
    if let Some(mut notice) = host.lifecycle() {
        if notice.phase != HostLifecyclePhase::Draining {
            return;
        }
        notice.phase = HostLifecyclePhase::Running;
        notice.target = None;
        notice.retry_after_ms = None;
        host.publish_lifecycle(notice);
    }
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
    host.publish_lifecycle(HostLifecycle {
        host_generation: host.generation(),
        phase: HostLifecyclePhase::Stopping,
        reason: HostLifecycleReason::Shutdown,
        target: None,
        retry_after_ms: None,
    });
}
