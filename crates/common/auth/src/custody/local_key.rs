//! Local custody of API keys: the key an application holds for one slot,
//! lowered into its provider's credential header.

use std::sync::Arc;

use super::{
    AccountFacts, CredentialCustodian, CredentialField, CredentialFields, CustodyError,
    CustodyFuture, CustodySlot, CustodyStatus, ExpectedFence, IdentityDigest, KeyFacts, KeySource,
    RefreshRequest,
};
use crate::CredentialGeneration;
use crate::api_key::{
    ApiKey, CredentialProviderTag, ExposeApiKey as _, KEY_BINDING_IDENTITY, provider_key_binding,
};

const KEY_ACCOUNT_DOMAIN: &[u8] = b"plak.custody.key-account.v1";
const KEY_AUTHORITY_DOMAIN: &[u8] = b"plak.custody.key-authority.v1";

/// How an API key is lowered into its credential header value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyHeaderForm {
    /// `Bearer <key>`.
    Bearer,
    /// The key itself.
    Raw,
}

impl KeyHeaderForm {
    /// The header value for `key`.
    #[must_use]
    pub fn field(self, key: &ApiKey) -> CredentialField {
        let text = key.expose_secret().as_bytes();
        match self {
            Self::Bearer => CredentialField::bearer(text),
            Self::Raw => CredentialField::exact(text.to_vec()),
        }
    }
}

/// Which header one kind's key fills, and how.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyInjection {
    /// The provider whose generation fingerprint the key mints.
    pub provider: CredentialProviderTag,
    /// The lowercase header name.
    pub header: &'static str,
    /// The value form.
    pub form: KeyHeaderForm,
}

/// One locally held key and where it came from.
#[derive(Debug)]
pub struct LocalKey {
    /// The key.
    pub key: ApiKey,
    /// Its source.
    pub source: KeySource,
    /// The environment variable that won, when one did.
    pub env_var: Option<String>,
}

/// The application's key lookup for one slot.
pub trait LocalKeyStore: Send + Sync {
    /// The slot's current key, or `None` when none is configured.
    ///
    /// # Errors
    /// [`CustodyError::Store`] when the key cannot be read safely.
    fn key(&self, slot: &CustodySlot) -> Result<Option<LocalKey>, CustodyError>;
}

/// Local custody of API keys over an application's [`LocalKeyStore`].
///
/// A key's account binding is a digest of its slot; its generation is the
/// leading eight bytes of its generation fingerprint, so replacing the key
/// moves the fence without changing the account. A key whose route binds a
/// digest of its bytes ([`provider_key_binding`]) reports that digest as
/// wire identity under [`KEY_BINDING_IDENTITY`].
#[derive(Clone)]
pub struct LocalKeyCustodian {
    store: Arc<dyn LocalKeyStore>,
    injection: fn(&str) -> Option<KeyInjection>,
}

impl LocalKeyCustodian {
    /// Custody over `store`, with `injection` naming each kind's header rule;
    /// a kind without one is unknown.
    #[must_use]
    pub fn new(store: Arc<dyn LocalKeyStore>, injection: fn(&str) -> Option<KeyInjection>) -> Self {
        Self { store, injection }
    }

    fn current(
        &self,
        slot: &CustodySlot,
    ) -> Result<(AccountFacts, Option<LocalKey>), CustodyError> {
        let injection = (self.injection)(slot.kind()).ok_or(CustodyError::UnknownAccount)?;
        let Some(local) = self.store.key(slot)? else {
            let mut facts = AccountFacts::signed_out(slot.clone());
            facts.key = Some(KeyFacts {
                source: KeySource::Missing,
                fingerprint: None,
                env_var: None,
            });
            return Ok((facts, None));
        };
        let fingerprint = local
            .key
            .generation_grant(injection.provider, ())
            .fingerprint();
        let leading: [u8; 8] = fingerprint.as_bytes()[..8]
            .try_into()
            .map_err(|_| CustodyError::Internal)?;
        let generation = CredentialGeneration::new(u64::from_be_bytes(leading).max(1))
            .map_err(|_| CustodyError::Internal)?;
        let mut facts = AccountFacts::signed_out(slot.clone());
        facts.status = CustodyStatus::Ready;
        facts.generation = Some(generation);
        facts.account_binding = Some(IdentityDigest::derive(
            KEY_ACCOUNT_DOMAIN,
            &[slot.as_str().as_bytes()],
        ));
        facts.credential_authority = Some(IdentityDigest::derive(
            KEY_AUTHORITY_DOMAIN,
            &[slot.as_str().as_bytes()],
        ));
        facts.key = Some(KeyFacts {
            source: local.source,
            fingerprint: Some(fingerprint.to_hex()),
            env_var: local.env_var.clone(),
        });
        if let Some(binding) = provider_key_binding(injection.provider, &local.key) {
            facts.wire_identity.insert(
                KEY_BINDING_IDENTITY.to_owned(),
                IdentityDigest::from_bytes(*binding.bytes()).to_hex(),
            );
        }
        Ok((facts, Some(local)))
    }
}

impl std::fmt::Debug for LocalKeyCustodian {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LocalKeyCustodian(<redacted>)")
    }
}

impl CredentialCustodian for LocalKeyCustodian {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move { self.current(slot).map(|(facts, _)| facts) })
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        _request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        // A key has no grant to renew: every refresh answers current facts.
        Box::pin(async move { self.current(slot).map(|(facts, _)| facts) })
    }

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        Box::pin(async move {
            let injection = (self.injection)(slot.kind()).ok_or(CustodyError::UnknownAccount)?;
            let (facts, local) = self.current(slot)?;
            facts.check_fence(expected)?;
            let local = local.ok_or(CustodyError::Internal)?;
            let mut fields = CredentialFields::new();
            fields.push(injection.header, injection.form.field(&local.key));
            Ok(fields)
        })
    }
}
