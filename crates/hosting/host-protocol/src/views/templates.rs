//! Agent-template readiness rows.

/// One agent template, as the host projected it with its readiness.
/// The host owns where templates live and which copy wins
/// a name collision; it computes readiness, and the TUI only renders it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TemplateRow {
    /// Template name, the `<name>` in `agents/<name>.toml`.
    pub name: String,
    /// Account the template binds.
    pub account: String,
    /// Exact catalog model id.
    pub model: String,
    /// Effort label where the provider admits one.
    pub effort: Option<String>,
    /// Composition selector label.
    pub composition: String,
    /// Processing tier label (`standard` or `priority`).
    pub tier: String,
    /// Fail-closed reason when the template is not Ready; `None` means Ready.
    pub unready_reason: Option<String>,
}

impl TemplateRow {
    /// Whether the host projected this template as Ready.
    #[must_use]
    pub const fn ready(&self) -> bool {
        self.unready_reason.is_none()
    }
}
