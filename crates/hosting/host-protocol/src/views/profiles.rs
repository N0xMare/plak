//! Shared profile summaries; resolution and readiness remain product-owned.

/// One role line in the Agents detail box.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
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
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
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
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ProfileDifference {
    /// The session uses another root account.
    RootAccount {
        /// Frozen session account.
        session: String,
        /// Current profile account.
        profile: String,
    },
    /// The session uses another root model.
    RootModel {
        /// Frozen session model.
        session: String,
        /// Current profile model.
        profile: String,
    },
    /// The session uses another root effort.
    RootEffort {
        /// Frozen session effort.
        session: String,
        /// Current profile effort.
        profile: String,
    },
    /// The session uses another fast-lane selection.
    RootFast {
        /// Frozen session fast-lane selection.
        session: bool,
    },
    /// Working-set names or order differ.
    WorkingSet,
    /// A named card differs from its current profile definition.
    Card(String),
    /// The frozen roster differs or could not be compared.
    Roster,
}

/// The product's profile list, as the Agents tab shows it.
#[derive(
    serde::Serialize, serde::Deserialize, Clone, Debug, Default, Eq, PartialEq, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ProfilesView {
    /// Built-ins first, then user profiles by name.
    pub rows: Vec<ProfileRow>,
    /// The row the config's root and working set already point at.
    pub current: Option<String>,
    /// One line per profile file that was left out, and why.
    pub diagnostics: Vec<String>,
}
