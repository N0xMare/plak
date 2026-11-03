//! The owner of one agent line, as the wire names it.

use serde::{Deserialize, Serialize};

/// The head of one agent line: the line, its generation, the head branch and
/// that branch's lease fence. Owners of one line are ordered by
/// `(generation, fence)`, so a successor head is always newer; a runner
/// refuses work from an owner older than the newest it has accepted.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LineOwner {
    /// The agent line.
    pub line: String,
    /// The line generation of the owning head branch.
    pub generation: u64,
    /// The head branch.
    pub branch: String,
    /// The head branch's lease fence.
    pub fence: u64,
}

impl LineOwner {
    /// Whether `self` is a strictly newer owner of the same line than
    /// `other`.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.line == other.line && (self.generation, self.fence) > (other.generation, other.fence)
    }

    /// Whether `self` is `other` or a newer owner of the same line.
    #[must_use]
    pub fn is_at_least(&self, other: &Self) -> bool {
        self == other || self.is_newer_than(other)
    }
}
