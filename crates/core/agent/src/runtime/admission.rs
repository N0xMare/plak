//! Exact configured authority admission and revalidation evidence.

use crate::runtime::AgentBuilder;
use crate::runtime::continuation::{
    runtime_component, runtime_manifest_error, runtime_rehydration_error,
};
use crate::runtime::durable::bootstrap::lowercase_hex;
use crate::{AgentError, AgentErrorKind, BindingReference, RuntimeComponentKind, RuntimeManifest};
use plakous_model::{
    CompiledProviderConfiguration, ConfiguredRuntimeAuthority, Digest,
    EffectiveProviderConfigurationId, ModelPreparationError, ModelPreparationErrorKind,
    RuntimeModelAuthority, Timestamp,
};
use std::sync::Arc;
pub(super) async fn sealed_runtime_admission(
    builder: &AgentBuilder,
) -> Result<SealedRuntimeAdmission, AgentError> {
    let checked_at = builder.clock.now();
    let authority = Arc::clone(&builder.model)
        .revalidate_runtime_authority(checked_at)
        .await
        .map_err(|error| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                format!("selected model cannot produce fresh runtime authority ({error})"),
            )
        })?;
    let configured_authority = validate_configuration_runtime_authority(
        &builder.provider_configuration,
        builder.provider_configuration.effective().descriptor(),
        &authority,
        checked_at,
    )
    .map_err(|_| {
        AgentError::new(
            AgentErrorKind::InvalidInput,
            "selected model produced unusable fresh runtime authority",
        )
    })?;
    let account = BindingReference::new(
        "model-runtime-authority",
        lowercase_hex(authority.account_binding().bytes()),
        authority.credential_generation(),
    )
    .map_err(runtime_manifest_error)?;
    let evidence = runtime_component(
        RuntimeComponentKind::AdmissionEvidence,
        serde_json::to_vec(&(
            "plakous-agent/configured-runtime-authority-v3",
            builder.provider_configuration.effective_identity(),
            configured_authority.binding(),
            authority.descriptor_binding(),
            authority.account_binding(),
            authority.credential_authority_binding(),
            authority.credential_generation(),
            authority.admission_witness(),
        ))
        .map_err(|_| {
            AgentError::new(
                AgentErrorKind::InvalidInput,
                "fresh model authority could not be sealed",
            )
        })?,
    )?;
    let fact = crate::RevalidationFact::new(
        account,
        authority.observed_at(),
        authority.valid_until(),
        evidence,
    )
    .map_err(runtime_manifest_error)?;
    Ok(SealedRuntimeAdmission { fact })
}

pub(super) fn validate_configuration_runtime_authority(
    configuration: &CompiledProviderConfiguration,
    descriptor: &plakous_model::ResolvedModelDescriptor,
    authority: &RuntimeModelAuthority,
    checked_at: Timestamp,
) -> Result<ConfiguredRuntimeAuthority, ModelPreparationError> {
    configuration
        .seal_runtime_authority(descriptor, authority.clone(), checked_at)
        .map_err(|_| {
            ModelPreparationError::rejected(
                ModelPreparationErrorKind::InvalidConfiguration,
                "fresh model authority disagrees with the effective provider configuration",
            )
        })
}

pub(super) fn recorded_admission_authority(
    fact: &crate::RevalidationFact,
    configuration: &CompiledProviderConfiguration,
) -> Result<ConfiguredRuntimeAuthority, crate::recovery_scope::RevalidationError> {
    let mismatch = crate::recovery_scope::RevalidationError::BindingMismatch;
    if fact.evidence().kind() != RuntimeComponentKind::AdmissionEvidence
        || fact.evidence().codec_revision() != 1
    {
        return Err(mismatch);
    }
    let ConfiguredAdmissionEvidence(
        domain,
        effective,
        binding,
        descriptor_binding,
        account,
        credential,
        generation,
        witness,
    ) = serde_json::from_slice(fact.evidence().bytes()).map_err(|_| mismatch)?;
    let reference = BindingReference::new(
        "model-runtime-authority",
        lowercase_hex(account.bytes()),
        generation,
    )
    .map_err(|_| mismatch)?;
    if domain != "plakous-agent/configured-runtime-authority-v3"
        || effective != configuration.effective_identity()
        || fact.reference() != &reference
    {
        return Err(mismatch);
    }
    let descriptor = configuration.effective().descriptor();
    let mut authority = RuntimeModelAuthority::new(
        descriptor,
        account,
        credential,
        generation,
        fact.observed_at(),
        fact.valid_until(),
    )
    .map_err(|_| mismatch)?;
    if let Some(witness) = witness {
        authority = authority.with_admission_witness(witness);
    }
    if descriptor_binding != authority.descriptor_binding() {
        return Err(mismatch);
    }
    // Historical authority is checked at its observation, never treated as a
    // current dispatch permit. Fresh authority is also checked at now below.
    let configured = configuration
        .seal_runtime_authority(descriptor, authority, fact.observed_at())
        .map_err(|_| mismatch)?;
    if configured.binding() != binding {
        return Err(mismatch);
    }
    Ok(configured)
}

pub(super) fn refreshed_reopen_admission(
    manifest: &RuntimeManifest,
    configuration: &CompiledProviderConfiguration,
    fresh: &crate::RevalidationFact,
) -> Result<crate::RevalidationFact, AgentError> {
    let verify = || -> Result<(), crate::recovery_scope::RevalidationError> {
        let historical = recorded_admission_authority(
            manifest.binding().admission(),
            manifest.provider_configuration(),
        )?;
        let current = recorded_admission_authority(fresh, configuration)?;
        if historical.effective_configuration() != current.effective_configuration()
            || historical.runtime().account_binding() != current.runtime().account_binding()
            || historical.runtime().credential_authority_binding()
                != current.runtime().credential_authority_binding()
            || current.runtime().credential_generation()
                < historical.runtime().credential_generation()
        {
            return Err(crate::recovery_scope::RevalidationError::BindingMismatch);
        }
        Ok(())
    };
    verify().map_err(|error| runtime_rehydration_error(error.into()))?;
    // Only this runtime-owned, verified admission may bridge refreshed evidence
    // to the historical reference. Neither the checkpoint nor Working attempt
    // authority is replaced, and generic reference equality stays exact.
    crate::RevalidationFact::new(
        manifest.binding().admission().reference().clone(),
        fresh.observed_at(),
        fresh.valid_until(),
        fresh.evidence().clone(),
    )
    .map_err(runtime_manifest_error)
}

#[derive(Clone)]
pub(in crate::runtime) struct SealedRuntimeAdmission {
    pub(in crate::runtime) fact: crate::RevalidationFact,
}

// This is the existing admission tuple encoding, not a new persisted format.
#[derive(serde::Deserialize)]
pub(in crate::runtime) struct ConfiguredAdmissionEvidence(
    String,
    EffectiveProviderConfigurationId,
    Digest,
    Digest,
    Digest,
    Digest,
    u64,
    Option<plakous_model::RuntimeAdmissionWitness>,
);
