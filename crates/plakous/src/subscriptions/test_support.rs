//! Explicit qualification-only native resolution from caller-owned model doubles.

use super::NativeToolMode;
use super::resolution::{ResolvedNativeModel, ResolvedProvider, SubscriptionResolutionEvidence};
use plakous_model::{ConfiguredLanguageModel, Timestamp};

/// Supplies native construction evidence without credentials or inference.
/// This test-clock-only seam has no supported embedder stability claim.
///
/// # Errors
/// Returns invalid fixed fixture time.
pub fn resolved_provider(
    configured: ConfiguredLanguageModel,
    native_tool_mode: Option<NativeToolMode>,
) -> Result<ResolvedProvider, Box<dyn std::error::Error>> {
    let descriptor = configured.registered().descriptor().clone();
    let model = configured.model();
    let route = configured.configuration().selected().route();
    let evidence = SubscriptionResolutionEvidence {
        account_fingerprint: "binding-fixture-account".into(),
        credential_authority_binding: "binding-fixture-authority".into(),
        credential_generation: 1,
        credential_selection_epoch: "binding-fixture-epoch".into(),
        route_fingerprint: route.route().as_str().to_owned(),
        descriptor_digest: "binding-fixture-descriptor".into(),
        dialect: route.protocol().as_str().to_owned(),
        adapter_revision: "binding-fixture-adapter".into(),
        catalog_observed_at: Timestamp::new(1, 0)?,
        catalog_expires_at: Timestamp::new(u64::MAX / 4, 0)?,
        runtime_admission_witness: None,
        profile_revision: route.integration_profile().as_str().to_owned(),
        exact: None,
        exact_provider: None,
    };
    Ok(ResolvedProvider {
        model: ResolvedNativeModel {
            service_tiers: Vec::new(),
            selected_model: descriptor.requested_model().clone(),
            descriptor,
            model,
            evidence,
            retrieval: None,
            image_gen: None,
        },
        configured,
        native_tool_mode,
        reresolution: None,
    })
}
