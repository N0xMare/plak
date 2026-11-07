//! The compiled compatibility profile, its acceptance and subscription status.

use super::{
    AccountBinding, COMPATIBILITY_EXPIRES_UNIX, COMPATIBILITY_KILLED, COMPATIBILITY_REVIEWED_UNIX,
    CredentialGeneration, CredentialSlotId, CredentialStatus, OfflineCredentialLifecycle, PathBuf,
    PrivateCredentialStore, PrivateStoreError, ReasoningEffort, Timestamp,
    XAI_GROK_BUILD_SOURCE_REVISION, XAI_GROK_COMPATIBILITY_NOTICE_REVISION,
    XAI_GROK_COMPATIBILITY_PROFILE, XAI_GROK_KILL_SWITCH_REVISION, XAI_OPENCODE_SOURCE_REVISION,
    XaiNativeAuthError, auth_profile_id, fmt, grok_enrollment, refuse_retired_payload,
};

/// Provenance of the fixed compatibility facts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum XaiCompatibilityProvenance {
    /// Facts were reviewed in immutable official and upstream source revisions.
    PinnedCompatibility,
}

/// Credential-free immutable facts for the built-in `SuperGrok` integration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XaiSubscriptionIntegrationProfile;

impl XaiSubscriptionIntegrationProfile {
    /// Returns the exact immutable profile identity.
    #[must_use]
    pub const fn id(self) -> &'static str {
        XAI_GROK_COMPATIBILITY_PROFILE
    }

    /// Returns the immutable public-client compatibility source revision.
    #[must_use]
    pub const fn source_revision(self) -> &'static str {
        XAI_OPENCODE_SOURCE_REVISION
    }

    /// Returns the immutable official Grok Build source revision.
    #[must_use]
    pub const fn official_model_source_revision(self) -> &'static str {
        XAI_GROK_BUILD_SOURCE_REVISION
    }

    /// Returns observed compatibility provenance, not provider endorsement.
    #[must_use]
    pub const fn provenance(self) -> XaiCompatibilityProvenance {
        XaiCompatibilityProvenance::PinnedCompatibility
    }

    /// Returns the UTC review timestamp.
    #[must_use]
    pub const fn reviewed_at(self) -> u64 {
        COMPATIBILITY_REVIEWED_UNIX
    }

    /// Returns the hard compatibility expiry.
    #[must_use]
    pub const fn expires_at(self) -> u64 {
        COMPATIBILITY_EXPIRES_UNIX
    }

    /// Returns the exact operator notice revision.
    #[must_use]
    pub const fn notice_revision(self) -> &'static str {
        XAI_GROK_COMPATIBILITY_NOTICE_REVISION
    }

    /// Returns the locally reviewable kill-switch revision.
    #[must_use]
    pub const fn kill_switch_revision(self) -> &'static str {
        XAI_GROK_KILL_SWITCH_REVISION
    }

    /// Reports the compiled local kill-switch state.
    #[must_use]
    pub const fn is_killed(self) -> bool {
        COMPATIBILITY_KILLED
    }
}

/// A deliberate exact-revision acceptance capability.
pub struct XaiCompatibilityAcceptance {
    pub(super) accepted_at: u64,
}

impl XaiCompatibilityAcceptance {
    /// Accepts exactly every immutable profile, source, notice, and kill revision.
    ///
    /// # Errors
    ///
    /// Rejects any alias, omitted revision, older revision, or future revision.
    /// The acceptance time is checked against the clock when the acceptance
    /// is used, never against the review date.
    pub fn accept_exact(
        profile_id: &str,
        source_revision: &str,
        official_model_source_revision: &str,
        notice_revision: &str,
        kill_switch_revision: &str,
        accepted_at: Timestamp,
    ) -> Result<Self, XaiNativeAuthError> {
        if profile_id != XAI_GROK_COMPATIBILITY_PROFILE
            || source_revision != XAI_OPENCODE_SOURCE_REVISION
            || official_model_source_revision != XAI_GROK_BUILD_SOURCE_REVISION
            || notice_revision != XAI_GROK_COMPATIBILITY_NOTICE_REVISION
            || kill_switch_revision != XAI_GROK_KILL_SWITCH_REVISION
        {
            return Err(XaiNativeAuthError::CompatibilityNotAccepted);
        }
        Ok(Self {
            accepted_at: accepted_at.seconds(),
        })
    }
}

impl fmt::Debug for XaiCompatibilityAcceptance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiCompatibilityAcceptance(<exact pinned revision>)")
    }
}

/// Credential-free selected-slot state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct XaiSubscriptionStatus {
    pub(super) generation: CredentialGeneration,
    pub(super) status: CredentialStatus,
    pub(super) account: AccountBinding,
}

impl XaiSubscriptionStatus {
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

    /// Returns the redacted provider-derived account binding.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account
    }
}

/// Credential-free lifecycle for offline SuperGrok selected-slot maintenance.
pub type XaiOfflineCredentialStatus = OfflineCredentialLifecycle;

/// Offline maintenance handle for one explicit native xAI slot.
///
/// This handle owns no HTTP client, accepts no compatibility capability, and
/// never exposes account, generation, access-token, or refresh-token data. It
/// remains available after profile expiry or local kill-switch activation.
///
/// ```compile_fail
/// # use plakous_xai::XaiOfflineSelectedSlot;
/// fn expose(slot: &XaiOfflineSelectedSlot) {
///     let _ = slot.access_token();
/// }
/// ```
pub struct XaiOfflineSelectedSlot {
    pub(super) store: PrivateCredentialStore,
}

impl XaiOfflineSelectedSlot {
    /// Reads an authenticated upstream-account digest without exposing identity or credentials.
    /// Missing, non-Ready or busy evidence is unknown, not another account.
    ///
    /// # Errors
    /// Refuses malformed, foreign or unsafe selected-slot records.
    pub fn upstream_account_fingerprint(
        &self,
    ) -> Result<Option<plakous_model::Digest>, XaiNativeAuthError> {
        let Some(fence) = self.store.load()? else {
            return Ok(None);
        };
        refuse_retired_payload(&self.store, &fence)?;
        if fence.status() != CredentialStatus::Ready {
            return Ok(None);
        }
        let Some(lease) = self.store.try_acquire_ready(&fence)? else {
            return Ok(None);
        };
        let stored: super::StoredTokens =
            serde_json::from_slice(lease.payload().expose_secret())
                .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)?;
        stored.validate(fence.account_binding())?;
        plakous_model::Digest::sha256(
            plakous_model::DigestDomain::ModelBinding,
            1,
            &[
                b"plak.native-upstream-account.v1",
                b"grok",
                stored.subject.as_bytes(),
            ],
        )
        .map(Some)
        .map_err(|_| XaiNativeAuthError::InvalidStoredCredential)
    }

    /// Opens exactly the application-supplied private slot without network or
    /// current-profile admission.
    ///
    /// # Errors
    ///
    /// Rejects an unsafe path, slot, or foreign/malformed stored record.
    pub fn open_explicit(
        path: impl Into<PathBuf>,
        slot: CredentialSlotId,
    ) -> Result<Self, XaiNativeAuthError> {
        Ok(Self {
            store: PrivateCredentialStore::open_slot(path, auth_profile_id()?, slot)?,
        })
    }

    /// Returns only the credential-free local lifecycle state.
    ///
    /// # Errors
    ///
    /// Fails closed on unsafe or malformed explicit storage, and refuses a
    /// Ready record whose payload format is retired exactly as the online
    /// manager does, so offline status never reads it as usable.
    pub fn status(&self) -> Result<XaiOfflineCredentialStatus, XaiNativeAuthError> {
        let status = (|| {
            if let Some(fence) = self.store.load()? {
                refuse_retired_payload(&self.store, &fence)?;
            }
            Ok(self.store.offline_lifecycle()?)
        })();
        match status {
            Err(
                XaiNativeAuthError::StoredCredentialWrittenByNewerBuild
                | XaiNativeAuthError::Store(PrivateStoreError::WrittenByNewerBuild),
            ) => Ok(OfflineCredentialLifecycle::WrittenByNewerBuild),
            status => status,
        }
    }

    /// Writes an always-available local revoked tombstone without network I/O.
    ///
    /// This operation deliberately does not check acceptance, expiry, or the
    /// compiled kill switch. An absent slot remains signed out.
    ///
    /// # Errors
    ///
    /// Rejects a stale mutation or unsafe/malformed explicit storage.
    pub fn logout_local(&self) -> Result<XaiOfflineCredentialStatus, XaiNativeAuthError> {
        Ok(self.store.logout_local()?)
    }
}

impl fmt::Debug for XaiOfflineSelectedSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiOfflineSelectedSlot(<explicit credential-free maintenance>)")
    }
}

/// Returns whether a SuperGrok catalog ID has complete release enrollment.
///
/// Catalog observation alone is insufficient. In particular,
/// `grok-build-0.1` has a recognized upstream row but no immutable effective
/// context threshold, so it remains unavailable.
#[must_use]
pub fn is_subscription_coding_model_id(model_id: &str) -> bool {
    grok_enrollment(model_id).is_some()
}

/// Returns whether a SuperGrok coding model is treated as effort-capable.
///
/// Only exact reviewed Grok 4.5/4.6/4.7 evidence admits reasoning effort.
#[must_use]
pub fn subscription_model_supports_reasoning(model_id: &str) -> bool {
    !subscription_model_reasoning_effort_options(model_id).is_empty()
}

/// Returns exact reviewed portable effort facts for a SuperGrok catalog ID.
///
/// Only literal Grok 4.5/4.6/4.7 identities have reviewed effort-option evidence.
/// Other exact IDs fail closed with no options; aliases and string heuristics
/// are not accepted.
#[must_use]
pub fn subscription_model_reasoning_effort_options(model_id: &str) -> &'static [ReasoningEffort] {
    grok_enrollment(model_id).map_or(&[], |row| row.efforts)
}
