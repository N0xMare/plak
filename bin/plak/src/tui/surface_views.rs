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
        lines.push(McpOverview::NONE_CONFIGURED.to_owned());
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
                    config
                        .lines(&server.id)
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
    fn unreadable_config_does_not_claim_that_no_servers_are_configured() {
        let text = super::mcp_overview(&crate::surface_protocol::McpOverview {
            diagnostics: vec!["MCP configuration could not be read".into()],
            ..Default::default()
        });
        assert!(text.contains("could not be read"));
        assert!(!text.contains("no MCP servers configured"));
    }

    #[test]
    fn orphaned_servers_have_their_own_heading() {
        use crate::surface_protocol::{McpOverview, McpServerView, McpSessionBinding};
        let text = super::mcp_overview(&McpOverview {
            servers: vec![McpServerView {
                id: "removed".into(),
                configuration: Ok(None),
                binding: McpSessionBinding::Bound {
                    tools: 1,
                    earlier_definition: false,
                },
            }],
            ..Default::default()
        });
        assert!(text.contains("bound servers no longer configured:\n  removed:"));
        assert!(!text.contains("\nconfigured servers:"));
        assert!(!text.contains("no MCP servers configured"));
    }

    #[test]
    #[cfg(feature = "mcp")]
    fn cli_and_panel_share_configured_server_lines() {
        use crate::commands::mcp::{McpCommand, run, session_overview};
        let (_root, paths) = crate::home::paths::PlakPaths::isolated_test_home();
        paths.ensure_layout().unwrap();
        std::fs::write(paths.config_file(), "[mcp_servers.fixture]\ntransport = \"stdio\"\ncommand = \"fixture-server\"\nargs = [\"--stdio\"]\n").unwrap();
        let mut output = Vec::new();
        run(
            &paths,
            McpCommand::List,
            &mut std::io::Cursor::new(Vec::<u8>::new()),
            &mut output,
            &mut Vec::new(),
            &mut crate::commands::login_flow::SystemLoginUi,
        )
        .unwrap();
        let cli = String::from_utf8(output).unwrap();
        assert!(cli.contains("fixture"));
        let panel = super::mcp_overview(&session_overview(&paths, None));
        for line in cli.lines().filter(|line| !line.trim().is_empty()) {
            assert!(
                panel
                    .lines()
                    .any(|row| row.strip_prefix("  ") == Some(line)),
                "CLI line {line:?} missing from panel:\n{panel}"
            );
        }
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
