//! The product's root binding, as the detached shell host edits it.
//!
//! A shell host with nothing attached is where an operator picks an account, a
//! model, an effort. Each pick is two writes that must agree: the session
//! record (host state) and the product's `[root]` template (product
//! configuration). The loop owns the first and the ordering; it must not own
//! the second, because the file's shape, its publication fence and its
//! composition table are product policy.
//!
//! So the loop edits a *draft* -- the six identity rows of a root binding --
//! against an opened document the product supplies, and the product persists
//! it. The refusal vocabulary travels with it, because the loop's rollback
//! depends on the distinction: a failure before publication is recoverable by
//! restoring the session record, one after it is not.

use crate::binding::{CompositionSelection, ProtocolSelection};

/// The six identity rows of a root binding (spec 02 §5.5).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RootBindingDraft {
    pub account: String,
    pub model: String,
    pub effort: Option<String>,
    /// Closed to `priority` on `codex`.
    pub extra: Option<String>,
    pub composition: Option<CompositionSelection>,
    pub protocol: Option<ProtocolSelection>,
}

impl RootBindingDraft {
    /// The wire the operator asked for, or the product's default.
    #[must_use]
    pub fn protocol_or_default(&self) -> ProtocolSelection {
        self.protocol.unwrap_or(ProtocolSelection::Auto)
    }
}

/// How a root-binding save failed.
///
/// The two arms are the loop's rollback decision, not a detail: before
/// publication the session write is rolled back and the operator keeps the old
/// binding; after it the binding stands and only its durability is in doubt.
#[derive(Debug)]
pub enum RootBindingSaveError {
    BeforePublication(String),
    PublishedDurabilityUncertain(String),
}

/// The product document a detached shell binds against.
pub trait RootBindingDocument: Send + Sync {
    /// Read the document once, for one operator pick.
    ///
    /// # Errors
    ///
    /// Returns a redaction-safe message when the document cannot be read; the
    /// loop then leaves both the session and the file alone.
    fn open(&self) -> Result<Box<dyn RootBindingEdit>, String>;
}

/// One opened root-binding document, edited and committed at most once.
pub trait RootBindingEdit: Send {
    /// The root binding the file carries now, if it configures one.
    fn current(&self) -> Option<RootBindingDraft>;

    /// The composition the product configures for one account+model, read from
    /// the same file this edit opened.
    fn composition_for(&self, account: Option<&str>, model: Option<&str>) -> CompositionSelection;

    /// Persist the edited binding. An empty account or model clears it.
    ///
    /// # Errors
    ///
    /// Returns the publication-relative failure the loop rolls back against.
    fn commit(self: Box<Self>, draft: RootBindingDraft) -> Result<(), RootBindingSaveError>;
}
