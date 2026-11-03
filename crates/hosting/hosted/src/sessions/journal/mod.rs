//! Result-mail batches and immutable branch-workflow request vocabulary.

use super::{ProductCompositionSelection, ProductTargetSelection, RootProfile};
use serde::{Deserialize, Serialize};

/// Exact durable tree messages included in one admitted parent command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultMailBatch {
    pub tree_id: String,
    pub recipient: String,
    pub message_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RebindSelection {
    pub provider: String,
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
    pub composition: ProductCompositionSelection,
    pub target_selection: ProductTargetSelection,
    pub protocol: String,
    pub route: String,
    pub requested_model: String,
    pub selected_configuration: String,
    pub effective_configuration: String,
    pub account_binding: String,
    /// The profile of the tree named after the successor's own id that it
    /// compiles against (a profile fork's seeded tree, or a py-sdk root with
    /// none), which its record freezes; `None` when it stays on its source's
    /// tree, node and profile.
    pub own_tree: Option<RootProfile>,
}

/// A profile switch of the root node onto the same line (live succession):
/// the target binding, the profile and its frozen roster.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchSelection {
    /// The successor's binding, as a rebind names it.
    pub rebind: RebindSelection,
    /// The profile switched to, as the Agents list names it.
    pub profile: String,
    /// Its root template and working set.
    pub root: RootProfile,
    /// The digest of the frozen roster the successor compiles against.
    pub roster_digest: String,
    /// The frozen pending snapshot's exact bytes for a root whose tree has
    /// no rows (a virtual tree of one); `None` when the tree's own
    /// `epoch_pending` row carries them.
    pub pending_snapshot: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SwitchSelectionWire {
    rebind: RebindSelection,
    profile: String,
    root: RootProfile,
    roster_digest: String,
    pending_snapshot: Option<String>,
}

impl SwitchSelection {
    /// The stored encoding.
    ///
    /// # Errors
    /// Returns a message for a selection that does not validate.
    pub fn encode(&self) -> Result<String, String> {
        self.validate()?;
        serde_json::to_string(&SwitchSelectionWire {
            rebind: self.rebind.clone(),
            profile: self.profile.clone(),
            root: self.root.clone(),
            roster_digest: self.roster_digest.clone(),
            pending_snapshot: self.pending_snapshot.clone(),
        })
        .map_err(|_| "switch selection is not serializable".to_owned())
    }

    /// Decodes [`Self::encode`]'s JSON.
    ///
    /// # Errors
    /// Returns a message for anything this build did not write, including
    /// pending snapshot bytes that are not a snapshot under `roster_digest`.
    pub fn decode(text: &str) -> Result<Self, String> {
        let wire: SwitchSelectionWire =
            serde_json::from_str(text).map_err(|_| "switch selection is corrupt".to_owned())?;
        let selection = Self {
            rebind: wire.rebind,
            profile: wire.profile,
            root: wire.root,
            roster_digest: wire.roster_digest,
            pending_snapshot: wire.pending_snapshot,
        };
        selection.validate()?;
        Ok(selection)
    }

    fn validate(&self) -> Result<(), String> {
        let invalid = || "switch selection is invalid".to_owned();
        let hex = self.roster_digest.len() == 64
            && self
                .roster_digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if self.profile.is_empty() || self.profile.chars().any(char::is_control) || !hex {
            return Err(invalid());
        }
        self.root.validate().map_err(|_| invalid())?;
        if let Some(bytes) = &self.pending_snapshot {
            let snapshot: crate::orchestration::OrchestrationSnapshot =
                serde_json::from_str(bytes).map_err(|_| invalid())?;
            if !snapshot.digest_matches() || snapshot.digest != self.roster_digest {
                return Err(invalid());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionWorkflowKind {
    Compact(ProductCompactReason),
    /// `/new`: a new session (a new node, tree and line) on the current
    /// binding; the source is left as it is.
    New,
    Fork,
    Rebind(Box<RebindSelection>),
    /// Decoded today; live succession writes it.
    Switch(Box<SwitchSelection>),
}

impl SessionWorkflowKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact(_) => "compact",
            Self::New => "new",
            Self::Fork => "fork",
            Self::Rebind(_) => "rebind",
            Self::Switch(_) => "switch",
        }
    }

    pub(super) const fn branches(&self) -> bool {
        matches!(self, Self::Fork | Self::Rebind(_) | Self::Switch(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProductCompactReason {
    Manual,
    Scheduled,
    /// Explicit operator recovery after a terminal in-turn compactable overflow.
    CapacityRecovery,
}

impl ProductCompactReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::CapacityRecovery => "capacity_recovery",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "manual" => Some(Self::Manual),
            "scheduled" => Some(Self::Scheduled),
            "capacity_recovery" => Some(Self::CapacityRecovery),
            _ => None,
        }
    }
}
