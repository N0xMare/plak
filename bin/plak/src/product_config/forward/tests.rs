use std::fs;

use super::super::{
    LOCAL_RUNNER_NAME, ProductConfig, ResumeAfterLoss, RunnerConfig, init_if_missing,
};
use crate::home::paths::PlakPaths;
use plakous_hosted::frozen_tools::McpPlacement;

const SERVER: &str = "[mcp_servers.notes]\ntransport = \"stdio\"\ncommand = \"notes-mcp\"\n";

#[test]
fn the_forward_keys_parse_and_are_named_for_doctor() {
    let config = ProductConfig::parse(&format!(
        "[host]\nresume_after_loss = \"continue\"\n\n{SERVER}placement = \"runner\"\n\n\
         [runners.buildbox]\nssh = \"me@buildbox\"\n"
    ))
    .unwrap();
    assert_eq!(config.resume_after_loss, Some(ResumeAfterLoss::Continue));
    assert_eq!(
        config.tool_sections.mcp_servers["notes"].placement,
        Some(McpPlacement::Runner)
    );
    assert_eq!(
        config.runners["buildbox"],
        RunnerConfig {
            ssh: "me@buildbox".to_owned()
        }
    );
    assert_eq!(
        config.forward_only_keys(),
        [
            "[host] resume_after_loss",
            "[runners.buildbox]",
            "[mcp_servers.notes] placement"
        ]
    );
    let plain = ProductConfig::parse(SERVER).unwrap();
    assert_eq!(plain.resume_after_loss, None);
    assert_eq!(plain.tool_sections.mcp_servers["notes"].placement, None);
    assert_eq!(plain.forward_only_keys(), [] as [String; 0]);
}

#[test]
fn malformed_forward_keys_are_named_load_errors() {
    for (text, expected) in [
        (
            "[host]\nresume_after_loss = \"later\"\n",
            "hold or continue",
        ),
        ("[host]\nresume_after_loss = true\n", "must be a string"),
        (
            "[host]\nresume = \"hold\"\n",
            "unknown config key: host.resume",
        ),
        (
            &format!("{SERVER}placement = \"remote\"\n"),
            "placement must be",
        ),
        (
            "[mcp_servers.notes]\ntransport = \"http\"\nurl = \"https://mcp.example/mcp\"\n\
             placement = \"runner\"\n",
            "needs transport = \"stdio\"",
        ),
        ("[runners.buildbox]\n", "needs ssh"),
        (
            "[runners.buildbox]\nssh = \"-oProxyCommand=x\"\n",
            "not starting with -",
        ),
        (
            "[runners.buildbox]\nssh = \"me buildbox\"\n",
            "no whitespace",
        ),
        (
            "[runners.buildbox]\nssh = \"buildbox\"\nroots = []\n",
            "unknown config key",
        ),
        (
            "[runners.Buildbox]\nssh = \"buildbox\"\n",
            "not a valid runner name",
        ),
        (
            &format!("[runners.{LOCAL_RUNNER_NAME}]\nssh = \"buildbox\"\n"),
            "host's own runner",
        ),
    ] {
        let error = ProductConfig::parse(text).unwrap_err();
        assert!(error.contains(expected), "{text:?}: {error}");
    }
}

/// A file `plak` writes stays loadable by a 0.0.3 build until the operator
/// sets a forward key: `config init` and a fresh save never write `[host]`
/// live, a save of another key never adds one, and only an explicit set
/// does — even of the default.
#[test]
fn forward_keys_are_written_only_when_set_explicitly() {
    let (_tmp, paths) = PlakPaths::isolated_test_home();
    assert!(init_if_missing(&paths).unwrap());
    let initialized = fs::read_to_string(paths.config_file()).unwrap();
    assert!(initialized.contains("# [host]\n"), "{initialized}");
    assert!(!initialized.lines().any(|line| line == "[host]"));
    assert!(!ProductConfig::default_product().render().contains("[host]"));

    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("tui.scroll_lines", "5").unwrap();
    config.save(&paths).unwrap();
    let saved = fs::read_to_string(paths.config_file()).unwrap();
    assert!(!saved.lines().any(|line| line == "[host]"), "{saved}");
    assert!(
        !saved
            .lines()
            .any(|line| line.starts_with("resume_after_loss")),
        "{saved}"
    );

    let mut config = ProductConfig::load(&paths).unwrap();
    config.set_key("host.resume_after_loss", "hold").unwrap();
    config.save(&paths).unwrap();
    let saved = fs::read_to_string(paths.config_file()).unwrap();
    assert!(saved.contains("resume_after_loss = \"hold\""), "{saved}");
    let loaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(loaded.resume_after_loss, Some(ResumeAfterLoss::Hold));
    assert_eq!(loaded.forward_only_keys(), ["[host] resume_after_loss"]);

    // Clearing the key takes its now-empty table with it: a bare [host]
    // header is refused by a 0.0.3 build just the same.
    let mut config = loaded;
    config.set_key("host.resume_after_loss", "").unwrap();
    config.save(&paths).unwrap();
    let saved = fs::read_to_string(paths.config_file()).unwrap();
    assert!(
        !saved
            .lines()
            .any(|line| line.starts_with("resume_after_loss") || line.trim() == "[host]"),
        "{saved}"
    );
    assert_eq!(
        ProductConfig::load(&paths).unwrap().forward_only_keys(),
        [] as [String; 0]
    );

    let fresh = PlakPaths::isolated_test_home();
    let mut config = ProductConfig::default_product();
    config.set_key("tui.scroll_lines", "5").unwrap();
    config.save(&fresh.1).unwrap();
    let rendered = fs::read_to_string(fresh.1.config_file()).unwrap();
    assert!(!rendered.contains("[host]"), "{rendered}");
}

/// A placement is spelled in the server's canonical table only when set, so
/// a table without one renders and freezes exactly as before the key.
#[test]
fn a_server_table_spells_placement_only_when_set() {
    let plain = ProductConfig::parse(SERVER).unwrap();
    let rendered = plain.tool_sections.render_server("notes").unwrap();
    assert!(!rendered.contains("placement"), "{rendered}");
    let placed = ProductConfig::parse(&format!("{SERVER}placement = \"host\"\n")).unwrap();
    let rendered = placed.tool_sections.render_server("notes").unwrap();
    assert!(rendered.contains("placement = \"host\"\n"), "{rendered}");
    assert_eq!(ProductConfig::parse(&rendered).unwrap(), placed);
}

/// A project file cannot set the home-scoped forward keys; its overlay
/// ignores them with a warning.
#[test]
fn a_project_file_names_its_home_scoped_forward_keys() {
    let project = ProductConfig::parse(
        "[host]\nresume_after_loss = \"continue\"\n\n[runners.buildbox]\nssh = \"buildbox\"\n",
    )
    .unwrap();
    assert_eq!(
        project.forward_home_keys(),
        ["[host] resume_after_loss", "[runners.buildbox]"]
    );
}

/// A forward-only table is named while it is present at all, even empty or
/// hand-written: a 0.0.3 build refuses the header as much as a key.
#[test]
fn an_empty_forward_only_table_is_still_named() {
    let config = ProductConfig::parse("[host]\n\n[runners]\n").unwrap();
    assert_eq!(config.forward_only_keys(), ["[host]", "[runners]"]);
    let config = ProductConfig::parse("[runners.buildbox]\nssh = \"buildbox\"\n").unwrap();
    assert_eq!(config.forward_only_keys(), ["[runners.buildbox]"]);
}
