use super::protocol::{ClientOp, HostOp};

pub(super) const LOGIN: u64 = 5;
pub(super) const SOCKET: u64 = 5;
pub(super) const PROBE: u64 = 30;
pub(super) const DOCTOR: u64 = 45;
pub(super) const BINDINGS: u64 = 75;
pub(super) const CANARY: u64 = 105;
pub(super) const UPGRADE: u64 = 720;

const MARGIN: u64 = 60; // SSH startup, image/hash and local filesystem work.
// Two unit commands, a rendered unit, five build probes, three gateway/account
// commands and the canary probe; two identity reads plus Welcome read/write.
const OBSERVE: u64 = 12 * PROBE + 4 * SOCKET;
const GENERATION: u64 = 2 * PROBE;
const PREFLIGHT: u64 = 2 * PROBE;
const TRANSFER: u64 = 240;

pub(super) fn host(op: &HostOp) -> u64 {
    LOGIN
        + MARGIN
        + match op {
            HostOp::Observe | HostOp::HostLink { .. } => OBSERVE,
            HostOp::Health { routes, .. } => {
                OBSERVE + DOCTOR + BINDINGS + GENERATION * routes.len() as u64
            }
            HostOp::CanaryPreflight { .. } => OBSERVE + PREFLIGHT,
            HostOp::Canary { .. } => OBSERVE + PREFLIGHT + 2 * GENERATION + CANARY,
            HostOp::StageHost { .. } => TRANSFER,
            HostOp::DestinationUnit { .. } => PROBE,
            HostOp::DestinationCanaryPreflight { .. } => PREFLIGHT,
            HostOp::Upgrade { drain, .. } => OBSERVE + 2 * PROBE + UPGRADE + u64::from(*drain),
            HostOp::MarkProduction { .. } => 0,
        }
}

pub(super) fn client(op: &ClientOp) -> u64 {
    LOGIN
        + MARGIN
        + match op {
            ClientOp::Stage => TRANSFER,
            ClientOp::Link { .. } | ClientOp::Check { .. } => PROBE,
        }
}
