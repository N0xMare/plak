//! Scripted [`GatewayService`](plakous_http::relay_server::GatewayService).
//!
//! The relay server core owns the protocol. This service only decides the
//! scripted answer: authenticate, inject an API-key value, refuse, or stall.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use plakous_gateway_protocol::envelope::RelayEnvelope;
use plakous_gateway_protocol::facts::{
    AccountFacts, CredentialFacts, CredentialStatus, IdentityFacts,
};
use plakous_gateway_protocol::refusal::GatewayRefusal;
use plakous_gateway_protocol::{Endpoint, GatewayAccount, HexDigest, UuidText};
use plakous_http::SensitiveBytes;
use plakous_http::relay_server::{
    GatewayService, Injection, Peer, RelayOutcome, RelayRecord, ServiceFuture,
};
use tokio::sync::Notify;

use super::GatewayScript;
use super::upstream::{self, Upstream};

pub(super) const LOGICAL: &str = "https://api.anthropic.com";
pub(super) const ACCOUNT: &str = "anthropic";
pub(super) const BINDING: &str = "abababababababababababababababababababababababababababababababab";
pub(super) const GENERATION: u64 = 1;

pub(super) struct Scripted {
    credentials: Vec<(String, Vec<u8>)>,
    key: Option<Vec<u8>>,
    pub(super) upstream: Arc<Upstream>,
    scripts: Mutex<VecDeque<GatewayScript>>,
    fence: Mutex<Option<(u64, AccountFacts)>>,
    relay_ordinal: AtomicU64,
    drop_gate: Mutex<Option<Arc<Notify>>>,
    last_outcome: Mutex<Option<RelayOutcome>>,
}

impl Scripted {
    pub(super) fn new(credentials: Vec<(String, Vec<u8>)>, key: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            credentials,
            key: key.map(String::into_bytes),
            upstream: Arc::new(Upstream::new()),
            scripts: Mutex::new(VecDeque::new()),
            fence: Mutex::new(None),
            relay_ordinal: AtomicU64::new(0),
            drop_gate: Mutex::new(None),
            last_outcome: Mutex::new(None),
        })
    }

    pub(super) fn push(&self, action: GatewayScript) {
        lock(&self.scripts).push_back(action);
    }

    pub(super) fn script_fence_moved(&self, nth: u64, facts: AccountFacts) {
        *lock(&self.fence) = Some((nth, facts));
    }

    pub(super) fn arm_drop(&self, gate: Arc<Notify>) {
        *lock(&self.drop_gate) = Some(gate);
    }

    pub(super) fn clear_drop(&self) {
        *lock(&self.drop_gate) = None;
    }

    pub(super) fn last_outcome(&self) -> Option<RelayOutcome> {
        lock(&self.last_outcome).clone()
    }

    fn pop_script(&self) -> GatewayScript {
        lock(&self.scripts)
            .pop_front()
            .unwrap_or_else(|| GatewayScript::Relay(super::UpstreamReply::default()))
    }

    fn injection(&self, envelope: &RelayEnvelope) -> Result<Injection, GatewayRefusal> {
        if !envelope.url.starts_with(&format!("{LOGICAL}/")) {
            return Err(GatewayRefusal::UpstreamNotPinned {});
        }
        if envelope.expected.account_binding.as_str() != BINDING {
            return Err(GatewayRefusal::AccountChanged {});
        }
        if envelope.expected.generation != GENERATION {
            return Err(GatewayRefusal::FenceMoved {
                facts: Box::new(scripted_facts(
                    envelope.expected.generation.saturating_add(1),
                )?),
            });
        }
        let mut values = Vec::new();
        for name in envelope.custodied_names() {
            let Some((_, value)) = self
                .credentials
                .iter()
                .find(|(candidate, _)| candidate == name)
            else {
                return Err(GatewayRefusal::CustodiedHeadersMismatch {});
            };
            values.push(SensitiveBytes::new(value.clone()));
        }
        Ok(Injection::new(values))
    }
}

impl GatewayService for Scripted {
    type Principal = ();

    fn authenticate(&self, peer: &Peer, key: Option<&[u8]>) -> Result<(), GatewayRefusal> {
        if let Some(expected) = &self.key {
            return if key == Some(expected.as_slice()) {
                Ok(())
            } else {
                Err(GatewayRefusal::Unauthenticated {})
            };
        }
        match peer {
            Peer::InProcess | Peer::Unix { same_user: true } => Ok(()),
            Peer::Unix { same_user: false } | Peer::Tcp(_) => {
                Err(GatewayRefusal::Unauthenticated {})
            }
        }
    }

    fn inject<'a>(
        &'a self,
        (): &'a (),
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection> {
        Box::pin(async move {
            let ordinal = self.relay_ordinal.fetch_add(1, Ordering::SeqCst) + 1;
            if let Some((nth, facts)) = lock(&self.fence).clone()
                && ordinal == nth
            {
                return Err(GatewayRefusal::FenceMoved {
                    facts: Box::new(facts),
                });
            }
            match self.pop_script() {
                GatewayScript::Refuse(refusal) => Err(refusal),
                GatewayScript::Stall(duration) => {
                    tokio::time::sleep(duration).await;
                    let reply = upstream::encode_reply(&super::UpstreamReply::default());
                    self.upstream.push_reply(reply);
                    self.injection(envelope)
                }
                GatewayScript::DropBeforeResponse => {
                    if let Some(gate) = lock(&self.drop_gate).clone() {
                        gate.notify_one();
                    }
                    std::future::pending().await
                }
                GatewayScript::DropAfterHead => {
                    self.upstream.push_hang(upstream::hang_after_head());
                    self.injection(envelope)
                }
                GatewayScript::Relay(reply) => {
                    self.upstream.push_reply(upstream::encode_reply(&reply));
                    self.injection(envelope)
                }
            }
        })
    }

    fn call<'a>(
        &'a self,
        (): &'a (),
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>> {
        Box::pin(async move {
            match endpoint {
                Endpoint::Facts | Endpoint::Refresh => Ok(body.to_vec()),
                Endpoint::Relay | Endpoint::Admin => Err(GatewayRefusal::ScopeDenied {}),
            }
        })
    }

    fn audit(&self, record: RelayRecord) {
        *lock(&self.last_outcome) = Some(record.outcome);
    }
}

fn scripted_facts(generation: u64) -> Result<AccountFacts, GatewayRefusal> {
    let binding = HexDigest::new(BINDING).map_err(|_| GatewayRefusal::Internal {})?;
    Ok(AccountFacts {
        account: GatewayAccount::new(ACCOUNT).map_err(|_| GatewayRefusal::Internal {})?,
        instance: UuidText::new("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
            .map_err(|_| GatewayRefusal::Internal {})?,
        observed_at_unix_ms: 1,
        credential: CredentialFacts {
            status: CredentialStatus::Ready,
            generation: Some(generation),
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
        },
        identity: Some(IdentityFacts {
            account_binding: binding.clone(),
            credential_authority: binding,
            email: None,
            plan: None,
        }),
        wire_identity: plakous_gateway_protocol::facts::WireIdentity::default(),
        profile: None,
        key: None,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
