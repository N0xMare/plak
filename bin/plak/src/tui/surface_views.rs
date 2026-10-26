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
    if view.servers.is_empty() {
        lines.push("no MCP servers configured; add one with `plak mcp add <id> -- <command> [args...]` or `plak mcp add <id> --url <url>`".to_owned());
    } else {
        lines.push("configured servers:".to_owned());
    }
    for server in &view.servers {
        if let Ok(Some(config)) = &server.configuration {
            lines.push(format!(
                "  {}  {}  {}  {}",
                server.id, config.transport, config.scope, config.target
            ));
            lines.push(format!(
                "    templates: {}",
                if config.templates.is_empty() {
                    "none".to_owned()
                } else {
                    config.templates.join(", ")
                }
            ));
            if !config.missing_secrets.is_empty() {
                lines.push(format!(
                    "    missing secrets: {} (plak secret set <name>)",
                    config.missing_secrets.join(", ")
                ));
            }
            if config.resources {
                lines.push("    resources: enabled".to_owned());
            }
            if let Some(login) = &config.login {
                lines.push(format!("    {login}"));
            }
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

#[cfg(test)]
mod tests {
    #[test]
    fn workspace_home_prefix_is_shortened_only_at_a_path_boundary() {
        use super::{WorkspaceChrome, workspace_path};
        for (path, expected) in [
            ("/example/home/dev/plak", "~/dev/plak"),
            ("/example/home", "~"),
            (
                "/example/home-sibling/dev/plak",
                "/example/home-sibling/dev/plak",
            ),
        ] {
            let view = WorkspaceChrome {
                path: path.into(),
                home: Some("/example/home".into()),
                branch: None,
            };
            assert_eq!(workspace_path(&view), expected);
        }
    }

    #[test]
    fn mcp_view_keeps_read_failures_distinct_from_removed_servers_and_frozen_tools() {
        use crate::surface_protocol::{McpOverview, McpServerView, McpSessionBinding};
        let view = McpOverview {
            servers: vec![
                McpServerView {
                    id: "broken".into(),
                    configuration: Err("credential store unavailable".into()),
                    binding: McpSessionBinding::NotBound,
                },
                McpServerView {
                    id: "removed".into(),
                    configuration: Ok(None),
                    binding: McpSessionBinding::Bound {
                        tools: 2,
                        earlier_definition: false,
                    },
                },
            ],
            omitted: vec!["optional server unavailable at bind".into()],
            ..Default::default()
        };
        let text = super::mcp_overview(&view);
        assert!(text.contains("broken: credential store unavailable"));
        assert!(!text.contains("broken: no longer configured"));
        assert!(text.contains("removed: no longer configured"));
        assert!(text.contains("bound, 2 tools"));
        assert!(text.contains("optional server unavailable at bind"));
    }

    #[test]
    fn every_panel_command_is_a_plak_command() {
        for command in super::PANEL_COMMANDS {
            let concrete = command
                .replace("<id>", "docs")
                .replace("<command> [args...]", "srv --stdio")
                .replace("<url>", "https://x.test/mcp")
                .replace("<name>", "worker");
            let words = concrete
                .split_whitespace()
                .skip(1)
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            assert!(
                matches!(
                    crate::cli::parse(words),
                    Ok(crate::command::Command::Mcp(_))
                ),
                "{command} does not parse as a plak mcp command"
            );
        }
    }
}
