//! Permanent content claims are identity facts, not replayable effect journals.
use serde::{Deserialize, Serialize};

/// The existing product-store identity domains; none authorizes execution.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveIdentityKind {
    /// Product creation operation.
    Create,
    /// Queue item, including a consumed or deleted item.
    Queue,
    /// Immutable runtime command submission.
    Command,
    /// Settled delivery intent.
    Delivery,
    /// Immutable product workflow request or its closed disposition.
    Workflow,
}

impl MoveIdentityKind {
    /// Existing product-store spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Queue => "queue",
            Self::Command => "command",
            Self::Delivery => "delivery",
            Self::Workflow => "workflow",
        }
    }
    /// Existing product-store domain, without permissive unknown variants.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "create" => Some(Self::Create),
            "queue" => Some(Self::Queue),
            "command" => Some(Self::Command),
            "delivery" => Some(Self::Delivery),
            "workflow" => Some(Self::Workflow),
            _ => None,
        }
    }
}

/// Immutable ownership/content binding retained even after a queue or receipt is compacted.
#[doc(hidden)]
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MoveIdentityClaim {
    /// Identity within its permanent product domain.
    pub identity: String,
    /// Domain of the identity.
    pub kind: MoveIdentityKind,
    /// Product owner; a move never rewrites it.
    pub owner: String,
    /// Exact existing content binding, not a destination authority grant.
    pub binding: String,
    /// A closed product disposition, when the original claim is terminal.
    pub disposition: Option<String>,
    /// Original terminal target/content identity where the domain requires it.
    pub outcome: Option<String>,
}

impl MoveIdentityClaim {
    /// Validates one bounded identity fact, without interpreting it as a command.
    ///
    /// # Errors
    /// Refuses malformed identities or a disposition outside its existing domain.
    pub fn validate(&self, owner: &str) -> Result<(), String> {
        let text = |value: &str| {
            !value.is_empty() && value.len() <= 1024 && !value.chars().any(char::is_control)
        };
        let disposition = self.disposition.as_deref();
        let valid = match self.kind {
            MoveIdentityKind::Create => disposition.is_none() && self.outcome.is_none(),
            MoveIdentityKind::Queue => match disposition {
                None | Some("deleted") => self.outcome.is_none(),
                Some("consumed") => self.outcome.is_some(),
                _ => false,
            },
            MoveIdentityKind::Command | MoveIdentityKind::Delivery => {
                matches!(disposition, None | Some("rejected" | "product_committed"))
                    && self.outcome.is_none()
            }
            MoveIdentityKind::Workflow => match disposition {
                None | Some("terminal_no_change" | "discarded") => self.outcome.is_none(),
                Some("product_committed") => self.outcome.is_some(),
                _ => false,
            },
        };
        if !valid
            || self.owner != owner
            || !text(&self.identity)
            || !text(&self.binding)
            || self.outcome.as_ref().is_some_and(|value| !text(value))
        {
            return Err("invalid ownership-move identity claim".into());
        }
        Ok(())
    }
}
