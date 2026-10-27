//! MCP configuration text shared by `plak mcp list` and the terminal panel.

impl super::McpOverview {
    pub(crate) const NONE_CONFIGURED: &str = "no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`";
}

impl super::McpServerConfiguration {
    /// One product-format server block, shared by CLI and terminal panels.
    pub(crate) fn lines(&self, id: &str) -> Vec<String> {
        let mut lines = vec![format!(
            "{id}  {}  {}  {}",
            self.transport, self.scope, self.target
        )];
        lines.push(format!(
            "  templates: {}",
            if self.templates.is_empty() {
                "none".to_owned()
            } else {
                self.templates.join(", ")
            }
        ));
        if !self.missing_secrets.is_empty() {
            lines.push(format!(
                "  missing secrets: {} (plak secret set <name>)",
                self.missing_secrets.join(", ")
            ));
        }
        if self.resources {
            lines.push("  resources: enabled".to_owned());
        }
        if let Some(login) = &self.login {
            lines.push(format!("  {login}"));
        }
        lines
    }
}
