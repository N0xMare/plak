//! Plak's extension of the host protocol: the typed settings snapshot the
//! product's surfaces edit, the intents and observations that carry it and
//! the usage overlay beside the closed [`plakous_host_protocol`] vocabulary,
//! and the concrete mailbox types every Plak surface holds.
//!
//! The protocol crate never reads this traffic; it only classifies it for the
//! bounded observation mailbox through [`ObservationExtension`].

use std::fmt;

use plakous_host_protocol::{
    CompositionSelection, IntentReceiver, IntentSendError, IntentSender, ObservationExtension,
    ObservationReceiver, ObservationSender, ProtocolExtension, ProtocolSelection, SessionIntent,
    SessionObservation, intent_channel, observation_channel,
};

/// Product-authoritative editable settings.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "spec 8.1 fixes nine independent switch rows; a state machine would hide them"
)]
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
    /// Whether mouse input is enabled on the next TUI launch.
    pub mouse: bool,
    /// Whether automatic product compaction is enabled.
    pub compaction: bool,
    /// Transcript lines moved per wheel tick.
    pub scroll_lines: u16,
    /// Render reasoning blocks where the provider policy admits them.
    pub show_thinking: bool,
    /// Verb grouping for consecutive tool rows (§5.5).
    pub group_tool_calls: bool,
    /// Render edits as a one-line diffstat.
    /// Suppress spinner, pulse and camera easing.
    pub reduced_motion: bool,
    /// Current supported product theme.
    pub theme: SettingsTheme,
}

impl Default for SettingsSnapshot {
    /// The config-schema defaults, so a test fixture cannot drift from §5.5.
    fn default() -> Self {
        let mut snapshot = crate::shell::settings::settings_snapshot(
            &crate::product_config::ProductConfig::default_product(),
        );
        // A default snapshot has no host-resolved composition availability.
        snapshot.composition_options = Box::default();
        snapshot
    }
}

/// Product-resolved availability for one exact composition choice and default binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SettingsCompositionOption {
    /// Closed selection represented by this option.
    pub composition: CompositionSelection,
    /// Exact model-visible presentation if admitted.
    pub effective_presentation: String,
    /// Fail-closed reason when this choice is not admitted for the binding.
    pub unavailable_reason: Option<String>,
}

/// Closed set of product themes supported by this TUI build (spec 02 §5.5).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SettingsTheme {
    /// Plak's original high-contrast dark product theme.
    #[default]
    DefaultDark,
    /// Plak's high-contrast light product theme.
    DefaultLight,
    /// Catppuccin's light flavor.
    CatppuccinLatte,
    /// Catppuccin's muted dark flavor.
    CatppuccinFrappe,
    /// Catppuccin's medium-contrast dark flavor.
    CatppuccinMacchiato,
    /// Catppuccin's darkest color-rich flavor.
    CatppuccinMocha,
}

impl SettingsTheme {
    /// Every theme in settings order.
    pub const ALL: [Self; 6] = [
        Self::DefaultDark,
        Self::DefaultLight,
        Self::CatppuccinLatte,
        Self::CatppuccinFrappe,
        Self::CatppuccinMacchiato,
        Self::CatppuccinMocha,
    ];

    /// Stable configuration label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DefaultDark => "default-dark",
            Self::DefaultLight => "default-light",
            Self::CatppuccinLatte => "catppuccin-latte",
            Self::CatppuccinFrappe => "catppuccin-frappe",
            Self::CatppuccinMacchiato => "catppuccin-macchiato",
            Self::CatppuccinMocha => "catppuccin-mocha",
        }
    }

    /// Next theme in the control-center cycle.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::DefaultDark => Self::DefaultLight,
            Self::DefaultLight => Self::CatppuccinLatte,
            Self::CatppuccinLatte => Self::CatppuccinFrappe,
            Self::CatppuccinFrappe => Self::CatppuccinMacchiato,
            Self::CatppuccinMacchiato => Self::CatppuccinMocha,
            Self::CatppuccinMocha => Self::DefaultDark,
        }
    }

    /// Parses a canonical label. `dark` and `light` remain accepted aliases.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "dark" => Some(Self::DefaultDark),
            "light" => Some(Self::DefaultLight),
            other => Self::ALL
                .into_iter()
                .find(|candidate| candidate.as_str() == other),
        }
    }
}

/// One role line in the Agents detail box.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProfileRow {
    /// Built-in id or `~/.plak/profiles` file stem.
    pub id: String,
    /// Other names `/agents <name>` accepts for this row.
    pub aliases: Vec<String>,
    /// Root template.
    pub root: ProfileRole,
    /// Working-set cards, in order.
    pub cards: Vec<ProfileRole>,
}

/// The product's profile list, as the Agents tab shows it.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProfilesView {
    /// Built-ins first, then user profiles by name.
    pub rows: Vec<ProfileRow>,
    /// The row the config's root and working set already point at.
    pub current: Option<String>,
    /// One line per profile file that was left out, and why.
    pub diagnostics: Vec<String>,
}

/// How an applied profile landed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileOutcome {
    /// Saved; the next new `plak` session uses it.
    Saved,
    /// This session branched onto it, and it is saved for the next session.
    Forked,
    /// An unattached, empty session took its root, and it is saved.
    Bound,
    /// This session already runs that root with those cards.
    AlreadyActive,
}

/// Complete typed settings replacement sent to the product host.
pub type SettingsUpdate = SettingsSnapshot;

/// Plak's product traffic beside the closed host protocol: the typed settings
/// pane and the usage overlay ride the same mailboxes as extension payloads.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlakExtension;

impl ProtocolExtension for PlakExtension {
    type Intent = SurfaceIntent;
    type Observation = SurfaceObservation;
}

/// One acknowledged attention occurrence.
///
/// An acknowledgement covers this attempt's attention label and end stamp,
/// not every later time the same attempt needs the operator.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
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
#[derive(Clone, Eq, PartialEq)]
pub enum SurfaceIntent {
    /// Open raw `config.toml` in `$EDITOR`.
    OpenConfigFile,
    /// Request the typed product settings snapshot.
    OpenSettings,
    /// Persist a complete validated product settings replacement.
    SaveSettings(SettingsUpdate),
    /// Open usage / limits pane.
    OpenUsage,
    /// Request the profile list ([`SurfaceObservation::Profiles`]).
    ListProfiles,
    /// Re-read workspace branch and profile facts for composer chrome.
    RefreshComposerChrome,
    /// Request the read-only MCP view ([`SurfaceObservation::McpOverview`]).
    ShowMcp,
    /// Select a named agent profile (built-in or `~/.plak/profiles`).
    ///
    /// `fork_now` branches the attached session onto that profile (or binds
    /// an unattached, empty one) and saves it once that lands. Otherwise the
    /// pointers are saved and the next new `plak` session uses them.
    ApplyProfile {
        /// Profile id or alias.
        name: String,
        /// Use it in this session now.
        fork_now: bool,
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SurfaceObservation {
    /// Replace and open the typed product settings pane.
    SettingsSnapshot(SettingsSnapshot),
    /// Complete settings replacement was durably persisted by the product host.
    SettingsApplied(SettingsSnapshot),
    /// Loading typed product settings failed and is retryable.
    SettingsLoadFailed(String),
    /// Persisting typed product settings failed; the draft remains open.
    SettingsSaveFailed(String),
    /// Open the honest usage overlay after preceding usage state updates.
    OpenUsageOverlay,
    /// The current profile list.
    Profiles(ProfilesView),
    /// Session identity and its own frozen cards. `id` is `custom` when no
    /// saved profile matches the complete binding and frozen roster.
    SessionProfile(ProfileRow),
    /// Fresh product-owned workspace presentation, never session authority.
    WorkspaceChrome(String),
    /// The read-only MCP view: configured servers and this session's tools.
    McpOverview(String),
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

    fn kind_label(&self) -> &'static str {
        match self {
            Self::SettingsSnapshot(_) => "settings_snapshot",
            Self::SettingsApplied(_) => "settings_applied",
            Self::SettingsLoadFailed(_) => "settings_load_failed",
            Self::SettingsSaveFailed(_) => "settings_save_failed",
            Self::OpenUsageOverlay => "usage_overlay",
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
/// Producer half of Plak's intent mailbox.
pub type UserActionSender = IntentSender<PlakExtension>;
/// Consumer half of Plak's intent mailbox.
pub type UserActionReceiver = IntentReceiver<PlakExtension>;
/// Typed refusal of a Plak intent by the bounded mailbox.
pub type UserActionSendError = IntentSendError<PlakExtension>;
/// Producer half of Plak's observation mailbox.
pub type HostEventSender = ObservationSender<PlakExtension>;
/// Consumer half of Plak's observation mailbox.
pub type HostEventReceiver = ObservationReceiver<PlakExtension>;

/// Construct Plak's bounded intent mailbox.
#[must_use]
pub fn user_action_channel() -> (UserActionSender, UserActionReceiver) {
    intent_channel()
}

/// Construct Plak's bounded observation mailbox.
#[must_use]
pub fn host_event_channel() -> (HostEventSender, HostEventReceiver) {
    observation_channel()
}
