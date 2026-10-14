//! Immutable xAI route identity and credential-authority bindings.

use std::fmt;
#[cfg(feature = "native-auth")]
use std::future::Future;
#[cfg(feature = "native-auth")]
use std::pin::Pin;
#[cfg(feature = "native-auth")]
use std::sync::Arc;

#[cfg(feature = "native-auth")]
use plakous_auth::subscription_limit::SubscriptionRequestPermit;
#[cfg(feature = "native-auth")]
use plakous_auth::{ReadyCredentialLease, SecretString};
#[cfg(feature = "native-auth")]
use plakous_model::DigestDomain;
use plakous_model::{Digest, DigestError, IdentityError, RouteId};
#[cfg(feature = "native-auth")]
use plakous_model::{
    ModelAdapterProfileId, ModelId, ResolvedModelDescriptor, RuntimeAdmissionWitness,
    RuntimeModelAuthority, RuntimeModelAuthorityError, Timestamp,
};
#[cfg(feature = "native-auth")]
use serde::Serialize;
use thiserror::Error;
#[cfg(feature = "native-auth")]
use zeroize::Zeroizing;

#[cfg(feature = "native-auth")]
use crate::native_auth::XaiNativeCatalogEntry;
use crate::{XaiAccessMode, XaiDialect, XaiRouteKind};

const XAI_GROK_SUBSCRIPTION_RESPONSES_PATH: &str = "/v1/responses";

#[cfg(test)]
static AUTHORITY_WITNESSES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());

#[inline]
pub(crate) fn authority_revalidation_witness(predicate: &'static str) {
    #[cfg(test)]
    if let Ok(mut witnesses) = AUTHORITY_WITNESSES.lock() {
        witnesses.push(predicate);
    }
    #[cfg(debug_assertions)]
    tracing::warn!(
        target: "plak.xai.authority",
        process_id = std::process::id(),
        predicate,
        "xAI runtime authority revalidation rejected"
    );
    #[cfg(not(debug_assertions))]
    let _ = predicate;
}

#[cfg(test)]
pub(crate) fn take_authority_revalidation_witnesses() -> Vec<&'static str> {
    AUTHORITY_WITNESSES.lock().map_or_else(
        |_| Vec::new(),
        |mut witnesses| std::mem::take(&mut *witnesses),
    )
}

/// A validated HTTPS origin without path, query, fragment, or user information.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct XaiOrigin(String);

impl XaiOrigin {
    /// Validates an xAI route origin.
    pub fn new(value: impl Into<String>) -> Result<Self, XaiRouteError> {
        let value = value.into();
        let Some(authority) = value.strip_prefix("https://") else {
            return Err(XaiRouteError::InvalidOrigin);
        };
        if authority.is_empty()
            || authority.contains(['@', '/', '?', '#'])
            || authority.chars().any(char::is_whitespace)
        {
            return Err(XaiRouteError::InvalidOrigin);
        }
        Ok(Self(value))
    }

    /// Returns the normalized origin text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for XaiOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("XaiOrigin").field(&self.0).finish()
    }
}

/// Source-owned account and credential-authority identity.
#[derive(Clone, Eq, PartialEq)]
pub struct XaiCredentialBinding {
    kind: XaiRouteKind,
    account_binding: String,
    credential_authority: String,
}

impl XaiCredentialBinding {
    #[cfg(feature = "native-auth")]
    pub(crate) fn grok_subscription(
        account_binding: impl Into<String>,
        credential_authority: impl Into<String>,
    ) -> Result<Self, XaiRouteError> {
        Self::new(
            XaiRouteKind::GrokSubscription,
            account_binding.into(),
            credential_authority.into(),
        )
    }

    #[cfg(feature = "native-auth")]
    fn new(
        kind: XaiRouteKind,
        account_binding: String,
        credential_authority: String,
    ) -> Result<Self, XaiRouteError> {
        if !valid_binding_component(&account_binding)
            || !valid_binding_component(&credential_authority)
        {
            return Err(XaiRouteError::InvalidCredentialBinding);
        }
        Ok(Self {
            kind,
            account_binding,
            credential_authority,
        })
    }

    /// Returns the authorized route family.
    #[must_use]
    pub const fn kind(&self) -> XaiRouteKind {
        self.kind
    }

    /// Returns the opaque stable account binding.
    #[must_use]
    pub fn account_binding(&self) -> &str {
        &self.account_binding
    }

    /// Returns the non-secret source authority identifier.
    #[must_use]
    pub fn credential_authority(&self) -> &str {
        &self.credential_authority
    }
}

impl fmt::Debug for XaiCredentialBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiCredentialBinding")
            .field("kind", &self.kind)
            .field("account_binding", &"<redacted>")
            .field("credential_authority", &"<redacted>")
            .finish()
    }
}

#[cfg(feature = "native-auth")]
fn valid_binding_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
        })
}

/// Immutable route/account/access authority identity.
#[derive(Clone, Eq, PartialEq)]
pub struct XaiRouteIdentity {
    kind: XaiRouteKind,
    route_id: RouteId,
    fingerprint: String,
    origin: XaiOrigin,
    access_mode: XaiAccessMode,
    account_binding: Digest,
    credential_authority_binding: Digest,
    credential_authority: String,
}

impl XaiRouteIdentity {
    #[cfg(feature = "native-auth")]
    fn new(
        kind: XaiRouteKind,
        origin: XaiOrigin,
        access_mode: XaiAccessMode,
        credential_binding: &XaiCredentialBinding,
    ) -> Result<Self, XaiRouteError> {
        if credential_binding.kind() != kind {
            return Err(XaiRouteError::CredentialBindingRouteMismatch);
        }
        let access = match access_mode {
            XaiAccessMode::PlatformApiKey => b"platform-api-key".as_slice(),
            XaiAccessMode::LocalTransportOnly => b"local-transport-only".as_slice(),
            XaiAccessMode::GrokSubscriptionOAuth => b"grok-subscription-oauth".as_slice(),
            XaiAccessMode::NonRoutableSubscription => b"non-routable".as_slice(),
        };
        let account_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-xai-account-binding-v1",
                kind.route_id().as_bytes(),
                origin.as_str().as_bytes(),
                credential_binding.account_binding().as_bytes(),
            ],
        )?;
        let credential_authority_binding = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                b"plakous-xai-credential-authority-binding-v1",
                kind.route_id().as_bytes(),
                origin.as_str().as_bytes(),
                credential_binding.credential_authority().as_bytes(),
            ],
        )?;
        let digest = Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[
                kind.route_id().as_bytes(),
                origin.as_str().as_bytes(),
                account_binding.bytes(),
                access,
                credential_binding.credential_authority().as_bytes(),
            ],
        )?;
        let fingerprint = digest.to_string();
        let route_id = RouteId::new(format!("{}:{fingerprint}", kind.route_id()))?;
        Ok(Self {
            kind,
            route_id,
            fingerprint,
            origin,
            access_mode,
            account_binding,
            credential_authority_binding,
            credential_authority: credential_binding.credential_authority().to_owned(),
        })
    }

    /// Returns the route family.
    #[must_use]
    pub const fn kind(&self) -> XaiRouteKind {
        self.kind
    }

    /// Returns the unique neutral route ID.
    #[must_use]
    pub const fn route_id(&self) -> &RouteId {
        &self.route_id
    }

    /// Returns the non-secret complete route fingerprint.
    #[must_use]
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// Returns the exact bound origin.
    #[must_use]
    pub const fn origin(&self) -> &XaiOrigin {
        &self.origin
    }

    /// Returns the access/billing mode.
    #[must_use]
    pub const fn access_mode(&self) -> XaiAccessMode {
        self.access_mode
    }

    /// Returns the non-secret account-authority digest.
    #[must_use]
    pub const fn account_binding(&self) -> Digest {
        self.account_binding
    }

    /// Returns the non-secret credential-authority digest.
    #[must_use]
    pub const fn credential_authority_binding(&self) -> Digest {
        self.credential_authority_binding
    }

    /// Returns the stable credential-authority label.
    #[must_use]
    pub fn credential_authority(&self) -> &str {
        &self.credential_authority
    }
}

impl fmt::Debug for XaiRouteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("XaiRouteIdentity")
            .field("kind", &self.kind)
            .field("route_id", &self.route_id)
            .field("fingerprint", &self.fingerprint)
            .field("origin", &self.origin)
            .field("access_mode", &self.access_mode)
            .field("account_binding", &"<redacted digest>")
            .field("credential_authority_binding", &"<redacted digest>")
            .field("credential_authority", &"<redacted authority>")
            .finish_non_exhaustive()
    }
}

/// Why a Grok subscription selection is unavailable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum XaiSubscriptionBlockReason {
    /// No library-owned OAuth authority was configured.
    OAuthManagerUnavailable,
}

#[cfg(feature = "native-auth")]
pub(crate) struct XaiRevalidatedSubscriptionAuthority {
    valid_until: Timestamp,
}

#[cfg(feature = "native-auth")]
impl XaiRevalidatedSubscriptionAuthority {
    pub(crate) const fn new(valid_until: Timestamp) -> Self {
        Self { valid_until }
    }

    pub(crate) const fn valid_until(&self) -> Timestamp {
        self.valid_until
    }
}

#[cfg(feature = "native-auth")]
pub(crate) trait XaiSubscriptionCredentialAuthority: Send + Sync + 'static {
    fn account_binding(&self) -> &str;
    fn credential_authority(&self) -> &str;
    fn credential_generation(&self) -> u64;
    fn selection_epoch(&self) -> &str;
    fn revalidate_runtime_authority(
        &self,
        checked_at: Timestamp,
    ) -> Result<XaiRevalidatedSubscriptionAuthority, RuntimeModelAuthorityError>;
    fn refresh_if_needed(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), XaiAuthError>> + Send + '_>>;
    /// Forces one claimed refresh after the provider answered an inference
    /// dispatch with HTTP 401, regardless of the stored expiry.
    fn refresh_after_rejection(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<(), XaiAuthError>> + Send + '_>>;
    /// Claims the shared refresh when due and re-resolves the exact selected
    /// catalog receipt into a new authority. This authority is never changed.
    fn renew<'a>(
        &'a self,
        floor: &'a XaiRenewalFloor,
    ) -> Pin<Box<dyn Future<Output = Result<XaiRenewedAuthority, XaiAuthError>> + Send + 'a>>;
    fn preflight(&self) -> Result<(), XaiAuthError>;
    fn acquire(&self) -> Result<XaiSubscriptionCredentialGrant, XaiAuthError>;
    fn acquire_inference_permit(
        &self,
        input_liability: u64,
    ) -> Result<Option<Box<dyn SubscriptionRequestPermit>>, XaiAuthError>;
    fn revalidate(&self, outgoing: &XaiSubscriptionCredentialGrant) -> Result<(), XaiAuthError>;
}

/// The earliest `checked_at` among the revalidations one renewal answers.
/// The renewed receipt never claims an observation later than it.
#[cfg(feature = "native-auth")]
#[derive(Debug, Default)]
pub(crate) struct XaiRenewalFloor(std::sync::Mutex<Option<Timestamp>>);

#[cfg(feature = "native-auth")]
impl XaiRenewalFloor {
    pub(crate) fn note(&self, checked_at: Timestamp) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = Some(floor.map_or(checked_at, |earliest| earliest.min(checked_at)));
        }
    }

    pub(crate) fn earliest(&self) -> Option<Timestamp> {
        self.0.lock().ok().and_then(|floor| *floor)
    }

    pub(crate) fn clear(&self) {
        if let Ok(mut floor) = self.0.lock() {
            *floor = None;
        }
    }
}

/// A fresh credential authority and the catalog admission it was resolved with.
#[cfg(feature = "native-auth")]
pub(crate) struct XaiRenewedAuthority {
    pub(crate) authority: Arc<dyn XaiSubscriptionCredentialAuthority>,
    pub(crate) admission: NativeXaiCatalogAdmission,
}

/// Exact credential-free catalog admission retained by the route.
#[cfg(feature = "native-auth")]
#[derive(Clone)]
pub(crate) struct NativeXaiCatalogAdmission {
    entries: Vec<XaiNativeCatalogEntry>,
    profile: ModelAdapterProfileId,
    observed_at: Timestamp,
    expires_at: Timestamp,
    credential_generation: u64,
    selection_epoch: String,
}

#[cfg(feature = "native-auth")]
impl NativeXaiCatalogAdmission {
    pub(crate) fn new(
        entries: Vec<XaiNativeCatalogEntry>,
        profile: ModelAdapterProfileId,
        observed_at: Timestamp,
        expires_at: Timestamp,
        credential_generation: u64,
        selection_epoch: String,
    ) -> Self {
        Self {
            entries,
            profile,
            observed_at,
            expires_at,
            credential_generation,
            selection_epoch,
        }
    }

    pub(crate) fn admits(&self, model: &ModelId, profile: &ModelAdapterProfileId) -> bool {
        self.profile == *profile && self.entry(model).is_some()
    }

    pub(crate) fn entry(&self, model: &ModelId) -> Option<&XaiNativeCatalogEntry> {
        self.entries.iter().find(|entry| entry.model() == model)
    }

    pub(crate) const fn observed_at(&self) -> Timestamp {
        self.observed_at
    }

    pub(crate) const fn expires_at(&self) -> Timestamp {
        self.expires_at
    }

    pub(crate) const fn credential_generation(&self) -> u64 {
        self.credential_generation
    }

    pub(crate) fn selection_epoch(&self) -> &str {
        &self.selection_epoch
    }

    fn selected_runtime_witness(
        &self,
        identity: &XaiRouteIdentity,
        descriptor: &ResolvedModelDescriptor,
        model: &ModelId,
    ) -> Result<RuntimeAdmissionWitness, RuntimeModelAuthorityError> {
        let selected = self
            .entry(model)
            .ok_or(RuntimeModelAuthorityError::Invalid)?;
        let descriptor_binding = descriptor
            .binding_digest()
            .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        let receipt_identity = self.receipt_identity(identity)?;
        let payload = serde_json::to_vec(&XaiSelectedCatalogAdmissionV1 {
            requested_model: model,
            effective_model: selected.effective_model(),
            reported_context_length: selected.diagnostics().reported_context_length().get(),
            receipt_identity,
            catalog_observed_at: self.observed_at,
            catalog_expires_at: self.expires_at,
            credential_generation: self.credential_generation,
            selection_epoch: &self.selection_epoch,
            account_binding: identity.account_binding(),
            credential_authority_binding: identity.credential_authority_binding(),
            route: identity.route_id(),
            dialect: XaiDialect::GrokSubscriptionResponses2026_07.protocol_id(),
            adapter_revision: crate::XAI_RESPONSES_ADAPTER_REVISION,
            catalog_decoder_revision: crate::native_auth::GROK_PROXY_CATALOG_DECODER_REVISION,
            catalog_header_profile_revision:
                crate::native_auth::GROK_PROXY_CATALOG_HEADER_PROFILE_REVISION,
            inference_header_profile_revision:
                crate::native_auth::GROK_PROXY_INFERENCE_HEADER_PROFILE_REVISION,
            descriptor_binding,
            profile: &self.profile,
        })
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        RuntimeAdmissionWitness::new("xai.native-selected-catalog-row", 1, payload)
    }

    pub(crate) fn receipt_identity(
        &self,
        identity: &XaiRouteIdentity,
    ) -> Result<Digest, RuntimeModelAuthorityError> {
        let receipt_bytes = serde_json::to_vec(&XaiCatalogReceiptIdentityV1 {
            entries: &self.entries,
            profile: &self.profile,
            observed_at: self.observed_at,
            expires_at: self.expires_at,
            credential_generation: self.credential_generation,
            selection_epoch: &self.selection_epoch,
            account_binding: identity.account_binding(),
            credential_authority_binding: identity.credential_authority_binding(),
            catalog_decoder_revision: crate::native_auth::GROK_PROXY_CATALOG_DECODER_REVISION,
            catalog_header_profile_revision:
                crate::native_auth::GROK_PROXY_CATALOG_HEADER_PROFILE_REVISION,
        })
        .map_err(|_| RuntimeModelAuthorityError::Invalid)?;
        Digest::sha256(
            DigestDomain::ModelBinding,
            1,
            &[b"plakous-xai/native-catalog-receipt-v1", &receipt_bytes],
        )
        .map_err(|_| RuntimeModelAuthorityError::Invalid)
    }
}

#[cfg(feature = "native-auth")]
#[derive(Serialize)]
struct XaiCatalogReceiptIdentityV1<'a> {
    entries: &'a [XaiNativeCatalogEntry],
    profile: &'a ModelAdapterProfileId,
    observed_at: Timestamp,
    expires_at: Timestamp,
    credential_generation: u64,
    selection_epoch: &'a str,
    account_binding: Digest,
    credential_authority_binding: Digest,
    catalog_decoder_revision: &'static str,
    catalog_header_profile_revision: &'static str,
}

#[cfg(feature = "native-auth")]
#[derive(Serialize)]
struct XaiSelectedCatalogAdmissionV1<'a> {
    requested_model: &'a ModelId,
    effective_model: &'a ModelId,
    reported_context_length: u32,
    receipt_identity: Digest,
    catalog_observed_at: Timestamp,
    catalog_expires_at: Timestamp,
    credential_generation: u64,
    selection_epoch: &'a str,
    account_binding: Digest,
    credential_authority_binding: Digest,
    route: &'a RouteId,
    dialect: &'a str,
    adapter_revision: &'a str,
    catalog_decoder_revision: &'static str,
    catalog_header_profile_revision: &'static str,
    inference_header_profile_revision: &'static str,
    descriptor_binding: Digest,
    profile: &'a ModelAdapterProfileId,
}

#[cfg(feature = "native-auth")]
impl fmt::Debug for NativeXaiCatalogAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NativeXaiCatalogAdmission")
            .field("entries", &self.entries)
            .field("profile", &self.profile)
            .field("observed_at", &self.observed_at)
            .field("expires_at", &self.expires_at)
            .field("credential_generation", &self.credential_generation)
            .field("selection_epoch", &"<redacted selection epoch>")
            .finish()
    }
}

/// One non-cloneable OAuth grant retained through response head.
#[cfg(feature = "native-auth")]
pub(crate) struct XaiSubscriptionCredentialGrant {
    token: Zeroizing<String>,
    _native_lease: ReadyCredentialLease,
}

#[cfg(feature = "native-auth")]
impl XaiSubscriptionCredentialGrant {
    pub(crate) fn native(token: &SecretString, lease: ReadyCredentialLease) -> Self {
        Self {
            token: Zeroizing::new(token.expose_secret().to_owned()),
            _native_lease: lease,
        }
    }

    pub(crate) fn expose(&self) -> &str {
        &self.token
    }
}

#[cfg(feature = "native-auth")]
impl fmt::Debug for XaiSubscriptionCredentialGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("XaiSubscriptionCredentialGrant(<redacted retained grant>)")
    }
}

#[derive(Clone)]
enum XaiGrokSubscriptionRouteState {
    Blocked(XaiSubscriptionBlockReason),
    #[cfg(feature = "native-auth")]
    Routable {
        identity: Box<XaiRouteIdentity>,
        credential_authority: Arc<dyn XaiSubscriptionCredentialAuthority>,
        native_catalog: NativeXaiCatalogAdmission,
    },
}

/// One explicit native SuperGrok route; it never falls back to Platform.
#[derive(Clone)]
pub struct XaiGrokSubscriptionRoute {
    state: XaiGrokSubscriptionRouteState,
}

impl XaiGrokSubscriptionRoute {
    /// Creates a fail-closed selection without OAuth authority.
    #[must_use]
    pub const fn non_routable() -> Self {
        Self {
            state: XaiGrokSubscriptionRouteState::Blocked(
                XaiSubscriptionBlockReason::OAuthManagerUnavailable,
            ),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn from_native_authority(
        credential_authority: Arc<dyn XaiSubscriptionCredentialAuthority>,
        admission: NativeXaiCatalogAdmission,
    ) -> Result<Self, XaiRouteError> {
        Self::from_native_authority_origin(
            credential_authority,
            admission,
            XaiOrigin::new(crate::SUPERGROK_RESPONSES_ORIGIN)?,
        )
    }

    #[cfg(all(feature = "native-auth", test))]
    pub(crate) fn from_native_authority_with_origin(
        credential_authority: Arc<dyn XaiSubscriptionCredentialAuthority>,
        admission: NativeXaiCatalogAdmission,
        origin: XaiOrigin,
    ) -> Result<Self, XaiRouteError> {
        Self::from_native_authority_origin(credential_authority, admission, origin)
    }

    #[cfg(feature = "native-auth")]
    fn from_native_authority_origin(
        credential_authority: Arc<dyn XaiSubscriptionCredentialAuthority>,
        admission: NativeXaiCatalogAdmission,
        origin: XaiOrigin,
    ) -> Result<Self, XaiRouteError> {
        if credential_authority.credential_generation() != admission.credential_generation()
            || credential_authority.selection_epoch() != admission.selection_epoch()
        {
            return Err(XaiRouteError::CredentialSourceUnavailable);
        }
        let credential_binding = XaiCredentialBinding::grok_subscription(
            credential_authority.account_binding(),
            credential_authority.credential_authority(),
        )?;
        let identity = XaiRouteIdentity::new(
            XaiRouteKind::GrokSubscription,
            origin,
            XaiAccessMode::GrokSubscriptionOAuth,
            &credential_binding,
        )?;
        Ok(Self {
            state: XaiGrokSubscriptionRouteState::Routable {
                identity: Box::new(identity),
                credential_authority,
                native_catalog: admission,
            },
        })
    }

    /// Builds the route for a renewed receipt on this route's exact origin.
    #[cfg(feature = "native-auth")]
    pub(crate) fn renewed(&self, renewed: XaiRenewedAuthority) -> Result<Self, XaiRouteError> {
        let origin = self
            .origin()
            .ok_or(XaiRouteError::CredentialSourceUnavailable)?
            .clone();
        Self::from_native_authority_origin(renewed.authority, renewed.admission, origin)
    }

    /// Reports whether this exact route can dispatch.
    #[must_use]
    pub const fn is_routable(&self) -> bool {
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(_) => false,
            #[cfg(feature = "native-auth")]
            XaiGrokSubscriptionRouteState::Routable { .. } => true,
        }
    }

    /// Returns the typed block reason.
    #[must_use]
    pub const fn block_reason(&self) -> Option<XaiSubscriptionBlockReason> {
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(reason) => Some(*reason),
            #[cfg(feature = "native-auth")]
            XaiGrokSubscriptionRouteState::Routable { .. } => None,
        }
    }

    /// Returns the exact access mode.
    #[must_use]
    pub const fn access_mode(&self) -> XaiAccessMode {
        if self.is_routable() {
            XaiAccessMode::GrokSubscriptionOAuth
        } else {
            XaiAccessMode::NonRoutableSubscription
        }
    }

    /// Returns the pinned subscription dialect.
    #[must_use]
    pub const fn dialect(&self) -> XaiDialect {
        XaiDialect::GrokSubscriptionResponses2026_07
    }

    /// Returns the exact identity only for a library-authorized route.
    #[must_use]
    pub const fn identity(&self) -> Option<&XaiRouteIdentity> {
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(_) => None,
            #[cfg(feature = "native-auth")]
            XaiGrokSubscriptionRouteState::Routable { identity, .. } => Some(identity),
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) const fn native_catalog_admission(&self) -> Option<&NativeXaiCatalogAdmission> {
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(_) => None,
            XaiGrokSubscriptionRouteState::Routable { native_catalog, .. } => Some(native_catalog),
        }
    }

    /// Captures the canonical selected-row witness from this already admitted
    /// route. Callers still own the surrounding catalog/credential lease; this
    /// pure operation never recursively reacquires manager state.
    #[cfg(feature = "native-auth")]
    pub(crate) fn runtime_admission_witness(
        &self,
        descriptor: &ResolvedModelDescriptor,
        model: &ModelId,
        profile: &ModelAdapterProfileId,
    ) -> Result<RuntimeAdmissionWitness, RuntimeModelAuthorityError> {
        let XaiGrokSubscriptionRouteState::Routable {
            identity,
            credential_authority,
            native_catalog,
        } = &self.state
        else {
            return Err(RuntimeModelAuthorityError::Unavailable);
        };
        if descriptor.protocol().as_str() != self.dialect().protocol_id()
            || descriptor.route().as_str() != identity.kind().route_id()
            || descriptor.requested_model() != model
            || descriptor.reported_model() != Some(model)
            || descriptor.profile() != profile
            || !native_catalog.admits(model, profile)
            || credential_authority.credential_generation()
                != native_catalog.credential_generation()
            || credential_authority.selection_epoch() != native_catalog.selection_epoch()
        {
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        native_catalog.selected_runtime_witness(identity, descriptor, model)
    }

    /// Returns the immutable endpoint origin only for a runnable route.
    #[must_use]
    pub fn origin(&self) -> Option<&XaiOrigin> {
        self.identity().map(XaiRouteIdentity::origin)
    }

    /// Returns the exact Responses resource path only for a runnable route.
    #[must_use]
    pub const fn responses_path(&self) -> Option<&'static str> {
        if self.is_routable() {
            Some(XAI_GROK_SUBSCRIPTION_RESPONSES_PATH)
        } else {
            None
        }
    }

    #[cfg(feature = "native-auth")]
    pub(crate) fn credential_authority(
        &self,
    ) -> Option<&Arc<dyn XaiSubscriptionCredentialAuthority>> {
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(_) => None,
            XaiGrokSubscriptionRouteState::Routable {
                credential_authority,
                ..
            } => Some(credential_authority),
        }
    }

    /// Revalidates the exact selected subscription authority from its owning
    /// auth manager and seals only credential-free facts for durability.
    #[cfg(feature = "native-auth")]
    pub(crate) fn revalidate_runtime_authority(
        &self,
        descriptor: &ResolvedModelDescriptor,
        model: &ModelId,
        profile: &ModelAdapterProfileId,
        checked_at: Timestamp,
    ) -> Result<RuntimeModelAuthority, RuntimeModelAuthorityError> {
        let XaiGrokSubscriptionRouteState::Routable {
            identity,
            credential_authority,
            native_catalog,
        } = &self.state
        else {
            authority_revalidation_witness("route.unavailable");
            return Err(RuntimeModelAuthorityError::Unavailable);
        };
        let generation = credential_authority.credential_generation();
        if identity.kind() != XaiRouteKind::GrokSubscription
            || identity.access_mode() != XaiAccessMode::GrokSubscriptionOAuth
            || descriptor.protocol().as_str() != self.dialect().protocol_id()
            || descriptor.route().as_str() != identity.kind().route_id()
            || descriptor.requested_model() != model
            || descriptor.reported_model() != Some(model)
            || descriptor.profile() != profile
        {
            authority_revalidation_witness("route.identity_or_descriptor");
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        if !native_catalog.admits(model, profile)
            || native_catalog.entries.is_empty()
            || native_catalog
                .entries
                .windows(2)
                .any(|pair| pair[0].model().as_str() >= pair[1].model().as_str())
            || native_catalog.entries.iter().any(|entry| {
                !crate::native_auth::is_subscription_coding_model_id(entry.model().as_str())
            })
        {
            authority_revalidation_witness("route.catalog_model");
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        if generation == 0 || generation != native_catalog.credential_generation() {
            authority_revalidation_witness("route.generation");
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        if credential_authority.selection_epoch() != native_catalog.selection_epoch()
            || !valid_selection_epoch(native_catalog.selection_epoch())
        {
            authority_revalidation_witness("route.selection_epoch");
            return Err(RuntimeModelAuthorityError::Invalid);
        }
        if checked_at < native_catalog.observed_at() || checked_at >= native_catalog.expires_at() {
            authority_revalidation_witness("route.catalog_time");
            return Err(RuntimeModelAuthorityError::Stale);
        }
        let manager_authority = credential_authority.revalidate_runtime_authority(checked_at)?;
        let witness = self
            .runtime_admission_witness(descriptor, model, profile)
            .inspect_err(|_| authority_revalidation_witness("route.admission_witness"))?;
        Ok(RuntimeModelAuthority::new(
            descriptor,
            identity.account_binding(),
            identity.credential_authority_binding(),
            generation,
            checked_at,
            Some(
                native_catalog
                    .expires_at()
                    .min(manager_authority.valid_until()),
            ),
        )?
        .with_admission_witness(witness))
    }
}

#[cfg(feature = "native-auth")]
fn valid_selection_epoch(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

impl fmt::Debug for XaiGrokSubscriptionRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = formatter.debug_struct("XaiGrokSubscriptionRoute");
        match &self.state {
            XaiGrokSubscriptionRouteState::Blocked(reason) => debug.field("block_reason", reason),
            #[cfg(feature = "native-auth")]
            XaiGrokSubscriptionRouteState::Routable { identity, .. } => debug
                .field("identity", identity)
                .field("credential_authority", &"<redacted OAuth authority>"),
        };
        debug.finish()
    }
}

impl Default for XaiGrokSubscriptionRoute {
    fn default() -> Self {
        Self::non_routable()
    }
}

/// A safe route-construction failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum XaiRouteError {
    /// Origin was not a bare HTTPS origin.
    #[error("xAI route origin must be a bare HTTPS origin")]
    InvalidOrigin,
    /// Account or authority identity was not bounded canonical text.
    #[error("xAI credential binding must use bounded canonical identifiers")]
    InvalidCredentialBinding,
    /// Source did not expose a typed binding.
    #[error("xAI credential source has no validated route binding")]
    CredentialBindingUnavailable,
    /// Binding belongs to another route family.
    #[error("xAI credential binding belongs to another route family")]
    CredentialBindingRouteMismatch,
    /// Current source/catalog authority was unavailable or inconsistent.
    #[error("xAI subscription credential source is unavailable")]
    CredentialSourceUnavailable,
    /// Digest construction failed.
    #[error(transparent)]
    Digest(#[from] DigestError),
    /// Neutral identity construction failed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
}

/// A safe credential-resolution failure.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum XaiAuthError {
    /// No credential was available.
    #[error("the selected xAI credential source is empty")]
    MissingCredential,
    /// Exact route/account/generation/selection binding changed.
    #[error("xAI credential does not match the exact admitted route")]
    BindingMismatch,
    /// Credential source failed without safe detail.
    #[error("the selected xAI credential source failed")]
    CredentialSource,
}
