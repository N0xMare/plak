//! A Claude login a credential custodian holds (spec 01 §7.1): the
//! manager's credential reads answer from the custodian's facts, its
//! `authorization` field is the custodian's, and refresh is the custodian's.
//!
//! The facts and field are read together before each dispatch (the
//! manager's refresh point) and kept for that attempt's synchronous reads;
//! the custodian checks the fence again at injection. The opaque grant
//! binding is the login selection digest's hex, which no wire or persisted
//! value carries.

use std::sync::{Arc, Mutex, PoisonError};

use plakous_auth::custody::{
    AccountFacts, CredentialCustodian, CustodyError, CustodySlot, CustodyStatus, RefreshRequest,
};
use zeroize::Zeroizing;

use super::custody::{CLAUDE_ACCOUNT_UUID_IDENTITY, CLAUDE_DEVICE_ID_IDENTITY};
use super::{AccountBinding, ClaudeNativeAuthError, ClaudeSubscriptionStatus, CredentialStatus};

/// The credential a Claude OAuth request carries in `authorization`.
pub(crate) enum ClaudeOAuthAccess {
    /// The stored access token, under local custody.
    Token(String),
    /// The custodian's exact header value for the attempt's fence.
    Custodied(Arc<Zeroizing<Vec<u8>>>),
}

impl ClaudeOAuthAccess {
    /// The access token, under local custody.
    #[cfg(test)]
    pub(crate) fn token(&self) -> Option<&str> {
        match self {
            Self::Token(token) => Some(token),
            Self::Custodied(_) => None,
        }
    }
}

/// One attempt's view of a custodied login: its facts and, when Ready, the
/// field for exactly that fence.
#[derive(Clone)]
struct View {
    facts: AccountFacts,
    field: Option<Arc<Zeroizing<Vec<u8>>>>,
}

/// One Claude login a custodian holds.
pub(super) struct ClaudeCustodiedLogin {
    custodian: Arc<dyn CredentialCustodian>,
    slot: CustodySlot,
    view: Mutex<View>,
}

/// A custodian refusal as the manager's error.
pub(super) fn custody_error(error: CustodyError) -> ClaudeNativeAuthError {
    match error {
        CustodyError::Unavailable | CustodyError::Busy { .. } => {
            ClaudeNativeAuthError::Transport(None)
        }
        CustodyError::NotReady { facts } => lifecycle_error(facts.status),
        _ => ClaudeNativeAuthError::NotReady,
    }
}

const fn lifecycle_error(status: CustodyStatus) -> ClaudeNativeAuthError {
    match status {
        CustodyStatus::SignedOut => ClaudeNativeAuthError::NotLoggedIn,
        CustodyStatus::WrittenByNewerBuild => {
            ClaudeNativeAuthError::StoredCredentialWrittenByNewerBuild
        }
        CustodyStatus::LoginRequired | CustodyStatus::ProfileChanged => {
            ClaudeNativeAuthError::InvalidStoredCredential
        }
        _ => ClaudeNativeAuthError::NotReady,
    }
}

impl ClaudeCustodiedLogin {
    /// Opens `slot` at `custodian` with one read.
    pub(super) async fn open(
        custodian: Arc<dyn CredentialCustodian>,
        slot: CustodySlot,
    ) -> Result<Self, ClaudeNativeAuthError> {
        let facts = custodian.facts(&slot).await.map_err(custody_error)?;
        let login = Self {
            custodian,
            slot,
            view: Mutex::new(View { facts, field: None }),
        };
        login.settle(login.view().facts).await?;
        Ok(login)
    }

    fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keeps `facts` and, when Ready, the custodian's field for their fence.
    async fn settle(&self, facts: AccountFacts) -> Result<AccountFacts, ClaudeNativeAuthError> {
        let field = match facts.expected_fence() {
            Some(expected) if facts.status == CustodyStatus::Ready => {
                let mut fields = self
                    .custodian
                    .fields(&self.slot, &expected)
                    .await
                    .map_err(custody_error)?;
                let field = fields
                    .take("authorization")
                    .ok_or(ClaudeNativeAuthError::InvalidStoredCredential)?;
                Some(Arc::new(Zeroizing::new(field.into_header_bytes())))
            }
            _ => None,
        };
        *self.view.lock().unwrap_or_else(PoisonError::into_inner) = View {
            facts: facts.clone(),
            field,
        };
        Ok(facts)
    }

    /// Current facts and field, read fresh.
    pub(super) async fn read(&self) -> Result<AccountFacts, ClaudeNativeAuthError> {
        let facts = self
            .custodian
            .facts(&self.slot)
            .await
            .map_err(custody_error)?;
        self.settle(facts).await
    }

    /// One refresh by the custodian, then its facts and field.
    pub(super) async fn refresh(
        &self,
        request: RefreshRequest,
    ) -> Result<AccountFacts, ClaudeNativeAuthError> {
        let facts = self
            .custodian
            .refresh(&self.slot, request)
            .await
            .map_err(custody_error)?;
        self.settle(facts).await
    }

    /// The custodian's `IfDue` refresh for the current generation.
    pub(super) async fn refresh_if_due(&self) -> Result<AccountFacts, ClaudeNativeAuthError> {
        let facts = self.read().await?;
        let Some(expected) = facts.expected_fence() else {
            return Ok(facts);
        };
        self.refresh(RefreshRequest::IfDue { expected }).await
    }

    /// The custodian's refresh after the provider rejected the current
    /// generation (or `generation`, when given) with HTTP 401.
    pub(super) async fn refresh_after_rejection(
        &self,
        generation: Option<plakous_auth::CredentialGeneration>,
    ) -> Result<AccountFacts, ClaudeNativeAuthError> {
        let facts = self.view().facts;
        let Some(mut rejected) = facts.expected_fence() else {
            return self.read().await;
        };
        if let Some(generation) = generation {
            rejected.generation = generation;
        }
        self.refresh(RefreshRequest::AfterRejection {
            rejected,
            selection_epoch_digest: facts.selection_epoch_digest,
        })
        .await
    }

    /// The status of the kept facts.
    pub(super) fn status(&self) -> Option<ClaudeSubscriptionStatus> {
        let facts = self.view().facts;
        let status = match facts.status {
            CustodyStatus::Ready => CredentialStatus::Ready,
            CustodyStatus::RefreshRequired => CredentialStatus::RefreshRequired,
            CustodyStatus::Refreshing | CustodyStatus::Quarantined => {
                CredentialStatus::RefreshOutcomeUnknown
            }
            CustodyStatus::Revoked => CredentialStatus::Revoked,
            _ => return None,
        };
        Some(ClaudeSubscriptionStatus {
            generation: facts.generation?,
            status,
        })
    }

    /// The kept facts, when they are Ready with a field.
    fn ready(&self) -> Result<(View, AccountFacts), ClaudeNativeAuthError> {
        let view = self.view();
        if view.facts.status != CustodyStatus::Ready {
            return Err(lifecycle_error(view.facts.status));
        }
        let facts = view.facts.clone();
        Ok((view, facts))
    }

    /// The kept access-token expiry in Unix seconds.
    pub(super) fn access_expires_at(&self) -> Result<u64, ClaudeNativeAuthError> {
        let (_, facts) = self.ready()?;
        facts
            .access_expires_at_unix_ms
            .map(|ms| ms / 1000)
            .ok_or(ClaudeNativeAuthError::NotReady)
    }

    /// Whether the kept facts are Ready.
    pub(super) fn is_ready(&self) -> bool {
        self.view().facts.status == CustodyStatus::Ready
    }

    /// The kept facts' account UUID, when the custodian reported one.
    pub(super) fn has_account_identity(&self) -> bool {
        self.view()
            .facts
            .wire_identity
            .contains_key(CLAUDE_ACCOUNT_UUID_IDENTITY)
    }

    /// The kept facts' dispatch credential, generation, grant binding and
    /// request identity.
    pub(super) fn identity_credentials(
        &self,
    ) -> Result<
        (
            (ClaudeOAuthAccess, u64, AccountBinding),
            crate::ClaudeOAuthRequestIdentity,
        ),
        ClaudeNativeAuthError,
    > {
        let (view, facts) = self.ready()?;
        let field = view.field.ok_or(ClaudeNativeAuthError::NotReady)?;
        let generation = facts.generation.ok_or(ClaudeNativeAuthError::NotReady)?;
        let grant = facts
            .selection_epoch_digest
            .ok_or(ClaudeNativeAuthError::NotReady)?;
        let grant = AccountBinding::new(grant.to_hex())
            .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?;
        let account = facts
            .wire_identity
            .get(CLAUDE_ACCOUNT_UUID_IDENTITY)
            .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?;
        let device = facts
            .wire_identity
            .get(CLAUDE_DEVICE_ID_IDENTITY)
            .ok_or(ClaudeNativeAuthError::AccountIdentityUnavailable)?;
        let identity = crate::ClaudeOAuthRequestIdentity::new(
            crate::ClaudeAccountUuid::new(account.clone())
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?,
            crate::ClaudeClientDeviceId::new(device.clone())
                .map_err(|_| ClaudeNativeAuthError::InvalidStoredCredential)?,
        );
        Ok((
            (ClaudeOAuthAccess::Custodied(field), generation.get(), grant),
            identity,
        ))
    }

    /// The kept field, for a control-plane read.
    pub(super) fn field(&self) -> Option<Arc<Zeroizing<Vec<u8>>>> {
        self.ready().ok().and_then(|(view, _)| view.field)
    }
}

impl std::fmt::Debug for ClaudeCustodiedLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClaudeCustodiedLogin(<custodied>)")
    }
}
