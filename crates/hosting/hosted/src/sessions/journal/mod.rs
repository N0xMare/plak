//! Result-mail batches and immutable branch-workflow request vocabulary.

use super::{ProductCompositionSelection, ProductTargetSelection};
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionWorkflowKind {
    Compact(ProductCompactReason),
    Clear,
    Fork,
    Rebind(Box<RebindSelection>),
}

impl SessionWorkflowKind {
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Compact(_) => "compact",
            Self::Clear => "clear",
            Self::Fork => "fork",
            Self::Rebind(_) => "rebind",
        }
    }

    pub(super) const fn branches(&self) -> bool {
        matches!(self, Self::Fork | Self::Rebind(_))
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
