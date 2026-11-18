//! One authenticated gateway call. The listener dials relays through the shared
//! server core; this buffered dispatcher is the admin, facts and refresh path
//! and the policy the tests call. It cannot stream a relay, so a decoded relay
//! here is refused `internal` and nothing is dialed.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use plakous::gateway::protocol::admin::{AdminRefusal, AdminRequest, AdminResponse, DisableTarget};
use plakous::gateway::protocol::call::{self, CallError};
use plakous::gateway::protocol::facts::{FactsReply, FactsRequest, RefreshReply, RefreshRequest};
use plakous::gateway::protocol::refusal::{GatewayRefusal, RefusalBody};
use plakous::gateway::protocol::scope::{CallerId, Capability, ScopeSet, SourceNet};
use plakous::gateway::protocol::{Endpoint, SUPPORTED_VERSIONS, VersionsReply};
use plakous::gateway::protocol::{GatewayAccount, SecretText};

use super::audit::{Audit, AuditEvent};
use super::auth::Principal;
#[cfg(test)]
use super::auth::{self, Peer};
use super::bridge::AccountBridge;
use super::callers::{CallerError, Callers};
use super::flags::Flags;
use super::http::{Request, Response};
use super::server_config::ServerConfig;

pub(crate) struct GatewayState {
    pub instance: plakous::gateway::protocol::UuidText,
    pub config: ServerConfig,
    pub callers: Mutex<Callers>,
    pub flags: Mutex<Flags>,
    pub audit: Audit,
    pub bridge: Mutex<Box<dyn AccountBridge>>,
    pub inflight: AtomicU32,
    pub caller_inflight: Mutex<std::collections::HashMap<String, u32>>,
    pub draining: AtomicBool,
    pub listeners: Mutex<Vec<String>>,
    pub shutdown: tokio::sync::Notify,
    /// Each streaming relay's cut, by its operation and attempt, for
    /// `disable --cut`.
    pub relays: Mutex<std::collections::HashMap<String, RelayCut>>,
}

/// Who one in-flight relay runs for and how to end it.
pub(crate) struct RelayCut {
    pub principal: Principal,
    pub account: GatewayAccount,
    pub token: plakous::gateway::server::CancellationToken,
}

impl RelayCut {
    fn matches(&self, target: &DisableTarget) -> bool {
        match target {
            DisableTarget::All {} => true,
            DisableTarget::Account { account } => self.account == *account,
            DisableTarget::Caller { caller } => {
                matches!(&self.principal, Principal::Caller(id) if id == caller)
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn handle(state: &Arc<GatewayState>, peer: &Peer, request: &Request) -> Response {
    let started = std::time::Instant::now();
    if state.draining.load(Ordering::Relaxed) {
        return refusal(&GatewayRefusal::Busy {
            retry_after_ms: 1000,
        });
    }
    if state.inflight.load(Ordering::Relaxed) >= state.config.max_inflight {
        return refusal(&GatewayRefusal::Busy {
            retry_after_ms: 1000,
        });
    }
    let authed = {
        let callers = match state.callers.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        match auth::authenticate(&callers, peer, request) {
            Ok((principal, record)) => Ok((principal, record.map(|record| record.scopes.clone()))),
            Err(refusal) => Err(refusal),
        }
    };
    let (principal, record_scopes) = match authed {
        Ok(pair) => pair,
        Err(refusal) => {
            audit(
                state,
                "unauthenticated",
                "",
                None,
                Some(refusal.code()),
                started,
            );
            return refusal_response(&refusal);
        }
    };
    serve_authed(state, &principal, record_scopes.as_ref(), request, started)
}

/// Runs one call for a principal the gateway has already authenticated.
pub(crate) fn serve_authed(
    state: &Arc<GatewayState>,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    if state.draining.load(Ordering::Relaxed)
        || state.inflight.load(Ordering::Relaxed) >= state.config.max_inflight
    {
        return refusal(&GatewayRefusal::Busy {
            retry_after_ms: 1000,
        });
    }
    if !admit_caller(state, principal) {
        return refusal(&GatewayRefusal::Busy {
            retry_after_ms: 500,
        });
    }
    state.inflight.fetch_add(1, Ordering::Relaxed);
    let response = dispatch(state, principal, scopes, request, started);
    state.inflight.fetch_sub(1, Ordering::Relaxed);
    release_caller(state, principal);
    response
}

pub(crate) fn admit_caller(state: &GatewayState, principal: &Principal) -> bool {
    let Principal::Caller(id) = principal else {
        return true;
    };
    let mut inflight = match state.caller_inflight.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    let count = inflight.entry(id.as_str().to_owned()).or_insert(0);
    if *count >= state.config.max_inflight_per_caller {
        return false;
    }
    *count += 1;
    true
}

pub(crate) fn release_caller(state: &GatewayState, principal: &Principal) {
    let Principal::Caller(id) = principal else {
        return;
    };
    let mut inflight = match state.caller_inflight.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if let Some(count) = inflight.get_mut(id.as_str()) {
        *count = count.saturating_sub(1);
    }
}

fn dispatch(
    state: &Arc<GatewayState>,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    // The local administrator's admin calls decide the kill switch per verb
    // (`admin_exempt`), so a global disable cannot lock out `enable`.
    let local_admin_call = principal.is_local_admin()
        && matches!(Endpoint::parse(&request.path), Some((_, Endpoint::Admin)));
    if let Some(blocked) = killed(state, principal).filter(|_| !local_admin_call) {
        let verb = if request.path == plakous::gateway::protocol::VERSIONS_PATH {
            "versions"
        } else {
            "killed"
        };
        audit(
            state,
            &principal.audit_name(),
            verb,
            None,
            Some(blocked.code()),
            started,
        );
        return refusal(&blocked);
    }
    if request.method == "GET" && request.path == plakous::gateway::protocol::VERSIONS_PATH {
        let body = VersionsReply {
            versions: SUPPORTED_VERSIONS.to_vec(),
            instance: state.instance.clone(),
        };
        audit(
            state,
            &principal.audit_name(),
            "versions",
            None,
            None,
            started,
        );
        return json_ok(&body);
    }
    let Some((version, endpoint)) = Endpoint::parse(&request.path) else {
        return refusal(&GatewayRefusal::EnvelopeInvalid {});
    };
    if !SUPPORTED_VERSIONS.contains(&version) {
        return refusal(&GatewayRefusal::VersionUnsupported {
            supported: SUPPORTED_VERSIONS.to_vec(),
        });
    }
    if request.method != "POST" {
        return refusal(&GatewayRefusal::EnvelopeInvalid {});
    }
    if !local_admin_call && flag_block(state, principal, None) {
        return refusal(&blocked_refusal(state, principal, None));
    }
    match endpoint {
        Endpoint::Admin => admin(state, principal, scopes, request, started),
        Endpoint::Facts => facts(state, principal, scopes, request, started),
        Endpoint::Refresh => refresh(state, principal, scopes, request, started),
        Endpoint::Relay => relay(state, principal, scopes, request, started),
    }
}

fn admin(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    let call: AdminRequest = match call::decode(&request.body) {
        Ok(call) => call,
        Err(error) => return call_error(error),
    };
    let verb = admin_verb(&call);
    let account = admin_account(&call);
    if !principal.is_local_admin() && local_only(&call) {
        audit(
            state,
            &principal.audit_name(),
            verb,
            account.as_ref(),
            Some("local_admin_required"),
            started,
        );
        return json_ok(&AdminResponse::Refused {
            refusal: AdminRefusal::LocalAdminRequired {},
        });
    }
    let exempt = admin_exempt(principal, &call);
    if let Some(blocked) = killed(state, principal).filter(|_| !exempt) {
        audit(
            state,
            &principal.audit_name(),
            verb,
            account.as_ref(),
            Some(blocked.code()),
            started,
        );
        return refusal_response(&blocked);
    }
    if let Some(refusal) = admin_scope(principal, scopes, &call) {
        audit(
            state,
            &principal.audit_name(),
            verb,
            account.as_ref(),
            Some(refusal.code()),
            started,
        );
        return refusal_response(&refusal);
    }
    // A flag verb names its target; that account's flag does not block it.
    let flag_verb = matches!(
        call,
        AdminRequest::Disable { .. } | AdminRequest::Enable { .. }
    );
    if let Some(account) = account.as_ref().filter(|_| !exempt && !flag_verb) {
        if flag_block(state, principal, Some(account)) {
            let refusal = blocked_refusal(state, principal, Some(account));
            audit(
                state,
                &principal.audit_name(),
                verb,
                Some(account),
                Some(refusal.code()),
                started,
            );
            return refusal_response(&refusal);
        }
    }
    let response = match &call {
        AdminRequest::CallerAdd {
            caller,
            scopes,
            sources,
        } => caller_add(state, caller, scopes, sources),
        AdminRequest::CallerList {} => caller_list(state),
        AdminRequest::CallerRotate { caller } => caller_rotate(state, caller),
        AdminRequest::CallerRemove { caller } => caller_remove(state, caller),
        AdminRequest::Disable { target, cut } => disable(state, target, *cut),
        AdminRequest::Enable { target } => enable(state, target),
        AdminRequest::Drain {} => {
            state.draining.store(true, Ordering::Relaxed);
            state.shutdown.notify_one();
            AdminResponse::Draining {}
        }
        other => bridge_call(state, principal.is_local_admin(), other.clone_request()),
    };
    let error = match &response {
        AdminResponse::Refused { refusal } => Some(refusal_name(refusal)),
        _ => None,
    };
    audit(
        state,
        &principal.audit_name(),
        verb,
        account.as_ref(),
        error.as_deref(),
        started,
    );
    json_ok(&response)
}

fn facts(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    let FactsRequest { accounts } = match call::decode(&request.body) {
        Ok(call) => call,
        Err(error) => return call_error(error),
    };
    if accounts.len() > call::MAX_FACTS_ACCOUNTS {
        return refusal(&GatewayRefusal::LimitExceeded {});
    }
    if let Some(refusal) = accounts_scope(principal, scopes, Capability::Relay, &accounts) {
        return refusal_response(&refusal);
    }
    for account in &accounts {
        if flag_block(state, principal, Some(account)) {
            return refusal_response(&blocked_refusal(state, principal, Some(account)));
        }
    }
    let facts = {
        let mut bridge = lock_bridge(state);
        bridge.facts(&accounts)
    };
    match facts {
        Ok(facts) => {
            audit(state, &principal.audit_name(), "facts", None, None, started);
            json_ok(&FactsReply { facts })
        }
        Err(refusal) => {
            audit(
                state,
                &principal.audit_name(),
                "facts",
                None,
                Some(refusal.code()),
                started,
            );
            refusal_response(&refusal)
        }
    }
}

fn refresh(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    let call: RefreshRequest = match call::decode(&request.body) {
        Ok(call) => call,
        Err(error) => return call_error(error),
    };
    if let Some(refusal) = accounts_scope(
        principal,
        scopes,
        Capability::Relay,
        std::slice::from_ref(&call.account),
    ) {
        return refusal_response(&refusal);
    }
    if flag_block(state, principal, Some(&call.account)) {
        return refusal_response(&blocked_refusal(state, principal, Some(&call.account)));
    }
    let outcome = {
        let mut bridge = lock_bridge(state);
        bridge.refresh(&call)
    };
    match outcome {
        Ok(facts) => {
            audit(
                state,
                &principal.audit_name(),
                "refresh",
                Some(&call.account),
                None,
                started,
            );
            json_ok(&RefreshReply { facts })
        }
        Err(refusal) => {
            audit(
                state,
                &principal.audit_name(),
                "refresh",
                Some(&call.account),
                Some(refusal.code()),
                started,
            );
            refusal_response(&refusal)
        }
    }
}

fn relay(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &Request,
    started: std::time::Instant,
) -> Response {
    // The listener's relay server core dials. This buffered path cannot stream
    // frames, so it still refuses after the policy checks and dials nothing.
    let decoded = plakous::gateway::protocol::envelope::decode_request(&request.body);
    let Ok((envelope, _body)) = decoded else {
        return refusal(&GatewayRefusal::EnvelopeInvalid {});
    };
    if let Some(refusal) = accounts_scope(
        principal,
        scopes,
        Capability::Relay,
        std::slice::from_ref(&envelope.account),
    ) {
        return refusal_response(&refusal);
    }
    if flag_block(state, principal, Some(&envelope.account)) {
        return refusal_response(&blocked_refusal(state, principal, Some(&envelope.account)));
    }
    let refusal = GatewayRefusal::Internal {};
    audit(
        state,
        &principal.audit_name(),
        "relay",
        Some(&envelope.account),
        Some(refusal.code()),
        started,
    );
    refusal_response(&refusal)
}

fn admin_scope(
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    request: &AdminRequest,
) -> Option<GatewayRefusal> {
    if principal.is_local_admin() {
        return None;
    }
    let scopes = scopes?;
    let needed = match request {
        AdminRequest::List {} => return list_scope(scopes),
        AdminRequest::Status { .. } | AdminRequest::Allowance { .. } => {
            let accounts = match request {
                AdminRequest::Status { accounts } => accounts.clone(),
                AdminRequest::Allowance { account } => vec![account.clone()],
                _ => Vec::new(),
            };
            return accounts_scope(principal, Some(scopes), Capability::Relay, &accounts);
        }
        AdminRequest::CallerAdd { .. }
        | AdminRequest::CallerList {}
        | AdminRequest::CallerRotate { .. }
        | AdminRequest::CallerRemove { .. }
        | AdminRequest::Enable { .. }
        | AdminRequest::Drain {} => return None,
        AdminRequest::Disable { target, .. } => return disable_scope(scopes, target),
        AdminRequest::KeySet { account, .. }
        | AdminRequest::KeyClear { account }
        | AdminRequest::Logout { account, .. }
        | AdminRequest::Refresh { account }
        | AdminRequest::Remove { account }
        | AdminRequest::LoginStart { account, .. } => {
            if scopes.allows(Capability::Admin, account) {
                return None;
            }
            return Some(GatewayRefusal::ScopeDenied {});
        }
        AdminRequest::LoginComplete { .. }
        | AdminRequest::LoginPoll { .. }
        | AdminRequest::LoginCancel { .. } => Capability::Admin,
    };
    if scopes
        .scopes()
        .iter()
        .any(|scope| scope.capability == needed)
    {
        None
    } else {
        Some(GatewayRefusal::ScopeDenied {})
    }
}

fn list_scope(scopes: &ScopeSet) -> Option<GatewayRefusal> {
    if scopes
        .scopes()
        .iter()
        .any(|scope| scope.capability == Capability::Relay)
    {
        None
    } else {
        Some(GatewayRefusal::ScopeDenied {})
    }
}

fn disable_scope(scopes: &ScopeSet, target: &DisableTarget) -> Option<GatewayRefusal> {
    match target {
        DisableTarget::Account { account } if scopes.allows(Capability::Admin, account) => None,
        DisableTarget::All {}
            if scopes.scopes().iter().any(|scope| {
                scope.capability == Capability::Admin
                    && matches!(
                        scope.accounts,
                        plakous::gateway::protocol::scope::AccountGlob::Any
                    )
            }) =>
        {
            None
        }
        DisableTarget::Caller { .. }
            if scopes
                .scopes()
                .iter()
                .any(|scope| scope.capability == Capability::Admin) =>
        {
            None
        }
        _ => Some(GatewayRefusal::ScopeDenied {}),
    }
}

fn accounts_scope(
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    capability: Capability,
    accounts: &[GatewayAccount],
) -> Option<GatewayRefusal> {
    if principal.is_local_admin() {
        return None;
    }
    let scopes = scopes?;
    if accounts
        .iter()
        .all(|account| scopes.allows(capability, account))
    {
        None
    } else {
        Some(GatewayRefusal::ScopeDenied {})
    }
}

/// The local administrator's verbs that a kill switch never blocks: without
/// them a global disable could not be cleared, tightened with `--cut`, or the
/// service drained.
/// Relays, facts and refresh stay blocked for everyone.
fn admin_exempt(principal: &Principal, request: &AdminRequest) -> bool {
    principal.is_local_admin()
        && matches!(
            request,
            AdminRequest::Enable { .. }
                | AdminRequest::Disable { .. }
                | AdminRequest::Drain {}
                | AdminRequest::CallerList {}
                | AdminRequest::Status { .. }
        )
}

/// A global or per-caller kill switch applies to every call, including
/// versions, except the local administrator's exempt admin verbs.
pub(crate) fn killed(state: &GatewayState, principal: &Principal) -> Option<GatewayRefusal> {
    let flags = match state.flags.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if flags.disabled_all() {
        return Some(GatewayRefusal::GatewayDisabled {});
    }
    if let Principal::Caller(id) = principal {
        if flags.caller_disabled(id) {
            return Some(GatewayRefusal::ScopeDenied {});
        }
    }
    None
}

fn flag_block(
    state: &GatewayState,
    principal: &Principal,
    account: Option<&GatewayAccount>,
) -> bool {
    !matches!(
        blocked_refusal(state, principal, account),
        GatewayRefusal::Unauthenticated {}
    ) && matches!(
        blocked_refusal(state, principal, account),
        GatewayRefusal::GatewayDisabled {}
            | GatewayRefusal::AccountDisabled {}
            | GatewayRefusal::ScopeDenied {}
    )
}

fn blocked_refusal(
    state: &GatewayState,
    principal: &Principal,
    account: Option<&GatewayAccount>,
) -> GatewayRefusal {
    let flags = match state.flags.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if flags.disabled_all() {
        return GatewayRefusal::GatewayDisabled {};
    }
    if let Principal::Caller(id) = principal {
        if flags.caller_disabled(id) {
            return GatewayRefusal::ScopeDenied {};
        }
    }
    if let Some(account) = account {
        if flags.account_disabled(account) {
            return GatewayRefusal::AccountDisabled {};
        }
    }
    GatewayRefusal::Unauthenticated {}
}

fn caller_add(
    state: &GatewayState,
    caller: &CallerId,
    scopes: &ScopeSet,
    sources: &[SourceNet],
) -> AdminResponse {
    let mut callers = lock_callers(state);
    match callers.add(caller.clone(), scopes.clone(), sources.to_vec()) {
        Ok(key) => issued(caller.clone(), &key.expose()),
        Err(CallerError::Exists) => refused_admin(AdminRefusal::CallerExists {}),
        Err(CallerError::Malformed(message)) if message.contains("source") => {
            refused_admin(AdminRefusal::ProviderRejected {})
        }
        Err(CallerError::Unknown) => refused_admin(AdminRefusal::CallerUnknown {}),
        Err(CallerError::Io(_) | CallerError::Malformed(_)) => {
            refused_admin(AdminRefusal::Unavailable {})
        }
    }
}

fn caller_list(state: &GatewayState) -> AdminResponse {
    AdminResponse::Callers {
        callers: lock_callers(state).rows(),
    }
}

fn caller_rotate(state: &GatewayState, caller: &CallerId) -> AdminResponse {
    match lock_callers(state).rotate(caller) {
        Ok(key) => issued(caller.clone(), &key.expose()),
        Err(CallerError::Unknown) => refused_admin(AdminRefusal::CallerUnknown {}),
        Err(_) => refused_admin(AdminRefusal::Unavailable {}),
    }
}

fn caller_remove(state: &GatewayState, caller: &CallerId) -> AdminResponse {
    match lock_callers(state).remove(caller) {
        Ok(()) => AdminResponse::CallerRemoved {
            caller: caller.clone(),
        },
        Err(CallerError::Unknown) => refused_admin(AdminRefusal::CallerUnknown {}),
        Err(_) => refused_admin(AdminRefusal::Unavailable {}),
    }
}

/// Sets the flag; with `cut`, also ends every matching relay in flight.
/// The flag is saved first, so a relay that starts after the cut is
/// refused rather than missed.
fn disable(state: &GatewayState, target: &DisableTarget, cut: bool) -> AdminResponse {
    let saved = {
        let mut flags = lock_flags(state);
        match target {
            DisableTarget::All {} => flags.disable_all(),
            DisableTarget::Account { account } => flags.disable_account(account.clone()),
            DisableTarget::Caller { caller } => flags.disable_caller(caller.clone()),
        }
    };
    if saved.is_err() {
        return refused_admin(AdminRefusal::Unavailable {});
    }
    if cut {
        let relays = match state.relays.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        for relay in relays.values().filter(|relay| relay.matches(target)) {
            relay.token.cancel();
        }
    }
    AdminResponse::Disabled {}
}

fn enable(state: &GatewayState, target: &DisableTarget) -> AdminResponse {
    let mut flags = lock_flags(state);
    let saved = match target {
        DisableTarget::All {} => flags.enable_all(),
        DisableTarget::Account { account } => flags.enable_account(account),
        DisableTarget::Caller { caller } => flags.enable_caller(caller),
    };
    match saved {
        Ok(()) => AdminResponse::Enabled {},
        Err(_) => refused_admin(AdminRefusal::Unavailable {}),
    }
}

fn bridge_call(state: &GatewayState, local_admin: bool, request: AdminRequest) -> AdminResponse {
    let mut bridge = lock_bridge(state);
    bridge.call(local_admin, request)
}

fn lock_callers(state: &GatewayState) -> std::sync::MutexGuard<'_, Callers> {
    match state.callers.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_flags(state: &GatewayState) -> std::sync::MutexGuard<'_, Flags> {
    match state.flags.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn lock_bridge(state: &GatewayState) -> std::sync::MutexGuard<'_, Box<dyn AccountBridge>> {
    match state.bridge.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn issued(caller: CallerId, key: &str) -> AdminResponse {
    let Ok(key) = SecretText::new(zeroize::Zeroizing::new(key.to_owned())) else {
        return refused_admin(AdminRefusal::Unavailable {});
    };
    AdminResponse::CallerKeyIssued { caller, key }
}

fn refused_admin(refusal: AdminRefusal) -> AdminResponse {
    AdminResponse::Refused { refusal }
}

fn call_error(error: CallError) -> Response {
    match error {
        CallError::TooLarge => refusal(&GatewayRefusal::LimitExceeded {}),
        CallError::Json | CallError::Encode => refusal(&GatewayRefusal::EnvelopeInvalid {}),
    }
}

fn refusal(refusal: &GatewayRefusal) -> Response {
    refusal_response(refusal)
}

pub(crate) fn refusal_response(refusal: &GatewayRefusal) -> Response {
    let body = RefusalBody {
        gateway_error: refusal.clone(),
    };
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| {
        format!("{{\"gateway_error\":{{\"code\":\"{}\"}}}}", refusal.code()).into_bytes()
    });
    Response::json(refusal.http_status(), bytes)
}

fn json_ok(value: &impl serde::Serialize) -> Response {
    match call::encode(value) {
        Ok(bytes) => Response::json(200, bytes),
        Err(_) => refusal(&GatewayRefusal::Internal {}),
    }
}

pub(crate) fn gate_relay(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    account: &GatewayAccount,
) -> Result<(), GatewayRefusal> {
    if let Some(refusal) = killed(state, principal) {
        return Err(refusal);
    }
    if let Some(refusal) = accounts_scope(
        principal,
        scopes,
        Capability::Relay,
        std::slice::from_ref(account),
    ) {
        return Err(refusal);
    }
    if flag_block(state, principal, Some(account)) {
        return Err(blocked_refusal(state, principal, Some(account)));
    }
    Ok(())
}

pub(crate) fn gate_admin_allowance(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    account: &GatewayAccount,
) -> Result<(), GatewayRefusal> {
    if let Some(refusal) = killed(state, principal) {
        return Err(refusal);
    }
    let request = AdminRequest::Allowance {
        account: account.clone(),
    };
    if let Some(refusal) = admin_scope(principal, scopes, &request) {
        return Err(refusal);
    }
    if flag_block(state, principal, Some(account)) {
        return Err(blocked_refusal(state, principal, Some(account)));
    }
    Ok(())
}

pub(crate) fn gate_admin_refresh(
    state: &GatewayState,
    principal: &Principal,
    scopes: Option<&ScopeSet>,
    account: &GatewayAccount,
) -> Result<(), GatewayRefusal> {
    if let Some(refusal) = killed(state, principal) {
        return Err(refusal);
    }
    let request = AdminRequest::Refresh {
        account: account.clone(),
    };
    if let Some(refusal) = admin_scope(principal, scopes, &request) {
        return Err(refusal);
    }
    if flag_block(state, principal, Some(account)) {
        return Err(blocked_refusal(state, principal, Some(account)));
    }
    Ok(())
}

pub(crate) fn bridge_facts(
    state: &GatewayState,
    accounts: &[GatewayAccount],
) -> Result<Vec<plakous::gateway::protocol::facts::AccountFacts>, GatewayRefusal> {
    lock_bridge(state).facts(accounts)
}

pub(crate) fn audit(
    state: &GatewayState,
    caller: &str,
    verb: &str,
    account: Option<&GatewayAccount>,
    gateway_error: Option<&str>,
    started: std::time::Instant,
) {
    state.audit.record(&AuditEvent {
        ts: super::audit::stamp(),
        instance: state.instance.as_str().to_owned(),
        caller: caller.to_owned(),
        verb: verb.to_owned(),
        account: account.map(ToString::to_string),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        gateway_error: gateway_error.map(str::to_owned),
        ..AuditEvent::default()
    });
}

fn admin_verb(request: &AdminRequest) -> &'static str {
    match request {
        AdminRequest::List {} => "list",
        AdminRequest::Status { .. } => "status",
        AdminRequest::KeySet { .. } => "key_set",
        AdminRequest::KeyClear { .. } => "key_clear",
        AdminRequest::Logout { .. } => "logout",
        AdminRequest::Refresh { .. } => "refresh",
        AdminRequest::Remove { .. } => "remove",
        AdminRequest::LoginStart { .. } => "login_start",
        AdminRequest::LoginComplete { .. } => "login_complete",
        AdminRequest::LoginPoll { .. } => "login_poll",
        AdminRequest::LoginCancel { .. } => "login_cancel",
        AdminRequest::Allowance { .. } => "allowance",
        AdminRequest::Disable { .. } => "disable",
        AdminRequest::Enable { .. } => "enable",
        AdminRequest::CallerAdd { .. } => "caller_add",
        AdminRequest::CallerList {} => "caller_list",
        AdminRequest::CallerRotate { .. } => "caller_rotate",
        AdminRequest::CallerRemove { .. } => "caller_remove",
        AdminRequest::Drain {} => "drain",
    }
}

fn admin_account(request: &AdminRequest) -> Option<GatewayAccount> {
    match request {
        AdminRequest::KeySet { account, .. }
        | AdminRequest::KeyClear { account }
        | AdminRequest::Logout { account, .. }
        | AdminRequest::Refresh { account }
        | AdminRequest::Remove { account }
        | AdminRequest::LoginStart { account, .. }
        | AdminRequest::Allowance { account } => Some(account.clone()),
        AdminRequest::Disable {
            target: DisableTarget::Account { account },
            ..
        }
        | AdminRequest::Enable {
            target: DisableTarget::Account { account },
        } => Some(account.clone()),
        _ => None,
    }
}

fn local_only(request: &AdminRequest) -> bool {
    matches!(
        request,
        AdminRequest::CallerAdd { .. }
            | AdminRequest::CallerList {}
            | AdminRequest::CallerRotate { .. }
            | AdminRequest::CallerRemove { .. }
            | AdminRequest::Enable { .. }
            | AdminRequest::Drain {}
    )
}

fn refusal_name(refusal: &AdminRefusal) -> String {
    match refusal {
        AdminRefusal::WrongAccountClass {} => "wrong_account_class",
        AdminRefusal::AccountSignedIn {} => "account_signed_in",
        AdminRefusal::AccountBusy {} => "account_busy",
        AdminRefusal::DefaultNotRemovable {} => "default_not_removable",
        AdminRefusal::EnvironmentOverride { .. } => "environment_override",
        AdminRefusal::NoticeMismatch {} => "notice_mismatch",
        AdminRefusal::WrittenByNewerBuild {} => "written_by_newer_build",
        AdminRefusal::DeviceLoginUnavailable {} => "device_login_unavailable",
        AdminRefusal::PasteInvalid {} => "paste_invalid",
        AdminRefusal::AttemptUnknown {} => "attempt_unknown",
        AdminRefusal::AttemptLimit {} => "attempt_limit",
        AdminRefusal::ProviderRejected {} => "provider_rejected",
        AdminRefusal::OutcomeUnknown {} => "outcome_unknown",
        AdminRefusal::Unavailable {} => "unavailable",
        AdminRefusal::LocalAdminRequired {} => "local_admin_required",
        AdminRefusal::CallerExists {} => "caller_exists",
        AdminRefusal::CallerUnknown {} => "caller_unknown",
    }
    .to_owned()
}

trait CloneRequest {
    fn clone_request(&self) -> AdminRequest;
}

impl CloneRequest for AdminRequest {
    fn clone_request(&self) -> AdminRequest {
        // AdminRequest is not Clone because secrets are not cloned by derive.
        // Re-encode the value the gateway already accepted.
        match call::encode(self).and_then(|bytes| call::decode(&bytes)) {
            Ok(request) => request,
            Err(_) => AdminRequest::List {},
        }
    }
}
