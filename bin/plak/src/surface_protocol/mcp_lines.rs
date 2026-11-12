//! Product CLI formatting for MCP configuration values.

pub(crate) const NONE_CONFIGURED: &str = "no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`";

/// The command that stores a secret a server placed `on_runner` reads.
pub(crate) const fn set_command(on_runner: bool) -> &'static str {
    if on_runner {
        "plak runner secret set"
    } else {
        "plak secret set"
    }
}

pub(crate) fn server_lines(
    config: &plakd_wire_types::McpServerConfiguration,
    id: &str,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{id}  {}  {}  {}",
        config.transport, config.scope, config.target
    )];
    lines.push(format!(
        "  templates: {}",
        if config.templates.is_empty() {
            "none".to_owned()
        } else {
            config.templates.join(", ")
        }
    ));
    if config.on_runner {
        lines.push("  placement: runner".to_owned());
    }
    if !config.missing_secrets.is_empty() {
        lines.push(format!(
            "  missing secrets: {} ({} <name>)",
            config.missing_secrets.join(", "),
            set_command(config.on_runner)
        ));
    }
    if config.resources {
        lines.push("  resources: enabled".to_owned());
    }
    if let Some(login) = &config.login {
        lines.push(format!("  {login}"));
    }
    lines
}
