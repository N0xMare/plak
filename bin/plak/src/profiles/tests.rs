use super::{ROLES, list, prepare};
use crate::home::paths::PlakPaths;
use crate::product_config::ProductConfig;
use std::fs;

fn home() -> (tempfile::TempDir, PlakPaths) {
    let (root, paths) = PlakPaths::isolated_test_home();
    fs::create_dir_all(paths.home().join("profiles")).unwrap();
    fs::create_dir_all(paths.user_agents_dir()).unwrap();
    (root, paths)
}

fn ids(config: &ProductConfig, paths: &PlakPaths) -> Vec<String> {
    list(config, paths.home())
        .rows
        .into_iter()
        .map(|row| row.id)
        .collect()
}

#[test]
fn builtins_are_listed_without_a_profiles_directory() {
    let (_root, paths) = PlakPaths::isolated_test_home();
    let view = list(&ProductConfig::default_product(), paths.home());
    assert_eq!(
        view.rows
            .iter()
            .map(|row| row.id.as_str())
            .collect::<Vec<_>>(),
        [
            "codex-default",
            "claude-default",
            "grok-default",
            "multi-default"
        ]
    );
    assert_eq!(view.diagnostics, [] as [String; 0]);
    assert!(
        view.rows[2]
            .cards
            .iter()
            .any(|card| card.label == "researcher" && card.model == "grok-4.7-build-fast")
    );
    let multi = &view.rows[3];
    assert_eq!(
        (multi.root.account.as_str(), multi.root.model.as_str()),
        ("claude", "claude-opus-5-5")
    );
    assert_eq!(
        multi
            .cards
            .iter()
            .filter(|card| card.depth == 0)
            .map(|card| (card.label.as_str(), card.account.as_str()))
            .collect::<Vec<_>>(),
        [
            ("worker", "codex"),
            ("reviewer", "claude"),
            ("researcher", "grok")
        ]
    );
}

/// Every built-in applies and saves as a loadable home whose roles under
/// the root are leaves: deeper trees are the user's to configure.
#[test]
fn every_builtin_saves_a_home_whose_roles_under_the_root_are_leaves() {
    for id in [
        "codex-default",
        "claude-default",
        "grok-default",
        "multi-default",
    ] {
        let (_root, paths) = home();
        let config = ProductConfig::load(&paths).unwrap();
        let prepared = prepare(&config, paths.home(), id).unwrap();
        prepared.config.save(&paths).unwrap();
        let reloaded = ProductConfig::load(&paths).unwrap();
        for role in ROLES {
            assert!(
                reloaded.templates[&format!("{id}-{role}")]
                    .spawns
                    .is_empty(),
                "{id}-{role} is a leaf"
            );
        }
        assert!(
            reloaded.templates[&format!("{id}-root")]
                .tools
                .entries()
                .is_empty(),
            "{id}: the root has core tools only"
        );
        assert_eq!(list(&reloaded, paths.home()).current.as_deref(), Some(id));
    }
}

#[test]
fn a_reserved_or_broken_user_file_is_reported_and_hides_nothing() {
    let (_root, paths) = home();
    let dir = paths.home().join("profiles");
    fs::write(dir.join("codex-default.toml"), "root = \"x\"\n").unwrap();
    fs::write(dir.join("broken.toml"), "root = \"x\"\nworking_set = [1]\n").unwrap();
    fs::write(
        dir.join("missing.toml"),
        "root = \"nope\"\nworking_set = [\"nope\"]\n",
    )
    .unwrap();
    fs::write(
        dir.join("extra.toml"),
        "root = \"x\"\nworking_set = [\"x\"]\nmodel = \"y\"\n",
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let view = list(&config, paths.home());
    assert_eq!(view.rows.len(), 4, "built-ins stay listed");
    assert_eq!(view.diagnostics.len(), 4, "{:?}", view.diagnostics);
    assert!(
        view.diagnostics
            .iter()
            .any(|line| line.contains("codex-default.toml") && line.contains("built-in"))
    );
    assert!(
        view.diagnostics
            .iter()
            .any(|line| line.contains("missing.toml") && line.contains("agents/nope.toml"))
    );
    assert!(prepare(&config, paths.home(), "missing").is_err());
}

#[test]
fn a_user_profile_owns_an_alias_stem_in_the_list_and_in_prepare() {
    let (_root, paths) = home();
    fs::write(
        paths.user_agents_dir().join("mine.toml"),
        "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"d\"\n",
    )
    .unwrap();
    fs::write(
        paths.home().join("profiles/codex.toml"),
        "root = \"mine\"\nworking_set = [\"mine\"]\n",
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let view = list(&config, paths.home());
    let builtin = view
        .rows
        .iter()
        .find(|row| row.id == "codex-default")
        .unwrap();
    assert_eq!(builtin.aliases, ["chatgpt"], "the user file owns `codex`");
    assert_eq!(
        ids(&config, &paths).last().map(String::as_str),
        Some("codex")
    );
    assert_eq!(prepare(&config, paths.home(), "codex").unwrap().id, "codex");
    assert_eq!(
        prepare(&config, paths.home(), "chatgpt").unwrap().id,
        "codex-default"
    );
}

/// A working-set card's own `spawns` are listed under it, indented one
/// level per hop, each template once.
#[test]
fn a_user_profile_lists_nested_children_under_their_parent() {
    let (_root, paths) = home();
    for (name, extra) in [
        (
            "lead",
            "spawns = [\"helper\", \"lead\"]\nmay_adopt = true\n",
        ),
        ("helper", "spawns = [\"leaf\"]\n"),
        ("leaf", ""),
        ("solo", ""),
    ] {
        fs::write(
            paths.user_agents_dir().join(format!("{name}.toml")),
            format!(
                "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
            ),
        )
        .unwrap();
    }
    fs::write(
        paths.home().join("profiles/nest.toml"),
        "root = \"solo\"\nworking_set = [\"lead\", \"solo\"]\n",
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let view = list(&config, paths.home());
    let row = view.rows.iter().find(|row| row.id == "nest").unwrap();
    assert_eq!(
        row.cards
            .iter()
            .map(|card| (card.label.as_str(), card.depth))
            .collect::<Vec<_>>(),
        [("lead", 0), ("helper", 1), ("leaf", 2), ("solo", 0)]
    );
}

/// A working-set card that names another working-set member shows that
/// name once under the parent, in config order, and does not walk it.
#[test]
fn a_working_set_sibling_is_a_reference_under_its_parent() {
    let (_root, paths) = home();
    for (name, extra) in [
        ("zeta", "spawns = [\"alpha\", \"zeta\"]\n"),
        ("alpha", "spawns = [\"note\"]\n"),
        ("note", ""),
        ("rootish", ""),
    ] {
        fs::write(
            paths.user_agents_dir().join(format!("{name}.toml")),
            format!(
                "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\ndescription = \"{name}\"\n{extra}"
            ),
        )
        .unwrap();
    }
    fs::write(
        paths.home().join("profiles/nest.toml"),
        "root = \"rootish\"\nworking_set = [\"zeta\", \"alpha\"]\n",
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let view = list(&config, paths.home());
    let row = view.rows.iter().find(|row| row.id == "nest").unwrap();
    assert_eq!(
        row.cards
            .iter()
            .map(|card| (card.label.as_str(), card.depth))
            .collect::<Vec<_>>(),
        [("zeta", 0), ("alpha", 1), ("alpha", 0), ("note", 1)]
    );
}

#[test]
fn prepare_writes_nothing_and_keeps_a_template_the_user_edited() {
    let (_root, paths) = home();
    let edited = "account = \"codex\"\nmodel = \"gpt-5.6-luna\"\neffort = \"low\"\ndescription = \"my worker\"\n";
    fs::write(
        paths.user_agents_dir().join("codex-default-worker.toml"),
        edited,
    )
    .unwrap();
    let config = ProductConfig::load(&paths).unwrap();
    let prepared = prepare(&config, paths.home(), "codex").unwrap();
    assert_eq!(prepared.id, "codex-default");
    assert_eq!(
        prepared.config.root_agent.as_deref(),
        Some("codex-default-root")
    );
    assert!(
        !paths
            .user_agents_dir()
            .join("codex-default-root.toml")
            .exists()
    );
    assert_eq!(
        prepared
            .config
            .templates
            .get("codex-default-worker")
            .map(|t| t.model.as_str()),
        Some("gpt-5.6-luna")
    );
    prepared.config.save(&paths).unwrap();
    assert!(
        paths
            .user_agents_dir()
            .join("codex-default-root.toml")
            .is_file()
    );
    assert_eq!(
        fs::read_to_string(paths.user_agents_dir().join("codex-default-worker.toml")).unwrap(),
        edited
    );
    let reloaded = ProductConfig::load(&paths).unwrap();
    assert_eq!(
        list(&reloaded, paths.home()).current.as_deref(),
        Some("codex-default")
    );
}
