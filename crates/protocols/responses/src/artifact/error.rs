use plakous_model::{Digest, DigestAlgorithm, DigestDomain};
use thiserror::Error;

use super::MAX_OPAQUE_ID_BYTES;

/// Fail-closed artifact construction, encoding, or validation error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ResponsesArtifactError {
    /// A behavior-bearing semantic revision used reserved version zero.
    #[error("Responses artifact {field} revision must be nonzero")]
    ZeroRevision {
        /// Revision field that was zero.
        field: &'static str,
    },
    /// The supplied model/request plan was not one exact admitted operation.
    #[error("Responses artifact operation was not independently admitted")]
    AdmissionMismatch,
    /// The admitted request lacked its core-resolved harness-plan identity.
    #[error("Responses artifact request lacks a resolved harness-plan identity")]
    MissingHarnessPlanIdentity,
    /// A digest carried the wrong algorithm, domain, or version.
    #[error("Responses artifact has an invalid {field} digest")]
    InvalidDigest {
        /// Digest field that failed validation.
        field: &'static str,
    },
    /// A reconstructed target identity was invalid.
    #[error("Responses artifact has an invalid {field} identity")]
    InvalidIdentity {
        /// Identity field that failed validation.
        field: &'static str,
    },
    /// The complete binding identity could not be derived.
    #[error("Responses artifact binding identity construction failed")]
    BindingConstruction,
    /// An opaque provider identity was empty or contained control characters.
    #[error("Responses artifact {field} is invalid")]
    InvalidOpaqueId {
        /// Opaque identity field that failed validation.
        field: &'static str,
    },
    /// A bounded opaque provider identity was too large.
    #[error("Responses artifact {field} exceeds {maximum} bytes")]
    OpaqueIdTooLong {
        /// Opaque identity field that exceeded its bound.
        field: &'static str,
        /// Maximum accepted byte count.
        maximum: usize,
    },
    /// A continuation claimed to cover no canonical projection items.
    #[error("Responses continuation must cover at least one projection item")]
    ZeroProjectionItems,
    /// Physical or canonical coordinates were arithmetically inconsistent.
    #[error("Responses artifact production coordinates are invalid")]
    InvalidCoordinates,
    /// Physical production facts were transplanted from another admitted request.
    #[error("Responses artifact production does not match its admitted binding")]
    ProductionBindingMismatch,
    /// No native fact justified retaining an artifact.
    #[error("Responses artifact contains no replay, continuation, or cache fact")]
    EmptyArtifact,
    /// The ordered replay list exceeded its item bound.
    #[error("Responses artifact exceeds {maximum} replay items")]
    TooManyReplayItems {
        /// Maximum retained replay items.
        maximum: usize,
    },
    /// Aggregate replay bytes exceeded the hard artifact replay budget.
    #[error("Responses artifact exceeds {maximum} aggregate replay bytes")]
    ReplayBytesTooLarge {
        /// Maximum aggregate canonical replay bytes.
        maximum: usize,
    },
    /// One replay item exceeded its byte bound.
    #[error("Responses artifact replay item {index} exceeds {maximum} bytes")]
    ReplayItemTooLarge {
        /// Ordered replay index.
        index: usize,
        /// Maximum canonical replay item bytes.
        maximum: usize,
    },
    /// Replay contract assigned one kind to multiple purposes.
    #[error("Responses replay contract assigns one kind to multiple purposes")]
    AmbiguousReplayContract,
    /// Replay purpose and exact dialect kind/version disagreed.
    #[error("Responses artifact replay item {index} has a mislabeled purpose or kind")]
    ReplayPurposeKindMismatch {
        /// Ordered replay index.
        index: usize,
    },
    /// A replay envelope was scoped to another dialect/model binding.
    #[error("Responses artifact replay item {index} has a foreign binding")]
    ReplayBindingMismatch {
        /// Ordered replay index.
        index: usize,
    },
    /// One replay item named an invalid response-relative coordinate.
    #[error("Responses artifact replay item {index} has an invalid coordinate")]
    InvalidReplayCoordinate {
        /// Ordered replay index.
        index: usize,
    },
    /// Two same-purpose replay facts claimed one exact response coordinate.
    #[error("Responses artifact replay item {index} duplicates an exact coordinate")]
    DuplicateReplayCoordinate {
        /// Ordered replay index.
        index: usize,
    },
    /// Local replay did not cover every native output item exactly once.
    #[error("Responses artifact replay does not completely cover native output")]
    IncompleteReplayCoverage,
    /// Encrypted native reasoning was incorrectly marked display-public.
    #[error("Responses encrypted reasoning item {index} must be sensitive or secret")]
    PublicEncryptedReasoning {
        /// Ordered replay index.
        index: usize,
    },
    /// Raw secret-classified replay material cannot enter the family artifact.
    #[error("Responses artifact replay item {index} contains raw secret material")]
    SecretReplayMaterial {
        /// Ordered replay index.
        index: usize,
    },
    /// A continuation existed without provider-side response storage.
    #[error("Responses continuation requires provider-stored response mode")]
    ContinuationWithoutStorage,
    /// Provider-stored output lacked its sole continuation coverage fact.
    #[error("Responses provider-stored output lacks a continuation")]
    MissingStoredContinuation,
    /// Continuation did not cover the exact terminal response and canonical prefix.
    #[error("Responses continuation coverage does not match artifact production")]
    ContinuationCoverageMismatch,
    /// A cache fact did not match the exact stable-prefix binding.
    #[error("Responses cache fact does not match the stable-prefix binding")]
    CachePrefixMismatch,
    /// A cache fact named neither an exact producing nor covered canonical prefix.
    #[error("Responses cache fact has invalid canonical coverage")]
    CacheCoverageMismatch,
    /// The artifact crossed its aggregate expiry boundary.
    #[error("Responses artifact is expired")]
    Expired,
    /// The caller expected a different exact target, request, or producer binding.
    #[error("Responses artifact binding mismatch")]
    BindingMismatch,
    /// Replayed native material was absent from its exact canonical coordinate.
    #[error("Responses artifact canonical replay coordinate mismatch")]
    CanonicalCoordinateMismatch,
    /// Native Responses replay appeared after the artifact's exact covered prefix.
    #[error("Responses artifact found unauthorized native replay at canonical position {position}")]
    UnauthorizedReplayTail {
        /// Absolute canonical position outside artifact coverage.
        position: usize,
    },
    /// The complete encoded artifact exceeded its hard bound.
    #[error("Responses artifact exceeds {maximum} bytes")]
    ArtifactTooLarge {
        /// Maximum accepted encoded artifact bytes.
        maximum: usize,
    },
    /// The family codec magic was absent.
    #[error("invalid Responses artifact magic")]
    InvalidMagic,
    /// The binary codec revision is unavailable.
    #[error("unsupported Responses artifact codec revision {found}")]
    UnsupportedCodecRevision {
        /// Encoded codec revision.
        found: u32,
    },
    /// The artifact semantic schema is unavailable.
    #[error("unsupported Responses artifact schema revision {found}")]
    UnsupportedSchemaRevision {
        /// Encoded semantic schema revision.
        found: u32,
    },
    /// A closed artifact discriminant was unavailable.
    #[error("unknown Responses artifact {field} tag {tag}")]
    UnknownTag {
        /// Closed field whose tag was unknown.
        field: &'static str,
        /// Encoded tag.
        tag: u8,
    },
    /// Bytes were truncated or carried an impossible length.
    #[error("malformed Responses artifact encoding")]
    Malformed,
    /// Bytes remained after the sole canonical value.
    #[error("Responses artifact encoding has trailing bytes")]
    TrailingBytes,
    /// The outer integrity witness disagreed with the complete body.
    #[error("Responses artifact integrity mismatch")]
    IntegrityMismatch,
    /// A versioned integrity witness could not be constructed.
    #[error("Responses artifact integrity construction failed")]
    IntegrityConstruction,
    /// A replay envelope could not be decoded and integrity-validated.
    #[error("invalid Responses replay encoding")]
    InvalidReplayEncoding,
    /// Replay bytes had a second structural representation.
    #[error("non-canonical Responses replay encoding")]
    NonCanonicalReplay,
    /// An encoded timestamp was structurally invalid.
    #[error("invalid Responses artifact timestamp")]
    InvalidTimestamp,
}

pub(super) fn validate_digest(
    digest: Digest,
    domain: DigestDomain,
    field: &'static str,
) -> Result<(), ResponsesArtifactError> {
    if digest.algorithm() != DigestAlgorithm::Sha256
        || digest.domain() != domain
        || digest.version() == 0
    {
        return Err(ResponsesArtifactError::InvalidDigest { field });
    }
    Ok(())
}

pub(super) fn validate_opaque_id(
    value: &str,
    field: &'static str,
) -> Result<(), ResponsesArtifactError> {
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(ResponsesArtifactError::InvalidOpaqueId { field });
    }
    if value.len() > MAX_OPAQUE_ID_BYTES {
        return Err(ResponsesArtifactError::OpaqueIdTooLong {
            field,
            maximum: MAX_OPAQUE_ID_BYTES,
        });
    }
    Ok(())
}
