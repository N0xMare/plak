//! Result-mail batches and immutable branch-workflow request vocabulary.

use super::{ProductCompositionSelection, ProductTargetSelection, RootProfile};
use serde::{Deserialize, Serialize};

/// Exact durable tree messages included in one admitted parent command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResultMailBatch {
    /// Tree whose durable mailbox owns these messages.
    pub tree_id: String,
    /// Exact parent node receiving the admitted command.
    pub recipient: String,
    /// Exact included message identities, not a request to reread all pending mail.
    pub message_ids: Vec<String>,
}

/// Reconstruction-complete successor request, journaled before runtime work.
/// Stored configuration and account evidence bind reconciliation to this exact
/// selection; current defaults must not silently replace any of them.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RebindSelection {
    /// Selected provider identity under the target's construction contract.
    pub provider: String,
    /// Exact default or named credential account.
    pub account: String,
    /// Resolved literal model id, not a recommendation alias.
    pub model: String,
    /// Explicit reasoning selection, absent for the admitted default.
    pub effort: Option<String>,
    /// Saved tool-presentation choice for the successor.
    pub composition: ProductCompositionSelection,
    /// Exact target, including gateway wire and routing facts where applicable.
    pub target_selection: ProductTargetSelection,
    /// Frozen protocol identity used by the reconstructed binding.
    pub protocol: String,
    /// Frozen route identity, independent of current product defaults.
    pub route: String,
    /// Original requested model spelling retained beside the resolved model.
    pub requested_model: String,
    /// Canonical selected provider configuration encoding.
    pub selected_configuration: String,
    /// Canonical effective provider configuration encoding after admission.
    pub effective_configuration: String,
    /// Exact account-binding evidence; not credential material or a fallback hint.
    pub account_binding: String,
    /// The profile of the tree named after the successor's own id that it
    /// compiles against (a profile fork's seeded tree, or a py-sdk root with
    /// none), which its record freezes; `None` when it stays on its source's
    /// tree, node and profile. A required key: absent is refused, `null` is
    /// the source's tree.
    #[serde(deserialize_with = "Option::deserialize")]
    pub own_tree: Option<RootProfile>,
    /// The non-root account choices a profile's fill gives the successor;
    /// `None`, omitted, keeps the source's (spec 02 §4.4.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_choices: Option<std::collections::BTreeMap<String, String>>,
}

/// A profile switch of the root node onto the same line (live succession):
/// the target binding, the profile and its frozen roster.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

/// Immutable workflow intent; its recorded operation identity governs replay.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionWorkflowKind {
    /// One paid compaction operation, retaining its original admission reason.
    Compact(ProductCompactReason),
    /// `/new`: a new session (a new node, tree and line) on the current
    /// binding; the source is left as it is.
    New,
    /// Explicit branch successor retaining the source's bound tuple.
    Fork,
    /// Explicit successor under a new, reconstruction-complete binding.
    Rebind(Box<RebindSelection>),
    /// Decoded today; live succession writes it.
    Switch(Box<SwitchSelection>),
}

impl SessionWorkflowKind {
    /// Stable workflow-kind token; payload details remain in the journaled intent.
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

/// Origin of a paid compaction admission, not permission to dispatch it again.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductCompactReason {
    /// Explicit operator-requested compaction.
    Manual,
    /// Host/tool scheduling policy requested compaction.
    Scheduled,
    /// Explicit operator recovery after a terminal in-turn compactable overflow.
    CapacityRecovery,
}

impl ProductCompactReason {
    /// Stable persisted reason token, independent of frontend command spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Scheduled => "scheduled",
            Self::CapacityRecovery => "capacity_recovery",
        }
    }

    /// Accept only the closed persisted token set; unknown tokens return `None`.
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
