use super::{DigestError, Error, HarnessProfileSpec, validate_decoded_spec};

/// Errors constructing or compiling harness profiles.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
#[non_exhaustive]
pub enum ProfileError {
    /// Profile identity is missing.
    #[error("harness profile identity is required")]
    MissingIdentity,
    /// Profile identity is invalid.
    #[error("harness profile identity is invalid")]
    InvalidIdentity,
    /// Context-source selector is invalid.
    #[error("context source selector is invalid")]
    InvalidSourceSelector,
    /// Tool override is invalid.
    #[error("tool exposure override is invalid")]
    InvalidToolOverride,
    /// Text field exceeds its bound.
    #[error("profile text exceeds {max_bytes} bytes (found {found})")]
    TextTooLarge {
        /// Maximum permitted bytes.
        max_bytes: usize,
        /// Observed byte length.
        found: usize,
    },
    /// Too many instruction sources.
    #[error("too many instruction sources")]
    TooManySources,
    /// Too many tool overrides.
    #[error("too many tool exposure overrides")]
    TooManyToolOverrides,
    /// Too many diagnostics.
    #[error("too many profile diagnostics")]
    TooManyDiagnostics,
    /// `PreferEffort` without an effort value.
    #[error("preferred reasoning effort is required")]
    MissingReasoningEffort,
    /// Effort supplied while reasoning is disabled.
    #[error("reasoning effort is not allowed when reasoning is disabled")]
    UnexpectedReasoningEffort,
    /// Unsupported schema version.
    #[error("unsupported harness profile schema version {found}")]
    UnsupportedSchemaVersion {
        /// Observed schema version.
        found: u32,
    },
    /// Instruction composition exceeded the runtime budget.
    #[error("composed instructions exceed the {max_bytes}-byte budget")]
    InstructionBudgetExceeded {
        /// Maximum permitted instruction bytes.
        max_bytes: usize,
    },
    /// A selected context source was not resolved before compilation.
    #[error("context source `{source_id}` was not resolved before compilation")]
    UnresolvedContextSource {
        /// Missing source identity.
        source_id: String,
    },
    /// Tool override names a tool absent from the frozen registry.
    #[error("canonical tool `{name}` is not present in the frozen registry")]
    UnknownCanonicalTool {
        /// Missing tool name.
        name: String,
    },
    /// Required feature cannot be satisfied by model/runtime facts.
    #[error("required feature `{feature}` is unavailable")]
    RequiredFeatureUnavailable {
        /// Feature name.
        feature: &'static str,
    },
    /// Context-source digest domain is not accepted.
    #[error("context source digest domain is invalid")]
    InvalidSourceDigestDomain,
    /// Immutable context-source reference is invalid.
    #[error("immutable context source reference is invalid")]
    InvalidContextSourceReference,
    /// Exact context-source content disagrees with its recorded digest.
    #[error("context source `{source_id}` content digest does not match")]
    ContextSourceDigestMismatch {
        /// Source whose exact content failed verification.
        source_id: String,
    },
    /// Observed context-source version disagrees with its immutable reference.
    #[error("context source `{source_id}` version does not match")]
    ContextSourceVersionMismatch {
        /// Source whose observed version failed verification.
        source_id: String,
    },
    /// A typed compiler authority fact carries a foreign digest domain.
    #[error("compiler input digest domain is invalid")]
    InvalidCompilerDigestDomain,
    /// Serialization failure while digesting profile or plan data.
    #[error("profile serialization failed: {reason}")]
    Serialization {
        /// Failure reason.
        reason: String,
    },
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// JSON profile decoding failed closed.
    #[error("harness profile JSON is invalid: {reason}")]
    InvalidJson {
        /// Failure reason.
        reason: String,
    },
    /// Context-source catalog already contains the identity.
    #[error("context source `{source_id}` is already registered")]
    DuplicateContextSource {
        /// Conflicting source identity.
        source_id: String,
    },
    /// Explicit plan binding does not match the selected model adapter facts.
    #[error("resolved harness plan is bound to a different model/route/adapter")]
    PlanModelBindingMismatch,
    /// Explicit plan tool exposure disagrees with the provided exposure map.
    #[error("resolved harness plan tool exposure disagrees with the tool exposure map")]
    PlanToolExposureMismatch,
}

impl ProfileError {
    /// Decodes a profile specification from JSON with unknown-field rejection.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::InvalidJson`] or a validation error.
    pub fn decode_spec(bytes: &[u8]) -> Result<HarnessProfileSpec, Self> {
        let spec: HarnessProfileSpec =
            serde_json::from_slice(bytes).map_err(|error| Self::InvalidJson {
                reason: error.to_string(),
            })?;
        validate_decoded_spec(&spec)?;
        Ok(spec)
    }
}
