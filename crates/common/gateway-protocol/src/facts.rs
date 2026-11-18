//! Account facts: the non-secret view of one custodied credential that a
//! caller revalidates against, and the relay-scope facts and refresh calls.
//!
//! Facts never carry a credential, a login selection epoch or a provider
//! account id in the clear. The only raw identity values are the ones a
//! caller must lower into request bodies, carried in [`WireIdentity`] under
//! provider-owned keys. Custody location (machine, path, gateway address) is
//! never a fact.

use std::collections::BTreeMap;
use std::fmt;

use crate::account::GatewayAccount;
use crate::envelope::ExpectedFence;
use crate::text::{BoundedText, HexDigest, TextError, UuidText};

/// The most entries one [`WireIdentity`] holds.
pub const MAX_WIRE_IDENTITY_ENTRIES: usize = 8;
/// The longest [`WireIdentity`] value.
pub const MAX_WIRE_IDENTITY_VALUE_BYTES: usize = 256;

/// A credential's lifecycle state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialStatus {
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

/// The credential half of the facts.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFacts {
    /// Lifecycle state.
    pub status: CredentialStatus,
    /// The stored generation, when a record exists.
    pub generation: Option<u64>,
    /// When the access token expires, in Unix milliseconds, when known.
    pub access_expires_at_unix_ms: Option<u64>,
    /// One-way digest of the login selection epoch, never the epoch.
    pub selection_epoch_digest: Option<HexDigest>,
}

/// The validated provider identity.
#[derive(Clone, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityFacts {
    /// Domain-separated digest of the provider account identity: the session
    /// account binding.
    pub account_binding: HexDigest,
    /// Digest of the credential authority (kind, slot and profile), never a
    /// path or an address.
    pub credential_authority: HexDigest,
    /// Provider-reported email, for presentation only.
    pub email: Option<BoundedText<320>>,
    /// Provider-reported plan, for presentation only.
    pub plan: Option<BoundedText<64>>,
}

impl fmt::Debug for IdentityFacts {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IdentityFacts")
            .field("account_binding", &self.account_binding)
            .field("credential_authority", &self.credential_authority)
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .field("plan", &self.plan)
            .finish()
    }
}

/// Whether the operator's compatibility acceptance is current.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consent {
    /// The acceptance matches the active profile.
    Current,
    /// The notice must be accepted again.
    Required,
    /// The compiled profile is no longer active.
    Inactive,
}

/// The integration profile a subscription credential is bound to.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFacts {
    /// The profile id.
    pub id: BoundedText<128>,
    /// The consent-bearing contract revision.
    pub contract_revision: BoundedText<128>,
    /// The acceptance state.
    pub consent: Consent,
    /// The exclusive review expiry in Unix milliseconds, for a windowed profile.
    pub review_expires_at_unix_ms: Option<u64>,
}

/// Where an API key resolved from on the gateway.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// The gateway's environment variable won.
    Environment,
    /// The private key file.
    File,
    /// No key is configured.
    Missing,
}

/// API-key facts.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyFacts {
    /// Where the key came from.
    pub source: KeySource,
    /// The key's non-secret fingerprint.
    pub fingerprint: Option<BoundedText<80>>,
    /// The name of the environment variable that won.
    pub env_var: Option<BoundedText<64>>,
}

/// Raw, non-secret identity values a caller lowers into request bodies,
/// keyed by provider-owned names matching `[a-z][a-z0-9_]{0,63}`.
#[derive(Clone, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(
    try_from = "BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>",
    into = "BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>"
)]
pub struct WireIdentity(BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>);

impl WireIdentity {
    /// Checks the entry count and key grammar.
    ///
    /// # Errors
    /// Refuses more than [`MAX_WIRE_IDENTITY_ENTRIES`] entries or a malformed key.
    pub fn new(
        entries: BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>,
    ) -> Result<Self, TextError> {
        if entries.len() > MAX_WIRE_IDENTITY_ENTRIES {
            return Err(TextError::new("wire identity has too many entries"));
        }
        if !entries.keys().all(|key| is_identity_key(key)) {
            return Err(TextError::new("wire identity key is malformed"));
        }
        Ok(Self(entries))
    }

    /// The value under `key`.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(BoundedText::as_str)
    }

    /// The entries in key order.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
    }
}

impl TryFrom<BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>> for WireIdentity {
    type Error = TextError;

    fn try_from(
        entries: BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>>,
    ) -> Result<Self, Self::Error> {
        Self::new(entries)
    }
}

impl From<WireIdentity> for BTreeMap<String, BoundedText<MAX_WIRE_IDENTITY_VALUE_BYTES>> {
    fn from(value: WireIdentity) -> Self {
        value.0
    }
}

fn is_identity_key(key: &str) -> bool {
    let bytes = key.as_bytes();
    matches!(bytes.first(), Some(b'a'..=b'z'))
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// The non-secret authority view of one custodied account.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountFacts {
    /// The account.
    pub account: GatewayAccount,
    /// The gateway instance that observed these facts.
    pub instance: UuidText,
    /// When the gateway observed them, in Unix milliseconds on its clock.
    pub observed_at_unix_ms: u64,
    /// Credential state.
    pub credential: CredentialFacts,
    /// Validated identity, once a credential has one.
    pub identity: Option<IdentityFacts>,
    /// Raw identity values the caller lowers into request bodies.
    pub wire_identity: WireIdentity,
    /// The integration profile, for a subscription credential.
    pub profile: Option<ProfileFacts>,
    /// Key facts, for an API key.
    pub key: Option<KeyFacts>,
}

impl AccountFacts {
    /// The fence a request prepared against these facts carries, when the
    /// credential has an identity and a generation.
    #[must_use]
    pub fn expected_fence(&self) -> Option<ExpectedFence> {
        Some(ExpectedFence {
            account_binding: self.identity.as_ref()?.account_binding.clone(),
            generation: self.credential.generation?,
        })
    }
}

/// `POST /v<N>/facts`: current facts for each named account.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsRequest {
    /// The accounts, at most [`crate::call::MAX_FACTS_ACCOUNTS`].
    pub accounts: Vec<GatewayAccount>,
}

/// The answer to [`FactsRequest`], in request order.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactsReply {
    /// One entry per requested account.
    pub facts: Vec<AccountFacts>,
}

/// Which refresh a caller asks the single writer for.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum RefreshMode {
    /// Refresh only when the gateway's clock says the access token is due and
    /// the store still holds `expected`; otherwise return current facts.
    IfDue {
        /// The credential state the caller holds.
        expected: ExpectedFence,
    },
    /// The provider rejected `rejected` with HTTP 401. Refresh only while the
    /// store still holds that generation and login selection; otherwise
    /// return current facts. Nothing is resent.
    AfterRejection {
        /// The rejected credential state.
        rejected: ExpectedFence,
        /// Digest of the rejected login selection epoch, when the route has one.
        selection_epoch_digest: Option<HexDigest>,
    },
}

/// `POST /v<N>/refresh`: one single-writer refresh request.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    /// The account.
    pub account: GatewayAccount,
    /// The refresh condition.
    pub mode: RefreshMode,
}

/// The facts after a [`RefreshRequest`], refreshed or not.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshReply {
    /// Current facts.
    pub facts: AccountFacts,
}
