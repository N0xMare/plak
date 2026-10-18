#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::collections::BTreeMap;

use super::*;

fn home() -> (tempfile::TempDir, PlakPaths) {
    let temp = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"));
    paths.ensure_layout().unwrap();
    (temp, paths)
}

fn stdio(command: &str, args: &[&str]) -> McpServerConfig {
    McpServerConfig {
        transport: McpTransport::Stdio {
            command: command.to_owned(),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            env: BTreeMap::new(),
            secret_env: BTreeMap::new(),
        },
        tools: None,
        startup_seconds: DEFAULT_MCP_STARTUP_SECONDS,
        call_seconds: DEFAULT_MCP_CALL_SECONDS,
    }
}

fn write_template(paths: &PlakPaths, name: &str, body: &str) -> PathBuf {
    let dir = paths.user_agents_dir();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.toml"));
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn add_keeps_the_file_and_writes_only_given_keys() {
    let (_temp, paths) = home();
    let original = "# my config\n[turn]\nauto_continue = false # keep\n";
    fs::write(paths.config_file(), original).unwrap();
    let path = add_server(
        &paths,
        ConfigWriteScope::User,
        "everything",
        &stdio("npx", &["-y", "@modelcontextprotocol/server-everything"]),
        false,
    )
    .unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert_eq!(
        text,
        format!(
            "{original}\n[mcp_servers.everything]\ntransport = \"stdio\"\ncommand = \"npx\"\nargs = [\"-y\", \"@modelcontextprotocol/server-everything\"]\n"
        )
    );
    let loaded = ProductConfig::load(&paths).unwrap();
    assert!(loaded.tool_sections.mcp_servers.contains_key("everything"));

    let refused = add_server(
        &paths,
        ConfigWriteScope::User,
        "everything",
        &stdio("other", &[]),
        false,
    )
    .unwrap_err();
    assert!(refused.contains("--replace"), "{refused}");
    add_server(
        &paths,
        ConfigWriteScope::User,
        "everything",
        &stdio("other", &[]),
        true,
    )
    .unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(original), "{text}");
    assert!(text.contains("command = \"other\""), "{text}");
    assert!(!text.contains("npx"), "{text}");
}

#[test]
fn add_refuses_what_the_load_parser_refuses_and_writes_nothing() {
    let (_temp, paths) = home();
    let server = McpServerConfig {
        transport: McpTransport::Http {
            url: "http://example.com/mcp".to_owned(),
            bearer_secret: None,
            headers: BTreeMap::new(),
            secret_headers: BTreeMap::new(),
        },
        tools: None,
        startup_seconds: DEFAULT_MCP_STARTUP_SECONDS,
        call_seconds: DEFAULT_MCP_CALL_SECONDS,
    };
    let refused = add_server(&paths, ConfigWriteScope::User, "web", &server, false).unwrap_err();
    assert!(refused.contains("url must be https://"), "{refused}");
    let refused = add_server(
        &paths,
        ConfigWriteScope::User,
        "Bad-Id",
        &stdio("x", &[]),
        false,
    )
    .unwrap_err();
    assert!(refused.contains("not a valid MCP server id"), "{refused}");
    assert!(!paths.config_file().exists());
}

#[test]
fn enable_and_disable_touch_only_the_tools_array() {
    let (_temp, paths) = home();
    let mut server = stdio("srv", &[]);
    server.tools = Some(vec!["read".to_owned(), "list".to_owned()]);
    add_server(&paths, ConfigWriteScope::User, "docs", &server, false).unwrap();
    let body = "# worker\naccount = \"codex\"\nmodel = \"m\" # pinned\ndescription = \"worker\"\ntools = [\"web_search\"]\n\n[env]\nA = \"b\"\n";
    let path = write_template(&paths, "worker", body);

    let (_, list) = edit_template_tools(
        &paths,
        "worker",
        "docs",
        &TemplateMcpEdit::EnableTools(vec!["read".to_owned()]),
    )
    .unwrap();
    assert_eq!(list, ["web_search", "mcp:docs/read"]);
    let refused = edit_template_tools(
        &paths,
        "worker",
        "docs",
        &TemplateMcpEdit::EnableTools(vec!["write".to_owned()]),
    )
    .unwrap_err();
    assert!(refused.contains("does not allow"), "{refused}");

    let (_, list) =
        edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::EnableAll).unwrap();
    assert_eq!(list, ["web_search", "mcp:docs"]);
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        body.replace("[\"web_search\"]", "[\"web_search\", \"mcp:docs\"]")
    );

    let (_, list) =
        edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::Disable).unwrap();
    assert_eq!(list, ["web_search"]);
    assert_eq!(fs::read_to_string(&path).unwrap(), body);
    assert!(edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::Disable).is_err());
    let refused =
        edit_template_tools(&paths, "worker", "nothere", &TemplateMcpEdit::EnableAll).unwrap_err();
    assert!(
        refused.contains("has no [mcp_servers.nothere]"),
        "{refused}"
    );
}

#[test]
fn remove_refuses_while_templates_list_the_server_unless_forced() {
    let (_temp, paths) = home();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "docs",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    let path = write_template(
        &paths,
        "worker",
        "account = \"codex\"\nmodel = \"m\"\ndescription = \"worker\"\ntools = [\"mcp:docs\"]\n",
    );
    let refused = remove_server(&paths, None, "docs", false).unwrap_err();
    assert!(
        refused.contains("templates still list mcp:docs: worker"),
        "{refused}"
    );
    assert!(
        ProductConfig::load(&paths)
            .unwrap()
            .tool_sections
            .mcp_servers
            .contains_key("docs")
    );

    let removed = remove_server(&paths, None, "docs", true).unwrap();
    assert_eq!(removed.disabled, ["worker"]);
    assert!(!fs::read_to_string(&path).unwrap().contains("mcp:docs"));
    let loaded = ProductConfig::load(&paths).unwrap();
    assert!(loaded.tool_sections.mcp_servers.is_empty());
    assert!(
        !fs::read_to_string(paths.config_file())
            .unwrap()
            .contains("mcp_servers")
    );
    assert!(remove_server(&paths, None, "docs", false).is_err());
}
