//! Gateway refusals.
//!
//! A refusal is the gateway's own answer, never an upstream one: a non-200
//! status, the [`crate::ERROR_HEADER`] naming the code, and a
//! [`RefusalBody`]. Every refusal is decided before any upstream byte is
//! sent, so a caller treats it as a request that was not started. An
//! upstream error status is never a refusal; it arrives as a
//! [`crate::frame::Head`] frame with the upstream bytes unchanged.

use crate::facts::{AccountFacts, CredentialStatus};

/// Why the gateway refused a call before any upstream dispatch.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "code", rename_all = "snake_case", deny_unknown_fields)]
pub enum GatewayRefusal {
    /// No valid caller key. Carries no further detail.
    Unauthenticated,
    /// The caller's scopes do not cover this call and account.
    ScopeDenied,
    /// The path names a protocol version the gateway does not serve.
    VersionUnsupported {
        /// The versions it serves.
        supported: Vec<u16>,
    },
    /// The gateway's kill switch refuses every call.
    GatewayDisabled,
    /// The kill switch refuses this account.
    AccountDisabled,
    /// The gateway holds no such account.
    AccountUnknown,
    /// The credential is not usable.
    CredentialNotReady {
        /// Its state.
        status: CredentialStatus,
    },
    /// The credential's generation differs from the request's fence.
    FenceMoved {
        /// Current facts.
        facts: Box<AccountFacts>,
    },
    /// The credential now belongs to another provider account.
    AccountChanged,
    /// The upstream origin, method or path is not pinned for this credential.
    UpstreamNotPinned,
    /// The request carries a header the caller may not send.
    ReservedHeader,
    /// The custodied entries' names differ from the credential's injection rule.
    CustodiedHeadersMismatch,
    /// The request or call body is malformed.
    EnvelopeInvalid,
    /// The request exceeds a gateway limit.
    LimitExceeded,
    /// The gateway is at capacity; try again after the advice.
    Busy {
        /// Advised wait.
        retry_after_ms: u64,
    },
    /// The gateway failed before dispatch.
    Internal,
}

impl GatewayRefusal {
    /// The code carried by the [`crate::ERROR_HEADER`] and the body's `code`.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unauthenticated => "unauthenticated",
            Self::ScopeDenied => "scope_denied",
            Self::VersionUnsupported { .. } => "version_unsupported",
            Self::GatewayDisabled => "gateway_disabled",
            Self::AccountDisabled => "account_disabled",
            Self::AccountUnknown => "account_unknown",
            Self::CredentialNotReady { .. } => "credential_not_ready",
            Self::FenceMoved { .. } => "fence_moved",
            Self::AccountChanged => "account_changed",
            Self::UpstreamNotPinned => "upstream_not_pinned",
            Self::ReservedHeader => "reserved_header",
            Self::CustodiedHeadersMismatch => "custodied_headers_mismatch",
            Self::EnvelopeInvalid => "envelope_invalid",
            Self::LimitExceeded => "limit_exceeded",
            Self::Busy { .. } => "busy",
            Self::Internal => "internal",
        }
    }

    /// The HTTP status the gateway answers with.
    #[must_use]
    pub const fn http_status(&self) -> u16 {
        match self {
            Self::Unauthenticated => 401,
            Self::ScopeDenied => 403,
            Self::AccountUnknown => 404,
            Self::CredentialNotReady { .. } | Self::FenceMoved { .. } | Self::AccountChanged => 409,
            Self::LimitExceeded => 413,
            Self::UpstreamNotPinned
            | Self::ReservedHeader
            | Self::CustodiedHeadersMismatch
            | Self::EnvelopeInvalid => 422,
            Self::VersionUnsupported { .. } => 426,
            Self::Internal => 500,
            Self::GatewayDisabled | Self::AccountDisabled | Self::Busy { .. } => 503,
        }
    }
}

/// The body of a refusal: `{"gateway_error": {"code": ..., ...}}`.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefusalBody {
    /// The refusal.
    pub gateway_error: GatewayRefusal,
}
