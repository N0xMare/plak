use std::{error::Error, fmt};

use super::NativeProvider;
use plakous_model::ProviderConfigurationError;

/// Redaction-safe failure from the curated subscription facade.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum NativeSubscriptionError {
    /// A served contract-bearing fact disagrees with this route's compiled pins.
    #[cfg(any(
        feature = "codex",
        feature = "grok",
        feature = "claude",
        feature = "auth"
    ))]
    CompatibilityDrift {
        /// Exact subscription route, never an API-key fallback.
        provider: NativeProvider,
        /// Credential-free mismatch; never an adaptive replacement value.
        reason: plakous_auth::CompatibilityDrift,
    },
    /// The exact requested subscription route was not compiled.
    NotCompiled(NativeProvider),
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
    /// A named account writer or pending credential operation is busy.
    AccountBusy,
    /// Bare default accounts can be signed out but cannot be removed.
    DefaultAccountNotRemovable,
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
        /// Provider minimum delay before another whole lookup operation.
        retry_after: Option<std::time::Duration>,
    },
    /// A bounded provider transport operation failed.
    Transport(Option<plakous_model::ModelNetworkFailure>),
    /// Internal application execution or coordination failed.
    Internal,
}

impl fmt::Display for NativeSubscriptionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(any(
            feature = "codex",
            feature = "grok",
            feature = "claude",
            feature = "auth"
        ))]
        if let Self::CompatibilityDrift { provider, reason } = self {
            return formatter.write_str(&reason.message(provider.as_str()));
        }
        if let Self::ModelLookup {
            attempts, cause, ..
        } = self
        {
            let unit = if *attempts == 1 {
                "attempt"
            } else {
                "attempts"
            };
            return write!(
                formatter,
                "model lookup failed after {attempts} {unit}: {cause}"
            );
        }
        if let Self::Transport(Some(cause)) = self {
            return write!(formatter, "native subscription transport failed: {cause}");
        }
        formatter.write_str(self.label())
    }
}

impl NativeSubscriptionError {
    fn label(&self) -> &'static str {
        match self {
            Self::NotCompiled(_) => "native subscription route is not included in this build",
            Self::UnsafeStorage => "native subscription storage is unsafe",
            Self::CredentialProfileChanged => {
                "stored native login was written by an older build or another profile; replace the login"
            }
            Self::CredentialWrittenByNewerBuild => {
                "stored native login was written by a newer build; use that build or replace the login; the writing build must then log in again"
            }
            Self::UnsupportedPlatform => "native subscription storage is unsupported",
            Self::AcceptanceRequired => "exact native compatibility acceptance is required",
            Self::ProfileInactive => "native compatibility profile is inactive",
            Self::InvalidReceipt => "native compatibility receipt is invalid",
            Self::CredentialUnavailable => "native subscription credential is unavailable",
            Self::AccountBusy => "account_busy: the named subscription account is busy",
            Self::DefaultAccountNotRemovable => {
                "default_account_not_removable: sign out the default account instead"
            }
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
            #[cfg(any(
                feature = "codex",
                feature = "grok",
                feature = "claude",
                feature = "auth"
            ))]
            Self::CompatibilityDrift { .. } => unreachable!("handled above"),
        }
    }
}

impl Error for NativeSubscriptionError {}
