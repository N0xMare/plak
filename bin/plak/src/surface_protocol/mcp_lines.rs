//! MCP configuration text shared by `plak mcp list` and the terminal panel.

impl super::McpOverview {
    pub(crate) const NONE_CONFIGURED: &str = "no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`";
}

/// The command that stores a secret a server placed `on_runner` reads.
pub(crate) const fn set_command(on_runner: bool) -> &'static str {
    if on_runner {
        "plak runner secret set"
    } else {
        "plak secret set"
    }
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
        if self.on_runner {
            lines.push("  placement: runner".to_owned());
        }
        if !self.missing_secrets.is_empty() {
            lines.push(format!(
                "  missing secrets: {} ({} <name>)",
                self.missing_secrets.join(", "),
                set_command(self.on_runner)
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
