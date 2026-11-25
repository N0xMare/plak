//! A SuperGrok login a credential custodian holds (spec 01 §7.1): the
//! manager's credential reads answer from the custodian's facts, its
//! `authorization` and `x-userid` values are the custodian's fields, and
//! refresh is the custodian's.
//!
//! The facts carry the route's raw binding inputs as wire identity (the
//! login selection epoch, the account subject and the credential-authority
//! id), each checked here against the facts' identity digests, so a session
//! bound under direct custody revalidates unchanged. Facts and fields are
//! read together at each refresh point and kept for the attempt's
//! synchronous checks; the custodian checks the fence again at injection.

use std::sync::PoisonError;

use plakous_auth::custody::{
    AccountFacts, CredentialCustodian, CustodyError, CustodySlot, CustodyStatus, ExpectedFence,
    IdentityDigest, RefreshRequest,
};

use super::custody::{
    SELECTION_DOMAIN, SUPERGROK_AUTHORITY_ID_IDENTITY, SUPERGROK_SELECTION_EPOCH_IDENTITY,
    SUPERGROK_SUBJECT_IDENTITY, grok_subscription_bindings, identity,
};
use super::{
    ACCESS_REFRESH_LEAD_SECONDS, AccountBinding, Arc, CatalogReceiptCore, CatalogRenewal,
    CredentialFence, CredentialStatus, HeaderValue, ManagerCore, Mutex, PrivateCredentialStore,
    RefreshForce, XaiNativeAuthError, XaiSubscriptionStatus, Zeroizing, access_needs_refresh,
    auth_profile_id, route_credential_authority_id, sensitive_header_value, system_unix_seconds,
    valid_login_selection_epoch,
};

/// One custodied credential header value.
type Field = Arc<Zeroizing<Vec<u8>>>;

/// How often reading the fields of fresh facts retries when the custodian
/// reports the fence moved in between.
const SETTLE_ATTEMPTS: usize = 3;

/// The Ready state of a custodied login, as one prepared request uses it.
pub(super) struct CustodiedReady {
    pub(super) fence: CredentialFence,
    pub(super) selection_epoch: String,
    pub(super) authority_id: String,
    /// The access-token expiry, in Unix seconds.
    pub(super) expires_at: u64,
    authorization: Field,
    subject: Field,
}

impl CustodiedReady {
    /// The custodian's `authorization` value for this fence.
    pub(super) fn authorization(&self) -> Result<HeaderValue, XaiNativeAuthError> {
        header(&self.authorization)
    }

    /// The custodian's `x-userid` value for this fence.
    pub(super) fn subject(&self) -> Result<HeaderValue, XaiNativeAuthError> {
        header(&self.subject)
    }

    /// The `authorization` field itself, for a grant that outlives this view.
    pub(super) fn authorization_field(&self) -> Field {
        Arc::clone(&self.authorization)
    }
}

fn header(field: &Field) -> Result<HeaderValue, XaiNativeAuthError> {
    sensitive_header_value(field.to_vec()).map_err(|_| XaiNativeAuthError::Request)
}

/// One attempt's view of the login: the latest facts and, when they are
/// Ready, their route identity and fields.
#[derive(Clone)]
struct View {
    facts: AccountFacts,
    ready: Option<Arc<CustodiedReady>>,
    /// The account of the latest Ready facts.
    account: Option<AccountBinding>,
}

/// One SuperGrok login a custodian holds.
pub(super) struct CustodiedLogin {
    custodian: Arc<dyn CredentialCustodian>,
    slot: CustodySlot,
    view: Mutex<View>,
}

/// A custodian refusal as the manager's error.
fn custody_error(error: CustodyError) -> XaiNativeAuthError {
    match error {
        CustodyError::Unavailable | CustodyError::Busy { .. } => {
            XaiNativeAuthError::Transport(None)
        }
        CustodyError::AccountChanged => XaiNativeAuthError::AccountChanged,
        CustodyError::UnknownAccount => XaiNativeAuthError::NotLoggedIn,
        CustodyError::NotReady { facts } => lifecycle_error(facts.status),
        _ => XaiNativeAuthError::NotReady,
    }
}

const fn lifecycle_error(status: CustodyStatus) -> XaiNativeAuthError {
    match status {
        CustodyStatus::SignedOut => XaiNativeAuthError::NotLoggedIn,
        CustodyStatus::WrittenByNewerBuild => {
            XaiNativeAuthError::StoredCredentialWrittenByNewerBuild
        }
        CustodyStatus::LoginRequired | CustodyStatus::ProfileChanged => {
            XaiNativeAuthError::StoredCredentialFormatRetired
        }
        _ => XaiNativeAuthError::NotReady,
    }
}

/// The store lifecycle of a custodied status, when it has one.
pub(super) const fn credential_status(status: CustodyStatus) -> Option<CredentialStatus> {
    match status {
        CustodyStatus::Ready => Some(CredentialStatus::Ready),
        CustodyStatus::RefreshRequired => Some(CredentialStatus::RefreshRequired),
        CustodyStatus::Refreshing | CustodyStatus::Quarantined => {
            Some(CredentialStatus::RefreshOutcomeUnknown)
        }
        CustodyStatus::Revoked => Some(CredentialStatus::Revoked),
        _ => None,
    }
}

/// The custodian's fence for a credential of `account` under `authority`
/// at `generation`.
fn expected_fence(
    account: &AccountBinding,
    authority: &str,
    generation: plakous_auth::CredentialGeneration,
) -> Option<ExpectedFence> {
    let (account, _) = grok_subscription_bindings(account.as_str(), authority)?;
    Some(ExpectedFence {
        account_binding: identity(account),
        generation,
    })
}

/// The route identity Ready `facts` name: the fence, the raw login
/// selection epoch and the credential authority id, each held to the facts'
/// digests.
fn identity_of(
    facts: &AccountFacts,
) -> Result<(CredentialFence, String, String), XaiNativeAuthError> {
    let value = |key: &str| {
        facts
            .wire_identity
            .get(key)
            .cloned()
            .ok_or(XaiNativeAuthError::InvalidStoredCredential)
    };
    let subject = value(SUPERGROK_SUBJECT_IDENTITY)?;
    let selection_epoch = value(SUPERGROK_SELECTION_EPOCH_IDENTITY)?;
    let authority_id = value(SUPERGROK_AUTHORITY_ID_IDENTITY)?;
    let generation = facts.generation.ok_or(XaiNativeAuthError::NotReady)?;
    let (account, authority) = grok_subscription_bindings(&subject, &authority_id)
        .ok_or(XaiNativeAuthError::InvalidStoredCredential)?;
    let selection = IdentityDigest::derive(
        SELECTION_DOMAIN,
        &[authority_id.as_bytes(), selection_epoch.as_bytes()],
    );
    if !valid_login_selection_epoch(&selection_epoch)
        || facts.account_binding != Some(identity(account))
        || facts.credential_authority != Some(identity(authority))
        || facts.selection_epoch_digest != Some(selection)
    {
        return Err(XaiNativeAuthError::InvalidStoredCredential);
    }
    let fence = CredentialFence::new(
        auth_profile_id()?,
        AccountBinding::new(subject)?,
        generation,
        CredentialStatus::Ready,
    );
    Ok((fence, selection_epoch, authority_id))
}

impl CustodiedLogin {
    /// Opens `slot` at `custodian` with one read.
    pub(super) async fn open(
        custodian: Arc<dyn CredentialCustodian>,
        slot: CustodySlot,
    ) -> Result<Self, XaiNativeAuthError> {
        let facts = custodian.facts(&slot).await.map_err(custody_error)?;
        let login = Self {
            custodian,
            slot,
            view: Mutex::new(View {
                facts: facts.clone(),
                ready: None,
                account: None,
            }),
        };
        login.settle(facts).await?;
        Ok(login)
    }

    fn view(&self) -> View {
        self.view
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Keeps `facts` and, when they are Ready, their identity and the
    /// custodian's fields for exactly their fence.
    async fn settle(&self, mut facts: AccountFacts) -> Result<AccountFacts, XaiNativeAuthError> {
        let mut attempts = 1;
        let ready = loop {
            if facts.status != CustodyStatus::Ready {
                break None;
            }
            let (fence, selection_epoch, authority_id) = identity_of(&facts)?;
            let expected = facts.expected_fence().ok_or(XaiNativeAuthError::NotReady)?;
            let mut fields = match self.custodian.fields(&self.slot, &expected).await {
                Ok(fields) => fields,
                Err(CustodyError::FenceMoved { facts: moved }) if attempts < SETTLE_ATTEMPTS => {
                    attempts += 1;
                    facts = *moved;
                    continue;
                }
                Err(error) => return Err(custody_error(error)),
            };
            let mut take = |name: &str| {
                fields
                    .take(name)
                    .map(|field| Arc::new(Zeroizing::new(field.into_header_bytes())))
                    .ok_or(XaiNativeAuthError::InvalidStoredCredential)
            };
            let authorization = take("authorization")?;
            let subject = take("x-userid")?;
            let expires_at = facts
                .access_expires_at_unix_ms
                .ok_or(XaiNativeAuthError::NotReady)?
                / 1000;
            break Some(Arc::new(CustodiedReady {
                fence,
                selection_epoch,
                authority_id,
                expires_at,
                authorization,
                subject,
            }));
        };
        let mut view = self.view.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(ready) = &ready {
            view.account = Some(ready.fence.account_binding().clone());
        }
        view.facts = facts.clone();
        view.ready = ready;
        Ok(facts)
    }

    /// Current facts and fields, read fresh. While a claimed refresh is in
    /// flight, the custodian's `IfDue` answers once it has settled.
    pub(super) async fn read(&self) -> Result<AccountFacts, XaiNativeAuthError> {
        let known = self.view().facts.account_binding;
        let facts = self
            .custodian
            .facts(&self.slot)
            .await
            .map_err(custody_error)?;
        if facts.status == CustodyStatus::Refreshing
            && let (Some(account_binding), Some(generation)) =
                (facts.account_binding.or(known), facts.generation)
        {
            return self
                .refresh(RefreshRequest::IfDue {
                    expected: ExpectedFence {
                        account_binding,
                        generation,
                    },
                })
                .await;
        }
        self.settle(facts).await
    }

    /// One refresh by the custodian, then its facts and fields.
    async fn refresh(&self, request: RefreshRequest) -> Result<AccountFacts, XaiNativeAuthError> {
        let facts = self
            .custodian
            .refresh(&self.slot, request)
            .await
            .map_err(custody_error)?;
        self.settle(facts).await
    }

    /// The kept facts.
    pub(super) fn facts(&self) -> AccountFacts {
        self.view().facts
    }

    /// The kept Ready state.
    pub(super) fn ready(&self) -> Result<Arc<CustodiedReady>, XaiNativeAuthError> {
        let view = self.view();
        view.ready.ok_or_else(|| match view.facts.status {
            CustodyStatus::Ready => XaiNativeAuthError::NotReady,
            status => lifecycle_error(status),
        })
    }

    /// The credential-free status of the kept facts.
    pub(super) fn status(&self) -> Result<Option<XaiSubscriptionStatus>, XaiNativeAuthError> {
        let view = self.view();
        if view.facts.status == CustodyStatus::SignedOut {
            return Ok(None);
        }
        let status = credential_status(view.facts.status)
            .ok_or_else(|| lifecycle_error(view.facts.status))?;
        let (Some(generation), Some(account)) = (view.facts.generation, view.account) else {
            return Err(XaiNativeAuthError::NotReady);
        };
        Ok(Some(XaiSubscriptionStatus {
            generation,
            status,
            account,
        }))
    }

    /// Refuses unless the kept Ready state is exactly `receipt`'s credential.
    pub(super) fn require_receipt(
        &self,
        receipt: &CatalogReceiptCore,
    ) -> Result<(), XaiNativeAuthError> {
        let ready = self
            .ready()
            .map_err(|_| XaiNativeAuthError::SelectionChanged)?;
        if ready.fence != receipt.fence
            || ready.selection_epoch != receipt.selection_epoch
            || ready.authority_id != receipt.authority_id
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(())
    }
}

impl std::fmt::Debug for CustodiedLogin {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CustodiedLogin(<custodied>)")
    }
}

impl ManagerCore {
    /// The selected slot, under local custody.
    pub(super) fn store(&self) -> Result<&PrivateCredentialStore, XaiNativeAuthError> {
        self.store.as_deref().ok_or(XaiNativeAuthError::Custodied)
    }

    pub(super) fn store_arc(&self) -> Result<Arc<PrivateCredentialStore>, XaiNativeAuthError> {
        self.store
            .as_ref()
            .map(Arc::clone)
            .ok_or(XaiNativeAuthError::Custodied)
    }

    /// The route credential-authority id: computed over the local slot, or
    /// the custodian's, which it computed over its own.
    pub(super) fn authority_id(&self) -> Result<String, XaiNativeAuthError> {
        if let Some(store) = &self.store {
            if self.gateway_v1_authority {
                return Ok(super::credential_authority_id(
                    store.path().as_os_str().as_encoded_bytes(),
                    b"\0xai-route-credential-authority-v2\0",
                    store.slot_id().as_str(),
                    self.source_scope.as_deref(),
                ));
            }
            return Ok(route_credential_authority_id(
                store.profile_id().as_str(),
                store.slot_id().as_str(),
                self.source_scope.as_deref(),
            ));
        }
        self.custodied
            .as_ref()
            .ok_or(XaiNativeAuthError::Custodied)?
            .ready()
            .map(|ready| ready.authority_id.clone())
    }

    /// Whether `expected` is the current route credential-authority id.
    pub(super) fn authority_is(&self, expected: &str) -> bool {
        self.authority_id()
            .is_ok_and(|authority| authority == expected)
    }

    /// Reads a custodian's current facts and fields for the synchronous
    /// checks that follow; nothing under local custody.
    pub(super) async fn observe(&self) -> Result<(), XaiNativeAuthError> {
        if let Some(login) = &self.custodied {
            login.read().await?;
        }
        Ok(())
    }

    /// The custodied counterpart of the leased catalog fetch.
    pub(super) async fn custodied_catalog(
        &self,
        login: &CustodiedLogin,
        renewal: Option<&CatalogRenewal<'_>>,
    ) -> Result<CatalogReceiptCore, XaiNativeAuthError> {
        match self
            .refresh_if_needed(
                renewal.map(|renewal| renewal.account),
                renewal.map(|renewal| renewal.selection_epoch),
                renewal.map(|renewal| renewal.authority_id),
            )
            .await
        {
            Ok(()) | Err(XaiNativeAuthError::NotReady) => {}
            Err(error) => return Err(error),
        }
        let ready = login.ready()?;
        if let Some(renewal) = renewal
            && (ready.fence.account_binding() != renewal.account
                || ready.selection_epoch != renewal.selection_epoch
                || ready.authority_id != renewal.authority_id)
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let requested_at = system_unix_seconds()?;
        let response = self
            .get_with(
                self.endpoints.catalog(),
                Some(ready.authorization()?),
                Some(
                    ready
                        .subject()
                        .map_err(|_| XaiNativeAuthError::CatalogUnavailable)?,
                ),
                renewal.is_none() && self.catalog_retries,
            )
            .await?;
        self.admit_catalog(
            &response,
            renewal,
            requested_at,
            ready.fence.clone(),
            ready.selection_epoch.clone(),
            ready.authority_id.clone(),
        )
    }

    /// The custodian's refresh for `force`: due-ness is judged on its clock,
    /// a rejection is refreshed only while it still holds that generation,
    /// and the answer is held to the expected account and selection.
    pub(super) async fn custodied_refresh(
        &self,
        login: &CustodiedLogin,
        expected_account: Option<&AccountBinding>,
        expected_selection_epoch: Option<&str>,
        expected_authority: Option<&str>,
        force: RefreshForce,
    ) -> Result<(), XaiNativeAuthError> {
        let _flight = self.refresh_flight.lock().await;
        let request = match force {
            RefreshForce::IfDue => {
                let facts = login.read().await?;
                match facts.expected_fence() {
                    Some(expected) if facts.status == CustodyStatus::Ready => {
                        RefreshRequest::IfDue { expected }
                    }
                    _ => return Err(lifecycle_error(facts.status)),
                }
            }
            RefreshForce::Always => RefreshRequest::Operator,
            RefreshForce::WhileCurrent(generation) => {
                let facts = login.facts();
                let rejected = match (expected_account, expected_authority) {
                    (Some(account), Some(authority)) => {
                        expected_fence(account, authority, generation)
                    }
                    _ => facts.account_binding.map(|account_binding| ExpectedFence {
                        account_binding,
                        generation,
                    }),
                }
                .ok_or(XaiNativeAuthError::NotReady)?;
                let selection_epoch_digest = match (expected_selection_epoch, expected_authority) {
                    (Some(selection), Some(authority)) => Some(IdentityDigest::derive(
                        SELECTION_DOMAIN,
                        &[authority.as_bytes(), selection.as_bytes()],
                    )),
                    _ => facts.selection_epoch_digest,
                };
                RefreshRequest::AfterRejection {
                    rejected,
                    selection_epoch_digest,
                }
            }
        };
        let facts = login.refresh(request).await?;
        if facts.status != CustodyStatus::Ready {
            return Err(lifecycle_error(facts.status));
        }
        let ready = login.ready()?;
        if expected_account.is_some_and(|account| account != ready.fence.account_binding()) {
            return Err(XaiNativeAuthError::AccountChanged);
        }
        if expected_selection_epoch.is_some_and(|selection| selection != ready.selection_epoch)
            || expected_authority.is_some_and(|authority| authority != ready.authority_id)
        {
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        Ok(())
    }

    /// The custodied counterpart of the leased route currency check, on the
    /// kept facts.
    pub(super) fn custodied_route_current(
        &self,
        login: &CustodiedLogin,
        expected_receipt: &CatalogReceiptCore,
        witness_failures: bool,
    ) -> Result<(), XaiNativeAuthError> {
        let reject = |label: &'static str| {
            if witness_failures {
                crate::route::log_authority_rejection(label);
            }
        };
        let ready = match login.ready() {
            Ok(ready) => ready,
            Err(XaiNativeAuthError::NotLoggedIn) => {
                reject("native.route_current.fence_missing");
                return Err(XaiNativeAuthError::NotLoggedIn);
            }
            Err(_) => {
                reject("native.route_current.fence_status");
                return Err(XaiNativeAuthError::SelectionChanged);
            }
        };
        let fence = &ready.fence;
        if fence.generation() != expected_receipt.fence.generation() {
            reject("native.route_current.generation");
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if fence.account_binding() != &expected_receipt.public.account {
            reject("native.route_current.account");
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if *fence != expected_receipt.fence {
            reject("native.route_current.fence_binding");
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        if ready.selection_epoch != expected_receipt.selection_epoch
            || ready.authority_id != expected_receipt.authority_id
        {
            reject("native.route_current.stored_selection");
            return Err(XaiNativeAuthError::SelectionChanged);
        }
        let now = system_unix_seconds().inspect_err(|_| reject("native.route_current.clock"))?;
        let active = self
            .active_catalog_receipt(fence, &ready.selection_epoch, now)
            .inspect_err(|_| reject("native.route_current.catalog"))?;
        if active != *expected_receipt {
            reject("native.route_current.catalog");
            return Err(XaiNativeAuthError::CatalogUnavailable);
        }
        Ok(())
    }

    /// The custodied counterpart of the leased grant: the custodian's
    /// `authorization` field for the receipt's exact fence.
    pub(super) fn custodied_grant(
        &self,
        login: &CustodiedLogin,
        expected_receipt: &CatalogReceiptCore,
    ) -> Result<crate::XaiSubscriptionCredentialGrant, crate::XaiAuthError> {
        let ready = login.ready().map_err(|error| match error {
            XaiNativeAuthError::NotLoggedIn => crate::XaiAuthError::MissingCredential,
            _ => crate::XaiAuthError::BindingMismatch,
        })?;
        if ready.fence != expected_receipt.fence
            || ready.fence.account_binding() != &expected_receipt.public.account
            || ready.selection_epoch != expected_receipt.selection_epoch
            || ready.authority_id != expected_receipt.authority_id
        {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        let now = system_unix_seconds().map_err(|_| crate::XaiAuthError::CredentialSource)?;
        let active = self
            .active_catalog_receipt(&ready.fence, &ready.selection_epoch, now)
            .map_err(|_| crate::XaiAuthError::BindingMismatch)?;
        if active != *expected_receipt {
            return Err(crate::XaiAuthError::BindingMismatch);
        }
        if access_needs_refresh(ready.expires_at, now, ACCESS_REFRESH_LEAD_SECONDS) {
            return Err(crate::XaiAuthError::MissingCredential);
        }
        Ok(crate::XaiSubscriptionCredentialGrant::custodied(
            ready.authorization_field(),
        ))
    }
}
