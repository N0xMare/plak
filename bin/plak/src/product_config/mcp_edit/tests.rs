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
        resources: false,
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
    .unwrap()
    .file;
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
            oauth: None,
        },
        tools: None,
        resources: false,
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
        refused.contains("templates would be left listing mcp:docs: worker;"),
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
    assert_eq!(
        removed.disabled,
        [TemplateFile {
            layer: Layer::Home,
            name: "worker".to_owned()
        }]
    );
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

const WORKER: &str = "account = \"codex\"\nmodel = \"m\"\ndescription = \"worker\"\n";

/// A home and a trusted project, each with a config file and agents directory.
fn trusted_project() -> (tempfile::TempDir, tempfile::TempDir, PlakPaths) {
    let temp = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join(".plak/agents")).unwrap();
    fs::write(project.path().join(".plak/config.toml"), "").unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"))
        .with_project_root(project.path().to_path_buf());
    paths.ensure_layout().unwrap();
    super::super::project_trust::trust(&paths, project.path()).unwrap();
    (temp, project, paths)
}

fn allowing(tools: &[&str]) -> McpServerConfig {
    let mut server = stdio("srv", &[]);
    server.tools = Some(tools.iter().map(|tool| (*tool).to_owned()).collect());
    server
}

fn project_template(project: &tempfile::TempDir, name: &str, tools: &str) -> PathBuf {
    let path = project.path().join(format!(".plak/agents/{name}.toml"));
    fs::write(&path, format!("{WORKER}tools = {tools}\n")).unwrap();
    path
}

#[test]
fn a_home_template_cannot_list_what_only_the_project_serves() {
    let (_temp, _project, paths) = trusted_project();
    add_server(
        &paths,
        ConfigWriteScope::Project,
        "gh",
        &allowing(&["a", "b"]),
        false,
    )
    .unwrap();
    let home = write_template(&paths, "worker", WORKER);
    let refused =
        edit_template_tools(&paths, "worker", "gh", &TemplateMcpEdit::EnableAll).unwrap_err();
    assert!(refused.contains("mcp:gh is defined only in"), "{refused}");
    assert!(
        refused.contains("a home template cannot list it"),
        "{refused}"
    );

    // A tool the project's allowlist has and the home's does not.
    add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &allowing(&["a"]),
        false,
    )
    .unwrap();
    let refused = edit_template_tools(
        &paths,
        "worker",
        "gh",
        &TemplateMcpEdit::EnableTools(vec!["b".to_owned()]),
    )
    .unwrap_err();
    assert!(refused.contains("the home configuration"), "{refused}");
    assert_eq!(fs::read_to_string(&home).unwrap(), WORKER);
    ProductConfig::load_user(&paths).unwrap();
    edit_template_tools(
        &paths,
        "worker",
        "gh",
        &TemplateMcpEdit::EnableTools(vec!["a".to_owned()]),
    )
    .unwrap();
    ProductConfig::load_user(&paths).unwrap();
}

#[test]
fn removing_the_home_table_checks_home_templates_the_project_table_hides() {
    let (_temp, _project, paths) = trusted_project();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    add_server(
        &paths,
        ConfigWriteScope::Project,
        "gh",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    let home = write_template(&paths, "worker", &format!("{WORKER}tools = [\"mcp:gh\"]\n"));
    let refused = remove_server(&paths, Some(ConfigWriteScope::User), "gh", false).unwrap_err();
    assert!(refused.contains("mcp:gh: worker;"), "{refused}");
    let removed = remove_server(&paths, Some(ConfigWriteScope::User), "gh", true).unwrap();
    assert_eq!(removed.disabled.len(), 1);
    assert!(!fs::read_to_string(&home).unwrap().contains("mcp:gh"));
    ProductConfig::load_user(&paths).unwrap();
    ProductConfig::load(&paths).unwrap();
}

#[test]
fn removing_the_project_table_checks_project_templates_against_the_home_allowlist() {
    let (_temp, project, paths) = trusted_project();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &allowing(&["a"]),
        false,
    )
    .unwrap();
    add_server(
        &paths,
        ConfigWriteScope::Project,
        "gh",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    let path = project_template(&project, "worker", "[\"mcp:gh/b\"]");
    let refused = remove_server(&paths, Some(ConfigWriteScope::Project), "gh", false).unwrap_err();
    assert!(refused.contains("worker (project)"), "{refused}");
    remove_server(&paths, Some(ConfigWriteScope::Project), "gh", true).unwrap();
    assert!(!fs::read_to_string(&path).unwrap().contains("mcp:gh"));
    ProductConfig::load(&paths).unwrap();
}

#[test]
fn removing_checks_a_home_template_a_project_template_shadows() {
    let (_temp, project, paths) = trusted_project();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    write_template(&paths, "worker", &format!("{WORKER}tools = [\"mcp:gh\"]\n"));
    project_template(&project, "worker", "[]");
    let refused = remove_server(&paths, None, "gh", false).unwrap_err();
    assert!(refused.contains("mcp:gh: worker;"), "{refused}");
    assert!(ProductConfig::load_user(&paths).is_ok());
}

#[test]
fn a_home_replace_is_checked_against_the_home_even_when_the_project_hides_it() {
    let (_temp, _project, paths) = trusted_project();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &allowing(&["a", "b"]),
        false,
    )
    .unwrap();
    let added = add_server(
        &paths,
        ConfigWriteScope::Project,
        "gh",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    assert_eq!(added.overrides, Some(paths.config_file()));
    write_template(
        &paths,
        "worker",
        &format!("{WORKER}tools = [\"mcp:gh/b\"]\n"),
    );
    let before = fs::read_to_string(paths.config_file()).unwrap();
    let refused = add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &allowing(&["a"]),
        true,
    )
    .unwrap_err();
    assert!(refused.contains("the home configuration"), "{refused}");
    assert_eq!(fs::read_to_string(paths.config_file()).unwrap(), before);
    let added = add_server(
        &paths,
        ConfigWriteScope::User,
        "gh",
        &allowing(&["b"]),
        true,
    )
    .unwrap();
    assert!(added.overridden_by.is_some());
}

#[test]
fn disable_keeps_the_comments_of_the_entries_that_stay() {
    let (_temp, paths) = home();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "docs",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    let body = |tools: &str| format!("{WORKER}tools = [\n{tools}]\n");
    let path = write_template(
        &paths,
        "worker",
        &body("  \"web_search\", # search\n  \"mcp:docs\", # docs\n  \"fetch_url\", # fetch\n"),
    );
    edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::Disable).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        body("  \"web_search\", # search\n  \"fetch_url\", # fetch\n")
    );
    edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::EnableAll).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        body("  \"web_search\", # search\n  \"fetch_url\", # fetch\n  \"mcp:docs\",\n")
    );
    edit_template_tools(&paths, "worker", "docs", &TemplateMcpEdit::Disable).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        body("  \"web_search\", # search\n  \"fetch_url\", # fetch\n")
    );
}

#[test]
fn remove_hands_the_header_comment_on_and_replace_keeps_it() {
    let (_temp, paths) = home();
    fs::write(
        paths.config_file(),
        "# servers\n\n# the docs server\n[mcp_servers.docs]\ntransport = \"stdio\"\ncommand = \"a\"\n\n[turn]\nauto_continue = false\n",
    )
    .unwrap();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "docs",
        &stdio("b", &[]),
        true,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(paths.config_file()).unwrap(),
        "# servers\n\n# the docs server\n[mcp_servers.docs]\ntransport = \"stdio\"\ncommand = \"b\"\n\n[turn]\nauto_continue = false\n"
    );
    remove_server(&paths, None, "docs", false).unwrap();
    let text = fs::read_to_string(paths.config_file()).unwrap();
    assert!(text.contains("# servers"), "{text}");
    assert!(text.contains("[turn]\nauto_continue = false"), "{text}");
    assert!(!text.contains("mcp_servers"), "{text}");
}

#[cfg(unix)]
#[test]
fn publication_writes_through_a_symlink_and_keeps_the_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let (temp, paths) = home();
    let real = temp.path().join("real-config.toml");
    fs::write(&real, "").unwrap();
    fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
    std::os::unix::fs::symlink(&real, paths.config_file()).unwrap();
    add_server(
        &paths,
        ConfigWriteScope::User,
        "docs",
        &stdio("srv", &[]),
        false,
    )
    .unwrap();
    assert!(
        fs::symlink_metadata(paths.config_file())
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(
        fs::read_to_string(&real)
            .unwrap()
            .contains("[mcp_servers.docs]")
    );
    assert_eq!(
        fs::metadata(&real).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
