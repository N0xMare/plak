//! The remote custodian: facts, refresh and custodied fields through a
//! credential gateway (spec 01 §7.1, §7.5).
//!
//! Its fields are custodied placeholders that a relaying
//! `plakous_http::Client` sends as `[name, null]`; the gateway fills them at
//! injection after re-checking the fence. Facts are fetched fresh for every
//! call: there is no facts cache.

use std::collections::BTreeMap;
use std::sync::Arc;

use plakous_gateway_protocol as protocol;
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_http::relay::{RelayCallError, RelayClient};

use super::{
    AccountFacts, Consent, CredentialCustodian, CredentialField, CredentialFields, CustodyError,
    CustodyFuture, CustodySlot, CustodyStatus, ExpectedFence, IdentityDigest, KeyFacts, KeySource,
    ProfileFacts, RefreshRequest,
};
use crate::CredentialGeneration;

impl CredentialField {
    /// The custodied placeholder for `slot` at `expected`: a credential the
    /// gateway fills at this header's position.
    #[must_use]
    pub fn custodied(slot: &CustodySlot, expected: &ExpectedFence) -> Self {
        Self::exact(plakous_http::relay::custodied_placeholder(
            slot.as_str(),
            &expected.account_binding.to_hex(),
            expected.generation.get(),
        ))
    }
}

/// Custody through a credential gateway.
#[derive(Clone)]
pub struct GatewayCustodian {
    relay: Arc<RelayClient>,
    injection: fn(&str) -> Option<&'static [&'static str]>,
}

impl std::fmt::Debug for GatewayCustodian {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GatewayCustodian(<gateway>)")
    }
}

impl GatewayCustodian {
    /// Custody through `relay`; `injection` names each kind's credential
    /// header names in injection order (the provider crates' tables).
    #[must_use]
    pub fn new(
        relay: Arc<RelayClient>,
        injection: fn(&str) -> Option<&'static [&'static str]>,
    ) -> Self {
        Self { relay, injection }
    }

    /// The relay client, for the `plakous_http::Client` routes dispatch
    /// through (`Client::with_relay`).
    #[must_use]
    pub fn relay(&self) -> &Arc<RelayClient> {
        &self.relay
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        slot: &CustodySlot,
        endpoint: protocol::Endpoint,
        request: &impl serde::Serialize,
    ) -> Result<T, CustodyError> {
        let body = protocol::call::encode(request).map_err(|_| CustodyError::Internal)?;
        let reply = self
            .relay
            .call(endpoint, body, &RelayClient::call_control())
            .await
            .map_err(|error| call_error(&error, slot))?;
        protocol::call::decode(&reply).map_err(|_| CustodyError::Internal)
    }
}

impl CredentialCustodian for GatewayCustodian {
    fn facts<'a>(&'a self, slot: &'a CustodySlot) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let request = protocol::facts::FactsRequest {
                accounts: vec![gateway_account(slot)?],
            };
            let reply: protocol::facts::FactsReply =
                self.call(slot, protocol::Endpoint::Facts, &request).await?;
            let [facts] = <[_; 1]>::try_from(reply.facts).map_err(|_| CustodyError::Internal)?;
            account_facts(&facts, slot)
        })
    }

    fn refresh<'a>(
        &'a self,
        slot: &'a CustodySlot,
        request: RefreshRequest,
    ) -> CustodyFuture<'a, AccountFacts> {
        Box::pin(async move {
            let account = gateway_account(slot)?;
            let mode = match request {
                RefreshRequest::IfDue { expected } => protocol::facts::RefreshMode::IfDue {
                    expected: wire_fence(&expected),
                },
                RefreshRequest::AfterRejection {
                    rejected,
                    selection_epoch_digest,
                } => protocol::facts::RefreshMode::AfterRejection {
                    rejected: wire_fence(&rejected),
                    selection_epoch_digest: selection_epoch_digest
                        .map(|digest| protocol::HexDigest::from_bytes(digest.as_bytes())),
                },
                RefreshRequest::Operator => {
                    let reply: protocol::admin::AdminResponse = self
                        .call(
                            slot,
                            protocol::Endpoint::Admin,
                            &protocol::admin::AdminRequest::Refresh { account },
                        )
                        .await?;
                    let protocol::admin::AdminResponse::Refreshed { facts } = reply else {
                        return Err(CustodyError::Internal);
                    };
                    return account_facts(&facts, slot);
                }
            };
            let reply: protocol::facts::RefreshReply = self
                .call(
                    slot,
                    protocol::Endpoint::Refresh,
                    &protocol::facts::RefreshRequest { account, mode },
                )
                .await?;
            account_facts(&reply.facts, slot)
        })
    }

    fn fields<'a>(
        &'a self,
        slot: &'a CustodySlot,
        expected: &'a ExpectedFence,
    ) -> CustodyFuture<'a, CredentialFields> {
        Box::pin(async move {
            let names = (self.injection)(slot.kind()).ok_or(CustodyError::UnknownAccount)?;
            let mut fields = CredentialFields::new();
            for name in names {
                fields.push(name, CredentialField::custodied(slot, expected));
            }
            Ok(fields)
        })
    }
}

fn gateway_account(slot: &CustodySlot) -> Result<protocol::GatewayAccount, CustodyError> {
    protocol::GatewayAccount::new(slot.as_str()).map_err(|_| CustodyError::UnknownAccount)
}

fn wire_fence(fence: &ExpectedFence) -> protocol::envelope::ExpectedFence {
    protocol::envelope::ExpectedFence {
        account_binding: protocol::HexDigest::from_bytes(fence.account_binding.as_bytes()),
        generation: fence.generation.get(),
    }
}

fn call_error(error: &RelayCallError, slot: &CustodySlot) -> CustodyError {
    match error {
        RelayCallError::Refused(refusal) => refusal_error(refusal, slot),
        RelayCallError::VersionMismatch { supported } => CustodyError::VersionMismatch {
            supported: supported.clone(),
        },
        RelayCallError::Transport(_) => CustodyError::Unavailable,
        RelayCallError::InvalidReply => CustodyError::Internal,
    }
}

/// A gateway refusal about `slot` as the custody refusal it means.
#[must_use]
pub fn refusal_error(refusal: &GatewayRefusal, slot: &CustodySlot) -> CustodyError {
    match refusal {
        GatewayRefusal::Unauthenticated {} | GatewayRefusal::ScopeDenied {} => {
            CustodyError::ScopeDenied
        }
        GatewayRefusal::VersionUnsupported { supported } => CustodyError::VersionMismatch {
            supported: supported.clone(),
        },
        GatewayRefusal::GatewayDisabled {} | GatewayRefusal::AccountDisabled {} => {
            CustodyError::Disabled
        }
        GatewayRefusal::AccountUnknown {} => CustodyError::UnknownAccount,
        GatewayRefusal::CredentialNotReady { status } => CustodyError::NotReady {
            facts: Box::new(AccountFacts {
                status: custody_status(*status),
                ..AccountFacts::signed_out(slot.clone())
            }),
        },
        GatewayRefusal::FenceMoved { facts } => match account_facts(facts, slot) {
            Ok(facts) => CustodyError::FenceMoved {
                facts: Box::new(facts),
            },
            Err(error) => error,
        },
        GatewayRefusal::AccountChanged {} => CustodyError::AccountChanged,
        GatewayRefusal::UpstreamNotPinned {} => CustodyError::UpstreamNotPinned,
        GatewayRefusal::ReservedHeader {} => CustodyError::ReservedHeader,
        GatewayRefusal::CustodiedHeadersMismatch {} => CustodyError::CustodiedHeadersMismatch,
        GatewayRefusal::Busy { retry_after_ms } => CustodyError::Busy {
            retry_after_ms: *retry_after_ms,
        },
        GatewayRefusal::EnvelopeInvalid {}
        | GatewayRefusal::LimitExceeded {}
        | GatewayRefusal::Internal {} => CustodyError::Internal,
    }
}

/// A custody refusal as the gateway refusal a server answers with.
#[must_use]
pub fn gateway_refusal(error: &CustodyError, instance: &protocol::UuidText) -> GatewayRefusal {
    match error {
        CustodyError::NotReady { facts } => GatewayRefusal::CredentialNotReady {
            status: wire_status(facts.status),
        },
        CustodyError::FenceMoved { facts } => match wire_facts(facts, instance, 0) {
            Ok(facts) => GatewayRefusal::FenceMoved {
                facts: Box::new(facts),
            },
            Err(_) => GatewayRefusal::Internal {},
        },
        CustodyError::AccountChanged => GatewayRefusal::AccountChanged {},
        CustodyError::UnknownAccount => GatewayRefusal::AccountUnknown {},
        CustodyError::UpstreamNotPinned => GatewayRefusal::UpstreamNotPinned {},
        CustodyError::ReservedHeader => GatewayRefusal::ReservedHeader {},
        CustodyError::CustodiedHeadersMismatch => GatewayRefusal::CustodiedHeadersMismatch {},
        CustodyError::Disabled => GatewayRefusal::AccountDisabled {},
        CustodyError::ScopeDenied => GatewayRefusal::ScopeDenied {},
        CustodyError::VersionMismatch { supported } => GatewayRefusal::VersionUnsupported {
            supported: supported.clone(),
        },
        CustodyError::Busy { retry_after_ms } => GatewayRefusal::Busy {
            retry_after_ms: *retry_after_ms,
        },
        CustodyError::Unavailable | CustodyError::Store | CustodyError::Internal => {
            GatewayRefusal::Internal {}
        }
    }
}

const fn custody_status(status: protocol::facts::CredentialStatus) -> CustodyStatus {
    use protocol::facts::CredentialStatus as Wire;
    match status {
        Wire::SignedOut => CustodyStatus::SignedOut,
        Wire::Ready => CustodyStatus::Ready,
        Wire::RefreshRequired => CustodyStatus::RefreshRequired,
        Wire::Refreshing => CustodyStatus::Refreshing,
        Wire::Quarantined => CustodyStatus::Quarantined,
        Wire::LoginRequired => CustodyStatus::LoginRequired,
        Wire::ProfileChanged => CustodyStatus::ProfileChanged,
        Wire::WrittenByNewerBuild => CustodyStatus::WrittenByNewerBuild,
        Wire::Revoked => CustodyStatus::Revoked,
    }
}

const fn wire_status(status: CustodyStatus) -> protocol::facts::CredentialStatus {
    use protocol::facts::CredentialStatus as Wire;
    match status {
        CustodyStatus::SignedOut => Wire::SignedOut,
        CustodyStatus::Ready => Wire::Ready,
        CustodyStatus::RefreshRequired => Wire::RefreshRequired,
        CustodyStatus::Refreshing => Wire::Refreshing,
        CustodyStatus::Quarantined => Wire::Quarantined,
        CustodyStatus::LoginRequired => Wire::LoginRequired,
        CustodyStatus::ProfileChanged => Wire::ProfileChanged,
        CustodyStatus::WrittenByNewerBuild => Wire::WrittenByNewerBuild,
        CustodyStatus::Revoked => Wire::Revoked,
    }
}

fn digest(value: &protocol::HexDigest) -> Result<IdentityDigest, CustodyError> {
    IdentityDigest::from_hex(value.as_str()).ok_or(CustodyError::Internal)
}

/// Wire facts as custody facts. The gateway's account must be `slot`.
///
/// # Errors
/// [`CustodyError::Internal`] for facts about another account or with an
/// invalid generation or digest.
pub fn account_facts(
    facts: &protocol::facts::AccountFacts,
    slot: &CustodySlot,
) -> Result<AccountFacts, CustodyError> {
    if facts.account.as_str() != slot.as_str() {
        return Err(CustodyError::Internal);
    }
    let generation = facts
        .credential
        .generation
        .map(CredentialGeneration::new)
        .transpose()
        .map_err(|_| CustodyError::Internal)?;
    Ok(AccountFacts {
        slot: slot.clone(),
        status: custody_status(facts.credential.status),
        generation,
        access_expires_at_unix_ms: facts.credential.access_expires_at_unix_ms,
        selection_epoch_digest: facts
            .credential
            .selection_epoch_digest
            .as_ref()
            .map(digest)
            .transpose()?,
        account_binding: facts
            .identity
            .as_ref()
            .map(|identity| digest(&identity.account_binding))
            .transpose()?,
        credential_authority: facts
            .identity
            .as_ref()
            .map(|identity| digest(&identity.credential_authority))
            .transpose()?,
        profile: facts.profile.as_ref().map(|profile| ProfileFacts {
            id: profile.id.as_str().to_owned(),
            contract_revision: profile.contract_revision.as_str().to_owned(),
            consent: match profile.consent {
                protocol::facts::Consent::Current => Consent::Current,
                protocol::facts::Consent::Required => Consent::Required,
                protocol::facts::Consent::Inactive => Consent::Inactive,
            },
            review_expires_at_unix_ms: profile.review_expires_at_unix_ms,
        }),
        key: facts.key.as_ref().map(|key| KeyFacts {
            source: match key.source {
                protocol::facts::KeySource::Environment => KeySource::Environment,
                protocol::facts::KeySource::File => KeySource::File,
                protocol::facts::KeySource::Missing => KeySource::Missing,
            },
            fingerprint: key
                .fingerprint
                .as_ref()
                .map(|value| value.as_str().to_owned()),
            env_var: key.env_var.as_ref().map(|value| value.as_str().to_owned()),
        }),
        wire_identity: facts
            .wire_identity
            .entries()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
    })
}

/// Custody facts as the wire facts a gateway answers with. Profile and key
/// text that does not fit its wire bound is omitted rather than truncated.
///
/// # Errors
/// [`CustodyError::Internal`] when a wire-identity value or the entry count
/// exceeds its wire bound: identity a caller binds to is never dropped.
pub fn wire_facts(
    facts: &AccountFacts,
    instance: &protocol::UuidText,
    observed_at_unix_ms: u64,
) -> Result<protocol::facts::AccountFacts, CustodyError> {
    let hex = |digest: &IdentityDigest| protocol::HexDigest::from_bytes(digest.as_bytes());
    let identity = facts
        .account_binding
        .as_ref()
        .zip(facts.credential_authority.as_ref())
        .map(|(binding, authority)| protocol::facts::IdentityFacts {
            account_binding: hex(binding),
            credential_authority: hex(authority),
            email: None,
            plan: None,
        });
    let wire_identity = facts
        .wire_identity
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                protocol::BoundedText::new(value.clone()).map_err(|_| CustodyError::Internal)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, CustodyError>>()?;
    let wire_identity =
        protocol::facts::WireIdentity::new(wire_identity).map_err(|_| CustodyError::Internal)?;
    Ok(protocol::facts::AccountFacts {
        account: protocol::GatewayAccount::new(facts.slot.as_str())
            .unwrap_or_else(|_| unreachable!("custody slots share the account grammar")),
        instance: instance.clone(),
        observed_at_unix_ms,
        credential: protocol::facts::CredentialFacts {
            status: wire_status(facts.status),
            generation: facts.generation.map(CredentialGeneration::get),
            access_expires_at_unix_ms: facts.access_expires_at_unix_ms,
            selection_epoch_digest: facts.selection_epoch_digest.as_ref().map(hex),
        },
        identity,
        wire_identity,
        profile: facts.profile.as_ref().and_then(|profile| {
            Some(protocol::facts::ProfileFacts {
                id: protocol::BoundedText::new(profile.id.clone()).ok()?,
                contract_revision: protocol::BoundedText::new(profile.contract_revision.clone())
                    .ok()?,
                consent: match profile.consent {
                    Consent::Current => protocol::facts::Consent::Current,
                    Consent::Required => protocol::facts::Consent::Required,
                    Consent::Inactive => protocol::facts::Consent::Inactive,
                },
                review_expires_at_unix_ms: profile.review_expires_at_unix_ms,
            })
        }),
        key: facts.key.as_ref().map(|key| protocol::facts::KeyFacts {
            source: match key.source {
                KeySource::Environment => protocol::facts::KeySource::Environment,
                KeySource::File => protocol::facts::KeySource::File,
                KeySource::Missing => protocol::facts::KeySource::Missing,
            },
            fingerprint: key
                .fingerprint
                .as_ref()
                .and_then(|value| protocol::BoundedText::new(value.clone()).ok()),
            env_var: key
                .env_var
                .as_ref()
                .and_then(|value| protocol::BoundedText::new(value.clone()).ok()),
        }),
    })
}
