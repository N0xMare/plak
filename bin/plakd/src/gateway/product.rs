//! The product gateway: callers, scopes, the kill switch and audit around
//! [`LocalCustody`](plakous::gateway::server::LocalCustody). The relay server
//! core dials; this type decides who may ask and records the result.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use plakous::gateway::protocol::Endpoint;
use plakous::gateway::protocol::GatewayAccount;
use plakous::gateway::protocol::admin::{AdminRequest, AdminResponse};
use plakous::gateway::protocol::call::{self, CallError};
use plakous::gateway::protocol::envelope::RelayEnvelope;
use plakous::gateway::protocol::facts::{FactsReply, FactsRequest, RefreshRequest};
use plakous::gateway::protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous::gateway::server::{
    GatewayService, Injection, Peer as ServerPeer, RelayOutcome, RelayRecord, ServiceFuture,
};

use super::audit::{AuditEvent, stamp};
use super::auth::{self, Peer, Principal};
use super::custody::{self, local_custody};
use super::dispatch::{self, GatewayState};
use super::http::Request;
use crate::home::paths::PlakPaths;

pub(crate) struct ProductGateway {
    state: Arc<GatewayState>,
    home: PlakPaths,
    custody: Arc<plakous::gateway::server::LocalCustody>,
    notes: Mutex<HashMap<String, RelayNote>>,
}

#[derive(Clone)]
pub(crate) struct Authed {
    principal: Principal,
    scopes: Option<plakous::gateway::protocol::scope::ScopeSet>,
}

struct RelayNote {
    principal: Principal,
    release: bool,
}

struct CallHold {
    state: Arc<GatewayState>,
    principal: Principal,
}

impl ProductGateway {
    pub(crate) fn new(home: &PlakPaths, state: Arc<GatewayState>) -> Self {
        let custody = Arc::new(local_custody(
            home,
            state.instance.clone(),
            &state.config.state_root,
        ));
        Self {
            state,
            home: home.clone(),
            custody,
            notes: Mutex::new(HashMap::new()),
        }
    }
}

impl CallHold {
    fn acquire(state: &Arc<GatewayState>, principal: &Principal) -> Result<Self, GatewayRefusal> {
        if state.draining.load(Ordering::Relaxed) {
            return Err(busy(1000));
        }
        if state.inflight.load(Ordering::Relaxed) >= state.config.max_inflight {
            return Err(busy(1000));
        }
        if !dispatch::admit_caller(state, principal) {
            return Err(busy(500));
        }
        state.inflight.fetch_add(1, Ordering::Relaxed);
        Ok(Self {
            state: Arc::clone(state),
            principal: principal.clone(),
        })
    }
}

impl Drop for CallHold {
    fn drop(&mut self) {
        self.state.inflight.fetch_sub(1, Ordering::Relaxed);
        dispatch::release_caller(&self.state, &self.principal);
    }
}

impl GatewayService for ProductGateway {
    type Principal = Authed;

    fn authenticate(
        &self,
        peer: &ServerPeer,
        key: Option<&[u8]>,
    ) -> Result<Authed, GatewayRefusal> {
        if self.state.draining.load(Ordering::Relaxed) {
            return Err(busy(1000));
        }
        if self.state.inflight.load(Ordering::Relaxed) >= self.state.config.max_inflight {
            return Err(busy(1000));
        }
        let request = key_request(key)?;
        let (principal, scopes) = {
            let callers = match self.state.callers.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let (principal, record) = auth::authenticate(&callers, &map_peer(peer), &request)?;
            (principal, record.map(|record| record.scopes.clone()))
        };
        // Every relay, facts and refresh path checks the switch again; the
        // local administrator's admin calls are decided per verb.
        if !principal.is_local_admin()
            && let Some(refusal) = dispatch::killed(&self.state, &principal)
        {
            return Err(refusal);
        }
        Ok(Authed { principal, scopes })
    }

    fn inject<'a>(
        &'a self,
        principal: &'a Authed,
        version: u16,
        envelope: &'a RelayEnvelope,
    ) -> ServiceFuture<'a, Injection> {
        Box::pin(async move {
            self.begin_relay(principal, envelope);
            if self.state.draining.load(Ordering::Relaxed) {
                return Err(busy(1000));
            }
            if !dispatch::admit_caller(&self.state, &principal.principal) {
                return Err(busy(500));
            }
            self.mark_release(envelope);
            dispatch::gate_relay(
                &self.state,
                &principal.principal,
                principal.scopes.as_ref(),
                &envelope.account,
            )?;
            let injection = self.custody.inject(version, envelope).await?;
            let key = note_key(&envelope.operation, envelope.attempt);
            let token = plakous::gateway::server::CancellationToken::new();
            lock_relays(&self.state).insert(
                key.clone(),
                dispatch::RelayCut {
                    principal: principal.principal.clone(),
                    account: envelope.account.clone(),
                    token: token.clone(),
                },
            );
            // A disable saves its flag before it cuts: one that landed while
            // this relay registered is seen here, so no relay escapes a cut.
            if let Err(refusal) = dispatch::gate_relay(
                &self.state,
                &principal.principal,
                principal.scopes.as_ref(),
                &envelope.account,
            ) {
                lock_relays(&self.state).remove(&key);
                return Err(refusal);
            }
            Ok(injection.cut_by(token))
        })
    }

    fn call<'a>(
        &'a self,
        principal: &'a Authed,
        version: u16,
        endpoint: Endpoint,
        body: &'a [u8],
    ) -> ServiceFuture<'a, Vec<u8>> {
        Box::pin(async move {
            match endpoint {
                Endpoint::Facts => self.facts(principal, version, body).await,
                Endpoint::Refresh => self.refresh(principal, version, body).await,
                Endpoint::Admin => self.admin(principal, version, body).await,
                Endpoint::Relay => Err(GatewayRefusal::EnvelopeInvalid {}),
            }
        })
    }

    fn audit(&self, record: RelayRecord) {
        let note = self.take_note(&record);
        if note.is_some() {
            self.state.inflight.fetch_sub(1, Ordering::Relaxed);
        }
        if let Some(note) = note.as_ref().filter(|note| note.release) {
            dispatch::release_caller(&self.state, &note.principal);
        }
        let caller = note
            .as_ref()
            .map(|note| note.principal.audit_name())
            .unwrap_or_else(|| "unknown".to_owned());
        self.state
            .audit
            .record(&relay_event(&self.state, &caller, &record));
    }
}

impl ProductGateway {
    async fn facts(
        &self,
        principal: &Authed,
        version: u16,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let FactsRequest { accounts } = match decode(body) {
            Ok(request) => request,
            Err(refusal) => return Err(refusal),
        };
        if !accounts.iter().any(custody::custodied) {
            return self
                .forward(principal, version, Endpoint::Facts, body)
                .await;
        }
        if accounts.len() > call::MAX_FACTS_ACCOUNTS {
            return Err(GatewayRefusal::LimitExceeded {});
        }
        let _hold = CallHold::acquire(&self.state, &principal.principal)?;
        let mut facts = Vec::with_capacity(accounts.len());
        for account in &accounts {
            dispatch::gate_relay(
                &self.state,
                &principal.principal,
                principal.scopes.as_ref(),
                account,
            )?;
            facts.extend(self.one_facts(version, account).await?);
        }
        self.record(principal, "facts", None, None);
        encode(&FactsReply { facts })
    }

    async fn one_facts(
        &self,
        version: u16,
        account: &GatewayAccount,
    ) -> Result<Vec<plakous::gateway::protocol::facts::AccountFacts>, GatewayRefusal> {
        if custody::custodied(account) {
            let reply = self
                .custody
                .facts(
                    version,
                    &FactsRequest {
                        accounts: vec![account.clone()],
                    },
                )
                .await?;
            return Ok(reply.facts);
        }
        let state = Arc::clone(&self.state);
        let account = account.clone();
        tokio::task::spawn_blocking(move || dispatch::bridge_facts(&state, &[account]))
            .await
            .map_err(|_| GatewayRefusal::Internal {})?
    }

    async fn refresh(
        &self,
        principal: &Authed,
        version: u16,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let request: RefreshRequest = match decode(body) {
            Ok(request) => request,
            Err(refusal) => return Err(refusal),
        };
        if !custody::custodied(&request.account) {
            return self
                .forward(principal, version, Endpoint::Refresh, body)
                .await;
        }
        let hold = CallHold::acquire(&self.state, &principal.principal)?;
        dispatch::gate_relay(
            &self.state,
            &principal.principal,
            principal.scopes.as_ref(),
            &request.account,
        )?;
        let account = request.account.clone();
        let custody = Arc::clone(&self.custody);
        let reply = claim(
            hold,
            async move { custody.refresh(version, &request).await },
        )
        .await?;
        self.record(principal, "refresh", Some(&account), None);
        encode(&reply)
    }

    async fn admin(
        &self,
        principal: &Authed,
        version: u16,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let request: AdminRequest = match decode(body) {
            Ok(request) => request,
            Err(_) => {
                return self
                    .forward(principal, version, Endpoint::Admin, body)
                    .await;
            }
        };
        if let AdminRequest::Allowance { account } = &request
            && custody::custodied_api_key(account)
        {
            return self.allowance(principal, account).await;
        }
        let AdminRequest::Refresh { account } = &request else {
            return self
                .forward(principal, version, Endpoint::Admin, body)
                .await;
        };
        if !custody::custodied(account) {
            return self
                .forward(principal, version, Endpoint::Admin, body)
                .await;
        }
        let hold = CallHold::acquire(&self.state, &principal.principal)?;
        dispatch::gate_admin_refresh(
            &self.state,
            &principal.principal,
            principal.scopes.as_ref(),
            account,
        )?;
        let custody = Arc::clone(&self.custody);
        let claimed = account.clone();
        let facts = claim(hold, async move {
            custody.operator_refresh(version, &claimed).await
        })
        .await?;
        self.record(principal, "refresh", Some(account), None);
        encode(&AdminResponse::Refreshed {
            facts: Box::new(facts),
        })
    }

    async fn allowance(
        &self,
        principal: &Authed,
        account: &GatewayAccount,
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let _hold = CallHold::acquire(&self.state, &principal.principal)?;
        dispatch::gate_admin_allowance(
            &self.state,
            &principal.principal,
            principal.scopes.as_ref(),
            account,
        )?;
        let response = super::allowance::read(&self.home, account).await;
        self.record(principal, "allowance", Some(account), None);
        encode(&response)
    }

    async fn forward(
        &self,
        principal: &Authed,
        version: u16,
        endpoint: Endpoint,
        body: &[u8],
    ) -> Result<Vec<u8>, GatewayRefusal> {
        let request = Request {
            method: "POST".to_owned(),
            path: endpoint.path(version),
            headers: std::collections::BTreeMap::new(),
            body: body.to_vec(),
        };
        let state = Arc::clone(&self.state);
        let inner = principal.principal.clone();
        let scopes = principal.scopes.clone();
        let response = tokio::task::spawn_blocking(move || {
            dispatch::serve_authed(
                &state,
                &inner,
                scopes.as_ref(),
                &request,
                std::time::Instant::now(),
            )
        })
        .await
        .map_err(|_| GatewayRefusal::Internal {})?;
        from_response(response)
    }

    fn begin_relay(&self, principal: &Authed, envelope: &RelayEnvelope) {
        self.state.inflight.fetch_add(1, Ordering::Relaxed);
        let mut notes = lock_notes(&self.notes);
        notes.insert(
            note_key(&envelope.operation, envelope.attempt),
            RelayNote {
                principal: principal.principal.clone(),
                release: false,
            },
        );
    }

    fn mark_release(&self, envelope: &RelayEnvelope) {
        let mut notes = lock_notes(&self.notes);
        if let Some(note) = notes.get_mut(&note_key(&envelope.operation, envelope.attempt)) {
            note.release = true;
        }
    }

    fn take_note(&self, record: &RelayRecord) -> Option<RelayNote> {
        let (operation, attempt) = record.operation.clone()?;
        let key = note_key(&operation, attempt);
        lock_relays(&self.state).remove(&key);
        lock_notes(&self.notes).remove(&key)
    }

    fn record(
        &self,
        principal: &Authed,
        verb: &str,
        account: Option<&GatewayAccount>,
        gateway_error: Option<&str>,
    ) {
        dispatch::audit(
            &self.state,
            &principal.principal.audit_name(),
            verb,
            account,
            gateway_error,
            std::time::Instant::now(),
        );
    }
}

/// Runs one refresh claim on its own task, counted in flight until it
/// settles: a caller that leaves must not drop a claim mid-exchange, and
/// drain waits for it before the holder lock is released.
async fn claim<T: Send + 'static>(
    hold: CallHold,
    refresh: impl Future<Output = Result<T, GatewayRefusal>> + Send + 'static,
) -> Result<T, GatewayRefusal> {
    tokio::spawn(async move {
        let _hold = hold;
        refresh.await
    })
    .await
    .map_err(|_| GatewayRefusal::Internal {})?
}

#[allow(
    clippy::redundant_closure_for_method_calls,
    reason = "the gateway facade lets audit code read these facts without exposing their HTTP types"
)]
fn relay_event(state: &GatewayState, caller: &str, record: &RelayRecord) -> AuditEvent {
    let (origin, path) = record
        .upstream
        .clone()
        .map_or((None, None), |(origin, path)| (Some(origin), Some(path)));
    AuditEvent {
        ts: stamp(),
        instance: state.instance.as_str().to_owned(),
        caller: caller.to_owned(),
        verb: "relay".to_owned(),
        account: record.account.as_ref().map(ToString::to_string),
        upstream_origin: origin,
        upstream_path: path,
        method: record.method.clone(),
        operation: record
            .operation
            .as_ref()
            .map(|(operation, _)| operation.as_str().to_owned()),
        attempt: record.operation.as_ref().map(|(_, attempt)| *attempt),
        upstream_status: record.upstream_status,
        bytes_down: record.bytes_down,
        duration_ms: u64::try_from(record.duration.as_millis()).unwrap_or(u64::MAX),
        gateway_error: relay_error(&record.outcome),
        upstream_failure: record
            .upstream_failure
            .map(|failure| super::audit::UpstreamFailure {
                connection_use: failure.connection_use().map(|use_| use_.label()),
                idle_age_ms: failure
                    .idle_age()
                    .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX)),
                http_version: failure
                    .wire_protocol()
                    .map(|protocol| protocol.http_version()),
                request_body_bytes_written: failure.request_body_bytes_written(),
                error_class: failure.transport_error_class(),
                tls_alert: failure.tls_cause().and_then(|cause| cause.alert()),
                http2_reason: failure.http2_reason(),
            }),
        ..AuditEvent::default()
    }
}

fn relay_error(outcome: &RelayOutcome) -> Option<String> {
    match outcome {
        RelayOutcome::Refused(code) => Some((*code).to_owned()),
        RelayOutcome::Complete => None,
        RelayOutcome::UpstreamFailed(_) => Some("upstream_failed".to_owned()),
        RelayOutcome::Abandoned => Some("abandoned".to_owned()),
        RelayOutcome::Cut => Some("cut".to_owned()),
    }
}

fn note_key(operation: &plakous::gateway::protocol::UuidText, attempt: u32) -> String {
    format!("{}:{attempt}", operation.as_str())
}

fn lock_notes(
    notes: &Mutex<HashMap<String, RelayNote>>,
) -> std::sync::MutexGuard<'_, HashMap<String, RelayNote>> {
    match notes.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_relays(
    state: &GatewayState,
) -> std::sync::MutexGuard<'_, HashMap<String, dispatch::RelayCut>> {
    match state.relays.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn key_request(key: Option<&[u8]>) -> Result<Request, GatewayRefusal> {
    let mut headers = std::collections::BTreeMap::new();
    if let Some(key) = key {
        let text = std::str::from_utf8(key).map_err(|_| GatewayRefusal::Unauthenticated {})?;
        headers.insert(
            plakous::gateway::protocol::KEY_HEADER.to_owned(),
            text.to_owned(),
        );
    }
    Ok(Request {
        method: "POST".to_owned(),
        path: String::new(),
        headers,
        body: Vec::new(),
    })
}

fn map_peer(peer: &ServerPeer) -> Peer {
    match peer {
        ServerPeer::Unix { same_user } => Peer {
            uid_matches: *same_user,
            address: None,
        },
        ServerPeer::Tcp(addr) => Peer {
            uid_matches: false,
            address: Some(addr.ip()),
        },
        ServerPeer::InProcess => Peer {
            uid_matches: true,
            address: None,
        },
    }
}

fn busy(retry_after_ms: u64) -> GatewayRefusal {
    GatewayRefusal::Busy { retry_after_ms }
}

fn decode<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, GatewayRefusal> {
    match call::decode(body) {
        Ok(value) => Ok(value),
        Err(CallError::TooLarge) => Err(GatewayRefusal::LimitExceeded {}),
        Err(CallError::Json | CallError::Encode) => Err(GatewayRefusal::EnvelopeInvalid {}),
    }
}

fn encode(value: &impl serde::Serialize) -> Result<Vec<u8>, GatewayRefusal> {
    call::encode(value).map_err(|_| GatewayRefusal::Internal {})
}

fn from_response(response: super::http::Response) -> Result<Vec<u8>, GatewayRefusal> {
    if response.status == 200 {
        return Ok(response.body);
    }
    match serde_json::from_slice::<RefusalBody>(&response.body) {
        Ok(body) => Err(body.gateway_error),
        Err(_) => Err(GatewayRefusal::Internal {}),
    }
}
