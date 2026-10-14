//! The tree runtime's own failure type.
//!
//! Distinct from [`TopologyError`](super::TopologyError), which is a refusal a
//! model asked for and reads on the wire under a `TopologyErrorCode`. Nothing
//! here has a wire code because nothing here answers a model: a poisoned hub
//! lock, a hub that is not attached, a store that will not open. The product
//! decides how to present one; the tree only says what happened.

use std::fmt;

/// A failure in the tree runtime's own plumbing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeRuntimeError {
    message: String,
}

impl TreeRuntimeError {
    /// A failure with `message` as its operator-facing detail.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// The operator-facing detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for TreeRuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TreeRuntimeError {}

/// What the tree runtime's plumbing returns.
pub type TreeRuntimeResult<T> = Result<T, TreeRuntimeError>;
