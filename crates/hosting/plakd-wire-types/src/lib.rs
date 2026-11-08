//! Product wire DTOs and extension classification for Plak hosts and clients.

#![allow(
    missing_docs,
    reason = "Wire declarations retain their original schema documentation verbatim."
)]

use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::{
    CompositionSelection, ObservationExtension, ProtocolExtension, ProtocolSelection,
};
use std::fmt;

mod reload;
pub use reload::{CarryCapacityProbe, ReloadProbe, ReloadRequest, ReloadTarget, ReopenProbe};

/// Checkout facts, not a preformatted border label or session authority.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChrome {
    pub path: std::path::PathBuf,
    pub home: Option<std::path::PathBuf>,
    pub branch: Option<String>,
}

/// Read-only MCP configuration and the session's frozen binding.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct McpOverview {
    pub config_readable: bool,
    pub unavailable: Option<String>,
    pub diagnostics: Vec<String>,
    pub servers: Vec<McpServerView>,
    pub omitted: Vec<String>,
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct McpServerView {
    pub id: String,
    pub configuration: Result<Option<McpServerConfiguration>, String>,
    pub binding: McpSessionBinding,
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct McpServerConfiguration {
    pub transport: String,
    pub target: String,
    pub scope: String,
    pub templates: Vec<String>,
    pub missing_secrets: Vec<String>,
    /// `true` for `placement = "runner"`: its secrets are runner secrets.
    pub on_runner: bool,
    pub resources: bool,
    pub login: Option<String>,
}

#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum McpSessionBinding {
    NotBound,
    Bound {
        tools: usize,
        earlier_definition: bool,
    },
}

/// Product-authoritative editable settings.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SettingsSnapshot {
    /// Default account for new sessions.
    pub default_account: Option<String>,
    /// Exact default model for new sessions.
    pub default_model: Option<String>,
    /// Explicit default effort.
    pub default_effort: Option<String>,
    /// Exact admitted model option/path, when the selected model exposes one.
    /// `Some("priority")` on Codex is the Fast processing tier.
    pub default_extra: Option<String>,
    /// Advanced composition default for future sessions only.
    pub composition: CompositionSelection,
    /// `OpenRouter` wire default for future sessions only.
    pub protocol: ProtocolSelection,
    /// Host-authoritative availability and effective projection for each choice.
    pub composition_options: Box<[SettingsCompositionOption]>,
    /// Whether automatic product compaction is enabled.
    pub compaction: bool,
}

/// Product settings and the independently owned terminal preferences edited together.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SettingsView {
    pub product: SettingsSnapshot,
}

/// Product-resolved availability for one exact composition choice and default binding.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SettingsCompositionOption {
    /// Closed selection represented by this option.
    pub composition: CompositionSelection,
    /// Exact model-visible presentation if admitted.
    pub effective_presentation: String,
    /// Fail-closed reason when this choice is not admitted for the binding.
    pub unavailable_reason: Option<String>,
}

pub use plakous_host_protocol::views::{ProfileDifference, ProfileRole, ProfileRow, ProfilesView};

/// Where an applied profile takes effect.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileApply {
    /// Saved as the default profile brand-new sessions bind.
    Default,
    /// This session's root continues as the profile on its own line; its
    /// children and processes keep running (Switch now).
    Switch,
    /// This session branches onto the profile, or an unattached, empty one
    /// binds it (Fork now, Use now).
    Fork,
}

/// How an applied profile landed.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileOutcome {
    /// Saved as the default profile brand-new sessions bind.
    Saved,
    /// This session's root continued as the profile; its children and
    /// processes kept running. The home default is unchanged.
    Switched,
    /// This session branched onto it; the home default is unchanged.
    Forked,
    /// An unattached, empty session took its root; a home without a default
    /// also saves it as the default.
    Bound,
    /// This session already runs that root with those cards.
    AlreadyActive,
}

/// Complete typed settings replacement sent to the product host.
pub type SettingsUpdate = SettingsView;

/// Plak's product traffic beside the closed host protocol: the typed settings
/// pane and the usage overlay ride the same mailboxes as extension payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlakExtension;

impl ProtocolExtension for PlakExtension {
    type Intent = SurfaceIntent;
    type Observation = SurfaceObservation;

    fn reads_usage(intent: &SurfaceIntent) -> bool {
        matches!(
            intent,
            SurfaceIntent::OpenUsage | SurfaceIntent::RereadUsage
        )
    }

    /// The installed-build check and probe (they exec the installed build
    /// and answer with its path, its configuration errors and the carry's
    /// capacity) and the reload-request handover (a same-user local
    /// channel, spec 04 §4.6).
    fn local_only(intent: &SurfaceIntent) -> bool {
        matches!(
            intent,
            SurfaceIntent::CheckBuild
                | SurfaceIntent::ProbeReload
                | SurfaceIntent::AcceptReloadRequest { .. }
                | SurfaceIntent::RefuseReloadRequest { .. }
        )
    }
}

/// One acknowledged attention occurrence.
///
/// An acknowledgement covers this attempt's attention label and end stamp,
/// not every later time the same attempt needs the operator.
#[derive(
    serde::Serialize,
    serde::Deserialize,
    Clone,
    Debug,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct AgentAcknowledgement {
    /// Exact agent id.
    pub agent_id: String,
    /// Attempt this occurrence belongs to.
    pub attempt_id: String,
    /// Attention label (`failed`, `needs recovery`, …).
    pub attention: String,
    /// End stamp of the occurrence. `None` when the attempt has none.
    pub ended_at: Option<u64>,
}

/// Product-owned intents the protocol carries for Plak's own surfaces.
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceIntent {
    /// Open raw `config.toml` in `$EDITOR`.
    OpenConfigFile,
    /// Request the typed product settings snapshot.
    OpenSettings,
    /// Persist a complete validated product settings replacement.
    SaveSettings(SettingsUpdate),
    /// Open usage / limits pane.
    OpenUsage,
    /// Explicitly refresh an eligible subscription credential; never logs in.
    RefreshUsageCredential {
        /// Exact OAuth account offered by the product.
        account: String,
    },
    /// Re-read provider allowances after an explicit credential refresh.
    RereadUsage,
    /// Arm or clear the busy allowance re-read. True only while Usage is open.
    WatchUsage(bool),
    /// Request the profile list ([`SurfaceObservation::Profiles`]).
    ListProfiles,
    /// Re-read workspace branch and profile facts for composer chrome.
    RefreshComposerChrome,
    /// Request the read-only MCP view ([`SurfaceObservation::McpOverview`]).
    ShowMcp,
    /// Select a named agent profile (built-in or `~/.plak/profiles`).
    ///
    /// `apply` says where: as the default for brand-new sessions, or in
    /// this session now (switching its root node, or forking it). Only the
    /// default writes the home's pointers.
    ApplyProfile {
        /// Profile id or alias.
        name: String,
        /// Where the profile applies.
        apply: ProfileApply,
    },
    /// Record the operator's acknowledgement of one attention occurrence;
    /// answered by [`SurfaceObservation::AgentAcknowledgements`].
    AcknowledgeAgent {
        /// Tree the agent belongs to.
        tree_id: String,
        /// Exact agent id.
        agent_id: String,
        /// The attempt acknowledged; a later attempt is not covered.
        attempt_id: String,
        /// Attention label of this occurrence.
        attention: String,
        /// End stamp of this occurrence. `None` when it has none.
        ended_at: Option<u64>,
    },
    /// Request the acknowledged attempts of one tree.
    ListAgentAcknowledgements {
        /// Tree id from the latest snapshot.
        tree_id: String,
    },
    /// Ask the host whether a newer `plak` is installed, and for the reload
    /// request waiting for the session it serves, which it announces as
    /// accepting them; answered by [`SurfaceObservation::BuildStatus`] and,
    /// while one is offered, [`SurfaceObservation::ReloadRequested`].
    CheckBuild,
    /// The face took the offered reload request; it is the face's to run or
    /// refuse from now on.
    AcceptReloadRequest {
        /// The request taken.
        request_id: String,
    },
    /// Ask the host which installed `plak` a reload of the session it serves
    /// would start, and whether this build refuses it; answered by
    /// [`SurfaceObservation::ReloadProbed`], or
    /// [`SurfaceObservation::ReloadProbeFailed`] when the check could not
    /// run.
    ProbeReload,
    /// Record a requested reload's refusal for its requester; only a request
    /// the host handed over.
    RefuseReloadRequest {
        /// The request refused.
        request_id: String,
        /// Why, as the operator reads it.
        reason: String,
    },
}

impl fmt::Debug for SurfaceIntent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OpenConfigFile => "OpenConfigFile",
            Self::OpenSettings => "OpenSettings",
            Self::SaveSettings(_) => "SaveSettings(..)",
            Self::OpenUsage => "OpenUsage",
            Self::RefreshUsageCredential { .. } => "RefreshUsageCredential{..}",
            Self::RereadUsage => "RereadUsage",
            Self::WatchUsage(_) => "WatchUsage(_)",
            Self::ListProfiles => "ListProfiles",
            Self::RefreshComposerChrome => "RefreshComposerChrome",
            Self::ShowMcp => "ShowMcp",
            Self::ApplyProfile { .. } => "ApplyProfile{..}",
            Self::AcknowledgeAgent { .. } => "AcknowledgeAgent{..}",
            Self::ListAgentAcknowledgements { .. } => "ListAgentAcknowledgements{..}",
            Self::CheckBuild => "CheckBuild",
            Self::ProbeReload => "ProbeReload",
            Self::AcceptReloadRequest { .. } => "AcceptReloadRequest{..}",
            Self::RefuseReloadRequest { .. } => "RefuseReloadRequest{..}",
        })
    }
}

/// Product-owned observations the protocol carries for Plak's own surfaces.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceObservation {
    /// Replace and open the typed product settings pane.
    SettingsView(SettingsView),
    /// Complete settings replacement was durably persisted by the product host.
    SettingsViewApplied(SettingsView),
    /// Loading typed product settings failed and is retryable.
    SettingsLoadFailed(String),
    /// Persisting typed product settings failed; the draft remains open.
    SettingsSaveFailed(String),
    /// Open the honest usage overlay after preceding usage state updates.
    OpenUsageOverlay,
    /// Exact accounts whose offline credential posture permits refresh.
    UsageRefreshAccounts(Vec<String>),
    /// One explicit refresh settled; failures normally use recorded telemetry notes.
    UsageCredentialRefreshed {
        /// Account whose explicit refresh settled.
        account: String,
        /// Fallback only when no recorded credential failure explains it.
        error: Option<String>,
    },
    /// The current profile list.
    Profiles(ProfilesView),
    /// Session identity and its own frozen cards. `id` is `custom` when no
    /// saved profile matches the complete binding and frozen roster.
    SessionProfile(ProfileRow),
    /// Fresh product-owned workspace presentation, never session authority.
    WorkspaceChrome(WorkspaceChrome),
    /// The read-only MCP view: configured servers and this session's tools.
    McpOverview(McpOverview),
    /// A profile was applied.
    ProfileApplied {
        /// Profile id.
        name: String,
        /// How it landed.
        outcome: ProfileOutcome,
    },
    /// A profile was not applied; nothing was saved.
    ProfileRefused {
        /// Profile name as requested.
        name: String,
        /// Operator-facing reason.
        reason: String,
    },
    /// Every acknowledged attention occurrence of one tree, complete.
    AgentAcknowledgements {
        /// Tree the set belongs to.
        tree_id: String,
        /// Acknowledged occurrences.
        attempts: Vec<AgentAcknowledgement>,
    },
    /// The product could not read acknowledgements for this tree.
    ///
    /// Not an empty set: the TUI must ask again rather than treat every
    /// attempt as unacknowledged forever, or as acknowledged.
    AgentAcknowledgementsUnavailable {
        /// Tree the failed read was for.
        tree_id: String,
    },
    /// Whether a newer `plak` than the running one is installed on the host.
    BuildStatus {
        /// A newer build is installed; `/reload` would start it.
        newer_installed: bool,
    },
    /// A same-user request that this session reload onto the installed
    /// build (spec 02 §5.1), offered until the face accepts it
    /// ([`SurfaceIntent::AcceptReloadRequest`]); the face runs it at its next
    /// idle boundary or refuses it ([`SurfaceIntent::RefuseReloadRequest`]).
    ReloadRequested(ReloadRequest),
    /// The installed build a reload would start (`None` when no `plak` is
    /// installed), and this build's refusal of it.
    ReloadProbed {
        /// The reload target.
        target: Option<Box<ReloadTarget>>,
        /// Why this build refuses it, if it does.
        refusal: Option<String>,
    },
    /// The installed-build check could not run.
    ReloadProbeFailed,
    /// Pre-bind lookup progress the launch publishes before the session
    /// opens (`retrying model lookup …`); `None` once it finished.
    LookupStatus {
        /// The progress line.
        text: Option<String>,
    },
}

impl ObservationExtension for SurfaceObservation {
    /// Every product observation replaces its own older value: a settings
    /// snapshot, a settings verdict, or the overlay request.
    fn latest_state(&self) -> bool {
        true
    }

    fn same_kind(&self, other: &Self) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// The views a surface opens answer the attachment that asked; the
    /// session's profile and workspace chrome are the session's state.
    fn wire_class(&self) -> ObservationClass {
        match self {
            Self::SessionProfile(_) | Self::WorkspaceChrome(_) | Self::LookupStatus { .. } => {
                ObservationClass::State
            }
            Self::SettingsView(_)
            | Self::SettingsViewApplied(_)
            | Self::SettingsLoadFailed(_)
            | Self::SettingsSaveFailed(_)
            | Self::OpenUsageOverlay
            | Self::UsageRefreshAccounts(_)
            | Self::UsageCredentialRefreshed { .. }
            | Self::Profiles(_)
            | Self::McpOverview(_)
            | Self::ProfileApplied { .. }
            | Self::ProfileRefused { .. }
            | Self::AgentAcknowledgements { .. }
            | Self::AgentAcknowledgementsUnavailable { .. }
            | Self::BuildStatus { .. }
            | Self::ReloadRequested(_)
            | Self::ReloadProbed { .. }
            | Self::ReloadProbeFailed => ObservationClass::Addressed,
        }
    }

    /// What answers or announces a local-face-only intent, and the
    /// launch's lookup progress: each names this machine's installed build
    /// or what the launch is reaching.
    fn local_only(&self) -> bool {
        matches!(
            self,
            Self::BuildStatus { .. }
                | Self::ReloadRequested(_)
                | Self::ReloadProbed { .. }
                | Self::ReloadProbeFailed
                | Self::LookupStatus { .. }
        )
    }

    fn kind_label(&self) -> &'static str {
        match self {
            Self::SettingsView(_) => "settings_view",
            Self::SettingsViewApplied(_) => "settings_view_applied",
            Self::SettingsLoadFailed(_) => "settings_load_failed",
            Self::SettingsSaveFailed(_) => "settings_save_failed",
            Self::OpenUsageOverlay => "usage_overlay",
            Self::UsageRefreshAccounts(_) => "usage_refresh_accounts",
            Self::UsageCredentialRefreshed { .. } => "usage_credential_refreshed",
            Self::Profiles(_) => "profiles",
            Self::SessionProfile(_) => "session_profile",
            Self::WorkspaceChrome(_) => "workspace_chrome",
            Self::McpOverview(_) => "mcp_overview",
            Self::ProfileApplied { .. } => "profile_applied",
            Self::ProfileRefused { .. } => "profile_refused",
            Self::AgentAcknowledgements { .. } => "agent_acknowledgements",
            Self::AgentAcknowledgementsUnavailable { .. } => "agent_acknowledgements_unavailable",
            Self::BuildStatus { .. } => "build_status",
            Self::ReloadRequested(_) => "reload_requested",
            Self::ReloadProbed { .. } => "reload_probed",
            Self::ReloadProbeFailed => "reload_probe_failed",
            Self::LookupStatus { .. } => "lookup_status",
        }
    }
}
