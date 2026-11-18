//! Admin calls: `POST /v<N>/admin` with one [`AdminRequest`], answered by one
//! [`AdminResponse`]. A gateway-level refusal (authentication, scope, kill
//! switch, version) is a [`crate::refusal::GatewayRefusal`]; an
//! operation-level refusal is [`AdminResponse::Refused`].
//!
//! Each request names the authority it needs. `relay` covers the reads
//! ([`AdminRequest::List`], [`AdminRequest::Status`],
//! [`AdminRequest::Allowance`]); `admin` covers account changes and remote
//! disable; the local administrator (a same-UID peer on the gateway's own
//! socket) alone manages callers, enables, drains, and replaces an account
//! with a different provider account. The gateway is the only writer of its
//! caller and kill-switch files, so even local verbs arrive as these calls.

use crate::account::GatewayAccount;
use crate::facts::AccountFacts;
use crate::scope::{CallerId, ScopeSet, SourceNet};
use crate::secret::SecretText;
use crate::text::{BoundedText, UuidText};

/// Whether logout contacts the provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogoutMode {
    /// Revoke at the provider where it offers revocation.
    Remote,
    /// Retire the stored credential only.
    Local,
}

/// What a disable or enable targets.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum DisableTarget {
    /// Every call.
    All,
    /// One account.
    Account {
        /// The account.
        account: GatewayAccount,
    },
    /// One caller.
    Caller {
        /// The caller id.
        caller: CallerId,
    },
}

/// One admin call.
#[derive(Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "request", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRequest {
    /// The custodied account references.
    List {},
    /// Status rows for the named accounts, or for all when empty.
    Status {
        /// The accounts.
        accounts: Vec<GatewayAccount>,
    },
    /// Store an API key.
    KeySet {
        /// The key account.
        account: GatewayAccount,
        /// The key.
        secret: SecretText,
    },
    /// Remove an API key file.
    KeyClear {
        /// The key account.
        account: GatewayAccount,
    },
    /// Log out of a subscription account.
    Logout {
        /// The account.
        account: GatewayAccount,
        /// Remote revocation or local retirement.
        mode: LogoutMode,
    },
    /// Force one claimed refresh, regardless of expiry.
    Refresh {
        /// The account.
        account: GatewayAccount,
    },
    /// Remove a signed-out named account.
    Remove {
        /// The account.
        account: GatewayAccount,
    },
    /// Begin a login: accept the exact notice and open the provider flow.
    LoginStart {
        /// The account.
        account: GatewayAccount,
        /// The compatibility notice revision the operator accepted.
        notice_revision: BoundedText<128>,
        /// Explicit consent to replace a record a newer build wrote.
        replace_newer: bool,
    },
    /// Complete a paste login.
    LoginComplete {
        /// The pending attempt.
        attempt: UuidText,
        /// The pasted provider response.
        pasted: SecretText,
    },
    /// Read a pending login's progress; at most once per second.
    LoginPoll {
        /// The pending attempt.
        attempt: UuidText,
    },
    /// Cancel a pending login.
    LoginCancel {
        /// The pending attempt.
        attempt: UuidText,
    },
    /// Read one account's provider allowance.
    Allowance {
        /// The account.
        account: GatewayAccount,
    },
    /// Engage the kill switch; `admin` may do this remotely.
    Disable {
        /// What to disable.
        target: DisableTarget,
        /// Also cancel matching relays already in flight.
        cut: bool,
    },
    /// Clear a kill switch; local administrator only.
    Enable {
        /// What to enable.
        target: DisableTarget,
    },
    /// Issue a key for a new caller; local administrator only.
    CallerAdd {
        /// The new caller id; ids are never reused.
        caller: CallerId,
        /// The key's scopes.
        scopes: ScopeSet,
        /// The source networks the key may arrive from; at least one.
        sources: Vec<SourceNet>,
    },
    /// Every caller; local administrator only.
    CallerList {},
    /// Issue a new key for a caller, revoking its old one; local administrator only.
    CallerRotate {
        /// The caller.
        caller: CallerId,
    },
    /// Revoke a caller; local administrator only.
    CallerRemove {
        /// The caller.
        caller: CallerId,
    },
    /// Stop accepting calls, settle in-flight relays and refresh claims, then
    /// exit; local administrator only.
    Drain {},
}

/// How a login completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginMethod {
    /// The provider's device-code flow; the gateway polls.
    DeviceCode,
    /// The operator pastes the provider's response.
    Paste,
}

/// Where a pending login is.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoginPhase {
    /// The flow is opening.
    Starting,
    /// Waiting for approval or a paste.
    AwaitingInput,
    /// Exchanging the grant.
    Exchanging,
    /// Cancellation is in progress.
    Cancelling,
    /// The credential is stored.
    Succeeded,
    /// The login failed with no effect.
    Failed,
    /// The exchange may have taken effect; read status.
    OutcomeUnknown,
    /// The operator cancelled.
    Cancelled,
    /// The attempt expired.
    Expired,
}

/// The settled outcome of an account's last claimed refresh.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefreshOutcome {
    /// A new generation was stored.
    Completed,
    /// The claim was restored; nothing was sent or the failure was retryable.
    Restored,
    /// The outcome is unknown.
    OutcomeUnknown,
    /// The credential was quarantined.
    Quarantined,
}

/// The last retained refresh outcome for one account, not credential state.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshObservation {
    /// The outcome.
    pub outcome: RefreshOutcome,
    /// When it settled, in Unix milliseconds.
    pub observed_at_unix_ms: u64,
    /// The closed, content-free failure class, when it failed.
    pub failure_class: Option<BoundedText<64>>,
}

/// One status row.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountStatusRow {
    /// Current facts.
    pub facts: AccountFacts,
    /// The last refresh outcome, when one is retained.
    pub last_refresh: Option<RefreshObservation>,
}

/// Why an admin operation was refused or did not finish.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminRefusal {
    /// The account is the wrong class for this call (a key call on a
    /// subscription, or the reverse).
    WrongAccountClass,
    /// The account still holds a credential.
    AccountSignedIn,
    /// Another writer holds the account.
    AccountBusy,
    /// Default accounts cannot be removed.
    DefaultNotRemovable,
    /// The gateway's environment variable wins, so the file write has no effect.
    EnvironmentOverride {
        /// The variable's name.
        variable: BoundedText<64>,
    },
    /// The notice revision is not the compiled one.
    NoticeMismatch,
    /// A newer build wrote the stored record and replacement was not requested.
    WrittenByNewerBuild,
    /// The provider offers no device-code login for this account.
    DeviceLoginUnavailable,
    /// The paste is malformed or does not match the attempt; the attempt stays.
    PasteInvalid,
    /// No such pending attempt.
    AttemptUnknown,
    /// The pending-login bounds are reached.
    AttemptLimit,
    /// The provider rejected the call; nothing changed.
    ProviderRejected,
    /// A started effect's outcome is unknown; read status before retrying.
    OutcomeUnknown,
    /// The provider or store is temporarily unavailable; nothing changed.
    Unavailable,
    /// Only the local administrator may make this call, or make it replace
    /// one provider account with another.
    LocalAdminRequired,
    /// The caller id is in use or was used before.
    CallerExists,
    /// No such caller.
    CallerUnknown,
}

/// One caller, without its key.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallerRow {
    /// The caller id.
    pub caller: CallerId,
    /// Its scopes.
    pub scopes: ScopeSet,
    /// The source networks it may arrive from.
    pub sources: Vec<SourceNet>,
    /// When its current key was issued, in Unix milliseconds.
    pub issued_at_unix_ms: u64,
    /// When it was revoked, in Unix milliseconds.
    pub revoked_at_unix_ms: Option<u64>,
}

/// The answer to one [`AdminRequest`].
#[derive(Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "response", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminResponse {
    /// The answer to [`AdminRequest::List`].
    Accounts {
        /// The account references.
        accounts: Vec<GatewayAccount>,
    },
    /// The answer to [`AdminRequest::Status`].
    Status {
        /// One row per account.
        rows: Vec<AccountStatusRow>,
    },
    /// A key, logout or removal changed the account.
    Changed {
        /// The account.
        account: GatewayAccount,
        /// For a logout: whether the provider grant may still be active.
        remote_grant_may_remain: Option<bool>,
    },
    /// A forced refresh stored a new generation.
    Refreshed {
        /// Current facts.
        facts: Box<AccountFacts>,
    },
    /// A login opened; show this where the command ran.
    Challenge {
        /// The pending attempt.
        attempt: UuidText,
        /// How it completes.
        method: LoginMethod,
        /// The provider URL to open.
        authorize_url: SecretText,
        /// The device user code, for a device-code login.
        user_code: Option<SecretText>,
        /// The challenge lifetime.
        expires_in_ms: u64,
        /// An operator warning, such as a retired record whose grant may remain.
        warning: Option<BoundedText<512>>,
    },
    /// A pending login's progress.
    Progress {
        /// The pending attempt.
        attempt: UuidText,
        /// Its phase.
        phase: LoginPhase,
        /// An operator warning, such as a sibling with the same upstream account.
        warning: Option<BoundedText<512>>,
    },
    /// The answer to [`AdminRequest::Allowance`].
    Allowance {
        /// The account.
        account: GatewayAccount,
        /// When the allowance was observed, in Unix milliseconds.
        observed_at_unix_ms: Option<u64>,
        /// The product-owned allowance view, as JSON.
        view: Option<serde_json::Value>,
        /// A closed reason when no view is available.
        unavailable: Option<BoundedText<64>>,
    },
    /// The kill switch is engaged.
    Disabled {},
    /// The kill switch is cleared.
    Enabled {},
    /// A caller key was issued; shown once and never stored in the clear.
    CallerKeyIssued {
        /// The caller.
        caller: CallerId,
        /// The key text.
        key: SecretText,
    },
    /// The answer to [`AdminRequest::CallerList`].
    Callers {
        /// Every caller, revoked ones included.
        callers: Vec<CallerRow>,
    },
    /// A caller was revoked.
    CallerRemoved {
        /// The caller.
        caller: CallerId,
    },
    /// The gateway is draining and will exit.
    Draining {},
    /// The operation was refused or did not finish.
    Refused {
        /// Why.
        refusal: AdminRefusal,
    },
}
