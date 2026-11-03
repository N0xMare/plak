//! Frozen provider-facing aliases over exact registered tool identities.

mod lowering;
mod map;
mod projection;
mod raising;

pub use lowering::{PI_READ_IMAGE_REFUSAL, UNIFIED_EXEC_MAX_YIELD_MS, UNIFIED_EXEC_MIN_YIELD_MS};
pub use map::{ToolExposure, ToolExposureMap};
pub(crate) use map::{
    exposed_definition, exposure_map_digest, registry_identity_digest,
    registry_identity_digest_from_identities,
};
pub(crate) use projection::projection_map_digest;
pub use projection::{
    CanonicalProcessOperationV2, ProjectedToolInvocationV2, ProviderModelProjectionIdentityV2,
    ToolProjectionExposureV2, ToolProjectionLoweringV2, ToolProjectionMap, ToolProjectionRaisingV2,
    ToolProjectionReceiptV2,
};
pub(crate) use raising::{ReadPageStyle, cut_read_page};

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
