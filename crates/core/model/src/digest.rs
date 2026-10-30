//! Versioned, domain-separated integrity digests.

use std::fmt;
use std::io::{self, Read};

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

/// The hash algorithm carried by a [`Digest`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum DigestAlgorithm {
    /// SHA-256.
    Sha256,
}

impl DigestAlgorithm {
    const fn identifier(self) -> &'static [u8] {
        match self {
            Self::Sha256 => b"sha-256",
        }
    }
}

/// The semantic domain whose bytes a digest identifies.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[non_exhaustive]
pub enum DigestDomain {
    /// Sensitive replay envelope identity and raw payload.
    ReplayEnvelope,
    /// All request-affecting inputs in a validated plan.
    RequestPlan,
    /// One immutable protocol, route, model, profile, and capability binding.
    ModelBinding,
    /// A canonical model-facing tool definition.
    ToolDefinition,
    /// Raw blob bytes.
    Blob,
    /// Framed snapshot bytes.
    Snapshot,
    /// Exact effect-authorization identity and policy inputs.
    EffectAuthorization,
    /// One immutable host-local execution environment authority binding.
    ExecutionEnvironment,
    /// One explicit terminal effect-reconciliation assertion.
    EffectReconciliation,
    /// One immutable result-carrier witness for promoted reconciliation audit.
    EffectReconciliationCarrier,
    /// One complete canonical provider-operation response.
    ProviderOperation,
    /// One immutable data-only harness profile specification.
    HarnessProfileSpec,
    /// One immutable compiled harness plan binding.
    ResolvedHarnessPlan,
    /// One closed core strategy-catalog revision.
    HarnessStrategyCatalog,
    /// One canonical persisted harness-plan component manifest.
    HarnessPlanComponentManifest,
    /// One immutable semantic canonical-history segment.
    CanonicalHistorySegment,
    /// Ordered immutable canonical-history segment identities.
    CanonicalHistory,
    /// One committed plus bounded-working model-visible projection.
    HistoryProjection,
    /// One ordered canonical-item prefix independent of semantic segment boundaries.
    CanonicalItemPrefix,
    /// One immutable provider-neutral stable request prefix.
    StableRequestPrefix,
    /// One exact scoped input-token measurement fact.
    TokenMeasurement,
    /// One exact context window and output reservation.
    ContextBudget,
    /// One checked whole-projection context admission.
    ContextAdmission,
    /// One closed whole-group context history plan or retained candidate.
    ContextPlan,
    /// One runtime-owned branch cache-affinity identity.
    CacheAffinity,
    /// One complete caller-selected provider configuration.
    SelectedProviderConfiguration,
    /// One complete compiled semantic provider configuration.
    EffectiveProviderConfiguration,
    /// One bounded provider-owned semantic evidence projection.
    ProviderSemanticEvidence,
    /// One stable resolved provider/account/credential-authority binding.
    ProviderAuthorityBinding,
    /// One model-visible prompt or tool-description string.
    Prompt,
}

impl DigestDomain {
    const fn identifier(self) -> &'static [u8] {
        match self {
            Self::ReplayEnvelope => b"replay-envelope",
            Self::RequestPlan => b"request-plan",
            Self::ModelBinding => b"model-binding",
            Self::ToolDefinition => b"tool-definition",
            Self::Blob => b"blob",
            Self::Snapshot => b"snapshot",
            Self::EffectAuthorization => b"effect-authorization",
            Self::ExecutionEnvironment => b"execution-environment",
            Self::EffectReconciliation => b"effect-reconciliation",
            Self::EffectReconciliationCarrier => b"effect-reconciliation-carrier",
            Self::ProviderOperation => b"provider-operation",
            Self::HarnessProfileSpec => b"harness-profile-spec",
            Self::ResolvedHarnessPlan => b"resolved-harness-plan",
            Self::HarnessStrategyCatalog => b"harness-strategy-catalog",
            Self::HarnessPlanComponentManifest => b"harness-plan-component-manifest",
            Self::CanonicalHistorySegment => b"canonical-history-segment",
            Self::CanonicalHistory => b"canonical-history",
            Self::HistoryProjection => b"history-projection",
            Self::CanonicalItemPrefix => b"canonical-item-prefix",
            Self::StableRequestPrefix => b"stable-request-prefix",
            Self::TokenMeasurement => b"token-measurement",
            Self::ContextBudget => b"context-budget",
            Self::ContextAdmission => b"context-admission",
            Self::ContextPlan => b"context-plan",
            Self::CacheAffinity => b"cache-affinity",
            Self::SelectedProviderConfiguration => b"selected-provider-configuration",
            Self::EffectiveProviderConfiguration => b"effective-provider-configuration",
            Self::ProviderSemanticEvidence => b"provider-semantic-evidence",
            Self::ProviderAuthorityBinding => b"provider-authority-binding",
            Self::Prompt => b"prompt",
        }
    }
}

/// An error constructing a versioned digest.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum DigestError {
    /// Format version zero is reserved and cannot be emitted.
    #[error("digest format version must be nonzero")]
    ZeroVersion,
}

/// Failure while hashing one declared-length stream into a canonical digest.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DigestStreamError {
    /// The digest revision was invalid.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// The source could not be read.
    #[error("digest stream read failed")]
    Io(#[from] io::Error),
    /// The stream length differed from its declared canonical length.
    #[error("digest stream length mismatch")]
    LengthMismatch,
}

/// A complete digest identity, including algorithm, domain, and format version.
#[derive(Clone, Copy, Eq, Hash, PartialEq, Serialize)]
pub struct Digest {
    algorithm: DigestAlgorithm,
    domain: DigestDomain,
    version: u32,
    bytes: [u8; 32],
}

#[derive(Deserialize)]
struct DigestWire {
    algorithm: DigestAlgorithm,
    domain: DigestDomain,
    version: u32,
    bytes: [u8; 32],
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = DigestWire::deserialize(deserializer)?;
        if wire.version == 0 {
            return Err(serde::de::Error::custom(DigestError::ZeroVersion));
        }
        Ok(Self {
            algorithm: wire.algorithm,
            domain: wire.domain,
            version: wire.version,
            bytes: wire.bytes,
        })
    }
}

impl Digest {
    /// Hashes one exact declared-length byte stream without retaining it.
    ///
    /// The result is byte-for-byte identical to `sha256(domain, version,
    /// &[complete_bytes])`. EOF before `byte_len`, or any additional byte after
    /// it, fails rather than changing the canonical preimage.
    ///
    /// # Errors
    ///
    /// Returns an error for revision zero, a source read failure, or a stream
    /// whose observed length differs from `byte_len`.
    pub fn sha256_reader(
        domain: DigestDomain,
        version: u32,
        byte_len: u64,
        reader: &mut impl Read,
    ) -> Result<Self, DigestStreamError> {
        if version == 0 {
            return Err(DigestError::ZeroVersion.into());
        }
        let mut hasher = Sha256::new();
        hasher.update(b"plak-digest\0");
        update_frame(&mut hasher, DigestAlgorithm::Sha256.identifier());
        update_frame(&mut hasher, domain.identifier());
        hasher.update(version.to_be_bytes());
        hasher.update(length_bytes(1));
        hasher.update(byte_len.to_be_bytes());

        let mut remaining = byte_len;
        let mut chunk = [0_u8; 16 * 1024];
        while remaining > 0 {
            let maximum = usize::try_from(remaining)
                .unwrap_or(usize::MAX)
                .min(chunk.len());
            let read = reader.read(&mut chunk[..maximum])?;
            if read == 0 {
                return Err(DigestStreamError::LengthMismatch);
            }
            hasher.update(&chunk[..read]);
            remaining -= u64::try_from(read).unwrap_or(u64::MAX);
        }
        if reader.read(&mut chunk[..1])? != 0 {
            return Err(DigestStreamError::LengthMismatch);
        }
        Ok(Self {
            algorithm: DigestAlgorithm::Sha256,
            domain,
            version,
            bytes: hasher.finalize().into(),
        })
    }

    /// Reconstructs a persisted SHA-256 digest identity from canonical fields.
    ///
    /// # Errors
    ///
    /// Returns [`DigestError::ZeroVersion`] when `version` is zero.
    pub fn from_sha256_bytes(
        domain: DigestDomain,
        version: u32,
        bytes: [u8; 32],
    ) -> Result<Self, DigestError> {
        if version == 0 {
            return Err(DigestError::ZeroVersion);
        }
        Ok(Self {
            algorithm: DigestAlgorithm::Sha256,
            domain,
            version,
            bytes,
        })
    }

    /// Computes a length-delimited SHA-256 digest over ordered byte parts.
    ///
    /// The algorithm ID, domain ID, format version, part count, and length of
    /// every part are hashed before the corresponding bytes.
    ///
    /// # Errors
    ///
    /// Returns [`DigestError::ZeroVersion`] for reserved format version zero.
    pub fn sha256(
        domain: DigestDomain,
        version: u32,
        parts: &[&[u8]],
    ) -> Result<Self, DigestError> {
        if version == 0 {
            return Err(DigestError::ZeroVersion);
        }

        let mut hasher = Sha256::new();
        hasher.update(b"plak-digest\0");
        update_frame(&mut hasher, DigestAlgorithm::Sha256.identifier());
        update_frame(&mut hasher, domain.identifier());
        hasher.update(version.to_be_bytes());
        hasher.update(length_bytes(parts.len()));
        for part in parts {
            update_frame(&mut hasher, part);
        }
        let bytes: [u8; 32] = hasher.finalize().into();
        Ok(Self {
            algorithm: DigestAlgorithm::Sha256,
            domain,
            version,
            bytes,
        })
    }

    /// Returns the digest algorithm.
    #[must_use]
    pub const fn algorithm(self) -> DigestAlgorithm {
        self.algorithm
    }

    /// Returns the semantic digest domain.
    #[must_use]
    pub const fn domain(self) -> DigestDomain {
        self.domain
    }

    /// Returns the domain format version.
    #[must_use]
    pub const fn version(self) -> u32 {
        self.version
    }

    /// Returns the raw digest bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; 32] {
        &self.bytes
    }

    /// Returns a stable length-framed encoding of the complete digest identity.
    ///
    /// This includes algorithm, domain, version, and raw bytes and is suitable
    /// for embedding one digest inside another versioned canonical preimage.
    #[must_use]
    pub fn identity_bytes(&self) -> Vec<u8> {
        let algorithm = self.algorithm.identifier();
        let domain = self.domain.identifier();
        let mut encoded = Vec::with_capacity(
            8_usize
                .saturating_mul(2)
                .saturating_add(algorithm.len())
                .saturating_add(domain.len())
                .saturating_add(4)
                .saturating_add(self.bytes.len()),
        );
        encoded.extend_from_slice(&length_bytes(algorithm.len()));
        encoded.extend_from_slice(algorithm);
        encoded.extend_from_slice(&length_bytes(domain.len()));
        encoded.extend_from_slice(domain);
        encoded.extend_from_slice(&self.version.to_be_bytes());
        encoded.extend_from_slice(&self.bytes);
        encoded
    }
}

/// Lowercase hexadecimal encoding of `bytes`.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

/// SHA-256 over the exact concatenation of `parts`.
///
/// This is the plain primitive: no domain separator, no version, and no length
/// framing, so `["ab", "c"]` and `["a", "bc"]` collide. It exists so callers
/// that already own their preimage stop hand-rolling a hasher. Anything that
/// needs a distinguishable, versioned identity wants [`Digest::sha256`]
/// instead, which frames every part and binds a [`DigestDomain`].
#[must_use]
pub fn sha256_bytes(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Lowercase hex SHA-256 over the exact concatenation of `parts`.
///
/// Carries the same unframed contract as [`sha256_bytes`].
#[must_use]
pub fn sha256_hex(parts: &[&[u8]]) -> String {
    hex_lower(&sha256_bytes(parts))
}

fn update_frame(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update(length_bytes(bytes.len()));
    hasher.update(bytes);
}

fn length_bytes(length: usize) -> [u8; 8] {
    u64::try_from(length).unwrap_or(u64::MAX).to_be_bytes()
}

impl fmt::Display for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.bytes {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Digest")
            .field("algorithm", &self.algorithm)
            .field("domain", &self.domain)
            .field("version", &self.version)
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests;
