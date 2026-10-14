//! The vocabulary the prompt text names, and nothing else.
//!
//! These three enums are the whole input surface of `prompts/**` beyond
//! `plakous` itself. They exist so this directory names no product type: a
//! prompt is instruction text plus the facts that shape it, and "which
//! `ProductCompositionSelection` did the operator configure" is not one of
//! those facts — "does this node call its tools directly or inside a cell" is.
//!
//! The crate above translates its own enums into these in one place, so a
//! composition root's configuration vocabulary never reaches a prompt.

/// How a node reaches its tools, as the instruction text sees it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Composition {
    /// The node calls presented tools directly.
    Direct,
    /// The node calls tools inside a provider-native code cell.
    NativeCell,
    /// The node calls tools inside a JavaScript cell.
    JavaScriptCell,
    /// The node calls tools inside a Python cell.
    PythonCell,
}

/// A child's capability ceiling, as its Start prompt says it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentTier {
    /// Process tools allowed; Plak file writes refused.
    ReadOnly,
    /// Process tools allowed; Plak file writes refused.
    Execute,
    /// Process tools and Plak file writes allowed.
    Mutate,
}

impl AgentTier {
    /// The spelling the Start prompt writes after `Tier:`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Execute => "execute",
            Self::Mutate => "mutate",
        }
    }
}
