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
    use crate::surface_protocol::McpOverview;
    use plakd_wire_types::{McpServerView, McpSessionBinding};
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
    use crate::surface_protocol::McpOverview;
    use plakd_wire_types::{McpServerView, McpSessionBinding};
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
