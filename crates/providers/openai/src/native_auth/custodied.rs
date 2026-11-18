//! A Codex login a credential custodian holds (spec 01 §7.1): the manager's
//! credential reads answer from the custodian's facts, its credential
//! headers are the custodian's fields, and refresh is the custodian's.
//!
//! The facts' identity digests are the exact bytes a direct-custody route
//! binds, so a session bound to the login revalidates unchanged. The
//! manager's opaque account binding is the account digest's hex, which no
//! wire or persisted value carries.

use plakous_auth::custody::{
    AccountFacts, CredentialCustodian, CustodyError, CustodySlot, CustodyStatus, ExpectedFence,
    IdentityDigest, RefreshRequest,
};

use super::{
    AccountBinding, Arc, AuthProfileId, CodexCredentialStatus, CredentialFence, CredentialStatus,
    Digest, DigestDomain, HeaderValue, Mutex, NativeCodexAuthError,
    OPENAI_CODEX_COMPATIBILITY_PROFILE_ID, sensitive_header_value,
};

/// The Ready state of a custodied login, as one prepared request uses it.
pub(super) struct CustodiedReady {
    pub(super) fence: CredentialFence,
    pub(super) expected: ExpectedFence,
    pub(super) selection_epoch: Digest,
    pub(super) access_expires_at: u64,
}

/// One Codex login a custodian holds.
pub(super) struct CustodiedLogin {
    custodian: Arc<dyn CredentialCustodian>,
    slot: CustodySlot,
    /// The route account fingerprint, fixed when the manager opened.
    pub(super) account: Digest,
    /// The credential authority, fixed when the manager opened.
    pub(super) authority: Digest,
    /// The opaque account binding of [`Self::account`].
    pub(super) binding: AccountBinding,
    /// The latest facts, for the manager's synchronous checks; the
    /// custodian checks the fence again at injection.
    last: Mutex<AccountFacts>,
}

fn binding_digest(value: IdentityDigest, version: u32) -> Result<Digest, NativeCodexAuthError> {
    Digest::from_sha256_bytes(DigestDomain::ModelBinding, version, *value.as_bytes())
        .map_err(|_| NativeCodexAuthError::Validation)
}

fn opaque_binding(account: IdentityDigest) -> Result<AccountBinding, NativeCodexAuthError> {
    AccountBinding::new(account.to_hex()).map_err(|_| NativeCodexAuthError::Validation)
}

/// A custodian refusal as the manager's error.
pub(super) fn custody_error(error: CustodyError) -> NativeCodexAuthError {
    match error {
        CustodyError::AccountChanged => NativeCodexAuthError::AccountChanged,
        CustodyError::Store => NativeCodexAuthError::Store,
        CustodyError::Unavailable | CustodyError::Busy { .. } => {
            NativeCodexAuthError::Transport(None)
        }
        CustodyError::NotReady { facts } => lifecycle_error(facts.status),
        _ => NativeCodexAuthError::Unavailable,
    }
}

const fn lifecycle_error(status: CustodyStatus) -> NativeCodexAuthError {
    match status {
        CustodyStatus::LoginRequired => NativeCodexAuthError::LoginRequired,
        CustodyStatus::ProfileChanged => NativeCodexAuthError::ProfileChanged,
        CustodyStatus::WrittenByNewerBuild => NativeCodexAuthError::WrittenByNewerBuild,
        _ => NativeCodexAuthError::Unavailable,
    }
}

impl CustodiedLogin {
    /// Opens `slot` at `custodian`: one facts read naming the login's
    /// account and credential authority.
    pub(super) async fn open(
        custodian: Arc<dyn CredentialCustodian>,
        slot: CustodySlot,
    ) -> Result<Self, NativeCodexAuthError> {
        let facts = custodian.facts(&slot).await.map_err(custody_error)?;
        let (Some(account), Some(authority)) = (facts.account_binding, facts.credential_authority)
        else {
            return Err(lifecycle_error(facts.status));
        };
        Ok(Self {
            account: binding_digest(account, 1)?,
            authority: binding_digest(authority, 1)?,
            binding: opaque_binding(account)?,
            custodian,
            slot,
            last: Mutex::new(facts),
        })
    }

    /// Current facts, read fresh.
    pub(super) async fn facts(&self) -> Result<AccountFacts, NativeCodexAuthError> {
        let facts = self
            .custodian
            .facts(&self.slot)
            .await
            .map_err(custody_error)?;
        self.remember(&facts);
        Ok(facts)
    }

    fn remember(&self, facts: &AccountFacts) {
        *self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = facts.clone();
    }

    fn last(&self) -> AccountFacts {
        self.last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The fence of `facts`, in the manager's vocabulary.
    fn fence_of(facts: &AccountFacts) -> Result<CredentialFence, NativeCodexAuthError> {
        let status = match facts.status {
            CustodyStatus::Ready => CredentialStatus::Ready,
            CustodyStatus::RefreshRequired => CredentialStatus::RefreshRequired,
            CustodyStatus::Refreshing | CustodyStatus::Quarantined => {
                CredentialStatus::RefreshOutcomeUnknown
            }
            CustodyStatus::Revoked => CredentialStatus::Revoked,
            other => return Err(lifecycle_error(other)),
        };
        let (Some(account), Some(generation)) = (facts.account_binding, facts.generation) else {
            return Err(NativeCodexAuthError::Unavailable);
        };
        Ok(CredentialFence::new(
            AuthProfileId::new(OPENAI_CODEX_COMPATIBILITY_PROFILE_ID)
                .map_err(|_| NativeCodexAuthError::Validation)?,
            opaque_binding(account)?,
            generation,
            status,
        ))
    }

    /// The current fence, read fresh.
    pub(super) async fn load_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        Self::fence_of(&self.facts().await?)
    }

    /// The fence of the latest facts, for a synchronous check.
    pub(super) fn last_fence(&self) -> Result<CredentialFence, NativeCodexAuthError> {
        Self::fence_of(&self.last())
    }

    /// The lifecycle of the latest facts.
    pub(super) fn last_status(&self) -> CodexCredentialStatus {
        use plakous_auth::OfflineCredentialLifecycle as Lifecycle;
        match self.last().status {
            CustodyStatus::SignedOut => Lifecycle::SignedOut,
            CustodyStatus::Ready => Lifecycle::Ready,
            CustodyStatus::RefreshRequired => Lifecycle::RefreshRequired,
            CustodyStatus::Refreshing => Lifecycle::Refreshing,
            CustodyStatus::Quarantined => Lifecycle::Quarantined,
            CustodyStatus::LoginRequired | CustodyStatus::ProfileChanged => {
                Lifecycle::LoginRequired
            }
            CustodyStatus::WrittenByNewerBuild => Lifecycle::WrittenByNewerBuild,
            CustodyStatus::Revoked => Lifecycle::Revoked,
        }
    }

    /// The access-token expiry of the latest Ready facts, in Unix seconds.
    pub(super) fn last_access_expires_at(&self) -> Result<u64, NativeCodexAuthError> {
        let facts = self.last();
        if facts.status != CustodyStatus::Ready {
            return Err(NativeCodexAuthError::Unavailable);
        }
        facts
            .access_expires_at_unix_ms
            .map(|ms| ms / 1000)
            .ok_or(NativeCodexAuthError::Unavailable)
    }

    fn ready_of(facts: &AccountFacts) -> Result<CustodiedReady, NativeCodexAuthError> {
        let fence = Self::fence_of(facts)?;
        if fence.status() != CredentialStatus::Ready {
            return Err(NativeCodexAuthError::Unavailable);
        }
        let expected = facts
            .expected_fence()
            .ok_or(NativeCodexAuthError::Unavailable)?;
        let selection_epoch = binding_digest(
            facts
                .selection_epoch_digest
                .ok_or(NativeCodexAuthError::Unavailable)?,
            2,
        )?;
        let access_expires_at = facts
            .access_expires_at_unix_ms
            .ok_or(NativeCodexAuthError::Unavailable)?
            / 1000;
        Ok(CustodiedReady {
            fence,
            expected,
            selection_epoch,
            access_expires_at,
        })
    }

    /// The current Ready state, read fresh.
    pub(super) async fn ready(&self) -> Result<CustodiedReady, NativeCodexAuthError> {
        Self::ready_of(&self.facts().await?)
    }

    /// The custodian's fence for `fence`, a fence of this login's facts.
    pub(super) fn expected(fence: &CredentialFence) -> Result<ExpectedFence, NativeCodexAuthError> {
        Ok(ExpectedFence {
            account_binding: IdentityDigest::from_hex(fence.account_binding().as_str())
                .ok_or(NativeCodexAuthError::AccountChanged)?,
            generation: fence.generation(),
        })
    }

    /// The `authorization` and `chatgpt-account-id` values of one request
    /// prepared against `expected`.
    pub(super) async fn fields(
        &self,
        expected: &ExpectedFence,
    ) -> Result<(HeaderValue, HeaderValue), NativeCodexAuthError> {
        let mut fields = self
            .custodian
            .fields(&self.slot, expected)
            .await
            .map_err(custody_error)?;
        let mut value = |name: &str| {
            let field = fields.take(name).ok_or(NativeCodexAuthError::Validation)?;
            sensitive_header_value(field.into_header_bytes())
                .map_err(|_| NativeCodexAuthError::Validation)
        };
        Ok((value("authorization")?, value("chatgpt-account-id")?))
    }

    /// One refresh by the custodian, then the facts it answers with.
    pub(super) async fn refresh(
        &self,
        request: RefreshRequest,
    ) -> Result<AccountFacts, NativeCodexAuthError> {
        let facts = self
            .custodian
            .refresh(&self.slot, request)
            .await
            .map_err(custody_error)?;
        self.remember(&facts);
        Ok(facts)
    }

    /// The custodian's facts as this login's Ready state after a refresh,
    /// held to the account (and, when given, the login selection) the
    /// caller expected.
    pub(super) fn after_refresh(
        &self,
        facts: &AccountFacts,
        selection_epoch: Option<Digest>,
    ) -> Result<(), NativeCodexAuthError> {
        if facts.account_binding.map(|digest| *digest.as_bytes()) != Some(*self.account.bytes()) {
            return Err(NativeCodexAuthError::AccountChanged);
        }
        let ready = Self::ready_of(facts)?;
        if selection_epoch.is_some_and(|epoch| epoch != ready.selection_epoch) {
            return Err(NativeCodexAuthError::SelectionChanged);
        }
        Ok(())
    }
}

impl std::fmt::Debug for CustodiedLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CustodiedLogin(<custodied>)")
    }
}
