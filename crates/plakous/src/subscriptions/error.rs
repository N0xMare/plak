use std::{error::Error, fmt};

use super::NativeAuthStatus;
use plakous_model::ProviderConfigurationError;

/// Redaction-safe failure from the curated subscription facade.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeSubscriptionError {
    /// The explicit state root or fixed descendant layout is unsafe.
    UnsafeStorage,
    /// A structurally valid selected-slot record belongs to another profile revision.
    CredentialProfileChanged,
    /// A newer build wrote the stored login or its compatibility state; this
    /// build neither uses nor rewrites it.
    CredentialWrittenByNewerBuild,
    /// This platform cannot enforce the required private storage contract.
    UnsupportedPlatform,
    /// Compatibility consent is missing or no longer matches exactly.
    AcceptanceRequired,
    /// The pinned compatibility profile is expired, killed, or not yet valid.
    ProfileInactive,
    /// A private receipt was malformed, oversized, or internally inconsistent.
    InvalidReceipt,
    /// The selected credential is absent, quarantined, revoked, or otherwise unavailable.
    CredentialUnavailable,
    /// The provider rejected or invalidated the native authentication operation.
    Authentication,
    /// Credential, route, or account binding evidence was contradictory.
    AuthenticationBinding,
    /// The provider rejected an authenticated native operation.
    AuthenticationProviderRejected,
    /// Locally validated native authentication material was contradictory.
    AuthenticationValidation,
    /// The authenticated account changed across one exact operation.
    AuthenticationAccountChanged,
    /// The authorization code could not be exchanged or its token envelope was invalid.
    LoginTokenExchange,
    /// The authorization-code token request failed before a complete response.
    LoginTokenTransport,
    /// The provider rejected the authorization-code exchange.
    LoginTokenRejected,
    /// A successful token response did not match the provider-owned contract.
    LoginTokenResponseInvalid,
    /// The login signing-key set could not be fetched or validated.
    LoginSigningKeys,
    /// Signed login identity, nonce, account, or `UserInfo` verification failed.
    LoginAccountVerification,
    /// A validated login could not be committed to the selected private slot.
    LoginPersistence,
    /// The browser callback URL or local callback request was invalid.
    InvalidCallback,
    /// The provider does not offer device-code login for this account or server.
    DeviceLoginUnavailable,
    /// The device code expired before the operator approved it.
    DeviceCodeExpired,
    /// The exact requested model was not admitted by fresh account-scoped evidence.
    ModelUnavailable,
    /// The explicit provider configuration was not admitted by exact resolved facts.
    ProviderConfigurationRejected,
    /// One explicit behavior dimension did not match the selected provider policy.
    ProviderConfigurationPolicyMismatch(&'static str),
    /// Canonical provider-policy identity construction failed.
    ProviderConfigurationIdentityRejected,
    /// Neutral selected-to-effective compilation rejected resolved provider facts.
    ProviderConfigurationCompileRejected(ProviderConfigurationError),
    /// Fresh executable authority failed immediate exact revalidation.
    ProviderAuthorityRevalidationRejected,
    /// A fresh resolution of a running model changed its login, account,
    /// credential authority, route or model semantics.
    ReresolutionBindingChanged,
    /// The executable model and compiled configuration did not form one exact aggregate.
    ConfiguredAggregateRejected,
    /// The pinned native provider client could not be constructed from admitted authority.
    ProviderClientConstructionRejected,
    /// The authenticated provider catalog failed its exact validation contract.
    CatalogValidationRejected,
    /// Selected-model resolution evidence failed its exact validation contract.
    ResolutionEvidenceRejected,
    /// Stored credential payload could not be decoded for catalog use.
    CatalogCredentialRejected,
    /// The authorization header could not be constructed for catalog use.
    CatalogAuthorizationHeaderRejected,
    /// The account header could not be constructed for catalog use.
    CatalogAccountHeaderRejected,
    /// Pre-catalog credential refresh failed strict local validation.
    CatalogRefreshRejected,
    /// The catalog response did not use the pinned JSON media type.
    CatalogMediaTypeRejected,
    /// The catalog response did not match the pinned JSON document schema.
    CatalogDocumentRejected,
    /// A catalog model identifier was outside the pinned grammar.
    CatalogIdentifierRejected,
    /// Transient model lookup failures exhausted the bounded retry budget.
    ModelLookup {
        /// Physical attempts that ran before exhaustion.
        attempts: u8,
        /// Redacted transport or provider cause.
        cause: &'static str,
    },
    /// A bounded provider transport operation failed.
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// Internal application execution or coordination failed.
    Internal,
}

impl fmt::Display for NativeSubscriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Self::ModelLookup { attempts, cause } = self {
            return write!(
                formatter,
                "model lookup failed after {attempts} attempts: {cause}"
            );
        }
        if let Self::Transport(Some(cause)) = self {
            return write!(formatter, "native subscription transport failed: {cause}");
        }
        formatter.write_str(match self {
            Self::UnsafeStorage => "native subscription storage is unsafe",
            Self::CredentialProfileChanged => {
                "stored native login was written by an older build or another profile; run `plak login <account>`, which replaces it"
            }
            Self::CredentialWrittenByNewerBuild => {
                "stored native login was written by a newer plak build; use that build, or replace it with `plak login <account>` (that build must then log in again)"
            }
            Self::UnsupportedPlatform => "native subscription storage is unsupported",
            Self::AcceptanceRequired => "exact native compatibility acceptance is required",
            Self::ProfileInactive => "native compatibility profile is inactive",
            Self::InvalidReceipt => "native compatibility receipt is invalid",
            Self::CredentialUnavailable => "native subscription credential is unavailable",
            Self::Authentication => "native subscription authentication failed",
            Self::AuthenticationBinding => "native subscription authentication binding failed",
            Self::AuthenticationProviderRejected => {
                "native subscription provider rejected authentication"
            }
            Self::AuthenticationValidation => {
                "native subscription authentication validation failed"
            }
            Self::AuthenticationAccountChanged => {
                "native subscription authentication account changed"
            }
            Self::LoginTokenExchange => "native subscription token exchange failed",
            Self::LoginTokenTransport => "native subscription token request transport failed",
            Self::LoginTokenRejected => "native subscription token endpoint rejected the exchange",
            Self::LoginTokenResponseInvalid => "native subscription token response was invalid",
            Self::LoginSigningKeys => "native subscription signing-key verification failed",
            Self::LoginAccountVerification => "native subscription account verification failed",
            Self::LoginPersistence => "native subscription credential persistence failed",
            Self::InvalidCallback => "native subscription browser callback was rejected",
            Self::DeviceLoginUnavailable => {
                "device-code login is not available for this account/server"
            }
            Self::DeviceCodeExpired => "device code expired before it was approved",
            Self::ModelUnavailable => "requested native subscription model is unavailable",
            Self::ProviderConfigurationRejected => {
                "explicit native provider configuration was rejected"
            }
            Self::ProviderConfigurationPolicyMismatch(_) => {
                "explicit native provider behavior did not match provider policy"
            }
            Self::ProviderConfigurationIdentityRejected => {
                "native provider policy identity construction failed"
            }
            Self::ProviderConfigurationCompileRejected(_) => {
                "resolved native provider facts rejected explicit configuration"
            }
            Self::ProviderAuthorityRevalidationRejected => {
                "native provider authority failed immediate revalidation"
            }
            Self::ReresolutionBindingChanged => {
                "a fresh resolution changed the selected login, account, credential authority, route or model semantics"
            }
            Self::ConfiguredAggregateRejected => {
                "native executable and compiled configuration did not match"
            }
            Self::ProviderClientConstructionRejected => {
                "native provider client construction was rejected"
            }
            Self::CatalogValidationRejected => {
                "authenticated native model catalog validation failed"
            }
            Self::ResolutionEvidenceRejected => {
                "native selected-model resolution evidence was rejected"
            }
            Self::CatalogCredentialRejected => "native catalog credential payload was rejected",
            Self::CatalogAuthorizationHeaderRejected => {
                "native catalog authorization header was rejected"
            }
            Self::CatalogAccountHeaderRejected => "native catalog account header was rejected",
            Self::CatalogRefreshRejected => "native catalog credential refresh was rejected",
            Self::CatalogMediaTypeRejected => "native catalog media type was rejected",
            Self::CatalogDocumentRejected => "native catalog document was rejected",
            Self::CatalogIdentifierRejected => "native catalog model identifier was rejected",
            Self::ModelLookup { .. } => unreachable!("handled above"),
            Self::Transport(_) => "native subscription transport failed",
            Self::Internal => "native subscription operation failed internally",
        })
    }
}

impl Error for NativeSubscriptionError {}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_codex_status(status: plakous_openai::CodexCredentialStatus) -> NativeAuthStatus {
    match status {
        plakous_openai::CodexCredentialStatus::SignedOut => NativeAuthStatus::SignedOut,
        plakous_openai::CodexCredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_openai::CodexCredentialStatus::RefreshRequired => NativeAuthStatus::RefreshRequired,
        plakous_openai::CodexCredentialStatus::Quarantined => NativeAuthStatus::Quarantined,
        plakous_openai::CodexCredentialStatus::Refreshing => NativeAuthStatus::Refreshing,
        plakous_openai::CodexCredentialStatus::Revoked => NativeAuthStatus::Revoked,
        plakous_openai::CodexCredentialStatus::LoginRequired => NativeAuthStatus::LoginRequired,
        plakous_openai::CodexCredentialStatus::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_credential_status(status: plakous_auth::CredentialStatus) -> NativeAuthStatus {
    match status {
        plakous_auth::CredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_auth::CredentialStatus::RefreshRequired => NativeAuthStatus::RefreshRequired,
        plakous_auth::CredentialStatus::RefreshOutcomeUnknown => NativeAuthStatus::Quarantined,
        plakous_auth::CredentialStatus::Revoked => NativeAuthStatus::Revoked,
        _ => NativeAuthStatus::Quarantined,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_xai_offline_status(
    status: plakous_xai::XaiOfflineCredentialStatus,
) -> NativeAuthStatus {
    match status {
        plakous_xai::XaiOfflineCredentialStatus::SignedOut => NativeAuthStatus::SignedOut,
        plakous_xai::XaiOfflineCredentialStatus::Ready => NativeAuthStatus::Ready,
        plakous_xai::XaiOfflineCredentialStatus::RefreshRequired => {
            NativeAuthStatus::RefreshRequired
        }
        plakous_xai::XaiOfflineCredentialStatus::Quarantined => NativeAuthStatus::Quarantined,
        plakous_xai::XaiOfflineCredentialStatus::Refreshing => NativeAuthStatus::Refreshing,
        plakous_xai::XaiOfflineCredentialStatus::Revoked => NativeAuthStatus::Revoked,
        plakous_xai::XaiOfflineCredentialStatus::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_codex_error(
    error: plakous_openai::NativeCodexAuthError,
) -> NativeSubscriptionError {
    match error {
        plakous_openai::NativeCodexAuthError::ProfileNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_openai::NativeCodexAuthError::ProfileNotCurrent => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_openai::NativeCodexAuthError::Store => NativeSubscriptionError::UnsafeStorage,
        plakous_openai::NativeCodexAuthError::WrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        // A login stored by an earlier build (no selection epoch) needs a new login.
        plakous_openai::NativeCodexAuthError::Unavailable
        | plakous_openai::NativeCodexAuthError::LoginRequired => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_openai::NativeCodexAuthError::Catalog => NativeSubscriptionError::ModelUnavailable,
        plakous_openai::NativeCodexAuthError::CatalogCredential => {
            NativeSubscriptionError::CatalogCredentialRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogAuthorizationHeader => {
            NativeSubscriptionError::CatalogAuthorizationHeaderRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogAccountHeader => {
            NativeSubscriptionError::CatalogAccountHeaderRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogRefresh => {
            NativeSubscriptionError::CatalogRefreshRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogMediaType => {
            NativeSubscriptionError::CatalogMediaTypeRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogDocument => {
            NativeSubscriptionError::CatalogDocumentRejected
        }
        plakous_openai::NativeCodexAuthError::CatalogIdentifier => {
            NativeSubscriptionError::CatalogIdentifierRejected
        }
        plakous_openai::NativeCodexAuthError::LoginTokenTransport => {
            NativeSubscriptionError::LoginTokenTransport
        }
        plakous_openai::NativeCodexAuthError::LoginTokenRejected => {
            NativeSubscriptionError::LoginTokenRejected
        }
        plakous_openai::NativeCodexAuthError::LoginTokenResponseInvalid => {
            NativeSubscriptionError::LoginTokenResponseInvalid
        }
        plakous_openai::NativeCodexAuthError::LoginSigningKeys => {
            NativeSubscriptionError::LoginSigningKeys
        }
        plakous_openai::NativeCodexAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_openai::NativeCodexAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_openai::NativeCodexAuthError::Lookup(error) => error
            .exhaustion()
            .map_or(NativeSubscriptionError::Internal, |(attempts, cause)| {
                NativeSubscriptionError::ModelLookup { attempts, cause }
            }),
        plakous_openai::NativeCodexAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(cause)
        }
        plakous_openai::NativeCodexAuthError::Coordination => NativeSubscriptionError::Internal,
        plakous_openai::NativeCodexAuthError::Callback => NativeSubscriptionError::InvalidCallback,
        plakous_openai::NativeCodexAuthError::DeviceLoginUnavailable => {
            NativeSubscriptionError::DeviceLoginUnavailable
        }
        plakous_openai::NativeCodexAuthError::DeviceCodeExpired => {
            NativeSubscriptionError::DeviceCodeExpired
        }
        plakous_openai::NativeCodexAuthError::Binding => {
            NativeSubscriptionError::AuthenticationBinding
        }
        plakous_openai::NativeCodexAuthError::Provider => {
            NativeSubscriptionError::AuthenticationProviderRejected
        }
        plakous_openai::NativeCodexAuthError::Validation => {
            NativeSubscriptionError::AuthenticationValidation
        }
        plakous_openai::NativeCodexAuthError::AccountChanged
        | plakous_openai::NativeCodexAuthError::SelectionChanged => {
            NativeSubscriptionError::AuthenticationAccountChanged
        }
        _ => NativeSubscriptionError::Internal,
    }
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_xai_error(error: plakous_xai::XaiNativeAuthError) -> NativeSubscriptionError {
    let mapped = match &error {
        plakous_xai::XaiNativeAuthError::CompatibilityNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_xai::XaiNativeAuthError::CompatibilityInactive => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_xai::XaiNativeAuthError::Store(plakous_auth::PrivateStoreError::ForeignProfile)
        | plakous_xai::XaiNativeAuthError::StoredCredentialFormatRetired => {
            NativeSubscriptionError::CredentialProfileChanged
        }
        plakous_xai::XaiNativeAuthError::Store(
            plakous_auth::PrivateStoreError::WrittenByNewerBuild,
        )
        | plakous_xai::XaiNativeAuthError::StoredCredentialWrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        plakous_xai::XaiNativeAuthError::Store(_) => NativeSubscriptionError::UnsafeStorage,
        plakous_xai::XaiNativeAuthError::NotLoggedIn
        | plakous_xai::XaiNativeAuthError::NotReady
        | plakous_xai::XaiNativeAuthError::RefreshQuarantined => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_xai::XaiNativeAuthError::CatalogUnavailable
        | plakous_xai::XaiNativeAuthError::RequestedModelNotAdmitted
        | plakous_xai::XaiNativeAuthError::Model(_)
        | plakous_xai::XaiNativeAuthError::LanguageModel(_) => {
            NativeSubscriptionError::ModelUnavailable
        }
        plakous_xai::XaiNativeAuthError::Request
        | plakous_xai::XaiNativeAuthError::HttpRequest(_) => {
            NativeSubscriptionError::Transport(None)
        }
        plakous_xai::XaiNativeAuthError::Lookup(error) => error
            .exhaustion()
            .map_or(NativeSubscriptionError::Internal, |(attempts, cause)| {
                NativeSubscriptionError::ModelLookup { attempts, cause }
            }),
        plakous_xai::XaiNativeAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(*cause)
        }
        plakous_xai::XaiNativeAuthError::LoginTokenExchange => {
            NativeSubscriptionError::LoginTokenExchange
        }
        plakous_xai::XaiNativeAuthError::LoginSigningKeys => {
            NativeSubscriptionError::LoginSigningKeys
        }
        plakous_xai::XaiNativeAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_xai::XaiNativeAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_xai::XaiNativeAuthError::InvalidCallback => {
            NativeSubscriptionError::InvalidCallback
        }
        plakous_xai::XaiNativeAuthError::LoginExpired => NativeSubscriptionError::InvalidCallback,
        plakous_xai::XaiNativeAuthError::RandomnessUnavailable
        | plakous_xai::XaiNativeAuthError::LoopbackUnavailable
        | plakous_xai::XaiNativeAuthError::AuthorizationDenied
        | plakous_xai::XaiNativeAuthError::InvalidTokenResponse(_)
        | plakous_xai::XaiNativeAuthError::InvalidIdToken(_)
        | plakous_xai::XaiNativeAuthError::InvalidJwks
        | plakous_xai::XaiNativeAuthError::UnknownSigningKey
        | plakous_xai::XaiNativeAuthError::AccountChanged
        | plakous_xai::XaiNativeAuthError::SelectionChanged
        | plakous_xai::XaiNativeAuthError::InvalidStoredCredential
        | plakous_xai::XaiNativeAuthError::HttpStatus(_) => NativeSubscriptionError::Authentication,
        plakous_xai::XaiNativeAuthError::Binding(_)
        | plakous_xai::XaiNativeAuthError::CredentialValue(_)
        | plakous_xai::XaiNativeAuthError::Route(_)
        | plakous_xai::XaiNativeAuthError::Identity(_) => NativeSubscriptionError::Internal,
        _ => NativeSubscriptionError::Internal,
    };
    drop(error);
    mapped
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive provider failures map to one redacted internal category"
)]
pub(super) fn map_claude_error(
    error: plakous_anthropic::ClaudeNativeAuthError,
) -> NativeSubscriptionError {
    let mapped = match &error {
        plakous_anthropic::ClaudeNativeAuthError::CompatibilityNotAccepted => {
            NativeSubscriptionError::AcceptanceRequired
        }
        plakous_anthropic::ClaudeNativeAuthError::CompatibilityInactive => {
            NativeSubscriptionError::ProfileInactive
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(
            plakous_auth::PrivateStoreError::ForeignProfile,
        )
        | plakous_anthropic::ClaudeNativeAuthError::StoredCredentialFormatRetired => {
            NativeSubscriptionError::CredentialProfileChanged
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(
            plakous_auth::PrivateStoreError::WrittenByNewerBuild,
        )
        | plakous_anthropic::ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild => {
            NativeSubscriptionError::CredentialWrittenByNewerBuild
        }
        plakous_anthropic::ClaudeNativeAuthError::Store(_) => {
            NativeSubscriptionError::UnsafeStorage
        }
        plakous_anthropic::ClaudeNativeAuthError::NotLoggedIn
        | plakous_anthropic::ClaudeNativeAuthError::NotReady
        | plakous_anthropic::ClaudeNativeAuthError::RefreshQuarantined => {
            NativeSubscriptionError::CredentialUnavailable
        }
        plakous_anthropic::ClaudeNativeAuthError::Transport(cause) => {
            NativeSubscriptionError::Transport(*cause)
        }
        plakous_anthropic::ClaudeNativeAuthError::HttpRequest(_) => {
            NativeSubscriptionError::Transport(None)
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginTokenExchange => {
            NativeSubscriptionError::LoginTokenExchange
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginAccountVerification => {
            NativeSubscriptionError::LoginAccountVerification
        }
        plakous_anthropic::ClaudeNativeAuthError::LoginPersistence => {
            NativeSubscriptionError::LoginPersistence
        }
        plakous_anthropic::ClaudeNativeAuthError::InvalidCallback
        | plakous_anthropic::ClaudeNativeAuthError::LoginExpired => {
            NativeSubscriptionError::InvalidCallback
        }
        plakous_anthropic::ClaudeNativeAuthError::AccountIdentityUnavailable
        | plakous_anthropic::ClaudeNativeAuthError::InvalidTokenResponse
        | plakous_anthropic::ClaudeNativeAuthError::InvalidStoredCredential
        | plakous_anthropic::ClaudeNativeAuthError::Binding(_)
        | plakous_anthropic::ClaudeNativeAuthError::CredentialValue(_) => {
            NativeSubscriptionError::Authentication
        }
        _ => NativeSubscriptionError::Internal,
    };
    drop(error);
    mapped
}

#[allow(
    clippy::match_same_arms,
    reason = "unknown non-exhaustive lifecycle states fail closed as quarantined"
)]
pub(super) fn map_claude_offline_status(
    status: plakous_auth::OfflineCredentialLifecycle,
) -> NativeAuthStatus {
    match status {
        plakous_auth::OfflineCredentialLifecycle::SignedOut => NativeAuthStatus::SignedOut,
        plakous_auth::OfflineCredentialLifecycle::Ready => NativeAuthStatus::Ready,
        plakous_auth::OfflineCredentialLifecycle::RefreshRequired => {
            NativeAuthStatus::RefreshRequired
        }
        plakous_auth::OfflineCredentialLifecycle::Quarantined => NativeAuthStatus::Quarantined,
        plakous_auth::OfflineCredentialLifecycle::Refreshing => NativeAuthStatus::Refreshing,
        plakous_auth::OfflineCredentialLifecycle::Revoked => NativeAuthStatus::Revoked,
        plakous_auth::OfflineCredentialLifecycle::LoginRequired => NativeAuthStatus::LoginRequired,
        plakous_auth::OfflineCredentialLifecycle::WrittenByNewerBuild => {
            NativeAuthStatus::WrittenByNewerBuild
        }
        _ => NativeAuthStatus::Quarantined,
    }
}
