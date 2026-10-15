//! Provider recovery locators, prepared call records and recovery results.

use std::fmt;
use std::num::NonZeroU32;
use std::sync::Arc;

use serde::{Deserialize, Deserializer, Serialize};
use thiserror::Error;

use super::authority::{RuntimeModelAuthority, RuntimeModelAuthorityError};
use super::preparation::{
    ModelPreparationContext, ModelPreparationError, PreparedModelProjection,
    validate_prepared_digest,
};
use super::prepared_call::PreparedCallError;
use super::{ModelError, ModelErrorKind, ModelRetryability};
use crate::{
    ContextAdmission, ContextAdmissionError, Digest, DigestDomain, ModelId, ModelOperationRoot,
    ModelRequest, ModelResponse, ProtocolId, ResolvedModelDescriptor, RouteId, Timestamp, Usage,
    ValidatedRequestPlan, ValidationMode,
};

/// Maximum serialized non-secret preparation record retained for recovery.
pub const MAX_PREPARED_CALL_RECORD_BYTES: usize = 4 * 1024 * 1024;

/// Current semantic/codec revision of [`PreparedCallRecord`].
pub const PREPARED_CALL_RECORD_SEMANTIC_REVISION: u32 = 5;

/// Current provider-operation digest revision for prepared and recovery records.
pub const PROVIDER_OPERATION_SEMANTIC_REVISION: u32 = 9;

/// Current neutral envelope revision for provider-owned recovery locators.
pub const PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION: u32 = 2;

/// Current neutral provider-call recovery request revision.
pub const MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION: u32 = 2;
/// Maximum opaque provider-owned bytes in one recovery locator.
pub const MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES: usize = 4_096;

/// Current neutral provider-call recovery result revision.
pub const MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION: u32 = 2;

/// Current neutral provider-call recovery evidence revision.
pub const MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION: u32 = 1;

/// The point at which a provider recovery locator became available.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum ProviderCallRecoveryLocatorStage {
    /// The locator was fixed before the provider-visible dispatch boundary.
    Prepared,
    /// The locator was learned from provider evidence after dispatch.
    LearnedAfterDispatch,
}

/// A bounded, versioned, credential-free provider recovery locator.
///
/// The provider owns the lookup semantics behind `provider_revision`. The
/// neutral envelope retains exact nonempty opaque bytes and binds them to the
/// stable operation root, descriptor, request, plan, and prepared projection.
/// The bytes are correlation material, not provider request/response content
/// or credential material. `Debug` never reveals them.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ProviderCallRecoveryLocator {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
    provider_material: Arc<[u8]>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProviderCallRecoveryLocatorWire {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
    provider_material: BoundedRecoveryLocatorBytes,
    identity: Digest,
}

struct BoundedRecoveryLocatorBytes(Arc<[u8]>);

impl<'de> Deserialize<'de> for BoundedRecoveryLocatorBytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = BoundedRecoveryLocatorBytes;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(
                    formatter,
                    "1..={MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES} opaque bytes"
                )
            }

            fn visit_bytes<E>(self, value: &[u8]) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_provider_locator_material(value).map_err(E::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(value)))
            }

            fn visit_byte_buf<E>(self, value: Vec<u8>) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                validate_provider_locator_material(&value).map_err(E::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(value)))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                if sequence
                    .size_hint()
                    .is_some_and(|length| length > MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES)
                {
                    return Err(serde::de::Error::custom(
                        ModelCallRecoveryError::InvalidProviderMaterial,
                    ));
                }
                let mut bytes = Vec::with_capacity(
                    sequence
                        .size_hint()
                        .unwrap_or(0)
                        .min(MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES),
                );
                while let Some(byte) = sequence.next_element::<u8>()? {
                    if bytes.len() == MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES {
                        return Err(serde::de::Error::custom(
                            ModelCallRecoveryError::InvalidProviderMaterial,
                        ));
                    }
                    bytes.push(byte);
                }
                validate_provider_locator_material(&bytes).map_err(serde::de::Error::custom)?;
                Ok(BoundedRecoveryLocatorBytes(Arc::from(bytes)))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

impl ProviderCallRecoveryLocator {
    /// Seals provider-generated recovery material available before dispatch.
    ///
    /// Providers must place the same material in their provider operation or
    /// otherwise prove it is directly resolvable after restart before calling
    /// this constructor. The bytes must be opaque correlation data,
    /// never credentials or raw request/response content.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized bytes or invalid prepared scope.
    pub fn seal(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        provider_material: impl AsRef<[u8]>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::from_material(
            projection,
            provider_revision,
            ProviderCallRecoveryLocatorStage::Prepared,
            provider_material.as_ref(),
        )
    }

    /// Seals provider-assigned material learned only after dispatch.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized bytes or invalid prepared scope.
    pub fn learned_after_dispatch(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        provider_material: impl AsRef<[u8]>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::from_material(
            projection,
            provider_revision,
            ProviderCallRecoveryLocatorStage::LearnedAfterDispatch,
            provider_material.as_ref(),
        )
    }

    fn from_material(
        projection: &PreparedModelProjection,
        provider_revision: NonZeroU32,
        availability_stage: ProviderCallRecoveryLocatorStage,
        provider_material: &[u8],
    ) -> Result<Self, ModelCallRecoveryError> {
        validate_provider_locator_material(provider_material)?;
        let mut value = Self {
            semantic_revision: PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION,
            provider_revision,
            availability_stage,
            operation_root: projection.operation_root,
            descriptor_binding: projection.descriptor_binding,
            dialect: projection.dialect.clone(),
            route: projection.route.clone(),
            effective_model: projection.effective_model.clone(),
            request: projection.request,
            plan: projection.plan,
            projection: projection.identity,
            provider_material: Arc::from(provider_material),
            identity: projection.identity,
        };
        value.identity = provider_recovery_locator_identity(&value)?;
        Ok(value)
    }

    fn validate_for_projection(
        &self,
        projection: &PreparedModelProjection,
    ) -> Result<(), ModelCallRecoveryError> {
        if self.semantic_revision != PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION
            || self.operation_root != projection.operation_root
            || self.descriptor_binding != projection.descriptor_binding
            || self.dialect != projection.dialect
            || self.route != projection.route
            || self.effective_model != projection.effective_model
            || self.request != projection.request
            || self.plan != projection.plan
            || self.projection != projection.identity
        {
            return Err(ModelCallRecoveryError::ForeignScope {
                field: "prepared projection",
            });
        }
        if provider_recovery_locator_identity(self)? != self.identity {
            return Err(ModelCallRecoveryError::IntegrityMismatch);
        }
        Ok(())
    }

    /// Returns the provider-owned locator format revision.
    #[must_use]
    pub const fn provider_revision(&self) -> NonZeroU32 {
        self.provider_revision
    }

    /// Returns when this locator became available.
    #[must_use]
    pub const fn availability_stage(&self) -> ProviderCallRecoveryLocatorStage {
        self.availability_stage
    }

    /// Returns the exact stable logical operation root.
    #[must_use]
    pub const fn operation_root(&self) -> ModelOperationRoot {
        self.operation_root
    }

    /// Returns the exact resolved-descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.descriptor_binding
    }

    /// Returns the exact provider dialect identity.
    #[must_use]
    pub const fn dialect(&self) -> &ProtocolId {
        &self.dialect
    }

    /// Returns the exact provider route identity.
    #[must_use]
    pub const fn route(&self) -> &RouteId {
        &self.route
    }

    /// Returns the exact effective model identity.
    #[must_use]
    pub const fn effective_model(&self) -> &ModelId {
        &self.effective_model
    }

    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.request
    }

    /// Returns the exact admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.plan
    }

    /// Returns the exact prepared projection identity.
    #[must_use]
    pub const fn projection(&self) -> Digest {
        self.projection
    }

    /// Returns the opaque provider lookup material.
    #[must_use]
    pub fn provider_material(&self) -> &[u8] {
        &self.provider_material
    }

    /// Returns the locator integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ProviderCallRecoveryLocator {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ProviderCallRecoveryLocatorWire::deserialize(deserializer)?;
        if wire.semantic_revision != PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let projection = PreparedRecoveryScope {
            operation_root: wire.operation_root,
            descriptor_binding: wire.descriptor_binding,
            dialect: wire.dialect,
            route: wire.route,
            effective_model: wire.effective_model,
            request: wire.request,
            plan: wire.plan,
            projection: wire.projection,
        };
        let value = provider_recovery_locator_from_scope(
            projection,
            wire.provider_revision,
            wire.availability_stage,
            wire.provider_material.0,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

impl fmt::Debug for ProviderCallRecoveryLocator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderCallRecoveryLocator")
            .field("semantic_revision", &self.semantic_revision)
            .field("provider_revision", &self.provider_revision)
            .field("availability_stage", &self.availability_stage)
            .field("operation_root", &self.operation_root)
            .field("descriptor_binding", &self.descriptor_binding)
            .field("dialect", &self.dialect)
            .field("route", &self.route)
            .field("effective_model", &self.effective_model)
            .field("request", &self.request)
            .field("plan", &self.plan)
            .field("projection", &self.projection)
            .field(
                "provider_material",
                &format_args!("<{} redacted bytes>", self.provider_material.len()),
            )
            .field("identity", &self.identity)
            .finish()
    }
}

#[derive(Clone)]
struct PreparedRecoveryScope {
    operation_root: ModelOperationRoot,
    descriptor_binding: Digest,
    dialect: ProtocolId,
    route: RouteId,
    effective_model: ModelId,
    request: Digest,
    plan: Digest,
    projection: Digest,
}

fn provider_recovery_locator_from_scope(
    scope: PreparedRecoveryScope,
    provider_revision: NonZeroU32,
    availability_stage: ProviderCallRecoveryLocatorStage,
    provider_material: Arc<[u8]>,
) -> Result<ProviderCallRecoveryLocator, ModelCallRecoveryError> {
    validate_provider_locator_material(&provider_material)?;
    let mut value = ProviderCallRecoveryLocator {
        semantic_revision: PROVIDER_RECOVERY_LOCATOR_SEMANTIC_REVISION,
        provider_revision,
        availability_stage,
        operation_root: scope.operation_root,
        descriptor_binding: scope.descriptor_binding,
        dialect: scope.dialect,
        route: scope.route,
        effective_model: scope.effective_model,
        request: scope.request,
        plan: scope.plan,
        projection: scope.projection,
        provider_material,
        identity: scope.projection,
    };
    value.identity = provider_recovery_locator_identity(&value)?;
    Ok(value)
}

fn provider_recovery_locator_identity(
    locator: &ProviderCallRecoveryLocator,
) -> Result<Digest, ModelCallRecoveryError> {
    for (digest, domain) in [
        (locator.descriptor_binding, DigestDomain::ModelBinding),
        (locator.request, DigestDomain::RequestPlan),
        (locator.plan, DigestDomain::RequestPlan),
        (locator.projection, DigestDomain::ContextBudget),
    ] {
        validate_prepared_digest(digest, domain)
            .map_err(|_| ModelCallRecoveryError::InvalidDigest)?;
    }
    let provider_revision = locator.provider_revision.get().to_be_bytes();
    let stage = match locator.availability_stage {
        ProviderCallRecoveryLocatorStage::Prepared => b"prepared".as_slice(),
        ProviderCallRecoveryLocatorStage::LearnedAfterDispatch => {
            b"learned-after-dispatch".as_slice()
        }
    };
    let root = serde_json::to_vec(&locator.operation_root)
        .map_err(|_| ModelCallRecoveryError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[
            b"plakous-model/provider-call-recovery-locator-v2",
            &locator.semantic_revision.to_be_bytes(),
            &provider_revision,
            stage,
            &root,
            &locator.descriptor_binding.identity_bytes(),
            locator.dialect.as_str().as_bytes(),
            locator.route.as_str().as_bytes(),
            locator.effective_model.as_str().as_bytes(),
            &locator.request.identity_bytes(),
            &locator.plan.identity_bytes(),
            &locator.projection.identity_bytes(),
            &locator.provider_material,
        ],
    )?)
}

fn validate_provider_locator_material(bytes: &[u8]) -> Result<(), ModelCallRecoveryError> {
    if bytes.is_empty() || bytes.len() > MAX_PROVIDER_CALL_RECOVERY_LOCATOR_BYTES {
        Err(ModelCallRecoveryError::InvalidProviderMaterial)
    } else {
        Ok(())
    }
}

/// Bounded serializable recovery record for one exact prepared call.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PreparedCallRecord {
    semantic_revision: u32,
    pub(super) projection: PreparedModelProjection,
    authority_evidence: RuntimeModelAuthority,
    context_admission: Digest,
    context_status: Digest,
    recovery_locator: Option<ProviderCallRecoveryLocator>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedCallRecordWire {
    semantic_revision: u32,
    projection: PreparedModelProjection,
    authority_evidence: RuntimeModelAuthority,
    context_admission: Digest,
    context_status: Digest,
    recovery_locator: Option<ProviderCallRecoveryLocator>,
    identity: Digest,
}

impl PreparedCallRecord {
    /// Seals a bounded inert record around an inspected prepared projection.
    ///
    /// # Errors
    ///
    /// Rejects a record whose canonical JSON exceeds the recovery bound.
    pub fn for_prepared(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        projection: PreparedModelProjection,
        context: &ModelPreparationContext,
    ) -> Result<Self, ModelPreparationError> {
        Self::for_prepared_with_recovery_locator(descriptor, plan, projection, context, None)
    }

    /// Seals a prepared record with an optional certified provider locator.
    ///
    /// A present locator is part of the record codec and identity before the
    /// one-shot start capability can perform provider-visible work. A provider
    /// may later supply a separately persisted `LearnedAfterDispatch` locator
    /// bound to this same root and projection.
    ///
    /// # Errors
    ///
    /// Rejects descriptor, projection, authority, admission, or locator drift.
    pub fn for_prepared_with_recovery_locator(
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        projection: PreparedModelProjection,
        context: &ModelPreparationContext,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, ModelPreparationError> {
        projection.validate_for_plan(descriptor, plan)?;
        context
            .authority
            .validate(descriptor, context.prepared_at)?;
        if context.authority.execution_authority_binding()? != projection.authority
            || context.authority.authority_class_binding()? != projection.authority_class
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            }
            .into());
        }
        let status = descriptor
            .context_status(plan, &projection, context.prepared_at)
            .map_err(ContextAdmissionError::from)?;
        let admission = match status.admit_with_requirement(
            plan.request(),
            plan.request().context_admission_requirement(),
        ) {
            Ok(admission) => admission,
            Err(error) => {
                let parts = projection.token_projection().input_parts();
                return Err(ModelPreparationError::from_context_admission(
                    status.with_input_parts(parts),
                    error,
                ));
            }
        };
        Self::with_admission(
            projection,
            context.authority.clone(),
            &admission,
            recovery_locator,
        )
        .map_err(Into::into)
    }

    fn with_admission(
        projection: PreparedModelProjection,
        authority_evidence: RuntimeModelAuthority,
        admission: &ContextAdmission,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, PreparedCallError> {
        if admission.request().identity() != projection.request
            || admission.status().model_input_identity() != projection.model_input
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission",
            });
        }
        if admission.status().scope().is_none_or(|scope| {
            scope.prepared_token_projection() != projection.token_projection.identity()
                || scope.lowering_revision() != projection.lowering_revision
        }) {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission projection",
            });
        }
        Self::seal(
            projection,
            authority_evidence,
            admission.identity(),
            admission.status().identity(),
            recovery_locator,
        )
    }

    fn seal(
        projection: PreparedModelProjection,
        authority_evidence: RuntimeModelAuthority,
        context_admission: Digest,
        context_status: Digest,
        recovery_locator: Option<ProviderCallRecoveryLocator>,
    ) -> Result<Self, PreparedCallError> {
        for digest in [context_admission, context_status] {
            validate_prepared_digest(digest, DigestDomain::ContextAdmission)?;
        }
        if authority_evidence.execution_authority_binding()? != projection.authority
            || authority_evidence.authority_class_binding()? != projection.authority_class
        {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            });
        }
        if let Some(locator) = &recovery_locator {
            locator.validate_for_projection(&projection)?;
        }
        let retained = (
            PREPARED_CALL_RECORD_SEMANTIC_REVISION,
            &projection,
            &authority_evidence,
            context_admission,
            context_status,
            &recovery_locator,
        );
        let retained_bytes =
            serde_json::to_vec(&retained).map_err(|_| PreparedCallError::Serialization)?;
        if retained_bytes.len() > MAX_PREPARED_CALL_RECORD_BYTES {
            return Err(PreparedCallError::RecordTooLarge);
        }
        let identity = Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/prepared-call-record-v5",
                &projection.identity.identity_bytes(),
                &retained_bytes,
            ],
        )?;
        Ok(Self {
            semantic_revision: PREPARED_CALL_RECORD_SEMANTIC_REVISION,
            projection,
            authority_evidence,
            context_admission,
            context_status,
            recovery_locator,
            identity,
        })
    }

    /// Returns the exact prepared-call semantic revision.
    #[must_use]
    pub const fn semantic_revision(&self) -> u32 {
        self.semantic_revision
    }

    /// Returns the complete non-secret prepared projection.
    #[must_use]
    pub const fn projection(&self) -> &PreparedModelProjection {
        &self.projection
    }

    /// Returns the exact runtime authority sealed at preparation.
    #[must_use]
    pub const fn authority_evidence(&self) -> &RuntimeModelAuthority {
        &self.authority_evidence
    }

    /// Returns the admitted whole-context identity.
    #[must_use]
    pub const fn context_admission_identity(&self) -> Digest {
        self.context_admission
    }

    /// Returns the admitted status identity.
    ///
    /// The digest proves which complete status was admitted; it cannot
    /// reconstruct that status.
    #[must_use]
    pub const fn context_status_identity(&self) -> Digest {
        self.context_status
    }

    /// Returns the sealed provider locator, when this call earned recovery.
    #[must_use]
    pub const fn recovery_locator(&self) -> Option<&ProviderCallRecoveryLocator> {
        self.recovery_locator.as_ref()
    }

    /// Derives the only valid read-only recovery request for this record.
    ///
    /// # Errors
    ///
    /// Returns [`ModelCallRecoveryError::MissingLocator`] when preparation did
    /// not certify provider recovery for this exact operation.
    pub fn recovery_request(&self) -> Result<ModelCallRecoveryRequest, ModelCallRecoveryError> {
        ModelCallRecoveryRequest::for_prepared(self)
    }

    /// Derives recovery using the latest separately persisted locator.
    ///
    /// This is the path for a provider locator learned after dispatch. The
    /// supplied exact locator supersedes the optional prepared locator only
    /// after it has independently crossed the runtime's checkpoint fence.
    ///
    /// # Errors
    ///
    /// Rejects root, projection, or locator-integrity drift.
    pub fn recovery_request_with_locator(
        &self,
        locator: ProviderCallRecoveryLocator,
    ) -> Result<ModelCallRecoveryRequest, ModelCallRecoveryError> {
        ModelCallRecoveryRequest::for_locator(self, locator)
    }

    /// Rechecks all neutral prepared and admission evidence at the final
    /// pre-dispatch fence without obtaining a provider call capability.
    ///
    /// # Errors
    ///
    /// Fails on descriptor or plan drift, stale stored authority evidence, or a
    /// recomputed context admission/status identity different from the record.
    pub fn revalidate(
        &self,
        descriptor: &ResolvedModelDescriptor,
        plan: &ValidatedRequestPlan,
        now: Timestamp,
    ) -> Result<(), ModelPreparationError> {
        self.projection.validate_for_plan(descriptor, plan)?;
        self.revalidate_selected_authority(&self.authority_evidence, descriptor, now)?;
        let status = descriptor
            .context_status(plan, &self.projection, now)
            .map_err(ContextAdmissionError::from)?;
        let admission = match status.admit_with_requirement(
            plan.request(),
            plan.request().context_admission_requirement(),
        ) {
            Ok(admission) => admission,
            Err(error) => {
                let parts = self.projection.token_projection().input_parts();
                return Err(ModelPreparationError::from_context_admission(
                    status.with_input_parts(parts),
                    error,
                ));
            }
        };
        let current = (admission.identity(), admission.status().identity());
        if current != (self.context_admission, self.context_status) {
            return Err(PreparedCallError::ScopeDrift {
                field: "context admission revalidation",
            }
            .into());
        }
        Ok(())
    }

    /// Checks a newly observed provider authority against this exact prepared
    /// execution binding while treating observation time only as freshness.
    ///
    /// # Errors
    ///
    /// Fails when evidence is stale/foreign or account, credential authority,
    /// or generation differs from the prepared call.
    pub fn revalidate_selected_authority(
        &self,
        authority: &RuntimeModelAuthority,
        descriptor: &ResolvedModelDescriptor,
        now: Timestamp,
    ) -> Result<(), ModelPreparationError> {
        authority.validate(descriptor, now)?;
        if authority.execution_authority_binding()? != self.projection.authority
            || authority.authority_class_binding()? != self.projection.authority_class
        {
            return Err(RuntimeModelAuthorityError::ForeignAuthority.into());
        }
        Ok(())
    }

    /// Rebuilds the exact plan this record prepared for `request` from another
    /// observation of the same semantic descriptor.
    ///
    /// A plan digest binds the freshness-sensitive descriptor binding, so a
    /// descriptor observed afresh (a reopen or a re-resolution) cannot
    /// recompute it. Capability decisions are recomputed from `descriptor`,
    /// whose dialect, route, requested and effective model and adapter must be
    /// the recorded ones; the plan keeps the descriptor binding this record and
    /// its authority evidence were prepared under, and must reproduce the
    /// recorded plan, request, model input, root and attempt. The caller proves
    /// the semantic descriptor binding through the effective provider
    /// configuration identity. The restored plan only commits or continues this
    /// record's response: [`ResolvedModelDescriptor::validate_plan_binding`]
    /// refuses it for any later dispatch under another observation.
    ///
    /// # Errors
    ///
    /// Fails when the authority evidence and projection disagree on the
    /// descriptor binding, the request does not validate under `mode`, or any
    /// projection axis differs.
    pub fn restore_plan(
        &self,
        descriptor: &ResolvedModelDescriptor,
        request: ModelRequest,
        mode: ValidationMode,
    ) -> Result<ValidatedRequestPlan, PreparedCallError> {
        let binding = self.projection.descriptor_binding;
        if self.authority_evidence.descriptor_binding() != binding {
            return Err(PreparedCallError::ScopeDrift {
                field: "runtime authority",
            });
        }
        let plan = descriptor
            .validate_request_for_binding(request, mode, binding)
            .map_err(|_| PreparedCallError::ScopeDrift { field: "plan" })?;
        self.projection.validate_plan_axes(descriptor, &plan)?;
        Ok(plan)
    }

    /// Returns the record integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for PreparedCallRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PreparedCallRecordWire::deserialize(deserializer)?;
        if wire.semantic_revision != PREPARED_CALL_RECORD_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self::seal(
            wire.projection,
            wire.authority_evidence,
            wire.context_admission,
            wire.context_status,
            wire.recovery_locator,
        )
        .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                PreparedCallError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

/// Exact read-only request to reconcile one previously prepared provider call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCallRecoveryRequest {
    semantic_revision: u32,
    prepared_call: Digest,
    locator: ProviderCallRecoveryLocator,
    identity: Digest,
}

impl ModelCallRecoveryRequest {
    fn for_prepared(record: &PreparedCallRecord) -> Result<Self, ModelCallRecoveryError> {
        let locator = record
            .recovery_locator
            .clone()
            .ok_or(ModelCallRecoveryError::MissingLocator)?;
        Self::for_locator(record, locator)
    }

    fn for_locator(
        record: &PreparedCallRecord,
        locator: ProviderCallRecoveryLocator,
    ) -> Result<Self, ModelCallRecoveryError> {
        locator.validate_for_projection(&record.projection)?;
        let mut value = Self {
            semantic_revision: MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION,
            prepared_call: record.identity,
            locator,
            identity: record.identity,
        };
        value.identity = model_call_recovery_request_identity(&value)?;
        Ok(value)
    }

    /// Rechecks this request against the exact retained prepared record.
    ///
    /// # Errors
    ///
    /// Rejects any prepared-call, locator, or scope drift.
    pub fn validate_for_record(
        &self,
        record: &PreparedCallRecord,
    ) -> Result<(), ModelCallRecoveryError> {
        let expected = Self::for_prepared(record)?;
        if *self == expected {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::ForeignScope {
                field: "prepared call",
            })
        }
    }

    /// Rechecks the exact resolved model descriptor without acquiring authority.
    ///
    /// # Errors
    ///
    /// Rejects a request for another descriptor. This operation is read-only
    /// and is not dispatch revalidation.
    pub fn validate_for_descriptor(
        &self,
        descriptor: &ResolvedModelDescriptor,
    ) -> Result<(), ModelCallRecoveryError> {
        let binding =
            descriptor
                .binding_digest()
                .map_err(|_| ModelCallRecoveryError::ForeignScope {
                    field: "descriptor",
                })?;
        if binding == self.locator.descriptor_binding {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::ForeignScope {
                field: "descriptor",
            })
        }
    }

    /// Returns the exact prepared-call/provider-operation identity.
    #[must_use]
    pub const fn prepared_call(&self) -> Digest {
        self.prepared_call
    }

    /// Returns the sealed provider-owned locator.
    #[must_use]
    pub const fn locator(&self) -> &ProviderCallRecoveryLocator {
        &self.locator
    }

    /// Returns the exact descriptor binding.
    #[must_use]
    pub const fn descriptor_binding(&self) -> Digest {
        self.locator.descriptor_binding
    }

    /// Returns the exact canonical request identity.
    #[must_use]
    pub const fn request(&self) -> Digest {
        self.locator.request
    }

    /// Returns the exact admitted plan identity.
    #[must_use]
    pub const fn plan(&self) -> Digest {
        self.locator.plan
    }

    /// Returns the exact prepared projection identity.
    #[must_use]
    pub const fn projection(&self) -> Digest {
        self.locator.projection
    }

    /// Returns this complete recovery-request identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

fn model_call_recovery_request_identity(
    request: &ModelCallRecoveryRequest,
) -> Result<Digest, ModelCallRecoveryError> {
    if request.semantic_revision != MODEL_CALL_RECOVERY_REQUEST_SEMANTIC_REVISION
        || request.prepared_call.domain() != DigestDomain::ProviderOperation
        || request.prepared_call.version() != PROVIDER_OPERATION_SEMANTIC_REVISION
    {
        return Err(ModelCallRecoveryError::InvalidDigest);
    }
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[
            b"plakous-model/model-call-recovery-request-v2",
            &request.semantic_revision.to_be_bytes(),
            &request.prepared_call.identity_bytes(),
            &request.locator.identity.identity_bytes(),
        ],
    )?)
}

/// Bounded provider-owned evidence for one read-only recovery classification.
///
/// This receipt is supplemental only and grants no classification by itself;
/// the enclosing sealed [`ModelCallRecoveryResult`] binds it to the exact
/// outcome, response or error, usage, and recovery request.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct ModelCallRecoveryEvidence {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    recovery_request: Digest,
    receipt: [u8; 32],
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCallRecoveryEvidenceWire {
    semantic_revision: u32,
    provider_revision: NonZeroU32,
    recovery_request: Digest,
    receipt: [u8; 32],
    identity: Digest,
}

impl ModelCallRecoveryEvidence {
    /// Seals one-way provider evidence to an exact recovery request.
    ///
    /// # Errors
    ///
    /// Rejects the all-zero provider-material sentinel.
    pub fn seal(
        request: &ModelCallRecoveryRequest,
        provider_material: [u8; 32],
    ) -> Result<Self, ModelCallRecoveryError> {
        if provider_material.iter().all(|byte| *byte == 0) {
            return Err(ModelCallRecoveryError::InvalidProviderMaterial);
        }
        let provider_revision = request.locator.provider_revision;
        let provider_revision_bytes = provider_revision.get().to_be_bytes();
        let receipt = *Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/model-call-recovery-evidence-receipt-v1",
                &request.identity.identity_bytes(),
                &provider_revision_bytes,
                &provider_material,
            ],
        )?
        .bytes();
        Self::from_receipt(request.identity, provider_revision, receipt)
    }

    fn from_receipt(
        recovery_request: Digest,
        provider_revision: NonZeroU32,
        receipt: [u8; 32],
    ) -> Result<Self, ModelCallRecoveryError> {
        if receipt.iter().all(|byte| *byte == 0) {
            return Err(ModelCallRecoveryError::InvalidProviderMaterial);
        }
        let provider_revision_bytes = provider_revision.get().to_be_bytes();
        let identity = Digest::sha256(
            DigestDomain::ProviderOperation,
            PROVIDER_OPERATION_SEMANTIC_REVISION,
            &[
                b"plakous-model/model-call-recovery-evidence-v1",
                &MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION.to_be_bytes(),
                &recovery_request.identity_bytes(),
                &provider_revision_bytes,
                &receipt,
            ],
        )?;
        Ok(Self {
            semantic_revision: MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION,
            provider_revision,
            recovery_request,
            receipt,
            identity,
        })
    }

    /// Returns the provider evidence format revision.
    #[must_use]
    pub const fn provider_revision(&self) -> NonZeroU32 {
        self.provider_revision
    }

    /// Returns the evidence integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ModelCallRecoveryEvidence {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelCallRecoveryEvidenceWire::deserialize(deserializer)?;
        if wire.semantic_revision != MODEL_CALL_RECOVERY_EVIDENCE_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self::from_receipt(wire.recovery_request, wire.provider_revision, wire.receipt)
            .map_err(serde::de::Error::custom)?;
        if value.identity != wire.identity {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

impl fmt::Debug for ModelCallRecoveryEvidence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModelCallRecoveryEvidence")
            .field("semantic_revision", &self.semantic_revision)
            .field("provider_revision", &self.provider_revision)
            .field("recovery_request", &self.recovery_request)
            .field("receipt", &"<redacted provider recovery evidence>")
            .field("identity", &self.identity)
            .finish()
    }
}

/// Closed provider-neutral meaning of one read-only recovery classification.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[non_exhaustive]
pub enum ModelCallRecoveryOutcome {
    /// Provider evidence proves physical dispatch never occurred.
    ProvenNotDispatched,
    /// The exact provider operation completed with this authoritative response.
    Completed(Box<ModelResponse>),
    /// The exact provider operation terminally failed with a sanitized error.
    DefinitelyFailed(Box<ModelError>),
    /// Available evidence cannot prove non-dispatch or a terminal outcome.
    Ambiguous,
    /// The model/provider implementation has no recovery facet.
    Unsupported,
    /// The facet exists but cannot currently resolve this exact locator.
    Unavailable,
}

/// Sealed result of one read-only provider recovery request.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ModelCallRecoveryResult {
    semantic_revision: u32,
    recovery_request: Digest,
    plan: Digest,
    outcome: ModelCallRecoveryOutcome,
    usage: Option<Usage>,
    evidence: Option<ModelCallRecoveryEvidence>,
    identity: Digest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCallRecoveryResultWire {
    semantic_revision: u32,
    recovery_request: Digest,
    plan: Digest,
    outcome: ModelCallRecoveryOutcome,
    usage: Option<Usage>,
    evidence: Option<ModelCallRecoveryEvidence>,
    identity: Digest,
}

impl ModelCallRecoveryResult {
    /// Classifies an exact operation as proven not dispatched.
    ///
    /// # Errors
    ///
    /// Rejects foreign evidence or identity construction failure.
    pub fn proven_not_dispatched(
        request: &ModelCallRecoveryRequest,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::ProvenNotDispatched,
            None,
            evidence,
        )
    }

    /// Returns the exact recovered terminal response and any supplemental usage.
    ///
    /// # Errors
    ///
    /// Rejects an invalid/foreign response, contradictory usage, foreign
    /// evidence, or identity construction failure.
    pub fn completed(
        request: &ModelCallRecoveryRequest,
        response: ModelResponse,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::Completed(Box::new(response)),
            usage,
            evidence,
        )
    }

    /// Returns a sanitized definite terminal failure for the exact operation.
    ///
    /// # Errors
    ///
    /// Rejects an ambiguous failure, foreign evidence, or identity
    /// construction failure.
    pub fn definitely_failed(
        request: &ModelCallRecoveryRequest,
        error: ModelError,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::DefinitelyFailed(Box::new(error)),
            usage,
            evidence,
        )
    }

    /// Classifies the exact operation as unresolved without authorizing replay.
    ///
    /// # Errors
    ///
    /// Rejects foreign evidence or identity construction failure.
    pub fn ambiguous(
        request: &ModelCallRecoveryRequest,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(
            request,
            ModelCallRecoveryOutcome::Ambiguous,
            usage,
            evidence,
        )
    }

    /// Returns the default fail-closed unsupported result.
    ///
    /// # Errors
    ///
    /// Returns an error only if sealed result identity construction fails.
    pub fn unsupported(request: &ModelCallRecoveryRequest) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(request, ModelCallRecoveryOutcome::Unsupported, None, None)
    }

    /// Reports that an implemented facet cannot resolve this exact locator.
    ///
    /// # Errors
    ///
    /// Returns an error only if sealed result identity construction fails.
    pub fn unavailable(request: &ModelCallRecoveryRequest) -> Result<Self, ModelCallRecoveryError> {
        Self::seal(request, ModelCallRecoveryOutcome::Unavailable, None, None)
    }

    fn seal(
        request: &ModelCallRecoveryRequest,
        outcome: ModelCallRecoveryOutcome,
        usage: Option<Usage>,
        evidence: Option<ModelCallRecoveryEvidence>,
    ) -> Result<Self, ModelCallRecoveryError> {
        if evidence.as_ref().is_some_and(|evidence| {
            evidence.recovery_request != request.identity
                || evidence.provider_revision != request.locator.provider_revision
        }) {
            return Err(ModelCallRecoveryError::ForeignScope { field: "evidence" });
        }
        match &outcome {
            ModelCallRecoveryOutcome::Completed(response) => {
                response
                    .validate_for_commit()
                    .map_err(|_| ModelCallRecoveryError::InvalidCompletedResponse)?;
                if response.plan_digest() != request.plan() {
                    return Err(ModelCallRecoveryError::ForeignScope {
                        field: "completed response",
                    });
                }
                if response.usage().is_some() && usage.is_some() && response.usage() != usage {
                    return Err(ModelCallRecoveryError::ConflictingUsage);
                }
            }
            ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
                if error.kind() == ModelErrorKind::OutcomeUnknown
                    || error.paid_recovery().is_some()
                    || error.context().is_some_and(|context| {
                        context.retryability() != ModelRetryability::Permanent
                    })
                {
                    return Err(ModelCallRecoveryError::AmbiguousDefiniteFailure);
                }
                if error.context().is_some_and(|context| {
                    context.protocol() != request.locator.dialect()
                        || context.route() != request.locator.route()
                        || context.model() != request.locator.effective_model()
                }) {
                    return Err(ModelCallRecoveryError::ForeignScope {
                        field: "definite failure target",
                    });
                }
            }
            ModelCallRecoveryOutcome::ProvenNotDispatched if usage.is_some() => {
                return Err(ModelCallRecoveryError::ConflictingUsage);
            }
            ModelCallRecoveryOutcome::Unsupported | ModelCallRecoveryOutcome::Unavailable
                if usage.is_some() || evidence.is_some() =>
            {
                return Err(ModelCallRecoveryError::ConflictingEvidence);
            }
            ModelCallRecoveryOutcome::ProvenNotDispatched
            | ModelCallRecoveryOutcome::Ambiguous
            | ModelCallRecoveryOutcome::Unsupported
            | ModelCallRecoveryOutcome::Unavailable => {}
        }
        let mut value = Self {
            semantic_revision: MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION,
            recovery_request: request.identity,
            plan: request.plan(),
            outcome,
            usage,
            evidence,
            identity: request.identity,
        };
        value.identity = model_call_recovery_result_identity(&value)?;
        Ok(value)
    }

    /// Rechecks result scope and integrity against the originating request.
    ///
    /// # Errors
    ///
    /// Rejects any request, plan, response, evidence, usage, or identity drift.
    pub fn validate_for_request(
        &self,
        request: &ModelCallRecoveryRequest,
    ) -> Result<(), ModelCallRecoveryError> {
        let expected = Self::seal(
            request,
            self.outcome.clone(),
            self.usage,
            self.evidence.clone(),
        )?;
        if *self == expected {
            Ok(())
        } else {
            Err(ModelCallRecoveryError::IntegrityMismatch)
        }
    }

    /// Returns the closed neutral recovery meaning.
    #[must_use]
    pub const fn outcome(&self) -> &ModelCallRecoveryOutcome {
        &self.outcome
    }

    /// Returns supplemental cumulative usage when provider evidence has it.
    #[must_use]
    pub const fn usage(&self) -> Option<Usage> {
        self.usage
    }

    /// Returns bounded provider-owned evidence when available.
    #[must_use]
    pub const fn evidence(&self) -> Option<&ModelCallRecoveryEvidence> {
        self.evidence.as_ref()
    }

    /// Returns the recovery-result integrity identity.
    #[must_use]
    pub const fn identity(&self) -> Digest {
        self.identity
    }
}

impl<'de> Deserialize<'de> for ModelCallRecoveryResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ModelCallRecoveryResultWire::deserialize(deserializer)?;
        if wire.semantic_revision != MODEL_CALL_RECOVERY_RESULT_SEMANTIC_REVISION {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::UnsupportedRevision,
            ));
        }
        let value = Self {
            semantic_revision: wire.semantic_revision,
            recovery_request: wire.recovery_request,
            plan: wire.plan,
            outcome: wire.outcome,
            usage: wire.usage,
            evidence: wire.evidence,
            identity: wire.identity,
        };
        validate_recovery_result_without_request(&value).map_err(serde::de::Error::custom)?;
        if model_call_recovery_result_identity(&value).map_err(serde::de::Error::custom)?
            != value.identity
        {
            return Err(serde::de::Error::custom(
                ModelCallRecoveryError::IntegrityMismatch,
            ));
        }
        Ok(value)
    }
}

fn validate_recovery_result_without_request(
    result: &ModelCallRecoveryResult,
) -> Result<(), ModelCallRecoveryError> {
    if result.recovery_request.domain() != DigestDomain::ProviderOperation
        || result.recovery_request.version() != PROVIDER_OPERATION_SEMANTIC_REVISION
        || result.plan.domain() != DigestDomain::RequestPlan
        || result
            .evidence
            .as_ref()
            .is_some_and(|evidence| evidence.recovery_request != result.recovery_request)
    {
        return Err(ModelCallRecoveryError::InvalidDigest);
    }
    match &result.outcome {
        ModelCallRecoveryOutcome::Completed(response) => {
            response
                .validate_for_commit()
                .map_err(|_| ModelCallRecoveryError::InvalidCompletedResponse)?;
            if response.plan_digest() != result.plan
                || (response.usage().is_some()
                    && result.usage.is_some()
                    && response.usage() != result.usage)
            {
                return Err(ModelCallRecoveryError::ConflictingUsage);
            }
        }
        ModelCallRecoveryOutcome::DefinitelyFailed(error) => {
            if error.kind() == ModelErrorKind::OutcomeUnknown
                || error.paid_recovery().is_some()
                || error
                    .context()
                    .is_some_and(|context| context.retryability() != ModelRetryability::Permanent)
            {
                return Err(ModelCallRecoveryError::AmbiguousDefiniteFailure);
            }
        }
        ModelCallRecoveryOutcome::ProvenNotDispatched if result.usage.is_some() => {
            return Err(ModelCallRecoveryError::ConflictingUsage);
        }
        ModelCallRecoveryOutcome::Unsupported | ModelCallRecoveryOutcome::Unavailable
            if result.usage.is_some() || result.evidence.is_some() =>
        {
            return Err(ModelCallRecoveryError::ConflictingEvidence);
        }
        ModelCallRecoveryOutcome::ProvenNotDispatched
        | ModelCallRecoveryOutcome::Ambiguous
        | ModelCallRecoveryOutcome::Unsupported
        | ModelCallRecoveryOutcome::Unavailable => {}
    }
    Ok(())
}

fn model_call_recovery_result_identity(
    result: &ModelCallRecoveryResult,
) -> Result<Digest, ModelCallRecoveryError> {
    let retained = (
        result.semantic_revision,
        result.recovery_request,
        result.plan,
        &result.outcome,
        result.usage,
        &result.evidence,
    );
    let bytes = serde_json::to_vec(&retained).map_err(|_| ModelCallRecoveryError::Serialization)?;
    Ok(Digest::sha256(
        DigestDomain::ProviderOperation,
        PROVIDER_OPERATION_SEMANTIC_REVISION,
        &[b"plakous-model/model-call-recovery-result-v1", &bytes],
    )?)
}

/// Invalid exact provider-call recovery data or classification.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ModelCallRecoveryError {
    /// Preparation did not seal a provider recovery locator.
    #[error("prepared call has no certified provider recovery locator")]
    MissingLocator,
    /// Provider material was empty or exceeded its exact byte bound.
    #[error("provider recovery material is empty or exceeds its byte bound")]
    InvalidProviderMaterial,
    /// A retained recovery value uses an unsupported semantic revision.
    #[error("provider call recovery revision is unsupported")]
    UnsupportedRevision,
    /// A recovery value belongs to another exact operation scope.
    #[error("provider call recovery scope drifted at {field}")]
    ForeignScope {
        /// Stable non-secret mismatched scope axis.
        field: &'static str,
    },
    /// A digest used the wrong domain or semantic revision.
    #[error("provider call recovery contains an invalid digest")]
    InvalidDigest,
    /// A recovered completed response is not commit-valid.
    #[error("provider call recovery returned an invalid completed response")]
    InvalidCompletedResponse,
    /// Supplemental and response usage disagree.
    #[error("provider call recovery usage evidence conflicts")]
    ConflictingUsage,
    /// An unsupported/unavailable result carried contradictory evidence.
    #[error("provider call recovery evidence conflicts with its classification")]
    ConflictingEvidence,
    /// A definite failure carried an outcome-unknown error.
    #[error("provider call recovery definite failure is ambiguous")]
    AmbiguousDefiniteFailure,
    /// Canonical identity serialization failed.
    #[error("provider call recovery identity serialization failed")]
    Serialization,
    /// Serialized fields disagreed with their sealed integrity identity.
    #[error("provider call recovery integrity mismatch")]
    IntegrityMismatch,
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] crate::DigestError),
}
