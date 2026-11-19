//! Account verbs the gateway forwards to today's account operations. Caller
//! keys, the kill switch and drain stay in the gateway; it is their only writer.

#[cfg(test)]
use std::collections::HashMap;
use std::io::Write as _;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex, Once, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use plakous::gateway::protocol::GatewayAccount;
#[cfg(test)]
use plakous::gateway::protocol::SecretText;
use plakous::gateway::protocol::UuidText;
use plakous::gateway::protocol::admin::{AdminRefusal, AdminRequest, AdminResponse};
#[cfg(test)]
use plakous::gateway::protocol::admin::{LoginMethod, LoginPhase};
use plakous::gateway::protocol::facts::{AccountFacts, RefreshRequest};
use plakous::gateway::protocol::refusal::GatewayRefusal;

use super::holder::HolderLock;

pub(crate) const MAX_PENDING_LOGINS: usize = 3;
const LOGIN_LIFETIME: Duration = Duration::from_secs(15 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(1);

pub(crate) trait AccountBridge: Send {
    fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse;
    fn facts(&mut self, accounts: &[GatewayAccount]) -> Result<Vec<AccountFacts>, GatewayRefusal>;
    fn refresh(&mut self, request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal>;
}

type BridgeCall = Box<dyn FnOnce(&mut dyn AccountBridge) + Send>;
type BridgeQueue = Arc<Mutex<Option<mpsc::Sender<BridgeCall>>>>;

pub(super) struct AccountWorker {
    calls: BridgeQueue,
}

pub(super) struct AccountThread {
    calls: BridgeQueue,
    thread: JoinHandle<()>,
}

impl AccountWorker {
    pub(super) fn start(
        bridge: Box<dyn AccountBridge>,
        lock: Arc<HolderLock>,
    ) -> std::io::Result<(Self, AccountThread)> {
        redact_bridge_panics();
        let (sender, receiver) = mpsc::channel::<BridgeCall>();
        let calls = Arc::new(Mutex::new(Some(sender)));
        // Tokio's spawn_blocking still enters a runtime. The synchronous
        // application edge owns its own executor and must run outside it.
        let thread = thread::Builder::new()
            .name("gateway-accounts".to_owned())
            .stack_size(plakous::agent::RUNTIME_EXECUTOR_STACK_BYTES)
            .spawn(move || {
                let _hold = lock;
                let mut bridge = bridge;
                for call in receiver {
                    IN_BRIDGE_CALL.with(|active| active.set(true));
                    if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| {
                        call(bridge.as_mut());
                    })) {
                        // Dropping the reply sender already refused this call.
                        // A custom panic payload may itself panic on drop.
                        if let Err(nested) = panic::catch_unwind(AssertUnwindSafe(|| drop(payload)))
                        {
                            std::mem::forget(nested);
                        }
                        let _ = writeln!(std::io::stderr(), "gateway account bridge call panicked");
                    }
                    IN_BRIDGE_CALL.with(|active| active.set(false));
                }
            })?;
        Ok((
            Self {
                calls: Arc::clone(&calls),
            },
            AccountThread { calls, thread },
        ))
    }

    fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut dyn AccountBridge) -> T + Send + 'static,
    ) -> Option<T> {
        let (reply, result) = mpsc::sync_channel(1);
        self.calls
            .lock()
            .ok()?
            .as_ref()?
            .send(Box::new(move |bridge| {
                let _ = reply.send(operation(bridge));
            }))
            .ok()?;
        result.recv().ok()
    }
}

impl AccountBridge for AccountWorker {
    fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse {
        self.run(move |bridge| bridge.call(local_admin, request))
            .unwrap_or(AdminResponse::Refused {
                refusal: AdminRefusal::Unavailable {},
            })
    }

    fn facts(&mut self, accounts: &[GatewayAccount]) -> Result<Vec<AccountFacts>, GatewayRefusal> {
        let accounts = accounts.to_vec();
        self.run(move |bridge| bridge.facts(&accounts))
            .ok_or(GatewayRefusal::Internal {})?
    }

    fn refresh(&mut self, request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal> {
        let request = request.clone();
        self.run(move |bridge| bridge.refresh(&request))
            .ok_or(GatewayRefusal::Internal {})?
    }
}

impl AccountThread {
    pub(super) fn close(&self) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
    }

    pub(super) fn join_until(self, deadline: Instant) -> bool {
        while !self.thread.is_finished() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            thread::sleep(remaining.min(Duration::from_millis(10)));
        }
        let _ = self.thread.join();
        true
    }
}

thread_local! {
    static IN_BRIDGE_CALL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn redact_bridge_panics() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let previous = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            if !IN_BRIDGE_CALL
                .try_with(std::cell::Cell::get)
                .unwrap_or(false)
            {
                previous(info);
            }
        }));
    });
}

#[cfg(test)]
#[test]
fn panicking_bridge_call_refuses_then_serves_status() {
    struct PanicOnList(MemoryBridge);

    impl AccountBridge for PanicOnList {
        fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse {
            assert!(!matches!(request, AdminRequest::List {}), "fixture panic");
            self.0.call(local_admin, request)
        }

        fn facts(
            &mut self,
            accounts: &[GatewayAccount],
        ) -> Result<Vec<AccountFacts>, GatewayRefusal> {
            self.0.facts(accounts)
        }

        fn refresh(&mut self, request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal> {
            self.0.refresh(request)
        }
    }

    let instance = UuidText::new(uuid::Uuid::new_v4().to_string()).unwrap();
    let (mut worker, thread) = AccountWorker::start(
        Box::new(PanicOnList(MemoryBridge::new(instance))),
        Arc::new(HolderLock::vacant()),
    )
    .unwrap();
    assert_eq!(
        worker.call(true, AdminRequest::List {}),
        AdminResponse::Refused {
            refusal: AdminRefusal::Unavailable {}
        },
    );
    assert!(matches!(
        worker.call(
            true,
            AdminRequest::Status {
                accounts: Vec::new()
            }
        ),
        AdminResponse::Status { .. }
    ));
    thread.close();
    assert!(thread.join_until(Instant::now() + Duration::from_secs(2)));
}

pub(crate) struct Attempt<T> {
    pub id: UuidText,
    pub account: GatewayAccount,
    pub started: Instant,
    pub last_poll: Option<Instant>,
    pub payload: T,
}

pub(crate) struct AttemptBook<T> {
    entries: Vec<Attempt<T>>,
}

impl<T> AttemptBook<T> {
    pub(crate) fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn expire(&mut self) {
        self.entries
            .retain(|attempt| attempt.started.elapsed() < LOGIN_LIFETIME);
    }

    pub(super) fn check_start(&mut self, account: &GatewayAccount) -> Result<(), AdminRefusal> {
        self.expire();
        if self.entries.len() >= MAX_PENDING_LOGINS
            || self
                .entries
                .iter()
                .any(|attempt| &attempt.account == account)
        {
            return Err(AdminRefusal::AttemptLimit {});
        }
        Ok(())
    }

    pub(crate) fn begin(
        &mut self,
        account: GatewayAccount,
        payload: T,
    ) -> Result<UuidText, AdminRefusal> {
        self.check_start(&account)?;
        let Ok(id) = UuidText::new(uuid::Uuid::new_v4().to_string()) else {
            return Err(AdminRefusal::Unavailable {});
        };
        self.entries.push(Attempt {
            id: id.clone(),
            account,
            started: Instant::now(),
            last_poll: None,
            payload,
        });
        Ok(id)
    }

    pub(crate) fn get_mut(&mut self, id: &UuidText) -> Result<&mut Attempt<T>, AdminRefusal> {
        self.expire();
        self.entries
            .iter_mut()
            .find(|attempt| attempt.id == *id)
            .ok_or(AdminRefusal::AttemptUnknown {})
    }

    pub(crate) fn take(&mut self, id: &UuidText) -> Result<Attempt<T>, AdminRefusal> {
        self.expire();
        let index = self
            .entries
            .iter()
            .position(|attempt| attempt.id == *id)
            .ok_or(AdminRefusal::AttemptUnknown {})?;
        Ok(self.entries.remove(index))
    }

    pub(crate) fn poll_allowed(&mut self, id: &UuidText) -> Result<(), AdminRefusal> {
        let attempt = self.get_mut(id)?;
        if attempt
            .last_poll
            .is_some_and(|last| last.elapsed() < POLL_INTERVAL)
        {
            return Err(AdminRefusal::AttemptLimit {});
        }
        attempt.last_poll = Some(Instant::now());
        Ok(())
    }
}

impl<T> Default for AttemptBook<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Login and account state for tests. Device-code approval and paste
/// acceptance are explicit.
#[cfg(test)]
#[derive(Clone, Debug)]
struct Slot {
    signed_in: bool,
}

#[cfg(test)]
pub(crate) struct MemoryBridge {
    slots: HashMap<String, Slot>,
    attempts: AttemptBook<MemoryAttempt>,
    instance: UuidText,
}

#[cfg(test)]
struct MemoryAttempt {
    method: LoginMethod,
    phase: LoginPhase,
}

#[cfg(test)]
impl MemoryBridge {
    pub(crate) fn new(instance: UuidText) -> Self {
        Self {
            slots: HashMap::new(),
            attempts: AttemptBook::new(),
            instance,
        }
    }

    fn slot(&mut self, account: &GatewayAccount) -> &mut Slot {
        self.slots
            .entry(account.as_str().to_owned())
            .or_insert(Slot { signed_in: false })
    }
}

#[cfg(test)]
impl AccountBridge for MemoryBridge {
    fn call(&mut self, local_admin: bool, request: AdminRequest) -> AdminResponse {
        match request {
            AdminRequest::List {} => AdminResponse::Accounts {
                accounts: self
                    .slots
                    .keys()
                    .filter_map(|account| GatewayAccount::new(account.clone()).ok())
                    .collect(),
            },
            AdminRequest::Status { accounts } => {
                status_rows(&self.instance, &self.slots, &accounts)
            }
            AdminRequest::KeySet { account, secret } => {
                if secret.expose().is_empty() {
                    return refused(AdminRefusal::ProviderRejected {});
                }
                self.slot(&account).signed_in = true;
                changed(account)
            }
            AdminRequest::KeyClear { account } => {
                self.slot(&account).signed_in = false;
                changed(account)
            }
            AdminRequest::Logout { account, .. } => {
                self.slot(&account).signed_in = false;
                AdminResponse::Changed {
                    account,
                    remote_grant_may_remain: Some(true),
                }
            }
            AdminRequest::Refresh { account } => refreshed(&self.instance, &account),
            AdminRequest::Remove { account } => {
                let slot = self.slot(&account).clone();
                if slot.signed_in {
                    return refused(AdminRefusal::AccountSignedIn {});
                }
                self.slots.remove(account.as_str());
                changed(account)
            }
            AdminRequest::LoginStart { account, .. } => self.login_start(local_admin, account),
            AdminRequest::LoginComplete { attempt, pasted } => {
                self.login_complete(local_admin, &attempt, pasted.expose())
            }
            AdminRequest::LoginPoll { attempt } => self.login_poll(&attempt),
            AdminRequest::LoginCancel { attempt } => {
                let Ok(attempt) = self.attempts.take(&attempt) else {
                    return refused(AdminRefusal::AttemptUnknown {});
                };
                progress(attempt.id, LoginPhase::Cancelled)
            }
            AdminRequest::Allowance { account } => AdminResponse::Allowance {
                account,
                observed_at_unix_ms: Some(super::callers::now_ms()),
                view: None,
                unavailable: Some(short_text("not-observed")),
            },
            AdminRequest::Disable { .. }
            | AdminRequest::Enable { .. }
            | AdminRequest::CallerAdd { .. }
            | AdminRequest::CallerList {}
            | AdminRequest::CallerRotate { .. }
            | AdminRequest::CallerRemove { .. }
            | AdminRequest::Drain {} => refused(AdminRefusal::Unavailable {}),
        }
    }

    fn facts(&mut self, accounts: &[GatewayAccount]) -> Result<Vec<AccountFacts>, GatewayRefusal> {
        accounts
            .iter()
            .map(|account| match self.slots.get(account.as_str()) {
                Some(slot) => Ok(fact(&self.instance, account, slot.signed_in)),
                None => Err(GatewayRefusal::AccountUnknown {}),
            })
            .collect()
    }

    fn refresh(&mut self, request: &RefreshRequest) -> Result<AccountFacts, GatewayRefusal> {
        let signed_in = self
            .slots
            .get(request.account.as_str())
            .map(|slot| slot.signed_in)
            .ok_or(GatewayRefusal::AccountUnknown {})?;
        Ok(fact(&self.instance, &request.account, signed_in))
    }
}

#[cfg(test)]
impl MemoryBridge {
    fn login_start(&mut self, local_admin: bool, account: GatewayAccount) -> AdminResponse {
        if account.kind() == "none" {
            return refused(AdminRefusal::DeviceLoginUnavailable {});
        }
        let signed_in = self.slot(&account).signed_in;
        let replacement = signed_in && account.name() == Some("replace");
        if replacement && !local_admin {
            return refused(AdminRefusal::LocalAdminRequired {});
        }
        let method = if account.kind() == "codex" {
            LoginMethod::DeviceCode
        } else {
            LoginMethod::Paste
        };
        let Ok(id) = self.attempts.begin(
            account,
            MemoryAttempt {
                method,
                phase: LoginPhase::AwaitingInput,
            },
        ) else {
            return refused(AdminRefusal::AttemptLimit {});
        };
        let Some(authorize_url) = secret("https://example.invalid/device") else {
            return refused(AdminRefusal::Unavailable {});
        };
        let user_code = if method == LoginMethod::DeviceCode {
            secret("ABCD-EFGH")
        } else {
            None
        };
        AdminResponse::Challenge {
            attempt: id,
            method,
            authorize_url,
            user_code,
            expires_in_ms: u64::try_from(LOGIN_LIFETIME.as_millis()).unwrap_or(0),
            warning: replacement.then(|| long_text("replacing the stored provider account")),
        }
    }

    fn login_complete(
        &mut self,
        local_admin: bool,
        attempt: &UuidText,
        pasted: &str,
    ) -> AdminResponse {
        let Ok(found) = self.attempts.get_mut(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if found.payload.method != LoginMethod::Paste {
            return refused(AdminRefusal::PasteInvalid {});
        }
        if pasted == "replace-other" && !local_admin {
            return refused(AdminRefusal::LocalAdminRequired {});
        }
        if pasted != "fixture-ok" && pasted != "replace-other" {
            return refused(AdminRefusal::PasteInvalid {});
        }
        let Ok(finished) = self.attempts.take(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        self.slot(&finished.account).signed_in = true;
        progress(finished.id, LoginPhase::Succeeded)
    }

    fn login_poll(&mut self, attempt: &UuidText) -> AdminResponse {
        if self.attempts.poll_allowed(attempt).is_err() {
            return refused(AdminRefusal::AttemptLimit {});
        }
        let Ok(found) = self.attempts.get_mut(attempt) else {
            return refused(AdminRefusal::AttemptUnknown {});
        };
        if found.payload.method == LoginMethod::DeviceCode {
            found.payload.phase = LoginPhase::Succeeded;
            let account = found.account.clone();
            let id = found.id.clone();
            self.slot(&account).signed_in = true;
            return progress(id, LoginPhase::Succeeded);
        }
        let id = found.id.clone();
        progress(id, found.payload.phase)
    }
}

#[cfg(test)]
fn status_rows(
    instance: &UuidText,
    slots: &HashMap<String, Slot>,
    accounts: &[GatewayAccount],
) -> AdminResponse {
    let selected: Vec<GatewayAccount> = if accounts.is_empty() {
        slots
            .keys()
            .filter_map(|account| GatewayAccount::new(account.clone()).ok())
            .collect()
    } else {
        accounts.to_vec()
    };
    let rows = selected
        .iter()
        .filter_map(|account| {
            let signed_in = slots.get(account.as_str())?.signed_in;
            Some(plakous::gateway::protocol::admin::AccountStatusRow {
                facts: fact(instance, account, signed_in),
                last_refresh: None,
            })
        })
        .collect();
    AdminResponse::Status { rows }
}

#[cfg(test)]
fn refreshed(instance: &UuidText, account: &GatewayAccount) -> AdminResponse {
    AdminResponse::Refreshed {
        facts: Box::new(fact(instance, account, true)),
    }
}

#[cfg(test)]
fn fact(instance: &UuidText, account: &GatewayAccount, signed_in: bool) -> AccountFacts {
    use plakous::gateway::protocol::facts::{CredentialFacts, CredentialStatus, WireIdentity};
    AccountFacts {
        account: account.clone(),
        instance: instance.clone(),
        observed_at_unix_ms: super::callers::now_ms(),
        credential: CredentialFacts {
            status: if signed_in {
                CredentialStatus::Ready
            } else {
                CredentialStatus::SignedOut
            },
            generation: signed_in.then_some(1),
            access_expires_at_unix_ms: None,
            selection_epoch_digest: None,
        },
        identity: None,
        wire_identity: WireIdentity::default(),
        profile: None,
        key: None,
    }
}

#[cfg(test)]
fn changed(account: GatewayAccount) -> AdminResponse {
    AdminResponse::Changed {
        account,
        remote_grant_may_remain: None,
    }
}

#[cfg(test)]
fn refused(refusal: AdminRefusal) -> AdminResponse {
    AdminResponse::Refused { refusal }
}

#[cfg(test)]
fn progress(attempt: UuidText, phase: LoginPhase) -> AdminResponse {
    AdminResponse::Progress {
        attempt,
        phase,
        warning: None,
    }
}

#[cfg(test)]
fn secret(text: &str) -> Option<SecretText> {
    SecretText::new(zeroize::Zeroizing::new(text.to_owned())).ok()
}

#[cfg(test)]
fn short_text(text: &str) -> plakous::gateway::protocol::BoundedText<64> {
    plakous::gateway::protocol::BoundedText::new(text).unwrap_or_else(|_| {
        plakous::gateway::protocol::BoundedText::new("unavailable").unwrap_or_else(|_| {
            plakous::gateway::protocol::BoundedText::new("").unwrap_or_else(|_| short_text("u"))
        })
    })
}

#[cfg(test)]
fn long_text(text: &str) -> plakous::gateway::protocol::BoundedText<512> {
    plakous::gateway::protocol::BoundedText::new(text).unwrap_or_else(|_| {
        plakous::gateway::protocol::BoundedText::new("").unwrap_or_else(|_| long_text("u"))
    })
}
