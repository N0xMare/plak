use super::Error;

/// A route or binding configuration failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A required configuration value was empty.
    #[error("{0} cannot be empty")]
    EmptyField(&'static str),
    /// An endpoint was not a credential-safe HTTPS origin.
    #[error("endpoint must be an HTTPS origin without credentials, path, query, or fragment")]
    InvalidOrigin,
    /// The first-party Platform route was assigned a non-OpenAI origin.
    #[error("first-party OpenAI Platform route requires the official API origin")]
    NonOfficialPlatformOrigin,
    /// The direct subscription route was assigned a non-ChatGPT origin.
    #[error("direct Codex subscription route requires the official ChatGPT origin")]
    NonOfficialCodexSubscriptionOrigin,
    /// A credential reference used an unsupported or unsafe shape.
    #[error("credential reference must be a bounded application identifier")]
    InvalidCredentialReference,
    /// A credential authority used an unsupported or unsafe shape.
    #[error("credential authority must be a bounded application identifier")]
    InvalidCredentialAuthority,
    /// An explicit project identifier was empty, oversized, or non-ASCII.
    #[error("OpenAI project identifier must be nonempty, bounded, ASCII text")]
    InvalidProjectId,
    /// A local execution fixture was not exact HTTPS loopback with an explicit port.
    #[error("OpenAI local fixture origin must be HTTPS loopback with an explicit port")]
    NonLocalFixtureOrigin,
    /// Fixture-only capacity or measurement authority could not be sealed.
    #[error("OpenAI local fixture context authority is invalid")]
    LocalFixtureContext,
    /// The direct subscription route lacks explicit Plakous-authorized OAuth.
    #[error(
        "direct Codex subscription route requires an explicitly injected Plakous-authorized OAuth source"
    )]
    SubscriptionOAuthRequired,
    /// The selected owner store could not produce a current source binding.
    #[error("direct Codex subscription credential source is unavailable")]
    SubscriptionCredentialUnavailable,
    /// The model descriptor names a different protocol dialect.
    #[error("model descriptor protocol does not match route dialect")]
    ProtocolMismatch,
    /// The model descriptor names a different immutable route.
    #[error("model descriptor route does not match selected route")]
    RouteMismatch,
    /// The request plan belongs to another binding.
    #[error("request plan belongs to another protocol/route/model/profile binding")]
    ForeignPlan,
    /// A route-account binding digest could not be created.
    #[error("route account binding digest failed")]
    BindingDigest,
    /// A descriptor claimed a feature explicitly excluded by route evidence.
    #[error("model descriptor claims a feature excluded by route conformance")]
    ExcludedConformanceClaim,
    /// A descriptor claimed a feature with no positive route evidence.
    #[error("model descriptor claims a feature unknown to route conformance")]
    UnknownConformanceClaim,
    /// An offline codec target contained a positive fact not backed by a fixture.
    #[error("offline codec targets require test-fixture provenance for every positive feature")]
    NonFixtureOfflineClaim,
    /// A fixture-backed capability was passed to the ordinary routable constructor.
    #[error("test-fixture capability claims require an explicit non-routable offline codec target")]
    FixtureClaimRequiresOfflineTarget,
    /// A routable positive capability lacked provider-route evidence.
    #[error("routable OpenAI capabilities require route-specific evidence")]
    RoutableCapabilityRequiresRouteEvidence,
    /// A routable positive capability's route evidence had expired.
    #[error("routable OpenAI capability evidence has expired")]
    RoutableCapabilityEvidenceExpired,
    /// A routable target omitted positive streaming support required by the codec.
    #[error("routable OpenAI targets require positive HTTP/SSE streaming conformance")]
    StreamingConformanceRequired,
    /// A native subscription target was not issued by its private catalog resolver.
    #[error("native Codex targets require an exact resolver-issued catalog witness")]
    NativeTargetRequiresCatalogWitness,
    /// A production Platform target was not issued by the exact reviewed manifest.
    #[error("OpenAI Platform targets require an exact reviewed-manifest witness")]
    PlatformTargetRequiresManifestWitness,
    /// No exact reviewed model/response/family/profile row matched the request.
    #[error("OpenAI Platform model tuple is not present in the reviewed manifest")]
    UnknownPlatformManifestTuple,
    /// A retained reviewed-manifest witness no longer matches its descriptor.
    #[error("OpenAI Platform reviewed-model witness does not match the descriptor")]
    PlatformManifestBinding,
    /// A caller requested more output than the exact reviewed model row allows.
    #[error("max output tokens exceed the exact reviewed model limit")]
    MaximumOutputTokensExceeded,
    /// The adapter cannot faithfully lower the requested tool definition form.
    #[error("requested tool form is not supported by the admitted OpenAI adapter subset")]
    UnsupportedToolForm,
    /// The requested tool choice does not name an exact advertised function tool.
    #[error("tool choice is incompatible with the exact advertised function tools")]
    InvalidToolChoice,
    /// Portable reasoning token budgets cannot be lowered exactly.
    #[error("reasoning token budgets are not supported by the OpenAI adapter")]
    UnsupportedReasoningBudget,
    /// The neutral reasoning effort is not in the admitted portable subset.
    #[error("reasoning effort is not supported by the admitted OpenAI adapter subset")]
    UnsupportedReasoningEffort,
    /// Offline codec evidence cannot be used for live route validation.
    #[error("offline OpenAI codec targets are non-routable")]
    NonRoutableTarget,
}

/// A redaction-safe credential resolution error.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum AuthError {
    /// No current credential exists for the configured source.
    #[error("configured credential is unavailable")]
    Unavailable,
    /// The credential is not authorized for the route origin or audience.
    #[error("credential is not authorized for the configured route binding")]
    BindingMismatch,
    /// Credential resolution or exposure did not belong to an active physical request.
    #[error("subscription authorization requires an active physical request")]
    NoActivePhysicalRequest,
    /// This physical request already spent its single credential-resolution allowance.
    #[error("subscription authorization was already resolved for this physical request")]
    AuthorizationAlreadyResolved,
    /// Refresh or source access failed without exposing secret details.
    #[error("credential source failed")]
    SourceFailure,
    /// The turn-scoped inference input bound rejected this request body.
    #[error("subscription inference input bound was exceeded")]
    InferenceInputLimit,
    /// Inference class, thread scope, or classified allowance was unavailable.
    #[error("subscription inference reserved {reserved} of allowance {allowance}")]
    InferenceClassLimit {
        /// Reservations already charged, including abandoned permits.
        reserved: u32,
        /// Immutable inference-class allowance.
        allowance: u32,
    },
}

impl From<plakous_auth::api_key::ApiKeyError> for AuthError {
    fn from(error: plakous_auth::api_key::ApiKeyError) -> Self {
        match error {
            plakous_auth::api_key::ApiKeyError::BindingMismatch => Self::BindingMismatch,
            plakous_auth::api_key::ApiKeyError::Unavailable => Self::Unavailable,
            _ => Self::SourceFailure,
        }
    }
}
