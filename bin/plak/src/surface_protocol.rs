//! Plak's extension of the host protocol: the typed settings snapshot the
//! product's surfaces edit, the intents and observations that carry it and
//! the usage overlay beside the closed [`plakous_host_protocol`] vocabulary,
//! and the concrete mailbox types every Plak surface holds.
//!
//! The protocol crate never reads this traffic; it only classifies it for the
//! bounded observation mailbox through [`ObservationExtension`].

use std::fmt;

use plakous_host_protocol::observation::ObservationClass;
use plakous_host_protocol::{
    CompositionSelection, IntentReceiver, IntentSendError, IntentSender, ObservationExtension,
    ProtocolExtension, ProtocolSelection, SessionIntent, SessionObservation, intent_channel,
};

#[cfg(any(feature = "mcp", feature = "tui"))]
pub(crate) mod mcp_lines;

/// Checkout facts, not a preformatted border label or session authority.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceChrome {
    pub path: std::path::PathBuf,
    pub home: Option<std::path::PathBuf>,
    pub branch: Option<String>,
}

/// Read-only MCP configuration and the session's frozen binding.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpOverview {
    pub config_readable: bool,
    pub unavailable: Option<String>,
    pub diagnostics: Vec<String>,
    pub servers: Vec<McpServerView>,
    pub omitted: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct McpServerView {
    pub id: String,
    pub configuration: Result<Option<McpServerConfiguration>, String>,
    pub binding: McpSessionBinding,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
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

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum McpSessionBinding {
    NotBound,
    Bound {
        tools: usize,
        earlier_definition: bool,
    },
}

/// Product-authoritative editable settings.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
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
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SettingsView {
    pub product: SettingsSnapshot,
    /// Client-local on wire v2: it never travels, and a received view
    /// holds the defaults.
    #[serde(skip)]
    pub tui: crate::tui_preferences::TuiPreferences,
}

impl Default for SettingsView {
    /// The config-schema defaults, so a test fixture cannot drift from §5.5.
    fn default() -> Self {
        let mut snapshot = crate::shell::settings::settings_view(
            &crate::product_config::ProductConfig::default_product(),
        );
        // A default snapshot has no host-resolved composition availability.
        snapshot.product.composition_options = Box::default();
        snapshot
    }
}

/// Product-resolved availability for one exact composition choice and default binding.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SettingsCompositionOption {
    /// Closed selection represented by this option.
    pub composition: CompositionSelection,
    /// Exact model-visible presentation if admitted.
    pub effective_presentation: String,
    /// Fail-closed reason when this choice is not admitted for the binding.
    pub unavailable_reason: Option<String>,
}

pub use crate::tui_preferences::SettingsTheme;

/// One role line in the Agents detail box.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileRole {
    /// Role (`root`, `worker`, …) or, for a user profile, the template name.
    pub label: String,
    /// Provider account.
    pub account: String,
    /// Model id.
    pub model: String,
    /// Effort, or `default` when the template names none.
    pub effort: String,
    /// Codex fast lane.
    pub fast: bool,
    /// Nesting below the working set: 0 for a working-set card, 1 for a
    /// template one of those spawns, and so on.
    pub depth: usize,
}

/// One selectable profile.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfileRow {
    /// Built-in id or `~/.plak/profiles` file stem; for a session's own row,
    /// the nearest profile.
    pub id: String,
    /// Bounded differences from that profile; empty for an exact match.
    pub differences: Vec<ProfileDifference>,
    /// Other names `/agents <name>` accepts for this row.
    pub aliases: Vec<String>,
    /// Root template.
    pub root: ProfileRole,
    /// Working-set cards, in order.
    pub cards: Vec<ProfileRole>,
}

/// How a session differs from its nearest profile: the root's own values
/// beside the profile's, and card or roster names. Never card bodies or
/// credentials.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ProfileDifference {
    RootAccount { session: String, profile: String },
    RootModel { session: String, profile: String },
    RootEffort { session: String, profile: String },
    RootFast { session: bool },
    WorkingSet,
    Card(String),
    Roster,
}

/// The product's profile list, as the Agents tab shows it.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ProfilesView {
    /// Built-ins first, then user profiles by name.
    pub rows: Vec<ProfileRow>,
    /// The row the config's root and working set already point at.
    pub current: Option<String>,
    /// One line per profile file that was left out, and why.
    pub diagnostics: Vec<String>,
}

/// Where an applied profile takes effect.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
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
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Eq, PartialEq)]
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
}

/// One acknowledged attention occurrence.
///
/// An acknowledgement covers this attempt's attention label and end stamp,
/// not every later time the same attempt needs the operator.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
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
#[derive(serde::Serialize, serde::Deserialize, Clone, Eq, PartialEq)]
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
        })
    }
}

/// Product-owned observations the protocol carries for Plak's own surfaces.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq)]
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
            Self::SessionProfile(_) | Self::WorkspaceChrome(_) => ObservationClass::State,
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
            | Self::AgentAcknowledgementsUnavailable { .. } => ObservationClass::Addressed,
        }
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
        }
    }
}

/// The intent enum Plak's surfaces send: the protocol plus [`SurfaceIntent`].
pub type UserAction = SessionIntent<PlakExtension>;
/// The observation enum Plak's surfaces receive: the protocol plus
/// [`SurfaceObservation`].
pub type HostEvent = SessionObservation<PlakExtension>;
/// Producer content that cannot carry keyed publication frames.
pub type ContentEvent = plakous_host_protocol::ContentObservation<PlakExtension>;
/// Producer half of Plak's intent mailbox.
pub type UserActionSender = IntentSender<PlakExtension>;
/// Consumer half of Plak's intent mailbox.
pub type UserActionReceiver = IntentReceiver<PlakExtension>;
/// Typed refusal of a Plak intent by the bounded mailbox.
pub type UserActionSendError = IntentSendError<PlakExtension>;
/// Producer half of Plak's observation mailbox.
#[cfg(test)]
pub type HostEventSender = plakous_host_protocol::ObservationSender<PlakExtension>;
/// What a Plak face receives: its observation mailbox, taken as an
/// attachment of the host service.
pub type HostEventReceiver = plakous_hosted::host_service::FaceReceiver<PlakExtension>;

/// Construct Plak's bounded intent mailbox.
#[must_use]
pub fn user_action_channel() -> (UserActionSender, UserActionReceiver) {
    intent_channel()
}

/// Construct Plak's bounded observation mailbox, received by a face.
#[cfg(test)]
#[must_use]
pub fn host_event_channel() -> (HostEventSender, HostEventReceiver) {
    let (sender, receiver) = plakous_host_protocol::observation_channel();
    (sender, HostEventReceiver::new(receiver))
}

/// The mailbox ends a Plak face holds on a host worker: its intents
/// tagged with its attachment and its observations taken through the host
/// service ([`plakous_hosted::host_service::face`]).
#[must_use]
pub fn face_of(
    (actions, events, worker): plakous_hosted::runtime::HostWorker<PlakExtension>,
) -> (
    UserActionSender,
    HostEventReceiver,
    plakous_hosted::runtime::HostWorkerHandle,
) {
    let (actions, events) = plakous_hosted::host_service::face(actions, events);
    (actions, events, worker)
}
