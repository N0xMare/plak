//! Product capability diagnostics.

/// Omitted implementation class; persisted names remain valid vocabulary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityKind {
    /// Provider authentication and model runtime.
    Route,
    /// Optional product input or presentation capability.
    Surface,
}

/// Typed build-time omission, not an authentication or model-admission failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityUnavailable {
    /// Stable route, composition or tool identity that was refused.
    pub id: &'static str,
    /// Implementation class that is absent.
    pub kind: CapabilityKind,
    /// Public Cargo feature needed to include the implementation.
    pub feature: &'static str,
}

impl std::fmt::Display for CapabilityUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "`{}` is not included in this build (compiled without feature `{}`)",
            self.id, self.feature
        )
    }
}
impl std::error::Error for CapabilityUnavailable {}

/// Sorted route and tool capability IDs reported to wire clients.
pub const PRODUCT_CAPABILITIES: &[&str] = &[
    "anthropic",
    "claude",
    "codex",
    "grok",
    "images",
    "js",
    "mcp",
    "openai",
    "openrouter",
    "python",
    "web-fetch",
    "xai-api",
];
