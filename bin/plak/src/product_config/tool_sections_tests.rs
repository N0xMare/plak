//! `access`, template `tools`, `[tools.*]`, `[mcp_servers.*]`, the reference
//! checks and the child subset rule.

#![allow(clippy::unwrap_used, reason = "unit tests")]

use std::fs;

use super::*;
use crate::home::paths::PlakPaths;

const FULL_SECTIONS: &str = r#"[tools.fetch_url]
max_bytes = 4096
timeout_seconds = 12

[tools.web_search]
max_uses = 3
allowed_domains = ["docs.rs", "example.com"]

[tools.x_search]
allowed_x_handles = ["xai", "rust_lang"]

[mcp_servers.github]
transport = "stdio"
command = "github-mcp-server"
args = ["stdio"]
env = { GITHUB_TOOLSETS = "repos,issues" }
secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = "github" }
tools = ["get_file_contents", "create_issue"]
startup_seconds = 15
call_seconds = 600

[mcp_servers.linear]
transport = "http"
url = "https://mcp.linear.app/mcp"
bearer_secret = "linear"
secret_headers = { "X-Workspace" = "linear-ws" }
headers = { "X-Client" = "plak" }
"#;

fn template_text(extra: &str) -> String {
    format!("account = \"codex\"\nmodel = \"m\"\ndescription = \"d\"\n{extra}")
}

fn with_template(config: &mut ProductConfig, name: &str, extra: &str) {
    let template = parse_template(&template_text(extra), name).unwrap();
    config.templates.insert(name.to_owned(), template);
}

#[test]
fn access_replaces_the_old_tools_string_with_a_migration_hint() {
    assert_eq!(
        parse_template(&template_text("tools = \"full\"\n"), "t").unwrap_err(),
        "template t: tools is now a list of tool names; the edit ceiling moved to access (line 4, column 9)"
    );
    assert_eq!(
        parse_template(&template_text("tools = \"read-execute\"\n"), "t").unwrap_err(),
        "template t: tools is now a list of tool names; the edit ceiling moved to access (line 4, column 9)"
    );
    assert_eq!(
        parse_template(&template_text("access = \"read-execute\"\n"), "t")
            .unwrap()
            .max_tier,
        Tier::Execute
    );
    assert_eq!(
        parse_template(&template_text(""), "t").unwrap().max_tier,
        Tier::Mutate
    );
    assert_eq!(
        parse_template(&template_text("access = \"root\"\n"), "t").unwrap_err(),
        "template t: access must be full|read-execute (line 4, column 10)"
    );
    assert_eq!(
        parse_template(&template_text("access = 1\n"), "t").unwrap_err(),
        "template t: access must be a string (line 4, column 10)"
    );
}

#[test]
fn template_tools_parse_in_order_with_located_errors() {
    let template = parse_template(
        &template_text("tools = [\"mcp:linear/list_issues\", \"web_search\", \"mcp:github\"]\n"),
        "t",
    )
    .unwrap();
    let names: Vec<String> = template
        .tools
        .entries()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        names,
        ["mcp:linear/list_issues", "web_search", "mcp:github"]
    );
    // The canonical render carries the list and reads back the same.
    let rendered = render_template(&template);
    assert!(
        rendered.contains("access = \"full\"\ntools = [\"mcp:linear/list_issues\", \"web_search\", \"mcp:github\"]\n"),
        "{rendered}"
    );
    assert_eq!(parse_template(&rendered, "t").unwrap(), template);
    // An empty list renders nothing.
    assert!(
        !render_template(&parse_template(&template_text("tools = []\n"), "t").unwrap())
            .contains("tools =")
    );

    for (list, message) in [
        (
            "[\"web_search\", \"web_search\"]",
            "tools: tool \"web_search\" is listed twice",
        ),
        ("[\"nope\"]", "tools: unknown tool \"nope\""),
        ("[\"mcp:Git\"]", "names an invalid MCP server id"),
        ("[\"mcp:github/\"]", "names an invalid MCP tool"),
        (
            "[\"mcp:github\", \"mcp:github/create_issue\"]",
            "is already included by mcp:github",
        ),
        ("[1]", "tools must be an array of tool names"),
        ("{ a = 1 }", "tools must be an array of tool names"),
    ] {
        let error = parse_template(&template_text(&format!("tools = {list}\n")), "t").unwrap_err();
        assert!(error.starts_with("template t: "), "{error}");
        assert!(error.contains(message), "{list}: {error}");
        assert!(error.ends_with("(line 4, column 9)"), "{list}: {error}");
    }
}

#[test]
fn tool_sections_parse_into_typed_options() {
    let config = ProductConfig::parse(FULL_SECTIONS).unwrap();
    let sections = &config.tool_sections;
    assert_eq!(
        sections.fetch_url,
        Some(FetchUrlOptions {
            max_bytes: 4096,
            timeout_seconds: 12
        })
    );
    assert_eq!(
        sections.web_search,
        Some(WebSearchOptions {
            max_uses: 3,
            allowed_domains: vec!["docs.rs".to_owned(), "example.com".to_owned()],
            blocked_domains: Vec::new(),
        })
    );
    assert_eq!(
        sections.x_search.as_ref().unwrap().allowed_x_handles,
        ["xai", "rust_lang"]
    );
    let github = &sections.mcp_servers["github"];
    assert_eq!(
        github.tools.as_deref(),
        Some(&["get_file_contents".to_owned(), "create_issue".to_owned()][..])
    );
    assert_eq!((github.startup_seconds, github.call_seconds), (15, 600));
    let McpTransport::Stdio {
        command,
        args,
        env,
        secret_env,
    } = &github.transport
    else {
        panic!("github is stdio");
    };
    assert_eq!(command, "github-mcp-server");
    assert_eq!(args, &["stdio"]);
    assert_eq!(env["GITHUB_TOOLSETS"], "repos,issues");
    assert_eq!(secret_env["GITHUB_PERSONAL_ACCESS_TOKEN"], "github");
    let linear = &sections.mcp_servers["linear"];
    assert_eq!(linear.tools, None);
    assert_eq!(
        (linear.startup_seconds, linear.call_seconds),
        (DEFAULT_MCP_STARTUP_SECONDS, DEFAULT_MCP_CALL_SECONDS)
    );
    assert_eq!(
        linear.transport,
        McpTransport::Http {
            url: "https://mcp.linear.app/mcp".to_owned(),
            bearer_secret: Some("linear".to_owned()),
            headers: BTreeMap::from([("X-Client".to_owned(), "plak".to_owned())]),
            secret_headers: BTreeMap::from([("X-Workspace".to_owned(), "linear-ws".to_owned())]),
        }
    );
    // A standard sub-table spells the same value as an inline one.
    let standard = ProductConfig::parse(
        "[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"gh\"\n[mcp_servers.github.env]\nA = \"b\"\n",
    )
    .unwrap();
    let inline = ProductConfig::parse(
        "[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"gh\"\nenv = { A = \"b\" }\n",
    )
    .unwrap();
    assert_eq!(standard, inline);
    // Presence enables nothing and defaults fill every omitted option.
    let defaults = ProductConfig::parse("[tools.fetch_url]\n[tools.web_search]\n").unwrap();
    assert_eq!(
        defaults.tool_sections.fetch_url,
        Some(FetchUrlOptions::default())
    );
    assert_eq!(
        defaults.tool_sections.web_search.unwrap().max_uses,
        DEFAULT_WEB_SEARCH_MAX_USES
    );
    // `config show` lists them; the secret names, never values.
    let shown = config.tool_sections.render();
    assert!(
        shown.contains("[mcp_servers.github]\ntransport = \"stdio\"\n"),
        "{shown}"
    );
    assert!(shown.contains("secret_env = { GITHUB_PERSONAL_ACCESS_TOKEN = \"github\" }"));
}

#[test]
fn every_tool_section_rule_is_a_located_load_error() {
    let stdio = "[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"gh\"\n";
    let http = "[mcp_servers.linear]\ntransport = \"http\"\nurl = \"https://mcp.linear.app/mcp\"\n";
    for (text, expected) in [
        (
            "[tools.fetch_url]\nmax_bytes = 0\n".to_owned(),
            "[tools.fetch_url] max_bytes must be an integer in 1..=8388608 (line 2, column 13)",
        ),
        (
            "[tools.fetch_url]\ntimeout_seconds = 121\n".to_owned(),
            "[tools.fetch_url] timeout_seconds must be an integer in 1..=120 (line 2, column 19)",
        ),
        (
            "[tools.web_search]\nmax_uses = 21\n".to_owned(),
            "[tools.web_search] max_uses must be an integer in 1..=20 (line 2, column 12)",
        ),
        (
            "[tools.web_search]\nallowed_domains = [\"a.com\"]\nblocked_domains = [\"b.com\"]\n"
                .to_owned(),
            "[tools.web_search] allowed_domains and blocked_domains cannot both be set; list one or the other (line 3, column 19)",
        ),
        (
            "[tools.web_search]\nblocked_domains = [\"not a domain\"]\n".to_owned(),
            "[tools.web_search] blocked_domains must be an array of at most 10 domain names (line 2, column 19)",
        ),
        (
            "[tools.x_search]\nallowed_x_handles = [\"@xai\"]\n".to_owned(),
            "[tools.x_search] allowed_x_handles must be an array of at most 20 X handles (1 to 15 letters, digits or _, no @) (line 2, column 21)",
        ),
        (
            "[tools.web_fetch]\nmax_uses = 1\n".to_owned(),
            "unknown config section [tools.web_fetch]; [tools.<name>] takes fetch_url, web_search or x_search",
        ),
        (
            "[tools.fetch_url]\nmax_redirects = 3\n".to_owned(),
            "unknown config key: tools.fetch_url.max_redirects",
        ),
        (
            format!("{stdio}cwd = \"/\"\n"),
            "unknown config key: mcp_servers.github.cwd",
        ),
        (
            "[mcp_servers.Git]\ntransport = \"stdio\"\ncommand = \"gh\"\n".to_owned(),
            "[mcp_servers.Git] is not a valid MCP server id; an id matches [a-z][a-z0-9]{0,15}(_[a-z0-9]+)* within 32 bytes (line 1, column 1)",
        ),
        (
            "[mcp_servers.github]\ncommand = \"gh\"\n".to_owned(),
            "[mcp_servers.github] needs transport = \"stdio\" or \"http\" (line 1, column 1)",
        ),
        (
            "[mcp_servers.github]\ntransport = \"sse\"\n".to_owned(),
            "[mcp_servers.github] transport must be \"stdio\" or \"http\" (line 2, column 13)",
        ),
        (
            "[mcp_servers.github]\ntransport = \"stdio\"\n".to_owned(),
            "[mcp_servers.github] needs command for transport = \"stdio\" (line 1, column 1)",
        ),
        (
            format!("{stdio}url = \"https://x.dev\"\n"),
            "[mcp_servers.github] url does not apply to transport = \"stdio\" (line 4, column 7)",
        ),
        (
            format!("{http}command = \"gh\"\n"),
            "[mcp_servers.linear] command does not apply to transport = \"http\" (line 4, column 11)",
        ),
        (
            "[mcp_servers.linear]\ntransport = \"http\"\nurl = \"http://example.com/mcp\"\n"
                .to_owned(),
            "[mcp_servers.linear] url must be https://, or http:// to a loopback literal (127.0.0.1 or [::1]) (line 3, column 7)",
        ),
        (
            format!("{http}bearer_secret = \"Linear Token\"\n"),
            "[mcp_servers.linear] bearer_secret names a secret: secret name \"Linear Token\" must match [a-z][a-z0-9-]{0,31} (line 4, column 17)",
        ),
        (
            format!("{http}headers = {{ Authorization = \"Bearer x\" }}\n"),
            "[mcp_servers.linear] headers cannot carry Authorization; name a secret with bearer_secret or secret_headers (line 4, column 11)",
        ),
        (
            format!("{stdio}env = {{ \"BAD NAME\" = \"x\" }}\n"),
            "[mcp_servers.github] env key \"BAD NAME\" is not one of the valid environment names (line 4, column 7)",
        ),
        (
            format!("{stdio}env = {{ A = \"x\" }}\nsecret_env = {{ A = \"a\" }}\n"),
            "[mcp_servers.github] A is in both env and secret_env; keep one (line 5, column 14)",
        ),
        (
            format!("{stdio}tools = [\"a\", \"a\"]\n"),
            "[mcp_servers.github] tools lists \"a\" twice (line 4, column 9)",
        ),
        (
            format!("{stdio}startup_seconds = 0\n"),
            "[mcp_servers.github] startup_seconds must be an integer in 1..=120 (line 4, column 19)",
        ),
        (
            format!("{stdio}call_seconds = 3601\n"),
            "[mcp_servers.github] call_seconds must be an integer in 1..=3600 (line 4, column 16)",
        ),
    ] {
        let error = ProductConfig::parse(&text).unwrap_err();
        assert!(error.starts_with(expected), "{text}\n{error}");
    }
}

#[test]
fn templates_must_name_configured_servers_and_allowed_tools() {
    let mut config = ProductConfig::parse(FULL_SECTIONS).unwrap();
    with_template(&mut config, "t", "tools = [\"mcp:jira\"]\n");
    assert_eq!(
        config.validate().unwrap_err(),
        "template t: tools names mcp:jira, and config.toml has no [mcp_servers.jira]"
    );
    with_template(&mut config, "t", "tools = [\"mcp:github/delete_repo\"]\n");
    assert_eq!(
        config.validate().unwrap_err(),
        "template t: tools names mcp:github/delete_repo, which [mcp_servers.github] tools does not allow"
    );
    // A server with no allowlist admits any explicit tool name.
    with_template(
        &mut config,
        "t",
        "tools = [\"mcp:github/create_issue\", \"mcp:linear/anything\"]\n",
    );
    config.validate().unwrap();
}

#[test]
fn a_child_gets_at_most_its_parents_local_tools_on_every_spawn_edge() {
    let mut config = ProductConfig::parse(FULL_SECTIONS).unwrap();
    with_template(
        &mut config,
        "root",
        "tools = [\"fetch_url\", \"mcp:github\"]\n",
    );
    config.root_agent = Some("root".to_owned());
    config.working_set = WorkingSet::parse(vec!["kid".to_owned()]).unwrap();

    // Hosted retrieval is exempt; the allowlist expansion admits a subset.
    with_template(
        &mut config,
        "kid",
        "tools = [\"x_search\", \"web_search\", \"fetch_url\", \"mcp:github/create_issue\"]\n",
    );
    config.validate().unwrap();

    // mcp:github expands to the server allowlist on both sides.
    with_template(&mut config, "kid", "tools = [\"mcp:github\"]\n");
    config.validate().unwrap();

    // Root -> working set names the edge and the extras.
    with_template(
        &mut config,
        "kid",
        "tools = [\"image_gen\", \"mcp:linear\", \"web_fetch\"]\n",
    );
    assert_eq!(
        config.validate().unwrap_err(),
        "[roster] working_set template kid lists tools its root template root lacks: image_gen, mcp:linear; a child gets at most its parent's fetch_url, image_gen and mcp tools (web_search, web_fetch and x_search are exempt)"
    );

    // Without a root agent the root has no optional tools.
    config.root_agent = None;
    with_template(&mut config, "kid", "tools = [\"fetch_url\"]\n");
    assert!(
        config.validate().unwrap_err().contains(
            "lists tools the root (no [root] agent, so no optional tools) lacks: fetch_url"
        )
    );
    config.root_agent = Some("root".to_owned());

    // Template -> spawns: a wildcard child under a narrower parent.
    with_template(
        &mut config,
        "kid",
        "tools = [\"mcp:linear/list_issues\"]\nspawns = [\"leaf\"]\n",
    );
    config.templates.get_mut("root").unwrap().tools =
        TemplateToolSelection::parse(["fetch_url", "mcp:github", "mcp:linear/list_issues"])
            .unwrap();
    with_template(&mut config, "leaf", "tools = [\"mcp:linear\"]\n");
    assert_eq!(
        config.validate().unwrap_err(),
        "template kid spawns leaf, which lists tools kid lacks: mcp:linear; a child gets at most its parent's fetch_url, image_gen and mcp tools (web_search, web_fetch and x_search are exempt)"
    );
    // The explicit tool the parent has is fine; a wildcard parent covers it.
    with_template(
        &mut config,
        "leaf",
        "tools = [\"mcp:linear/list_issues\", \"x_search\"]\n",
    );
    config.validate().unwrap();

    // The same rule refuses `config set` before anything is written.
    with_template(&mut config, "wide", "tools = [\"mcp:linear\"]\n");
    let before = config.clone();
    assert!(
        config
            .set_key("roster.working_set", "kid,wide")
            .unwrap_err()
            .contains(
                "working_set template wide lists tools its root template root lacks: mcp:linear"
            )
    );
    assert_eq!(config, before);
}

#[test]
fn a_trusted_project_adds_and_replaces_tool_tables_by_name() {
    let temp = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"))
        .with_project_root(project.path().to_path_buf());
    paths.ensure_layout().unwrap();
    fs::write(paths.config_file(), FULL_SECTIONS).unwrap();
    let home = project.path().join(".plak");
    fs::create_dir_all(&home).unwrap();
    fs::write(
        home.join("config.toml"),
        "[tools.fetch_url]\nmax_bytes = 1\n[mcp_servers.github]\ntransport = \"stdio\"\ncommand = \"project-gh\"\n[mcp_servers.jira]\ntransport = \"http\"\nurl = \"http://127.0.0.1:9000/mcp\"\n",
    )
    .unwrap();

    // Untrusted: nothing applies.
    let untrusted = ProductConfig::load(&paths).unwrap();
    assert_eq!(untrusted.tool_sections.fetch_url.unwrap().max_bytes, 4096);
    assert!(!untrusted.tool_sections.mcp_servers.contains_key("jira"));

    project_trust::trust(&paths, project.path()).unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let sections = &config.tool_sections;
    assert_eq!(sections.fetch_url.unwrap().max_bytes, 1);
    // Replaced whole, not merged key by key: the home allowlist is gone.
    let github = &sections.mcp_servers["github"];
    assert_eq!(github.tools, None);
    assert!(matches!(
        &github.transport,
        McpTransport::Stdio { command, .. } if command == "project-gh"
    ));
    assert!(sections.mcp_servers.contains_key("jira"));
    // Home entries the project does not name stay.
    assert!(sections.mcp_servers.contains_key("linear"));
    assert_eq!(sections.web_search.as_ref().unwrap().max_uses, 3);
}

/// A `[root] agent` template's tools travel from the files to the session
/// bind: the policy fact carries them, and the root takes them only while it
/// runs that template's exact tuple. Binding them on the route is the
/// hosted half (`a_codex_root_template_selecting_x_search_refuses_the_native_bind`).
#[test]
fn the_root_template_fact_carries_its_tools_to_the_session_bind() {
    let temp = tempfile::tempdir().unwrap();
    let paths = PlakPaths::explicit(temp.path().join("home"));
    paths.ensure_layout().unwrap();
    fs::write(
        paths.user_agents_dir().join("root-agent.toml"),
        template_text("tools = [\"x_search\"]\n"),
    )
    .unwrap();
    fs::write(paths.config_file(), "[root]\nagent = \"root-agent\"\n").unwrap();
    let policy = SessionPolicy::load(&paths).unwrap();
    let fact = policy.root_template.unwrap();
    let selection = RootTemplateFact::root_tool_selection(Some(&fact), Some("codex"), Some("m"));
    let names: Vec<String> = selection
        .entries()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(names, ["x_search"]);
    assert!(
        RootTemplateFact::root_tool_selection(Some(&fact), Some("codex"), Some("other")).is_empty()
    );
    assert!(RootTemplateFact::root_tool_selection(None, Some("codex"), Some("m")).is_empty());
}
