//! The compiled compatibility profile and its acceptance.

use super::{
    NativeCodexAuthError, OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, OPENAI_CODEX_KILL_SWITCH_REVISION,
    OPENAI_CODEX_SOURCE_REVISION, OfflineCredentialLifecycle, PROFILE_EXPIRES_AT, PROFILE_KILLED,
    PROFILE_REVIEWED_AT, Timestamp, fmt,
};

/// Non-coercible provenance for this integration profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum SubscriptionIntegrationProvenance {
    /// Observed compatibility, not provider support or endorsement.
    PinnedCompatibility,
}

/// Credential-free immutable facts for the built-in native profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodexSubscriptionIntegrationProfile;

impl CodexSubscriptionIntegrationProfile {
    /// Returns the immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
    }

    /// Returns the exact official source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        OPENAI_CODEX_SOURCE_REVISION
    }

    /// Returns the exact compatibility notice revision an operator must accept
    /// before any online authentication or inference admission.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION
    }

    /// Returns compatibility provenance; this is not provider authorization.
    #[must_use]
    pub const fn provenance(self) -> SubscriptionIntegrationProvenance {
        SubscriptionIntegrationProvenance::PinnedCompatibility
    }

    /// Returns the review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        PROFILE_REVIEWED_AT
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        PROFILE_EXPIRES_AT
    }

    /// Returns the local kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        OPENAI_CODEX_KILL_SWITCH_REVISION
    }

    /// Reports the compiled kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        PROFILE_KILLED
    }
}

/// Exact operator acceptance of the experimental compatibility notice.
pub struct CodexCompatibilityAcceptance {
    pub(super) accepted_at: u64,
}

impl CodexCompatibilityAcceptance {
    /// Accepts only the exact immutable profile, source, and notice revisions.
    ///
    /// # Errors
    ///
    /// Rejects any drift instead of silently accepting a later profile.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        notice_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, NativeCodexAuthError> {
        if profile_id != OPENAI_CODEX_COMPATIBILITY_PROFILE_ID
            || source_revision != OPENAI_CODEX_SOURCE_REVISION
            || notice_revision != OPENAI_CODEX_COMPATIBILITY_NOTICE_REVISION
        {
            return Err(NativeCodexAuthError::ProfileNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for CodexCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CodexCompatibilityAcceptance(<exact notice accepted>)")
    }
}

/// Credential-free lifecycle for the selected Codex slot (online + offline admin).
pub type CodexCredentialStatus = OfflineCredentialLifecycle;
