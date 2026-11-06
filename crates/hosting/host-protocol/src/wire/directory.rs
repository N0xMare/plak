//! The session directory a host of many sessions serves (spec 02 §4.4): one
//! row per session, read by `list_sessions` and watched on a connection's
//! host attachment.

use serde::{Deserialize, Serialize};

use super::NodeAddress;

/// One session as the directory shows it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRow {
    /// The durable session id.
    pub session: String,
    /// Its root node, which an attach names.
    pub node: NodeAddress,
    /// Its title, once it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Its workspace.
    pub workspace: String,
    /// Its root template, or the binding's account and model when it has
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Unix seconds of its last change.
    pub updated_at: u64,
    /// Whether this host holds it now.
    pub residency: SessionResidency,
    /// What it is doing.
    pub activity: SessionActivity,
    /// The work it holds.
    pub live_work: LiveWork,
    /// The environment its tools run with.
    pub environment: SessionEnvironment,
    /// The other host process that holds its tree, proven running or on
    /// another machine; a holder proven gone is not named.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_elsewhere: Option<HostedElsewhere>,
}

/// Whether the host holds a session (spec 02 §4.4, residency in a host).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionResidency {
    /// Released: no lease, no actor, no custody.
    Dormant,
    /// Being materialized for an attach or a host intent.
    Opening,
    /// Open on this host.
    Resident,
    /// Releasing; the next attach waits for it, then opens it again.
    Releasing,
}

/// What a session is doing.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivity {
    /// Nothing runs.
    Idle,
    /// A turn runs.
    Busy,
    /// An approval or a credential wait needs the operator.
    AwaitingOperator,
    /// Its host was lost and it holds for the operator's Resume.
    HeldAfterLoss,
    /// Its work was stopped.
    Stopped,
}

/// The work a session holds. A dormant session has no children or
/// processes running.
#[derive(
    Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LiveWork {
    /// Its running sub-agents.
    pub children: u32,
    /// Its running background processes.
    pub processes: u32,
    /// Its tree's sub-agents held for the operator.
    pub held_children: u32,
}

/// The environment a session's tools run with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SessionEnvironment {
    /// The login-shell environment `plak host` captured once at its start.
    HostLoginShell,
    /// The environment of the shell a TUI or `plak agent` was started in.
    Cli,
}

/// The host process that holds a session's tree.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostedElsewhere {
    /// The machine it runs on.
    pub host_name: String,
    /// Its binary id, when it recorded one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// Its process id.
    pub pid: u32,
}

/// What keeps a session resident after `release_session` detached its
/// attachments.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResidentReason {
    /// A turn, a queued or deferred command, recovery debt, a pending
    /// continuation or a resident child in its tree.
    Work,
    /// A background process.
    Processes,
    /// An attachment made since the request.
    Attachment,
}
