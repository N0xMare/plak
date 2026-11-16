//! Generic host lifecycle envelope and closed pre-handshake discovery reasons.

use super::{HostFrame, ProtocolExtension};
use serde::{Deserialize, Serialize};

/// Advisory lifecycle envelope; the product owns phase, reason and target DTOs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields, bound(serialize = "", deserialize = ""))]
#[schemars(rename = "HostLifecycle", bound = "")]
pub struct HostLifecycle<P: ProtocolExtension> {
    /// This process's generation, also carried by welcome.
    pub host_generation: u64,
    /// Planned transition phase.
    pub phase: P::HostLifecyclePhase,
    /// Planned transition reason.
    pub reason: P::HostLifecycleReason,
    /// A validated destination, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<P::HostLifecycleTarget>,
    /// Advisory retry hint; never permission to replay a request.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "retry"
    )]
    #[schemars(range(max = 30000))]
    pub retry_after_ms: Option<u32>,
}

impl<P: ProtocolExtension> From<HostLifecycle<P>> for HostFrame<P> {
    fn from(notice: HostLifecycle<P>) -> Self {
        Self::HostLifecycle {
            host_generation: notice.host_generation,
            phase: notice.phase,
            reason: notice.reason,
            target: notice.target,
            retry_after_ms: notice.retry_after_ms,
        }
    }
}

pub(super) fn retry<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let value = Option::<u32>::deserialize(d)?;
    if value.is_some_and(|value| value > 30_000) {
        return Err(serde::de::Error::custom("retry_after_ms exceeds 30000"));
    }
    Ok(value)
}

/// Closed transport-discovery reasons, shared by every exact-version handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BridgeUnavailableReason {
    /// No live socket or held host lock was found.
    Absent,
    /// A host did not reach readiness before the discovery deadline.
    Starting,
    /// A verified held host lock reports planned stopping.
    Stopping,
    /// A live host serves an incompatible state key.
    IncompatibleState,
    /// Discovery could not verify host authority; repair is required.
    VerificationFailed,
}
