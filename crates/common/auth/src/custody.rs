//! Credential custody: who holds a credential, and how a prepared request
//! receives it (spec 01 §7.1).
//!
//! A route prepares its request and sends it through one
//! `plakous_http::Client`. Every credential header value it inserts is a
//! [`CredentialField`] supplied by the credential's custodian, never a value
//! the route formats from secret material itself. Under local custody the
//! field is the exact header value. A [`CredentialCustodian`] reports the
//! non-secret [`AccountFacts`] a caller revalidates against, runs the single
//! writer's refresh, and hands out the fields for one exact [`ExpectedFence`].

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use sha2::{Digest as _, Sha256};
use zeroize::Zeroizing;

use crate::CredentialGeneration;

/// Custody through a credential gateway (feature `gateway`).
#[cfg(feature = "gateway")]
pub mod gateway;
#[cfg(feature = "http-refresh")]
#[doc(hidden)]
pub mod local;
mod local_key;
mod pins;

pub use local_key::{KeyHeaderForm, KeyInjection, LocalKey, LocalKeyCustodian, LocalKeyStore};
pub use pins::{PathSegment, PinPath, PinQuery, UpstreamPin, pinned};

/// One credential header value as its custodian supplies it.
///
/// Local custody supplies the exact value. The bytes are zeroized on drop and
/// never printed.
pub struct CredentialField(Zeroizing<Vec<u8>>);

impl CredentialField {
    /// The exact header value.
    #[must_use]
    pub fn exact(value: Vec<u8>) -> Self {
        Self(Zeroizing::new(value))
    }

    /// `Bearer <token>`.
    #[must_use]
    pub fn bearer(token: &[u8]) -> Self {
        let mut value = Vec::with_capacity(7 + token.len());
        value.extend_from_slice(b"Bearer ");
        value.extend_from_slice(token);
        Self::exact(value)
    }

    /// The header value bytes, moved out without a copy.
    #[must_use]
    pub fn into_header_bytes(mut self) -> Vec<u8> {
        std::mem::take(&mut *self.0)
    }
}

impl fmt::Debug for CredentialField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialField(<redacted>)")
    }
}

/// What a custodian keeps held while a request carries its fields, such as
/// a credential store's Ready lease: refresh, replacement and logout of that
/// record wait until it is dropped.
pub struct CredentialHold(Box<dyn std::any::Any + Send>);

impl CredentialHold {
    /// Holds `guard`.
    #[must_use]
    pub fn new(guard: impl Send + 'static) -> Self {
        Self(Box::new(guard))
    }

    /// The held guard.
    #[must_use]
    pub fn into_inner(self) -> Box<dyn std::any::Any + Send> {
        self.0
    }
}

impl fmt::Debug for CredentialHold {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialHold(<held>)")
    }
}

/// The credential fields of one request, in the order the custodian's
/// injection rule names them, and what the custodian holds while they are in
/// use. Names are lowercase header names.
#[derive(Debug, Default)]
pub struct CredentialFields {
    fields: Vec<(&'static str, CredentialField)>,
    hold: Option<CredentialHold>,
}

impl CredentialFields {
    /// No fields yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            fields: Vec::new(),
            hold: None,
        }
    }

    /// Appends one field.
    pub fn push(&mut self, name: &'static str, field: CredentialField) {
        self.fields.push((name, field));
    }

    /// Keeps `hold` with these fields until a caller takes it.
    #[must_use]
    pub fn holding(mut self, hold: CredentialHold) -> Self {
        self.hold = Some(hold);
        self
    }

    /// Takes what the custodian holds for these fields, for a caller that
    /// keeps it until the request's response head.
    pub fn take_hold(&mut self) -> Option<CredentialHold> {
        self.hold.take()
    }

    /// The field names, in order.
    pub fn names(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.fields.iter().map(|(name, _)| *name)
    }

    /// Takes the field named `name`.
    pub fn take(&mut self, name: &str) -> Option<CredentialField> {
        let index = self.fields.iter().position(|(field, _)| *field == name)?;
        Some(self.fields.remove(index).1)
    }

    /// The number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    /// Whether there are no fields.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }
}

impl IntoIterator for CredentialFields {
    type Item = (&'static str, CredentialField);
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.fields.into_iter()
    }
}

/// One custodied account: `<kind>` or `<kind>/<name>`, each segment
/// `[a-z][a-z0-9-]{0,31}`, the name `default` reserved. Kinds are opaque
/// here; the provider vocabulary belongs to the product.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CustodySlot(String);

impl CustodySlot {
    /// Checks the slot grammar.
    ///
    /// # Errors
    /// Refuses a malformed kind or name and the reserved name `default`.
    pub fn new(value: impl Into<String>) -> Result<Self, CustodyError> {
        let value = value.into();
        let (kind, name) = match value.split_once('/') {
            Some((kind, name)) => (kind, Some(name)),
            None => (value.as_str(), None),
        };
        let valid = is_segment(kind)
            && match name {
                Some("default") => false,
                Some(name) => is_segment(name),
                None => true,
            };
        if valid {
            Ok(Self(value))
        } else {
            Err(CustodyError::UnknownAccount)
        }
    }

    /// The full reference.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The kind segment.
    #[must_use]
    pub fn kind(&self) -> &str {
        self.0
            .split_once('/')
            .map_or(self.0.as_str(), |(kind, _)| kind)
    }

    /// The local name, or `None` for the kind's default account.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.0.split_once('/').map(|(_, name)| name)
    }
}

impl fmt::Display for CustodySlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn is_segment(value: &str) -> bool {
    let bytes = value.as_bytes();
    matches!(bytes.first(), Some(b'a'..=b'z'))
        && bytes.len() <= 32
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-'))
}

/// A SHA-256 digest of non-secret identity, compared exactly.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct IdentityDigest([u8; 32]);

impl IdentityDigest {
    /// Hashes `parts` under `domain`, each part length-prefixed.
    #[must_use]
    pub fn derive(domain: &[u8], parts: &[&[u8]]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update((domain.len() as u64).to_be_bytes());
        hasher.update(domain);
        for part in parts {
            hasher.update((part.len() as u64).to_be_bytes());
            hasher.update(part);
        }
        Self(hasher.finalize().into())
    }

    /// Wraps digest bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Parses 64 lowercase hex digits.
    #[must_use]
    pub fn from_hex(value: &str) -> Option<Self> {
        let bytes = value.as_bytes();
        if bytes.len() != 64 {
            return None;
        }
        let mut decoded = [0_u8; 32];
        for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
            decoded[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
        }
        Some(Self(decoded))
    }

    /// The digest bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 64 lowercase hex digits.
    #[must_use]
    pub fn to_hex(&self) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = String::with_capacity(64);
        for byte in self.0 {
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        encoded
    }
}

const fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Debug for IdentityDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("IdentityDigest")
            .field(&self.to_hex())
            .finish()
    }
}

/// The credential state a request is prepared against. A custodian supplies
/// fields only while its credential still has exactly this account binding
/// and generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExpectedFence {
    /// Digest of the provider account identity.
    pub account_binding: IdentityDigest,
    /// The credential generation.
    pub generation: CredentialGeneration,
}

/// A custodied credential's lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CustodyStatus {
    /// No credential is stored.
    SignedOut,
    /// Usable.
    Ready,
    /// The access token aged out and the stored refresh cannot renew it.
    RefreshRequired,
    /// A claimed refresh is in flight.
    Refreshing,
    /// A refresh outcome is unknown; the credential is fenced.
    Quarantined,
    /// The stored login cannot be used by this build; log in again.
    LoginRequired,
    /// The record was written under another integration profile.
    ProfileChanged,
    /// A newer build wrote the record.
    WrittenByNewerBuild,
    /// The provider or a logout ended the grant.
    Revoked,
}

impl CustodyStatus {
    /// The custody status of a stored subscription credential's offline
    /// lifecycle; a lifecycle this build does not know is quarantined.
    #[must_use]
    pub const fn from_lifecycle(lifecycle: crate::OfflineCredentialLifecycle) -> Self {
        use crate::OfflineCredentialLifecycle as Lifecycle;
        match lifecycle {
            Lifecycle::SignedOut => Self::SignedOut,
            Lifecycle::Ready => Self::Ready,
            Lifecycle::RefreshRequired => Self::RefreshRequired,
            Lifecycle::Refreshing => Self::Refreshing,
            Lifecycle::Revoked => Self::Revoked,
            Lifecycle::LoginRequired => Self::LoginRequired,
            Lifecycle::WrittenByNewerBuild => Self::WrittenByNewerBuild,
            Lifecycle::Quarantined => Self::Quarantined,
        }
    }
}

/// Where a custodied API key resolved from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeySource {
    /// An environment variable won.
    Environment,
    /// The private key file.
    File,
    /// No key is configured.
    Missing,
}

/// Non-secret API-key facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyFacts {
    /// Where the key came from.
    pub source: KeySource,
    /// The key's non-secret generation fingerprint, lowercase hex.
    pub fingerprint: Option<String>,
    /// The environment variable that won, when one did.
    pub env_var: Option<String>,
}

/// Whether the operator's compatibility acceptance is current.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Consent {
    /// The acceptance matches the active profile.
    Current,
    /// The notice must be accepted again.
    Required,
    /// The compiled profile is no longer active.
    Inactive,
}

/// The integration profile a subscription credential is bound to.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileFacts {
    /// The profile id.
    pub id: String,
    /// The consent-bearing contract revision.
    pub contract_revision: String,
    /// The acceptance state.
    pub consent: Consent,
    /// The exclusive review expiry in Unix milliseconds, for a windowed profile.
    pub review_expires_at_unix_ms: Option<u64>,
}

/// The non-secret view of one custodied credential. Custody location
/// (machine, path, gateway address) is never a fact. Debug output names the
/// wire-identity keys, not their values.
#[derive(Clone, Eq, PartialEq)]
pub struct AccountFacts {
    /// The account.
    pub slot: CustodySlot,
    /// Lifecycle state.
    pub status: CustodyStatus,
    /// The stored generation, when a record exists.
    pub generation: Option<CredentialGeneration>,
    /// When the access token expires, in Unix milliseconds, when known.
    pub access_expires_at_unix_ms: Option<u64>,
    /// One-way digest of the login selection epoch. A kind whose route
    /// binds the raw epoch (`SuperGrok`) also reports it as wire identity.
    pub selection_epoch_digest: Option<IdentityDigest>,
    /// Digest of the provider account identity: the fence's account half.
    pub account_binding: Option<IdentityDigest>,
    /// Digest of the credential authority (kind, slot and profile).
    pub credential_authority: Option<IdentityDigest>,
    /// The integration profile, for a subscription credential.
    pub profile: Option<ProfileFacts>,
    /// Key facts, for an API key.
    pub key: Option<KeyFacts>,
    /// Raw non-secret identity values a caller lowers into request bodies,
    /// under provider-owned keys.
    pub wire_identity: BTreeMap<String, String>,
}

impl fmt::Debug for AccountFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AccountFacts")
            .field("slot", &self.slot)
            .field("status", &self.status)
            .field("generation", &self.generation)
            .field("access_expires_at_unix_ms", &self.access_expires_at_unix_ms)
            .field("selection_epoch_digest", &self.selection_epoch_digest)
            .field("account_binding", &self.account_binding)
            .field("credential_authority", &self.credential_authority)
            .field("profile", &self.profile)
            .field("key", &self.key)
            .field(
                "wire_identity",
                &self.wire_identity.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl AccountFacts {
    /// Facts for a slot that holds nothing.
    #[must_use]
    pub const fn signed_out(slot: CustodySlot) -> Self {
        Self {
            slot,
            status: CustodyStatus::SignedOut,
            generation: None,
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
            account_binding: None,
            credential_authority: None,
            profile: None,
            key: None,
            wire_identity: BTreeMap::new(),
        }
    }

    /// The fence a request prepared against these facts carries, when the
    /// credential has an identity and a generation.
    #[must_use]
    pub fn expected_fence(&self) -> Option<ExpectedFence> {
        Some(ExpectedFence {
            account_binding: self.account_binding?,
            generation: self.generation?,
        })
    }

    /// Checks `expected` against these facts, as a custodian does before it
    /// supplies fields.
    ///
    /// # Errors
    /// [`CustodyError::NotReady`] unless Ready, [`CustodyError::AccountChanged`]
    /// for another account and [`CustodyError::FenceMoved`] for another
    /// generation.
    pub fn check_fence(&self, expected: &ExpectedFence) -> Result<(), CustodyError> {
        if self.status != CustodyStatus::Ready {
            return Err(CustodyError::NotReady {
                facts: Box::new(self.clone()),
            });
        }
        if self.account_binding != Some(expected.account_binding) {
            return Err(CustodyError::AccountChanged);
        }
        if self.generation != Some(expected.generation) {
            return Err(CustodyError::FenceMoved {
                facts: Box::new(self.clone()),
            });
        }
        Ok(())
    }
}

/// Which refresh a caller asks the single writer for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RefreshRequest {
    /// Refresh only when the custodian's clock says the access token is due
    /// and it still holds `expected`; otherwise answer current facts.
    IfDue {
        /// The credential state the caller holds.
        expected: ExpectedFence,
    },
    /// The provider rejected `rejected` with HTTP 401. Refresh only while the
    /// custodian still holds that generation and login selection; otherwise
    /// answer current facts. Nothing is resent.
    AfterRejection {
        /// The rejected credential state.
        rejected: ExpectedFence,
        /// Digest of the rejected login selection epoch, when the route has one.
        selection_epoch_digest: Option<IdentityDigest>,
    },
    /// The operator forces one grant regardless of expiry.
    Operator,
}

/// Why a custodian refused. Never carries secret material.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum CustodyError {
    /// The credential is not usable.
    #[error("credential is not ready")]
    NotReady {
        /// Current facts.
        facts: Box<AccountFacts>,
    },
    /// The credential's generation or selection differs from the request's fence.
    #[error("credential fence moved")]
    FenceMoved {
        /// Current facts.
        facts: Box<AccountFacts>,
    },
    /// The credential now belongs to another provider account.
    #[error("credential account changed")]
    AccountChanged,
    /// The custodian holds no such account.
    #[error("credential account is unknown")]
    UnknownAccount,
    /// The upstream origin, method or path is not pinned for this credential.
    #[error("upstream is not pinned for this credential")]
    UpstreamNotPinned,
    /// The request carries a header the caller may not send.
    #[error("request carries a reserved header")]
    ReservedHeader,
    /// The request's credential slots differ from the credential's injection rule.
    #[error("credential header slots do not match")]
    CustodiedHeadersMismatch,
    /// The custodian's kill switch refuses the account or every call.
    #[error("credential custody is disabled")]
    Disabled,
    /// The caller's scope does not cover this account.
    #[error("credential custody scope denied")]
    ScopeDenied,
    /// The custodian speaks no protocol version this caller speaks.
    #[error("credential custodian protocol version mismatch")]
    VersionMismatch {
        /// The versions it speaks.
        supported: Vec<u16>,
    },
    /// The custodian is at capacity, or the credential is contended or moving.
    #[error("credential custodian is busy")]
    Busy {
        /// Advised wait.
        retry_after_ms: u64,
    },
    /// The custodian could not be reached or did not answer.
    #[error("credential custodian is unavailable")]
    Unavailable,
    /// The custodian's credential store failed.
    #[error("credential store failed")]
    Store,
    /// The custodian failed internally.
    #[error("credential custodian failed")]
    Internal,
}

impl From<crate::PrivateStoreError> for CustodyError {
    fn from(error: crate::PrivateStoreError) -> Self {
        use crate::PrivateStoreError as Store;
        match error {
            Store::StoreBusy
            | Store::FenceMismatch
            | Store::RecordMissing
            | Store::CredentialNotReady => Self::Busy {
                retry_after_ms: 100,
            },
            _ => Self::Store,
        }
    }
}

/// A boxed custody future.
pub type CustodyFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, CustodyError>> + Send + 'a>>;

/// Whoever holds a credential: local custody over this process's stores, or
/// a credential gateway.
pub trait CredentialCustodian: Send + Sync {
    /// Current facts for `slot`, read fresh.
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts>;

    /// Runs one single-writer refresh, or answers current facts when the
    /// request's condition does not hold.
    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts>;

    /// The credential fields for one request prepared against `expected`, in
    /// injection order. Refuses unless the credential still holds exactly
    /// that fence ([`AccountFacts::check_fence`]).
    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "unit tests")]
mod tests;
