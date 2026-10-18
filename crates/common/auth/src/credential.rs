//! Credential-free identity and lifecycle metadata.

use std::fmt;

use thiserror::Error;

use crate::SecretBytes;

/// Maximum length of a subscription-integration profile identifier in bytes.
pub const MAX_PROFILE_ID_BYTES: usize = 128;
/// Maximum length of an opaque account binding in bytes.
pub const MAX_ACCOUNT_BINDING_BYTES: usize = 256;
/// Maximum length of a selected-account slot identifier in bytes.
pub const MAX_CREDENTIAL_SLOT_ID_BYTES: usize = 128;
/// Maximum decoded provider payload size in bytes.
pub const MAX_CREDENTIAL_PAYLOAD_BYTES: usize = 256 * 1024;

/// A validated, opaque subscription-integration profile identifier.
///
/// This is route/auth compatibility provenance, not a model-facing harness
/// profile identity or a model-adapter admission identity.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SubscriptionIntegrationProfileId(String);

impl SubscriptionIntegrationProfileId {
    /// Validates a subscription-integration profile identifier.
    ///
    /// Values are deliberately restricted to a bounded ASCII token so they
    /// are safe to compare and cannot smuggle control or path characters.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialValueError::InvalidSubscriptionIntegrationProfileId`] for an empty,
    /// oversized, or non-token value.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialValueError> {
        let value = value.into();
        if !valid_opaque_token(&value, MAX_PROFILE_ID_BYTES) {
            return Err(CredentialValueError::InvalidSubscriptionIntegrationProfileId);
        }
        Ok(Self(value))
    }

    /// Returns the validated identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Orders this stored profile against the one compiled into this build.
    ///
    /// Compatibility profiles are `<vendor>.<product>.compat.<YYYY-MM-DD>.r<N>`.
    /// For the same `<vendor>.<product>` the date, then the revision, decide.
    /// An id this build cannot place (another prefix or another grammar) is
    /// [`RecordAge::Newer`]: only a newer build can have minted it, and
    /// replacing it would cost that build its login.
    #[must_use]
    pub fn age_against(&self, current: &Self) -> RecordAge {
        if self == current {
            return RecordAge::Current;
        }
        let Some(current) = CompatibilityRevision::parse(current.as_str()) else {
            return RecordAge::Older;
        };
        match CompatibilityRevision::parse(self.as_str()) {
            Some(stored) if stored.prefix == current.prefix => {
                if (stored.date, stored.revision) > (current.date, current.revision) {
                    RecordAge::Newer
                } else {
                    RecordAge::Older
                }
            }
            _ => RecordAge::Newer,
        }
    }
}

/// How a stored versioned record relates to the version this build writes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordAge {
    /// Written by an earlier build in a format this build retired.
    Older,
    /// Written in exactly the format this build writes.
    Current,
    /// Written by a newer build; this build must neither use nor replace it
    /// without the operator's explicit confirmation.
    Newer,
}

impl RecordAge {
    /// Orders a stored integer format version against this build's.
    #[must_use]
    pub const fn of_version(stored: u64, current: u64) -> Self {
        if stored < current {
            Self::Older
        } else if stored == current {
            Self::Current
        } else {
            Self::Newer
        }
    }
}

/// The ordered parts of a `<prefix>.compat.<YYYY-MM-DD>.r<N>` profile id.
struct CompatibilityRevision<'a> {
    prefix: &'a str,
    date: (u16, u8, u8),
    revision: u32,
}

impl<'a> CompatibilityRevision<'a> {
    fn parse(id: &'a str) -> Option<Self> {
        let (prefix, rest) = id.rsplit_once(".compat.")?;
        let (date, revision) = rest.split_once(".r")?;
        if prefix.is_empty() || date.len() != 10 || !digits(revision) {
            return None;
        }
        let mut parts = date.split('-');
        let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
            return None;
        }
        if ![year, month, day].iter().all(|part| digits(part)) {
            return None;
        }
        Some(Self {
            prefix,
            date: (year.parse().ok()?, month.parse().ok()?, day.parse().ok()?),
            revision: revision.parse().ok()?,
        })
    }
}

fn digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

impl fmt::Debug for SubscriptionIntegrationProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SubscriptionIntegrationProfileId(<opaque>)")
    }
}

impl TryFrom<&str> for SubscriptionIntegrationProfileId {
    type Error = CredentialValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A validated provider-supplied opaque account identity.
///
/// The value is credential-free, but its debug representation remains
/// redacted because account identifiers can be identifying metadata.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct AccountBinding(String);

impl AccountBinding {
    /// Validates an opaque account binding.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialValueError::InvalidAccountBinding`] for an empty,
    /// oversized, or non-token value.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialValueError> {
        let value = value.into();
        if !valid_opaque_token(&value, MAX_ACCOUNT_BINDING_BYTES) {
            return Err(CredentialValueError::InvalidAccountBinding);
        }
        Ok(Self(value))
    }

    /// Returns the validated opaque binding.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccountBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccountBinding(<redacted opaque binding>)")
    }
}

impl TryFrom<&str> for AccountBinding {
    type Error = CredentialValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A validated local slot selecting one current provider account.
///
/// This is deliberately distinct from [`AccountBinding`]: the slot remains
/// stable while an explicit login/switch may replace the actual provider
/// account stored inside it.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialSlotId(String);

impl CredentialSlotId {
    /// Validates a bounded local slot identifier.
    ///
    /// # Errors
    ///
    /// Rejects empty, oversized, or non-token values.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialValueError> {
        let value = value.into();
        if !valid_opaque_token(&value, MAX_CREDENTIAL_SLOT_ID_BYTES) {
            return Err(CredentialValueError::InvalidSlotId);
        }
        Ok(Self(value))
    }

    /// Returns the validated slot identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CredentialSlotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialSlotId(<opaque>)")
    }
}

impl TryFrom<&str> for CredentialSlotId {
    type Error = CredentialValueError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A positive, monotonically increasing stored credential generation.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialGeneration(u64);

impl CredentialGeneration {
    /// Validates a positive generation number.
    ///
    /// # Errors
    ///
    /// Zero is reserved and rejected.
    pub const fn new(value: u64) -> Result<Self, CredentialValueError> {
        if value == 0 {
            Err(CredentialValueError::InvalidGeneration)
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the numeric generation for compare-and-swap operations.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Returns the next generation.
    ///
    /// # Errors
    ///
    /// Returns [`CredentialValueError::GenerationExhausted`] at `u64::MAX`.
    pub const fn next(self) -> Result<Self, CredentialValueError> {
        match self.0.checked_add(1) {
            Some(value) => Ok(Self(value)),
            None => Err(CredentialValueError::GenerationExhausted),
        }
    }
}

impl fmt::Debug for CredentialGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CredentialGeneration")
            .field(&self.0)
            .finish()
    }
}

impl TryFrom<u64> for CredentialGeneration {
    type Error = CredentialValueError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Credential-free authorization lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum CredentialStatus {
    /// The payload may be used for an authorized request.
    Ready,
    /// Provider-owned refresh logic must refresh before authorized use.
    RefreshRequired,
    /// A refresh might have reached the provider and must not be retried
    /// automatically until provider-owned reconciliation succeeds.
    RefreshOutcomeUnknown,
    /// The credential has been logged out or revoked and carries no payload.
    Revoked,
}

/// Credential-free lifecycle for offline admin and dialect status surfaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum OfflineCredentialLifecycle {
    /// No credential record exists in the explicit slot.
    SignedOut,
    /// A locally usable credential exists; no secret or identity is exposed.
    Ready,
    /// The credential requires refresh before inference.
    RefreshRequired,
    /// A remote outcome is ambiguous and inference remains blocked.
    Quarantined,
    /// A refresh claim owned by a live process (this one or another) is in
    /// flight: the durable Unknown record is not yet a settled outcome.
    Refreshing,
    /// A local revoked tombstone prevents stale resurrection.
    Revoked,
    /// A record exists that this build cannot use (for example a login made
    /// by an earlier build); a new login is required.
    LoginRequired,
    /// A record a newer build wrote. This build neither uses nor rewrites
    /// it; only an explicit confirmed replacement or local retirement may.
    WrittenByNewerBuild,
}

impl OfflineCredentialLifecycle {
    /// Projects store status for offline/admin and dialect status APIs.
    #[must_use]
    pub const fn classify(status: Option<CredentialStatus>) -> Self {
        match status {
            None => Self::SignedOut,
            Some(CredentialStatus::Ready) => Self::Ready,
            Some(CredentialStatus::RefreshRequired) => Self::RefreshRequired,
            Some(CredentialStatus::RefreshOutcomeUnknown) => Self::Quarantined,
            Some(CredentialStatus::Revoked) => Self::Revoked,
        }
    }
}

/// A credential-free exact-state fence for one stored record.
///
/// A fence is safe to retain across provider work and can be used for stale
/// writer checks. Its debug representation is fully redacted because profile
/// and account bindings may be identifying metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct CredentialFence {
    profile_id: SubscriptionIntegrationProfileId,
    account_binding: AccountBinding,
    generation: CredentialGeneration,
    status: CredentialStatus,
}

impl CredentialFence {
    /// Constructs an exact credential-state fence.
    #[must_use]
    pub const fn new(
        profile_id: SubscriptionIntegrationProfileId,
        account_binding: AccountBinding,
        generation: CredentialGeneration,
        status: CredentialStatus,
    ) -> Self {
        Self {
            profile_id,
            account_binding,
            generation,
            status,
        }
    }

    /// Returns the fenced profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &SubscriptionIntegrationProfileId {
        &self.profile_id
    }

    /// Returns the fenced account identity.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account_binding
    }

    /// Returns the fenced generation.
    #[must_use]
    pub const fn generation(&self) -> CredentialGeneration {
        self.generation
    }

    /// Returns the fenced lifecycle status.
    #[must_use]
    pub const fn status(&self) -> CredentialStatus {
        self.status
    }
}

impl fmt::Debug for CredentialFence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialFence(<redacted exact state>)")
    }
}

/// One provider-opaque credential record.
///
/// The payload is never interpreted by this crate and is not cloneable.
pub struct CredentialRecord {
    profile_id: SubscriptionIntegrationProfileId,
    account_binding: AccountBinding,
    generation: CredentialGeneration,
    status: CredentialStatus,
    payload: SecretBytes,
}

impl CredentialRecord {
    /// Constructs a bounded provider-opaque credential record.
    ///
    /// # Errors
    ///
    /// Rejects oversized payloads, empty live payloads, and revoked records
    /// that retain credential bytes.
    pub fn new(
        profile_id: SubscriptionIntegrationProfileId,
        account_binding: AccountBinding,
        generation: CredentialGeneration,
        status: CredentialStatus,
        payload: SecretBytes,
    ) -> Result<Self, CredentialValueError> {
        if payload.len() > MAX_CREDENTIAL_PAYLOAD_BYTES {
            return Err(CredentialValueError::PayloadTooLarge);
        }
        match status {
            CredentialStatus::Revoked if !payload.is_empty() => {
                return Err(CredentialValueError::RevokedPayload);
            }
            CredentialStatus::Revoked => {}
            _ if payload.is_empty() => return Err(CredentialValueError::MissingPayload),
            _ => {}
        }
        Ok(Self {
            profile_id,
            account_binding,
            generation,
            status,
            payload,
        })
    }

    /// Returns the profile identity.
    #[must_use]
    pub const fn profile_id(&self) -> &SubscriptionIntegrationProfileId {
        &self.profile_id
    }

    /// Returns the opaque account identity.
    #[must_use]
    pub const fn account_binding(&self) -> &AccountBinding {
        &self.account_binding
    }

    /// Returns this record's generation.
    #[must_use]
    pub const fn generation(&self) -> CredentialGeneration {
        self.generation
    }

    /// Returns this record's credential-free lifecycle status.
    #[must_use]
    pub const fn status(&self) -> CredentialStatus {
        self.status
    }

    /// Borrows the opaque payload inside the storage implementation.
    ///
    /// Public readers obtain Ready payloads only through a retained-lock
    /// [`crate::ReadyCredentialLease`], and refresh payloads only after
    /// consuming a [`crate::RefreshClaim`] into a
    /// [`crate::StartedRefreshClaim`].
    #[must_use]
    pub(crate) const fn payload(&self) -> &SecretBytes {
        &self.payload
    }

    /// Returns a credential-free fence for this exact record state.
    #[must_use]
    pub fn fence(&self) -> CredentialFence {
        CredentialFence::new(
            self.profile_id.clone(),
            self.account_binding.clone(),
            self.generation,
            self.status,
        )
    }

    pub(crate) fn set_status(&mut self, status: CredentialStatus) {
        self.status = status;
    }

    pub(crate) fn set_generation(&mut self, generation: CredentialGeneration) {
        self.generation = generation;
    }
}

impl fmt::Debug for CredentialRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialRecord")
            .field("profile_id", &self.profile_id)
            .field("account_binding", &self.account_binding)
            .field("generation", &self.generation)
            .field("status", &self.status)
            .field("payload", &"<redacted>")
            .finish()
    }
}

/// Validation failure for credential-free values or opaque payload bounds.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum CredentialValueError {
    /// The profile token is empty, oversized, or contains unsafe characters.
    #[error("invalid profile identifier")]
    InvalidSubscriptionIntegrationProfileId,
    /// The account token is empty, oversized, or contains unsafe characters.
    #[error("invalid account binding")]
    InvalidAccountBinding,
    /// The selected-account slot is empty, oversized, or unsafe.
    #[error("invalid credential slot identifier")]
    InvalidSlotId,
    /// Generation zero is reserved.
    #[error("invalid credential generation")]
    InvalidGeneration,
    /// No later generation can be represented.
    #[error("credential generation exhausted")]
    GenerationExhausted,
    /// A live status had no provider payload.
    #[error("live credential payload is missing")]
    MissingPayload,
    /// A revoked status retained provider payload bytes.
    #[error("revoked credential must not retain a payload")]
    RevokedPayload,
    /// The opaque provider payload exceeds the fixed storage bound.
    #[error("credential payload exceeds the storage bound")]
    PayloadTooLarge,
}

fn valid_opaque_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b':')
        })
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "unit tests may assert with expect/unwrap"
)]
mod tests {
    use super::*;

    #[test]
    fn stored_profile_ids_order_by_date_then_revision_and_unplaceable_ids_are_newer()
    -> Result<(), CredentialValueError> {
        let id = SubscriptionIntegrationProfileId::new;
        let current = id("openai.codex.compat.2026-07-31.r1")?;
        for (stored, age) in [
            ("openai.codex.compat.2026-07-31.r1", RecordAge::Current),
            ("openai.codex.compat.2026-07-31.r0", RecordAge::Older),
            ("openai.codex.compat.2026-06-30.r9", RecordAge::Older),
            ("openai.codex.compat.2025-12-31.r12", RecordAge::Older),
            ("openai.codex.compat.2026-07-31.r2", RecordAge::Newer),
            ("openai.codex.compat.2026-08-01.r0", RecordAge::Newer),
            ("openai.codex.compat.2027-01-01.r1", RecordAge::Newer),
            ("openai.codex.v2.compat.2026-07-31.r1", RecordAge::Newer),
            ("xai.grok.compat.2026-07-31.r1", RecordAge::Newer),
            ("openai.codex.compat.2026-7-31.r1", RecordAge::Newer),
            ("openai.codex.compat.2026-07-31.rx", RecordAge::Newer),
            ("openai.codex.profile.3", RecordAge::Newer),
            ("previous-profile", RecordAge::Newer),
        ] {
            assert_eq!(id(stored)?.age_against(&current), age, "{stored}");
        }
        // A build whose own id is outside the grammar keeps refusing other
        // ids as another profile.
        assert_eq!(id("work")?.age_against(&id("home")?), RecordAge::Older);
        assert_eq!(RecordAge::of_version(2, 3), RecordAge::Older);
        assert_eq!(RecordAge::of_version(3, 3), RecordAge::Current);
        assert_eq!(RecordAge::of_version(4, 3), RecordAge::Newer);
        Ok(())
    }

    #[test]
    fn values_are_bounded_and_debug_safe() -> Result<(), CredentialValueError> {
        let profile = SubscriptionIntegrationProfileId::new("work")?;
        let account = AccountBinding::new("opaque:account")?;
        let slot = CredentialSlotId::new("primary")?;
        assert!(!format!("{profile:?}").contains("work"));
        assert!(!format!("{account:?}").contains("account"));
        assert!(!format!("{slot:?}").contains("primary"));
        assert_eq!(
            SubscriptionIntegrationProfileId::new("bad/path"),
            Err(CredentialValueError::InvalidSubscriptionIntegrationProfileId)
        );
        assert_eq!(
            AccountBinding::new("\n"),
            Err(CredentialValueError::InvalidAccountBinding)
        );
        assert_eq!(
            CredentialSlotId::new("bad/slot"),
            Err(CredentialValueError::InvalidSlotId)
        );
        Ok(())
    }

    #[test]
    fn record_enforces_payload_lifecycle() -> Result<(), CredentialValueError> {
        let profile = SubscriptionIntegrationProfileId::new("work")?;
        let account = AccountBinding::new("opaque")?;
        let generation = CredentialGeneration::new(1)?;
        assert_eq!(
            CredentialRecord::new(
                profile.clone(),
                account.clone(),
                generation,
                CredentialStatus::Ready,
                SecretBytes::empty()
            )
            .map(|_| ()),
            Err(CredentialValueError::MissingPayload)
        );
        assert_eq!(
            CredentialRecord::new(
                profile,
                account,
                generation,
                CredentialStatus::Revoked,
                SecretBytes::new(vec![1])
            )
            .map(|_| ()),
            Err(CredentialValueError::RevokedPayload)
        );
        Ok(())
    }
}
