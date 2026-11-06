//! Reference-client-envelope SuperGrok input-envelope enrollment.
//!
//! The generic model layer owns admission algebra. This module owns the
//! literal subscription models, exact authenticated catalog receipt binding,
//! closed Responses estimator coefficients, and reference corpus identity.

#[cfg(test)]
use plakous_model::DigestDomain;
use plakous_model::{
    CapacityCertificate, CapacityRule, ContextContract, ContextContractError,
    ContextEvidenceSource, CountCertificateKind, CountObservation, Digest, EvidenceFreshness,
    MeasurementCertificate, ProtocolId, RuntimeModelAuthority, Timestamp,
};
use plakous_responses::{ResponsesWireOccurrence, ResponsesWireOccurrenceCounts};
use thiserror::Error;

use crate::native_auth::grok_enrollment;
#[cfg(test)]
use crate::native_auth::{
    GROK_REFERENCE_REVISION, XAI_GROK_46_CODING_MODEL, XAI_GROK_47_CODING_MODEL,
    XAI_GROK_47_ENROLLMENT_REVISION, XAI_GROK_47_FAST_CODING_MODEL, XAI_GROK_BUILD_MODEL,
    XAI_GROK_PRIMARY_CODING_MODEL,
};

pub(crate) const GROK_CONTEXT_ACCOUNTING_REVISION: &str = "responses-wire-accounting-v4";
pub(crate) const GROK_CONTEXT_PROJECTION_REVISION: &str = "responses-canonical-grok-v3";
pub(crate) const GROK_OUTPUT_RESERVATION_REVISION: &str = "xai-grok-wire-max-output-v1";
pub(crate) const GROK_MEASUREMENT_CERTIFICATE_ID: &str = "xai-grok-responses-upper-bound";
pub(crate) const GROK_MEASUREMENT_CERTIFICATE_REVISION: &str = "release-2026-08-10-r3";
pub(crate) const GROK_ESTIMATOR_METHOD: &str = "complete-wire-byte-occurrence-upper-bound";
pub(crate) const GROK_ESTIMATOR_REVISION: &str = "grok-responses-v3";

const RESPONSES_FAMILY: &str = "responses-family";
const FIXED_MARGIN: u64 = 4_096;
#[cfg(test)]
const PROOF_CORPUS: &[u8] =
    include_bytes!("../tests/fixtures/context/grok-responses-v1/corpus.json");

pub(crate) fn build_context_contract(
    model: &str,
    reported_context_length: u64,
    account_binding: Digest,
    credential_authority_binding: Digest,
    observed_at: Timestamp,
    expires_at: Timestamp,
    catalog_receipt: Digest,
) -> Result<Option<ContextContract>, ContextContractError> {
    let Some(enrollment) = grok_enrollment(model) else {
        return Ok(None);
    };
    // The window is the authenticated catalog's own value and nothing else:
    // a row that reports none (or zero) has no contract, so the model cannot
    // be opened rather than being run against a guessed window.
    if reported_context_length == 0 {
        return Ok(None);
    }
    let window = reported_context_length;
    // Admit the whole window. Grok Build's 80% is the compact trigger, not a
    // send cap; stacking 85% on an 80% admit was the old 340k treadmill.
    let maximum_projected_input = window;
    let auto_compact_input = enrollment
        .auto_compact_percent
        .map(|percent| {
            window
                .checked_mul(percent)
                .and_then(|tokens| tokens.checked_div(100))
                .filter(|tokens| *tokens != 0)
                .ok_or(ContextContractError::IntegrityMismatch {
                    field: "Grok Build reference compact trigger",
                })
        })
        .transpose()?;
    let authority_binding =
        RuntimeModelAuthority::authority_class_for(account_binding, credential_authority_binding)
            .map_err(|_| ContextContractError::IntegrityMismatch {
            field: "xAI runtime authority class",
        })?;
    let freshness = EvidenceFreshness::new(observed_at, Some(expires_at))?;
    let capacity = CapacityCertificate::reference_client_envelope(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        enrollment.capacity_evidence_id,
        GROK_MEASUREMENT_CERTIFICATE_REVISION,
        CapacityRule::ProviderReservedInput {
            reported_context_window: Some(reported_context_length),
            maximum_projected_input,
            auto_compact_input,
        },
        authority_binding,
        freshness,
        catalog_receipt,
        enrollment.reference_revision,
    )?;
    let measurement = MeasurementCertificate::standard(
        &plakous_model::StandardIntegrationAuthority::release_owned(),
        GROK_MEASUREMENT_CERTIFICATE_ID,
        GROK_MEASUREMENT_CERTIFICATE_REVISION,
        GROK_ESTIMATOR_METHOD,
        GROK_ESTIMATOR_REVISION,
        ContextEvidenceSource::AuthenticatedDiscovery,
        CountCertificateKind::ReferenceClientEnvelopeUpperBound { catalog_receipt },
        Some(authority_binding),
        freshness,
    )?;
    Ok(Some(ContextContract::new(
        ProtocolId::new(RESPONSES_FAMILY).map_err(|_| ContextContractError::IntegrityMismatch {
            field: "Responses family identity",
        })?,
        GROK_CONTEXT_ACCOUNTING_REVISION,
        GROK_CONTEXT_PROJECTION_REVISION,
        GROK_OUTPUT_RESERVATION_REVISION,
        capacity,
        vec![measurement],
    )?))
}

pub(crate) fn count_observation(
    model: &str,
    wire_bytes: u64,
    counts: ResponsesWireOccurrenceCounts,
) -> Result<CountObservation, GrokContextEstimateError> {
    if grok_enrollment(model).is_none() {
        return Err(GrokContextEstimateError::UnenrolledModel);
    }
    // An image's data URL is not text the model reads; its token cost is the
    // per-image margin instead of its serialized bytes.
    let mut upper_bound = wire_bytes
        .checked_sub(counts.image_url_bytes())
        .and_then(|bytes| bytes.checked_add(FIXED_MARGIN))
        .ok_or(GrokContextEstimateError::ArithmeticOverflow)?;
    for occurrence in ResponsesWireOccurrence::ALL {
        let count = u64::from(counts.count(occurrence));
        let margin = occurrence_margin(occurrence);
        upper_bound = upper_bound
            .checked_add(
                count
                    .checked_mul(margin)
                    .ok_or(GrokContextEstimateError::ArithmeticOverflow)?,
            )
            .ok_or(GrokContextEstimateError::ArithmeticOverflow)?;
    }
    CountObservation::new(
        GROK_MEASUREMENT_CERTIFICATE_ID,
        GROK_MEASUREMENT_CERTIFICATE_REVISION,
        upper_bound,
    )
    .map_err(GrokContextEstimateError::Contract)
}

#[derive(Debug, Error)]
pub(crate) enum GrokContextEstimateError {
    #[error("the model has no reviewed reference-client-envelope Grok context enrollment")]
    UnenrolledModel,
    #[error("the Grok context upper-bound arithmetic overflowed")]
    ArithmeticOverflow,
    #[error(transparent)]
    Contract(#[from] ContextContractError),
}

const fn occurrence_margin(occurrence: ResponsesWireOccurrence) -> u64 {
    match occurrence {
        ResponsesWireOccurrence::Message => 64,
        ResponsesWireOccurrence::InputText
        | ResponsesWireOccurrence::OutputText
        | ResponsesWireOccurrence::Refusal
        | ResponsesWireOccurrence::ReasoningSummary => 32,
        ResponsesWireOccurrence::FunctionCall | ResponsesWireOccurrence::FunctionCallOutput => 96,
        ResponsesWireOccurrence::FunctionTool => 256,
        ResponsesWireOccurrence::SchemaNode => 16,
        ResponsesWireOccurrence::ReasoningReplay => 128,
        ResponsesWireOccurrence::OpaqueReplay => 256,
        ResponsesWireOccurrence::InputImage => plakous_model::CONSERVATIVE_IMAGE_MAX_TOKENS,
    }
}

#[cfg(test)]
mod tests;
