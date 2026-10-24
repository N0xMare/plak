//! The compiled compatibility profile, its acceptance and subscription status.

use super::{
    CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID, CLAUDE_OAUTH_EXPIRES_AT_UNIX,
    CLAUDE_OAUTH_KILL_SWITCH_REVISION, CLAUDE_OAUTH_NOTICE_REVISION, CLAUDE_OAUTH_REVIEWED_AT_UNIX,
    CLAUDE_OAUTH_SOURCE_REVISION, ClaudeNativeAuthError, CredentialGeneration, CredentialSlotId,
    CredentialStatus, OfflineCredentialLifecycle, PathBuf, PrivateCredentialStore,
    PrivateStoreError, Timestamp, auth_profile_id, fmt, validate_current_ready_record,
};

/// Credential-free immutable facts for the built-in Claude OAuth integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeSubscriptionIntegrationProfile;

impl ClaudeSubscriptionIntegrationProfile {
    /// Returns the exact immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
    }

    /// Returns the immutable source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        CLAUDE_OAUTH_SOURCE_REVISION
    }

    /// Returns the exact operator notice revision.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        CLAUDE_OAUTH_NOTICE_REVISION
    }

    /// Returns the review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        CLAUDE_OAUTH_REVIEWED_AT_UNIX
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        CLAUDE_OAUTH_EXPIRES_AT_UNIX
    }

    /// Returns the locally reviewable kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        CLAUDE_OAUTH_KILL_SWITCH_REVISION
    }

    /// Reports the compiled kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        false
    }
}

/// A deliberate exact-revision acceptance capability.
pub struct ClaudeCompatibilityAcceptance {
    pub(super) accepted_at: u64,
}

impl ClaudeCompatibilityAcceptance {
    /// Accepts exactly every immutable profile, source, notice, and kill revision.
    ///
    /// # Errors
    ///
    /// Rejects any alias or omitted revision. The acceptance time is checked
    /// against the clock when the acceptance is used, never against the
    /// review date.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        notice_revision: &str,
        kill_switch_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, ClaudeNativeAuthError> {
        if profile_id != CLAUDE_OAUTH_COMPATIBILITY_PROFILE_ID
            || source_revision != CLAUDE_OAUTH_SOURCE_REVISION
            || notice_revision != CLAUDE_OAUTH_NOTICE_REVISION
            || kill_switch_revision != CLAUDE_OAUTH_KILL_SWITCH_REVISION
        {
            return Err(ClaudeNativeAuthError::CompatibilityNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for ClaudeCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeCompatibilityAcceptance(<exact pinned revision>)")
    }
}

/// Credential-free selected-slot state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeSubscriptionStatus {
    pub(super) generation: CredentialGeneration,
    pub(super) status: CredentialStatus,
}

impl ClaudeSubscriptionStatus {
    /// Returns the durable generation.
    #[must_use]
    pub const fn generation(&self) -> CredentialGeneration {
        self.generation
    }

    /// Returns the credential-free lifecycle state.
    #[must_use]
    pub const fn status(&self) -> CredentialStatus {
        self.status
    }
}

/// One enrolled Claude OAuth coding model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClaudeEnrolledModel {
    pub(super) exact_id: &'static str,
    pub(super) input_tokens: u64,
}

impl ClaudeEnrolledModel {
    /// Exact catalog model id.
    #[must_use]
    pub const fn exact_id(self) -> &'static str {
        self.exact_id
    }

    /// Offline reference-client catalog value for presentation only.
    ///
    /// This value is not account entitlement or context-admission evidence.
    #[must_use]
    pub const fn input_tokens(self) -> u64 {
        self.input_tokens
    }
}

/// Offline maintenance handle for one explicit Claude selected slot.
pub struct ClaudeOfflineSelectedSlot {
    pub(super) store: PrivateCredentialStore,
}

impl ClaudeOfflineSelectedSlot {
    /// Opens exactly the application-supplied private slot without network.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe path, slot, or foreign/malformed stored record.
    pub fn open_explicit(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
    ) -> Result<Self, ClaudeNativeAuthError> {
        Ok(Self {
            store: PrivateCredentialStore::open_slot(path, auth_profile_id()?, slot)?,
        })
    }

    /// Returns only the credential-free local lifecycle state.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed explicit storage.
    pub fn status(&self) -> Result<OfflineCredentialLifecycle, ClaudeNativeAuthError> {
        let status = validate_current_ready_record(&self.store)
            .and_then(|()| Ok(self.store.offline_lifecycle()?));
        match status {
            Err(
                ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild
                | ClaudeNativeAuthError::Store(PrivateStoreError::WrittenByNewerBuild),
            ) => Ok(OfflineCredentialLifecycle::WrittenByNewerBuild),
            status => status,
        }
    }

    /// Writes a local revoked tombstone without network I/O.
    ///
    /// # Errors
    ///
    /// Rejects a stale mutation or unsafe/malformed explicit storage.
    pub fn logout_local(&self) -> Result<OfflineCredentialLifecycle, ClaudeNativeAuthError> {
        Ok(self.store.logout_local()?)
    }
}

impl fmt::Debug for ClaudeOfflineSelectedSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ClaudeOfflineSelectedSlot(<explicit credential-free maintenance>)")
    }
}
