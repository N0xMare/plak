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
    /// Root placement, explicitly local or pinned to a configured runner.
    pub runner: super::RunnerLabel,
    /// Latest host-generation readiness verdict; directory reads never probe.
    pub runner_status: super::RunnerStatus,
    /// Any unresolved operator decision in this resident tree, independently of busy.
    pub needs_operator: bool,
    /// Durable head checkpoint revision; zero before runtime creation.
    pub activity_seq: u64,
    /// Product visibility/read-only state, not runtime-history archival.
    pub archived: bool,
    /// Its root template, or the binding's account and model when it has
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Bounded differences from the nearest current-config profile.
    pub differences: Vec<crate::views::ProfileDifference>,
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
    /// The other process that holds it (its tree's custody, or its runtime
    /// lease), unless that process is proven gone: one on another machine,
    /// or one whose lock file is missing, is named unproven.
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
    /// Its host was lost and it holds for the operator's Resume.
    HeldAfterLoss,
    /// A continuation that runs by itself waits for the next open (a
    /// planned handoff, or a decided continuation a crash left pending).
    Continuing,
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
    /// Its live sub-agents, including pending, parked and sealing work.
    pub children: u32,
    /// Its running background processes.
    pub processes: u32,
    /// Its tree's sub-agents awaiting consent to continue after host loss.
    pub held_children: u32,
}

/// The environment a session's tools run with.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SessionEnvironment {
    /// The host process's own: a session it serves, or would open.
    Host,
    /// Another process's: the shell a TUI or `plak agent` that holds the
    /// session was started in.
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
