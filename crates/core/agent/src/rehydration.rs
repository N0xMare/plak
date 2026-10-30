//! Bounded canonical persistence for the harness-plan component of rehydration.
//!
//! This is deliberately not the full session rehydration manifest promised by
//! It preserves only immutable profile-compiler facts and carries no
//! session topology, conversation state, providers, credentials, callbacks,
//! executors, or trait objects.

use std::fmt;

use plakous_model::{Digest, DigestDomain};
use thiserror::Error;

use crate::profile::{
    HARNESS_COMPILER_INPUT_ENCODING_VERSION, HARNESS_PROFILE_COMPILER_REVISION,
    HARNESS_PROFILE_ENCODING_VERSION, HARNESS_PROFILE_SCHEMA_VERSION,
    HARNESS_STRATEGY_CATALOG_REVISION, HarnessCompilerInputs, HarnessProfileCompiler,
    HarnessProfileSpec, ProfileError, RESOLVED_HARNESS_PLAN_ENCODING_VERSION, ResolvedHarnessPlan,
    canonical_compiler_input_bytes, canonical_plan_bytes, canonical_profile_bytes,
    decode_canonical_compiler_inputs, decode_canonical_profile,
};

/// Application/runtime-policy interpretation bound into the harness-plan
/// component. Frozen; a change bumps the runtime format epoch instead.
pub const RUNTIME_POLICY_REVISION: u32 = 5;
/// Canonical binary envelope format emitted by this codec.
pub const HARNESS_PLAN_COMPONENT_FORMAT_VERSION: u32 = 1;
/// Hard limit for one encoded envelope.
pub(crate) const MAX_HARNESS_PLAN_COMPONENT_BYTES: usize = 2 * 1024 * 1024;

const MAGIC: &[u8; 8] = b"PLAKHPC\0";
const ENVELOPE_KIND: u8 = 1;
const DIGEST_ALGORITHM_SHA256: u8 = 1;
const MANIFEST_DIGEST_VERSION: u32 = 1;
const MAX_PROFILE_BYTES: usize = 256 * 1024;
const MAX_INPUT_BYTES: usize = 512 * 1024;
const MAX_PLAN_BYTES: usize = 1024 * 1024;

/// Reconstructible neutral facts for one harness plan, not a session snapshot.
#[derive(Clone, Eq, PartialEq)]
pub struct HarnessPlanComponentManifest {
    profile_schema_version: u32,
    compiler_revision: u32,
    strategy_catalog_revision: u32,
    runtime_policy_version: u32,
    profile_encoding_version: u32,
    compiler_input_encoding_version: u32,
    resolved_plan_encoding_version: u32,
    profile_bytes: Vec<u8>,
    compiler_input_bytes: Vec<u8>,
    resolved_plan_bytes: Vec<u8>,
    profile_digest: Digest,
    resolved_plan_digest: Digest,
}

impl fmt::Debug for HarnessPlanComponentManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HarnessPlanComponentManifest")
            .field("profile_schema_version", &self.profile_schema_version)
            .field("compiler_revision", &self.compiler_revision)
            .field("strategy_catalog_revision", &self.strategy_catalog_revision)
            .field("runtime_policy_version", &self.runtime_policy_version)
            .field("profile_encoding_version", &self.profile_encoding_version)
            .field(
                "compiler_input_encoding_version",
                &self.compiler_input_encoding_version,
            )
            .field(
                "resolved_plan_encoding_version",
                &self.resolved_plan_encoding_version,
            )
            .field(
                "profile_bytes",
                &format_args!("<{} bytes>", self.profile_bytes.len()),
            )
            .field(
                "compiler_input_bytes",
                &format_args!("<{} bytes>", self.compiler_input_bytes.len()),
            )
            .field(
                "resolved_plan_bytes",
                &format_args!("<{} bytes>", self.resolved_plan_bytes.len()),
            )
            .field("profile_digest", &self.profile_digest)
            .field("resolved_plan_digest", &self.resolved_plan_digest)
            .finish()
    }
}

impl HarnessPlanComponentManifest {
    /// Returns the application/runtime-policy version bound by the caller.
    #[must_use]
    pub const fn runtime_policy_version(&self) -> u32 {
        self.runtime_policy_version
    }

    /// Returns the recorded plan identity without treating it as reconstruction data.
    #[must_use]
    pub const fn resolved_plan_digest(&self) -> &Digest {
        &self.resolved_plan_digest
    }
}

/// Versioned, integrity-bound canonical harness-plan component envelope.
#[derive(Clone, Eq, PartialEq)]
pub struct HarnessPlanComponentEnvelope {
    manifest: HarnessPlanComponentManifest,
}

impl fmt::Debug for HarnessPlanComponentEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HarnessPlanComponentEnvelope")
            .field("manifest", &self.manifest)
            .finish()
    }
}

impl HarnessPlanComponentEnvelope {
    /// Captures only validated, reconstructible compiler-owned data.
    ///
    /// # Errors
    ///
    /// Fails when the supplied plan is not exactly reproducible from the
    /// supplied profile and inputs, or any canonical payload exceeds a bound.
    pub fn capture(
        profile: &HarnessProfileSpec,
        inputs: &HarnessCompilerInputs,
        plan: &ResolvedHarnessPlan,
        runtime_policy_version: u32,
    ) -> Result<Self, HarnessManifestCodecError> {
        if runtime_policy_version == 0 {
            return Err(HarnessManifestCodecError::ZeroRuntimePolicyVersion);
        }
        let profile_bytes = canonical_profile_bytes(profile)?;
        let compiler_input_bytes = canonical_compiler_input_bytes(inputs)?;
        let resolved_plan_bytes = canonical_plan_bytes(plan)?;
        check_component_bounds(&profile_bytes, &compiler_input_bytes, &resolved_plan_bytes)?;

        let reproduced = HarnessProfileCompiler::new().compile(profile, inputs)?;
        if reproduced.plan_digest() != plan.plan_digest()
            || canonical_plan_bytes(&reproduced)? != resolved_plan_bytes
        {
            return Err(HarnessManifestCodecError::InconsistentPlan);
        }
        Ok(Self {
            manifest: HarnessPlanComponentManifest {
                profile_schema_version: profile.schema_version(),
                compiler_revision: plan.compiler_revision(),
                strategy_catalog_revision: HARNESS_STRATEGY_CATALOG_REVISION,
                runtime_policy_version,
                profile_encoding_version: HARNESS_PROFILE_ENCODING_VERSION,
                compiler_input_encoding_version: HARNESS_COMPILER_INPUT_ENCODING_VERSION,
                resolved_plan_encoding_version: RESOLVED_HARNESS_PLAN_ENCODING_VERSION,
                profile_bytes,
                compiler_input_bytes,
                resolved_plan_bytes,
                profile_digest: profile.content_digest()?,
                resolved_plan_digest: *plan.plan_digest(),
            },
        })
    }

    /// Encodes this component in its sole canonical binary representation.
    ///
    /// # Errors
    ///
    /// Fails when a component or the complete envelope exceeds its bound.
    pub fn encode(&self) -> Result<Vec<u8>, HarnessManifestCodecError> {
        let body = encode_manifest(&self.manifest)?;
        let integrity = digest_manifest_bytes(&body)?;
        let body_len =
            u32::try_from(body.len()).map_err(|_| HarnessManifestCodecError::EnvelopeTooLarge)?;
        let mut bytes = Vec::with_capacity(8 + 4 + 1 + 4 + body.len() + 1 + 4 + 32);
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(&HARNESS_PLAN_COMPONENT_FORMAT_VERSION.to_be_bytes());
        bytes.push(ENVELOPE_KIND);
        bytes.extend_from_slice(&body_len.to_be_bytes());
        bytes.extend_from_slice(&body);
        write_digest_payload(
            &mut bytes,
            &integrity,
            DigestDomain::HarnessPlanComponentManifest,
        )?;
        if bytes.len() > MAX_HARNESS_PLAN_COMPONENT_BYTES {
            return Err(HarnessManifestCodecError::EnvelopeTooLarge);
        }
        Ok(bytes)
    }

    /// Returns the contained neutral harness-plan component.
    #[must_use]
    pub const fn manifest(&self) -> &HarnessPlanComponentManifest {
        &self.manifest
    }
}

/// Exact harness-plan reconstruction result suitable for a session controller.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub(crate) enum HarnessPlanReadiness {
    /// A freshly compiled trusted plan exactly matches the recorded plan.
    Exact(ResolvedHarnessPlan),
    /// Valid persisted data requires an unavailable historical component.
    Mismatch(HarnessPlanMismatch),
}

/// Typed reason why exact harness-plan reconstruction is not ready.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub(crate) enum HarnessPlanMismatch {
    /// The recorded profile schema is unavailable.
    #[error("unsupported harness profile schema version {found}")]
    ProfileSchema {
        /// Persisted profile-schema version.
        found: u32,
    },
    /// The exact historical compiler is unavailable.
    #[error("unavailable harness compiler revision {found}")]
    CompilerRevision {
        /// Persisted compiler revision.
        found: u32,
    },
    /// The exact historical closed strategy catalog is unavailable.
    #[error("unavailable harness strategy catalog revision {found}")]
    StrategyCatalogRevision {
        /// Persisted strategy-catalog revision.
        found: u32,
    },
    /// The caller does not implement the recorded runtime-policy semantics.
    #[error("unavailable runtime policy version {found}")]
    RuntimePolicyVersion {
        /// Persisted runtime-policy version.
        found: u32,
    },
    /// A compiler-owned canonical component encoding is unavailable.
    #[error("unavailable {component} encoding version {found}")]
    ComponentEncoding {
        /// Stable component name.
        component: &'static str,
        /// Persisted encoding version.
        found: u32,
    },
    /// The stored profile bytes are invalid or disagree with their identity.
    #[error("recorded harness profile does not validate")]
    Profile,
    /// The stored immutable compiler inputs do not validate.
    #[error("recorded harness compiler inputs do not validate")]
    CompilerInputs,
    /// Deterministic recompilation failed under the recorded requirements.
    #[error("recorded harness plan cannot be recompiled")]
    Recompilation,
    /// Recompilation did not reproduce the recorded plan identity and bytes.
    #[error("recompiled harness plan does not exactly match the recorded plan")]
    Plan,
}

/// Decodes, verifies, and recompiles the harness-plan component.
///
/// Unknown envelope versions/tags, malformed fields, trailing bytes, corrupt
/// integrity, zero runtime-policy versions, and oversized data fail closed.
/// Available-component disagreements remain typed readiness mismatches.
///
/// # Errors
///
/// Returns [`HarnessManifestCodecError`] for invalid durable bytes.
pub(crate) fn rehydrate_harness_plan(
    bytes: &[u8],
    available_runtime_policy_version: u32,
) -> Result<HarnessPlanReadiness, HarnessManifestCodecError> {
    if available_runtime_policy_version == 0 {
        return Err(HarnessManifestCodecError::ZeroRuntimePolicyVersion);
    }
    let envelope = decode_envelope(bytes)?;
    let manifest = envelope.manifest;
    if manifest.profile_schema_version != HARNESS_PROFILE_SCHEMA_VERSION {
        return Ok(mismatch(HarnessPlanMismatch::ProfileSchema {
            found: manifest.profile_schema_version,
        }));
    }
    if manifest.compiler_revision != HARNESS_PROFILE_COMPILER_REVISION {
        return Ok(mismatch(HarnessPlanMismatch::CompilerRevision {
            found: manifest.compiler_revision,
        }));
    }
    if manifest.strategy_catalog_revision != HARNESS_STRATEGY_CATALOG_REVISION {
        return Ok(mismatch(HarnessPlanMismatch::StrategyCatalogRevision {
            found: manifest.strategy_catalog_revision,
        }));
    }
    if manifest.runtime_policy_version != available_runtime_policy_version {
        return Ok(mismatch(HarnessPlanMismatch::RuntimePolicyVersion {
            found: manifest.runtime_policy_version,
        }));
    }
    for (component, found, available) in [
        (
            "profile",
            manifest.profile_encoding_version,
            HARNESS_PROFILE_ENCODING_VERSION,
        ),
        (
            "compiler-input",
            manifest.compiler_input_encoding_version,
            HARNESS_COMPILER_INPUT_ENCODING_VERSION,
        ),
        (
            "resolved-plan",
            manifest.resolved_plan_encoding_version,
            RESOLVED_HARNESS_PLAN_ENCODING_VERSION,
        ),
    ] {
        if found != available {
            return Ok(mismatch(HarnessPlanMismatch::ComponentEncoding {
                component,
                found,
            }));
        }
    }

    let Ok(profile) = decode_canonical_profile(&manifest.profile_bytes) else {
        return Ok(mismatch(HarnessPlanMismatch::Profile));
    };
    if profile.schema_version() != manifest.profile_schema_version
        || profile.content_digest()? != manifest.profile_digest
    {
        return Ok(mismatch(HarnessPlanMismatch::Profile));
    }
    let Ok(inputs) = decode_canonical_compiler_inputs(&manifest.compiler_input_bytes) else {
        return Ok(mismatch(HarnessPlanMismatch::CompilerInputs));
    };
    let Ok(plan) = HarnessProfileCompiler::new().compile(&profile, &inputs) else {
        return Ok(mismatch(HarnessPlanMismatch::Recompilation));
    };
    if plan.plan_digest() != &manifest.resolved_plan_digest
        || canonical_plan_bytes(&plan)? != manifest.resolved_plan_bytes
    {
        return Ok(mismatch(HarnessPlanMismatch::Plan));
    }
    Ok(HarnessPlanReadiness::Exact(plan))
}

const fn mismatch(reason: HarnessPlanMismatch) -> HarnessPlanReadiness {
    HarnessPlanReadiness::Mismatch(reason)
}

fn encode_manifest(
    manifest: &HarnessPlanComponentManifest,
) -> Result<Vec<u8>, HarnessManifestCodecError> {
    if manifest.runtime_policy_version == 0 {
        return Err(HarnessManifestCodecError::ZeroRuntimePolicyVersion);
    }
    check_component_bounds(
        &manifest.profile_bytes,
        &manifest.compiler_input_bytes,
        &manifest.resolved_plan_bytes,
    )?;
    let mut out = Vec::new();
    for value in [
        manifest.profile_schema_version,
        manifest.compiler_revision,
        manifest.strategy_catalog_revision,
        manifest.runtime_policy_version,
        manifest.profile_encoding_version,
        manifest.compiler_input_encoding_version,
        manifest.resolved_plan_encoding_version,
    ] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    write_bounded_bytes(&mut out, &manifest.profile_bytes, MAX_PROFILE_BYTES)?;
    write_bounded_bytes(&mut out, &manifest.compiler_input_bytes, MAX_INPUT_BYTES)?;
    write_bounded_bytes(&mut out, &manifest.resolved_plan_bytes, MAX_PLAN_BYTES)?;
    write_digest_payload(
        &mut out,
        &manifest.profile_digest,
        DigestDomain::HarnessProfileSpec,
    )?;
    write_digest_payload(
        &mut out,
        &manifest.resolved_plan_digest,
        DigestDomain::ResolvedHarnessPlan,
    )?;
    Ok(out)
}

fn decode_envelope(
    bytes: &[u8],
) -> Result<HarnessPlanComponentEnvelope, HarnessManifestCodecError> {
    if bytes.len() > MAX_HARNESS_PLAN_COMPONENT_BYTES {
        return Err(HarnessManifestCodecError::EnvelopeTooLarge);
    }
    let mut reader = Reader::new(bytes);
    if reader.take(MAGIC.len())? != MAGIC {
        return Err(HarnessManifestCodecError::InvalidMagic);
    }
    let version = reader.u32()?;
    if version != HARNESS_PLAN_COMPONENT_FORMAT_VERSION {
        return Err(HarnessManifestCodecError::UnsupportedEnvelopeVersion { found: version });
    }
    let kind = reader.u8()?;
    if kind != ENVELOPE_KIND {
        return Err(HarnessManifestCodecError::UnknownTag { tag: kind });
    }
    let body_len = reader.length(MAX_HARNESS_PLAN_COMPONENT_BYTES)?;
    let body = reader.take(body_len)?;
    let integrity = reader.digest(DigestDomain::HarnessPlanComponentManifest)?;
    reader.finish()?;
    if digest_manifest_bytes(body)? != integrity {
        return Err(HarnessManifestCodecError::IntegrityMismatch);
    }
    let mut body_reader = Reader::new(body);
    let manifest = HarnessPlanComponentManifest {
        profile_schema_version: body_reader.u32()?,
        compiler_revision: body_reader.u32()?,
        strategy_catalog_revision: body_reader.u32()?,
        runtime_policy_version: body_reader.nonzero_u32()?,
        profile_encoding_version: body_reader.nonzero_u32()?,
        compiler_input_encoding_version: body_reader.nonzero_u32()?,
        resolved_plan_encoding_version: body_reader.nonzero_u32()?,
        profile_bytes: body_reader.bytes(MAX_PROFILE_BYTES)?,
        compiler_input_bytes: body_reader.bytes(MAX_INPUT_BYTES)?,
        resolved_plan_bytes: body_reader.bytes(MAX_PLAN_BYTES)?,
        profile_digest: body_reader.digest(DigestDomain::HarnessProfileSpec)?,
        resolved_plan_digest: body_reader.digest(DigestDomain::ResolvedHarnessPlan)?,
    };
    body_reader.finish()?;
    Ok(HarnessPlanComponentEnvelope { manifest })
}

fn digest_manifest_bytes(bytes: &[u8]) -> Result<Digest, HarnessManifestCodecError> {
    Digest::sha256(
        DigestDomain::HarnessPlanComponentManifest,
        MANIFEST_DIGEST_VERSION,
        &[bytes],
    )
    .map_err(|error| HarnessManifestCodecError::Profile(error.into()))
}

fn write_bounded_bytes(
    out: &mut Vec<u8>,
    bytes: &[u8],
    max: usize,
) -> Result<(), HarnessManifestCodecError> {
    if bytes.len() > max {
        return Err(HarnessManifestCodecError::ComponentTooLarge);
    }
    let len =
        u32::try_from(bytes.len()).map_err(|_| HarnessManifestCodecError::ComponentTooLarge)?;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

fn write_digest_payload(
    out: &mut Vec<u8>,
    digest: &Digest,
    expected_domain: DigestDomain,
) -> Result<(), HarnessManifestCodecError> {
    if digest.domain() != expected_domain || digest.version() == 0 {
        return Err(HarnessManifestCodecError::InvalidDigest);
    }
    out.push(DIGEST_ALGORITHM_SHA256);
    out.extend_from_slice(&digest.version().to_be_bytes());
    out.extend_from_slice(digest.bytes());
    Ok(())
}

fn check_component_bounds(
    profile: &[u8],
    inputs: &[u8],
    plan: &[u8],
) -> Result<(), HarnessManifestCodecError> {
    if profile.len() > MAX_PROFILE_BYTES
        || inputs.len() > MAX_INPUT_BYTES
        || plan.len() > MAX_PLAN_BYTES
    {
        return Err(HarnessManifestCodecError::ComponentTooLarge);
    }
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], HarnessManifestCodecError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or(HarnessManifestCodecError::Malformed)?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(HarnessManifestCodecError::Malformed)?;
        self.offset = end;
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8, HarnessManifestCodecError> {
        self.take(1).map(|bytes| bytes[0])
    }

    fn u32(&mut self) -> Result<u32, HarnessManifestCodecError> {
        let bytes: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| HarnessManifestCodecError::Malformed)?;
        Ok(u32::from_be_bytes(bytes))
    }

    fn nonzero_u32(&mut self) -> Result<u32, HarnessManifestCodecError> {
        let value = self.u32()?;
        if value == 0 {
            Err(HarnessManifestCodecError::ZeroVersion)
        } else {
            Ok(value)
        }
    }

    fn length(&mut self, max: usize) -> Result<usize, HarnessManifestCodecError> {
        let len = usize::try_from(self.u32()?).map_err(|_| HarnessManifestCodecError::Malformed)?;
        if len > max {
            Err(HarnessManifestCodecError::ComponentTooLarge)
        } else {
            Ok(len)
        }
    }

    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, HarnessManifestCodecError> {
        let len = self.length(max)?;
        Ok(self.take(len)?.to_vec())
    }

    fn digest(&mut self, domain: DigestDomain) -> Result<Digest, HarnessManifestCodecError> {
        let tag = self.u8()?;
        if tag != DIGEST_ALGORITHM_SHA256 {
            return Err(HarnessManifestCodecError::UnknownTag { tag });
        }
        let version = self.nonzero_u32()?;
        let bytes: [u8; 32] = self
            .take(32)?
            .try_into()
            .map_err(|_| HarnessManifestCodecError::Malformed)?;
        Digest::from_sha256_bytes(domain, version, bytes)
            .map_err(|_| HarnessManifestCodecError::InvalidDigest)
    }

    fn finish(self) -> Result<(), HarnessManifestCodecError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(HarnessManifestCodecError::TrailingBytes)
        }
    }
}

/// Fail-closed framing and validation errors for persisted component bytes.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum HarnessManifestCodecError {
    /// The complete envelope exceeds its hard bound.
    #[error("harness-plan component envelope is too large")]
    EnvelopeTooLarge,
    /// One embedded compiler-owned component exceeds its hard bound.
    #[error("harness-plan component is too large")]
    ComponentTooLarge,
    /// Runtime-policy version zero is reserved.
    #[error("runtime policy version must be nonzero")]
    ZeroRuntimePolicyVersion,
    /// A persisted component or digest version is zero.
    #[error("persisted component version must be nonzero")]
    ZeroVersion,
    /// Capture inputs do not reproduce the supplied plan.
    #[error("harness plan is inconsistent with its profile and compiler inputs")]
    InconsistentPlan,
    /// The fixed envelope magic is absent.
    #[error("invalid harness-plan component magic")]
    InvalidMagic,
    /// The outer binary envelope version is not implemented.
    #[error("unsupported harness-plan component envelope version {found}")]
    UnsupportedEnvelopeVersion {
        /// Persisted envelope version.
        found: u32,
    },
    /// A closed binary tag is not implemented.
    #[error("unknown harness-plan component tag {tag}")]
    UnknownTag {
        /// Persisted tag.
        tag: u8,
    },
    /// Bytes are truncated or contain an impossible length.
    #[error("malformed harness-plan component encoding")]
    Malformed,
    /// Bytes remain after the fixed envelope fields.
    #[error("harness-plan component encoding has trailing bytes")]
    TrailingBytes,
    /// A digest has the wrong domain, algorithm, or version.
    #[error("invalid harness-plan component digest identity")]
    InvalidDigest,
    /// The embedded integrity digest disagrees with the manifest bytes.
    #[error("harness-plan component integrity mismatch")]
    IntegrityMismatch,
    /// A compiler-owned component failed validation.
    #[error(transparent)]
    Profile(#[from] ProfileError),
}

#[cfg(test)]
mod tests;
