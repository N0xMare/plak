//! Presentation of typed product observations.

use super::{McpOverview, McpSessionBinding, WorkspaceChrome};

/// The commands the `/mcp` panel names; a test walks each through the parser.
const PANEL_COMMANDS: &[&str] = &[
    "plak mcp add <id> -- <command> [args...]",
    "plak mcp add <id> --url <url>",
    "plak mcp test <id>",
    "plak mcp enable <id> --template <name>",
];

pub(super) fn workspace_path(view: &WorkspaceChrome) -> String {
    view.home
        .as_ref()
        .and_then(|home| view.path.strip_prefix(home).ok())
        .map_or_else(
            || view.path.display().to_string(),
            |suffix| {
                if suffix.as_os_str().is_empty() {
                    "~".to_owned()
                } else {
                    format!("~/{}", suffix.display())
                }
            },
        )
}

pub(super) fn mcp_overview(view: &McpOverview) -> String {
    if let Some(reason) = &view.unavailable {
        return reason.clone();
    }
    let mut lines = view.diagnostics.clone();
    let configured = view
        .servers
        .iter()
        .filter(|server| !matches!(server.configuration, Ok(None)))
        .collect::<Vec<_>>();
    if configured.is_empty() && view.config_readable {
        lines.push(NONE_CONFIGURED.to_owned());
    }
    let orphaned = view
        .servers
        .iter()
        .filter(|server| matches!(server.configuration, Ok(None)))
        .collect::<Vec<_>>();
    for (heading, servers) in [
        ("configured servers:", configured),
        ("bound servers no longer configured:", orphaned),
    ] {
        if servers.is_empty() {
            continue;
        }
        lines.push(heading.to_owned());
        for server in servers {
            if let Ok(Some(config)) = &server.configuration {
                lines.extend(
                    configuration_lines(config, &server.id)
                        .into_iter()
                        .map(|line| format!("  {line}")),
                );
            } else if let Err(error) = &server.configuration {
                lines.push(format!("  {}: {error}", server.id));
            } else {
                lines.push(format!("  {}: no longer configured", server.id));
            }
            lines.push(match server.binding {
            McpSessionBinding::NotBound => "    this session: not bound".to_owned(),
            McpSessionBinding::Bound { tools, earlier_definition } => {
                let count = format!("{tools} tool{}", if tools == 1 { "" } else { "s" });
                if earlier_definition { format!("    this session: bound with an earlier definition, {count}; new sessions get the current one") }
                else { format!("    this session: bound, {count}") }
            }
        });
        }
    }
    if !view.omitted.is_empty() {
        lines.push(String::new());
        lines.push("left out when this session bound (as its transcript still shows):".to_owned());
        lines.extend(view.omitted.iter().map(|line| format!("  {line}")));
    }
    lines.push(String::new());
    lines.push("change it from a shell (new sessions pick it up):".to_owned());
    lines.extend(PANEL_COMMANDS.iter().map(|command| format!("  {command}")));
    lines.join("\n")
}

const NONE_CONFIGURED: &str = "no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`";

fn configuration_lines(config: &plakd_wire_types::McpServerConfiguration, id: &str) -> Vec<String> {
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
            if config.on_runner {
                "plak runner secret set"
            } else {
                "plak secret set"
            }
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

#[cfg(test)]
mod tests;
