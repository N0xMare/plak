use plakous_host_protocol::views::ReadyCatalog;
use plakous_host_protocol::wire::{
    DiscoveredWorkspace, HostedElsewhere, KindAccounts, NodeAddress, RecentWorkspace, RunnerLabel,
    SessionActivity, SessionEnvironment, SessionResidency,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::shared::SharedPlacementReason;
use super::{ProfileDifference, ProfilesView, SESSION_PREVIEW_BYTES, TemplateRow};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRow {
    pub session: String,
    pub node: NodeAddress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub workspace: String,
    pub runner: RunnerLabel,
    pub runner_status: RunnerStatus,
    pub attention: Vec<AttentionReason>,
    /// Last assistant text, or the last operator prompt if there is no
    /// assistant text. Omit when neither is available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<SessionPreview>,
    pub cost_root: Cost,
    pub cost_tree: TreeCost,
    pub activity_seq: u64,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub differences: Vec<ProfileDifference>,
    pub created_at: u64,
    pub updated_at: u64,
    pub residency: SessionResidency,
    pub activity: SessionActivity,
    pub live_work: LiveWork,
    pub environment: SessionEnvironment,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hosted_elsewhere: Option<HostedElsewhere>,
}

/// A one-line plain-text preview of at most 160 UTF-8 bytes.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(try_from = "String")]
#[schemars(!try_from)]
pub struct SessionPreview(#[schemars(length(max = 160))] String);

impl SessionPreview {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for SessionPreview {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if text.len() > SESSION_PREVIEW_BYTES
            || text
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
        {
            return Err("expected one line of plain text of at most 160 UTF-8 bytes");
        }
        Ok(Self(text))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Cost {
    Unknown,
    Measured {
        /// Integer USD micro-units; a measured zero is not unknown.
        micros: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TreeCost {
    pub cost: Cost,
    /// False means only a subtotal (or no known measurement).
    pub complete: bool,
}

/// Times are whole Unix seconds, from the host's UTC wall clock.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum AttentionReason {
    ApprovalNeeded {
        since: u64,
    },
    CredentialWait {
        since: u64,
    },
    ContextCapacity {
        since: u64,
    },
    RecoveryRequired {
        since: u64,
    },
    RunnerUnavailable {
        since: u64,
    },
    ChildNeedsOperator {
        since: u64,
    },
    UsageLimit {
        account: String,
        since: u64,
    },
    /// Quiet notice; not an operator-action badge.
    InterruptedByUpgrade {
        since: u64,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LiveWork {
    /// Descendants that are pending, running or sealing, not parked.
    pub children: u32,
    /// Process views that are running or stopping.
    pub processes: u32,
    /// Descendants waiting on the operator.
    pub held_children: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerPhase {
    Ssh,
    Daemon,
    Bind,
    Options,
    Discovery,
    Reconcile { subphase: ReconcileSubphase },
    LinkLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReconcileSubphase {
    Store,
    ObsoleteLines,
    Terminals,
    Jobs,
    Stops,
    Foreground,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum PlacementReason {
    Draft(PlacementReasonV13),
    Shared(SharedPlacementReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlacementReasonV13 {
    RunnerUnreachable { phase: RunnerPhase, timed_out: bool },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerStatus {
    Unknown,
    Ready,
    Blocked { reason: PlacementReason },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunnerOptions {
    pub runner: RunnerLabel,
    pub aliases: Vec<String>,
    pub status: RunnerStatus,
    pub checked_at: Option<u64>,
    pub blocked_since: Option<u64>,
    pub roots: Option<Vec<String>>,
    pub default_workspace: Option<String>,
    #[schemars(length(max = 20))]
    pub recent_workspaces: Vec<RecentWorkspace>,
    #[schemars(length(max = 200))]
    pub discovered_workspaces: Vec<DiscoveredWorkspace>,
    pub discovery_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged, deny_unknown_fields)]
pub enum WorkspaceOptions {
    Valid {
        canonical_path: String,
        profiles: ProfilesView,
        templates: Vec<TemplateRow>,
        models: ReadyCatalog,
    },
    Refused {
        refusal: PlacementReason,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionOptions {
    pub runners: Vec<RunnerOptions>,
    pub models: ReadyCatalog,
    pub profiles: ProfilesView,
    pub templates: Vec<TemplateRow>,
    pub workspace: Option<WorkspaceOptions>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<KindAccounts>,
}
