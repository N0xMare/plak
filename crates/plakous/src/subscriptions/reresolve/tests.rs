use std::collections::BTreeMap;
use std::error::Error;
use std::num::NonZeroU32;
use std::sync::Arc;

use plakous_model::{
    Capability, CapabilityConstraints, CapabilitySet, Digest, DigestDomain, FactProvenance,
    FactSource, FeatureSupport, LanguageModel, LocalDurabilityDisposition, ModelAdapterProfileId,
    ModelFamilyId, ModelId, OutputBoundDisposition, ProtocolId, ResolvedModelDescriptor, RouteId,
    RuntimeModelAuthority, Support, Timestamp,
};

use super::super::{
    NativeToolMode, ResolvedNativeModel, ResolvedProvider, SubscriptionResolutionEvidence,
};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

pub(in crate::subscriptions) struct Fixture {
    observed: u64,
    epoch: &'static str,
    route: &'static str,
    account: &'static [u8],
    generation: u64,
    tool_mode: NativeToolMode,
}

pub(in crate::subscriptions) const BASE: Fixture = Fixture {
    observed: 1,
    epoch: "login-epoch-a",
    route: "route-fingerprint-a",
    account: b"account-a",
    generation: 3,
    tool_mode: NativeToolMode::Direct,
};

pub(in crate::subscriptions) fn resolved(fixture: &Fixture) -> TestResult<ResolvedProvider> {
    let observed = Timestamp::new(fixture.observed, 0)?;
    let expires = Timestamp::new(fixture.observed + 21_600, 0)?;
    let mut features = BTreeMap::new();
    features.insert(
        Capability::Streaming,
        FeatureSupport::new(
            Support::Native,
            CapabilityConstraints::default(),
            FactProvenance::new(FactSource::RouteEvidence, observed, Some(expires)),
        ),
    );
    let descriptor = crate::test_model::with_context_contract(
        ResolvedModelDescriptor::new(
            &plakous_model::StandardIntegrationAuthority::release_owned(),
            ProtocolId::new("test.protocol")?,
            RouteId::new("test.route")?,
            ModelId::new("test.model")?,
            Some(ModelId::new("test.model")?),
            Some(ModelFamilyId::new("test.family")?),
            ModelAdapterProfileId::new("test.profile")?,
            CapabilitySet::new(features, BTreeMap::new()),
        ),
        observed,
    )?;
    let authority = RuntimeModelAuthority::new(
        &descriptor,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[fixture.account])?,
        Digest::sha256(DigestDomain::ModelBinding, 1, &[b"credential-authority"])?,
        fixture.generation,
        observed,
        Some(expires),
    )?;
    let model: Arc<dyn LanguageModel> = Arc::new(
        plakous_test_support::ScriptedModel::new(descriptor.clone(), [])
            .with_runtime_authority(authority.clone()),
    );
    let configured = crate::test_model::configured_model(
        Arc::clone(&model),
        descriptor.clone(),
        authority,
        std::iter::empty(),
        LocalDurabilityDisposition::Ephemeral,
        OutputBoundDisposition::Enforced(NonZeroU32::new(128).ok_or("nonzero bound")?),
    )?;
    Ok(ResolvedProvider {
        model: ResolvedNativeModel {
            service_tiers: Vec::new(),
            selected_model: descriptor.requested_model().clone(),
            descriptor,
            model,
            evidence: SubscriptionResolutionEvidence {
                account_fingerprint: "fixture-account".to_owned(),
                credential_authority_binding: "fixture-credential".to_owned(),
                credential_generation: fixture.generation,
                credential_selection_epoch: fixture.epoch.to_owned(),
                route_fingerprint: fixture.route.to_owned(),
                descriptor_digest: "fixture-descriptor".to_owned(),
                dialect: "test.protocol".to_owned(),
                adapter_revision: "fixture-adapter".to_owned(),
                catalog_observed_at: observed,
                catalog_expires_at: expires,
                runtime_admission_witness: None,
                profile_revision: "fixture-profile".to_owned(),
                exact: None,
                exact_provider: None,
            },
            retrieval: None,
            image_gen: None,
        },
        configured,
        native_tool_mode: Some(fixture.tool_mode),
        reresolution: None,
    })
}

#[test]
fn a_later_observation_of_the_same_login_and_model_is_admitted() -> TestResult {
    let previous = resolved(&BASE)?;
    let fresh = resolved(&Fixture {
        observed: 30_000,
        generation: 4,
        ..BASE
    })?;
    assert_ne!(
        previous.model.descriptor.binding_digest()?,
        fresh.model.descriptor.binding_digest()?,
        "the fresh descriptor is a new descriptor"
    );
    assert_eq!(
        previous.configuration().effective_identity(),
        fresh.configuration().effective_identity(),
        "observation and expiry stay out of the effective identity"
    );
    assert!(previous.admits_fresh(&fresh));
    Ok(())
}

#[test]
fn a_new_login_another_account_a_changed_row_or_an_older_generation_is_refused() -> TestResult {
    let previous = resolved(&BASE)?;
    for (case, fresh) in [
        (
            "new login of the same account",
            Fixture {
                epoch: "login-epoch-b",
                ..BASE
            },
        ),
        (
            "another account",
            Fixture {
                account: b"account-b",
                ..BASE
            },
        ),
        (
            "changed route or catalog row",
            Fixture {
                route: "route-fingerprint-b",
                ..BASE
            },
        ),
        (
            "changed native tool mode",
            Fixture {
                tool_mode: NativeToolMode::CodeMode,
                ..BASE
            },
        ),
        (
            "older credential generation",
            Fixture {
                generation: 2,
                ..BASE
            },
        ),
    ] {
        assert!(
            !previous.admits_fresh(&resolved(&fresh)?),
            "{case} must fail closed"
        );
    }
    Ok(())
}

#[test]
fn a_resolution_without_a_window_has_no_reresolution() -> TestResult {
    let provider = resolved(&BASE)?;
    assert_eq!(provider.admitted_until(), None);
    Ok(())
}
